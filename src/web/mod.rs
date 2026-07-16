mod assets;
mod dto;
mod events;
mod operations;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::info;
use uuid::Uuid;

use self::dto::{
    AddScanSourceRequest, BootstrapDto, ConfigDto, DiscardRequest, DiscardResultDto, ErrorDto,
    FolderSelectionDto, RepositoryPatchDto, RepositorySummaryDto, StartOperationRequest,
    StatisticsDto,
};
use self::events::EventBus;
use self::operations::{
    OperationManager, execute_operation, fetch_readiness, reusable_fetch_batch,
};
use crate::config::AppConfig;
use crate::db::Database;
use crate::git::ProxyConfig;

#[derive(Clone)]
pub struct WebState {
    events: EventBus,
    operations: OperationManager,
    revision: Arc<AtomicU64>,
    csrf_token: Arc<str>,
    port: u16,
    repository_ids: Arc<RwLock<HashSet<String>>>,
    proxy: ProxyConfig,
    security_check: bool,
}

impl WebState {
    fn new(port: u16, proxy: ProxyConfig, security_check: bool) -> anyhow::Result<Self> {
        Ok(Self {
            events: EventBus::new(),
            operations: OperationManager::new()?,
            revision: Arc::new(AtomicU64::new(1)),
            csrf_token: Arc::from(format!("{}{}", Uuid::new_v4(), Uuid::new_v4())),
            port,
            repository_ids: Arc::new(RwLock::new(HashSet::new())),
            proxy,
            security_check,
        })
    }

    /// 原子替换服务端已知 ID 集合，并返回本次真正消失的仓库。
    fn replace_repository_ids(&self, current: HashSet<String>) -> Vec<String> {
        let mut known = self
            .repository_ids
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = known.difference(&current).cloned().collect();
        *known = current;
        removed
    }
}

pub async fn serve(
    port: u16,
    open_browser: bool,
    proxy: ProxyConfig,
    security_check: bool,
) -> anyhow::Result<()> {
    if port == 0 {
        anyhow::bail!("Web 服务端口必须大于 0");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "getlatestrepo=info,tower_http=info".into()),
        )
        .with_target(false)
        .try_init()
        .ok();

    let state = WebState::new(port, proxy, security_check)?;
    let app = Router::new()
        .route("/api/v1/bootstrap", get(bootstrap))
        .route("/api/v1/config", get(get_config))
        .route("/api/v1/dialogs/folder", post(select_scan_source_folder))
        .route("/api/v1/sources", post(add_scan_source))
        .route("/api/v1/sources/{index}", delete(remove_scan_source))
        .route(
            "/api/v1/repositories/{repo_id}/discard",
            post(discard_repository_changes),
        )
        .route("/api/v1/events", get(events::event_stream))
        .route("/api/v1/operations", post(start_operation))
        .route(
            "/api/v1/operations/{operation_id}",
            delete(cancel_operation),
        )
        .route("/", get(assets::serve_index))
        .route("/{*path}", get(assets::serve_asset))
        // 默认谓词会排除 SSE，静态资源与 JSON 则按客户端能力使用 Brotli/Gzip。
        .layer(CompressionLayer::new())
        .layer(RequestBodyLimitLayer::new(64 * 1024))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            header::HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            header::HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
            ),
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            validate_local_request,
        ))
        .with_state(state.clone());

    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(address).await?;
    let url = format!("http://127.0.0.1:{port}");
    println!("✓ Web 控制台已启动：{url}");
    println!("ℹ 数据仅通过本机回环地址传输，按 Ctrl+C 停止服务");
    if open_browser {
        open_local_browser(&url);
    }
    info!(%address, "Web 控制台开始监听");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            while !crate::signal_handler::is_shutdown_requested() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await?;
    state.operations.shutdown_and_join();
    Ok(())
}

async fn validate_local_request(
    State(state): State<WebState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let expected = format!("127.0.0.1:{}", state.port);
    let localhost = format!("localhost:{}", state.port);
    let host_valid = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host == expected || host == localhost);
    if !host_valid {
        return error_response(StatusCode::FORBIDDEN, "拒绝非本机 Host 请求");
    }

    if let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        let origin_valid =
            origin == format!("http://{expected}") || origin == format!("http://{localhost}");
        if !origin_valid {
            return error_response(StatusCode::FORBIDDEN, "拒绝跨来源请求");
        }
    }
    next.run(request).await
}

async fn bootstrap(State(state): State<WebState>) -> Response {
    // 先读取事件水位，再读取数据库；并发发生的更新即使已进入快照，也可安全重放去重。
    let event_sequence = state.events.current_sequence().to_string();
    let revision = state.revision.load(Ordering::Acquire);
    let loaded = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let config = AppConfig::load()?;
        let repositories = if config.is_initialized() {
            Database::open()?.list_repositories()?
        } else {
            Vec::new()
        };
        let statistics = StatisticsDto::from(repositories.as_slice());
        let repositories = repositories
            .into_iter()
            .map(|repository| RepositorySummaryDto::from_repository(repository, revision))
            .collect::<Vec<_>>();
        let fetch_readiness = fetch_readiness(&Database::open()?)?;
        Ok((repositories, statistics, fetch_readiness))
    })
    .await;
    let (repositories, statistics, fetch_readiness) = match loaded {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("读取仓库状态失败：{error}"),
            );
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("仓库状态任务异常：{error}"),
            );
        }
    };
    state.replace_repository_ids(
        repositories
            .iter()
            .map(|repository| repository.repo_id.clone())
            .collect(),
    );
    Json(BootstrapDto {
        version: env!("CARGO_PKG_VERSION"),
        revision: revision.to_string(),
        event_sequence,
        csrf_token: state.csrf_token.to_string(),
        repositories,
        statistics,
        active_operation: state.operations.current(),
        fetch_readiness,
    })
    .into_response()
}

async fn get_config() -> Response {
    match tokio::task::spawn_blocking(|| AppConfig::load().map(ConfigDto::from)).await {
        Ok(Ok(config)) => Json(config).into_response(),
        Ok(Err(error)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("读取配置失败：{error}"),
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("配置任务异常：{error}"),
        ),
    }
}

/// 打开系统文件夹选择器。
///
/// macOS 的 `rfd` 在无窗口 CLI 进程中从 Tokio 请求线程调用会触发内部 panic；
/// 因此 macOS 固定调用系统自带的 AppleScript 选择器，其他平台继续使用 `rfd`。
#[cfg(target_os = "macos")]
fn open_scan_source_folder_dialog() -> anyhow::Result<Option<std::path::PathBuf>> {
    let output = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"选择 GetLatestRepo 扫描文件夹\")",
        ])
        .output()?;
    parse_macos_folder_dialog_result(output.status.success(), &output.stdout, &output.stderr)
}

#[cfg(target_os = "macos")]
fn parse_macos_folder_dialog_result(
    succeeded: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> anyhow::Result<Option<std::path::PathBuf>> {
    if succeeded {
        let path = std::str::from_utf8(stdout)?.trim();
        if path.is_empty() {
            anyhow::bail!("系统文件夹选择器返回了空路径");
        }
        return Ok(Some(std::path::PathBuf::from(path)));
    }

    let message = String::from_utf8_lossy(stderr);
    if message.contains("User canceled") || message.contains("(-128)") {
        return Ok(None);
    }
    anyhow::bail!("系统文件夹选择器失败：{}", message.trim());
}

#[cfg(not(target_os = "macos"))]
fn open_scan_source_folder_dialog() -> anyhow::Result<Option<std::path::PathBuf>> {
    Ok(rfd::FileDialog::new()
        .set_title("选择 GetLatestRepo 扫描文件夹")
        .pick_folder())
}

async fn select_scan_source_folder(State(state): State<WebState>, headers: HeaderMap) -> Response {
    if !valid_csrf(&state, &headers) {
        return error_response(StatusCode::FORBIDDEN, "CSRF 校验失败");
    }
    if has_active_operation(&state) {
        return error_response(StatusCode::LOCKED, "仓库批量操作正在运行，暂不能修改扫描源");
    }

    match tokio::task::spawn_blocking(open_scan_source_folder_dialog).await {
        Ok(Ok(Some(path))) => Json(FolderSelectionDto {
            path: path.to_string_lossy().into_owned(),
        })
        .into_response(),
        Ok(Ok(None)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("打开系统文件夹选择器失败：{error}"),
        ),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("系统文件夹选择任务异常：{error}"),
        ),
    }
}

async fn add_scan_source(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(request): Json<AddScanSourceRequest>,
) -> Response {
    if !valid_csrf(&state, &headers) {
        return error_response(StatusCode::FORBIDDEN, "CSRF 校验失败");
    }
    if has_active_operation(&state) {
        return error_response(StatusCode::LOCKED, "仓库批量操作正在运行，暂不能修改扫描源");
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<ConfigDto> {
        let mut config = AppConfig::load()?;
        let original = config.clone();
        let mut source = config.prepare_scan_source(std::path::Path::new(&request.path))?;
        let database = Database::open()?;
        let previous_database_source = database
            .list_scan_sources()?
            .into_iter()
            .find(|existing| existing.root_path == source.root_path);
        database.upsert_scan_source(&mut source)?;
        if let Err(save_error) = config.save() {
            let config_rollback = original.save();
            let database_rollback = if let Some(mut previous) = previous_database_source {
                database.upsert_scan_source(&mut previous)
            } else {
                database.delete_scan_source_by_path(&source.root_path)
            };
            if config_rollback.is_err() || database_rollback.is_err() {
                anyhow::bail!(
                    "保存配置失败：{save_error}；回滚也失败：配置={:?}，数据库={:?}",
                    config_rollback.err(),
                    database_rollback.err()
                );
            }
            return Err(save_error.into());
        }
        Ok(ConfigDto::from(config))
    })
    .await;
    match result {
        Ok(Ok(config)) => {
            state.events.publish(
                "resync.required",
                &serde_json::json!({
                    "reason": "config_changed"
                }),
            );
            (StatusCode::CREATED, Json(config)).into_response()
        }
        Ok(Err(error)) => error_response(StatusCode::BAD_REQUEST, &error.to_string()),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("新增扫描源任务异常：{error}"),
        ),
    }
}

async fn remove_scan_source(
    State(state): State<WebState>,
    Path(index): Path<usize>,
    headers: HeaderMap,
) -> Response {
    if !valid_csrf(&state, &headers) {
        return error_response(StatusCode::FORBIDDEN, "CSRF 校验失败");
    }
    if has_active_operation(&state) {
        return error_response(StatusCode::LOCKED, "仓库批量操作正在运行，暂不能修改扫描源");
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<ConfigDto> {
        let mut config = AppConfig::load()?;
        let original = config.clone();
        let removed = config.take_scan_source_by_index(index)?;
        let database = Database::open()?;
        database.delete_scan_source_by_path(&removed.root_path)?;
        if let Err(error) = config.save() {
            let mut restored = removed;
            restored.id = None;
            let config_rollback = original.save();
            let database_rollback = database.upsert_scan_source(&mut restored);
            if config_rollback.is_err() || database_rollback.is_err() {
                anyhow::bail!(
                    "保存配置失败：{error}；回滚也失败：配置={:?}，数据库={:?}",
                    config_rollback.err(),
                    database_rollback.err()
                );
            }
            return Err(error.into());
        }
        Ok(ConfigDto::from(config))
    })
    .await;
    match result {
        Ok(Ok(config)) => {
            state.events.publish(
                "resync.required",
                &serde_json::json!({
                    "reason": "config_changed"
                }),
            );
            Json(config).into_response()
        }
        Ok(Err(error)) => error_response(StatusCode::BAD_REQUEST, &error.to_string()),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("移除扫描源任务异常：{error}"),
        ),
    }
}

async fn discard_repository_changes(
    State(state): State<WebState>,
    Path(repo_id): Path<i64>,
    headers: HeaderMap,
    Json(request): Json<DiscardRequest>,
) -> Response {
    if !valid_csrf(&state, &headers) {
        return error_response(StatusCode::FORBIDDEN, "CSRF 校验失败");
    }
    if !request.confirmed {
        return error_response(StatusCode::BAD_REQUEST, "必须明确确认丢弃本地修改");
    }
    if state
        .operations
        .current()
        .is_some_and(|operation| operation.state.is_active())
    {
        return error_response(StatusCode::LOCKED, "仓库批量操作正在运行");
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
        let database = Database::open()?;
        let cached = database
            .get_repository_by_id(repo_id)?
            .ok_or_else(|| anyhow::anyhow!("仓库不存在"))?;
        let path = std::path::PathBuf::from(&cached.path);
        let canonical = path.canonicalize()?;
        let config = AppConfig::load()?;
        let in_enabled_source = config
            .scan_sources
            .iter()
            .filter(|source| source.enabled)
            .any(|source| {
                std::path::Path::new(&source.root_path)
                    .canonicalize()
                    .is_ok_and(|root| canonical.starts_with(root))
            });
        if !in_enabled_source {
            anyhow::bail!("仓库不在启用的扫描源内，拒绝修改");
        }
        let discarded = crate::git::GitOps::discard_changes(&canonical, true)?;
        let mut refreshed = crate::git::GitOps::inspect(&canonical, &cached.root_path)?;
        refreshed.id = cached.id;
        refreshed.last_fetch_at = cached.last_fetch_at;
        refreshed.last_pull_at = cached.last_pull_at;
        database.upsert_repository(&mut refreshed)?;
        Ok(discarded)
    })
    .await;
    match result {
        Ok(Ok(discarded_files)) => {
            if let Err(error) = publish_repository_snapshot(&state).await {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("修改已完成，但刷新状态失败：{error}"),
                );
            }
            Json(DiscardResultDto { discarded_files }).into_response()
        }
        Ok(Err(error)) => error_response(StatusCode::BAD_REQUEST, &error.to_string()),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("丢弃修改任务异常：{error}"),
        ),
    }
}

async fn start_operation(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(request): Json<StartOperationRequest>,
) -> Response {
    if !valid_csrf(&state, &headers) {
        return error_response(StatusCode::FORBIDDEN, "CSRF 校验失败");
    }
    if request.kind.requires_confirmation() && !request.confirmed {
        return error_response(StatusCode::BAD_REQUEST, "该操作必须明确确认后才能执行");
    }
    if request.request_id.is_empty() || request.request_id.len() > 128 {
        return error_response(StatusCode::BAD_REQUEST, "请求 ID 长度必须为 1–128 个字符");
    }
    match state
        .operations
        .existing_request(&request.request_id, request.kind)
    {
        Ok(Some(operation)) => return Json(operation).into_response(),
        Ok(None) => {}
        Err(error) => return error_response(StatusCode::CONFLICT, &error.to_string()),
    }
    let kind = request.kind;
    let scope = match tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let database = Database::open()?;
        if matches!(
            kind,
            dto::OperationKind::PullSafe
                | dto::OperationKind::PullForce
                | dto::OperationKind::PullBackup
        ) {
            let Some((batch, repositories)) = reusable_fetch_batch(&database)? else {
                anyhow::bail!("没有可用的远程状态结果，请先执行第一步");
            };
            Ok((repositories.len(), Some(batch.batch_id)))
        } else {
            Ok((database.list_repositories()?.len(), None))
        }
    })
    .await
    {
        Ok(Ok(scope)) => scope,
        Ok(Err(error)) => {
            return error_response(
                if request.kind.requires_confirmation() {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
                &error.to_string(),
            );
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("数据库任务异常：{error}"),
            );
        }
    };
    let (operation, created) =
        match state
            .operations
            .start(request.kind, scope.0, request.request_id, scope.1)
        {
            Ok(result) => result,
            Err(error) => return error_response(StatusCode::CONFLICT, &error.to_string()),
        };
    if !created {
        return Json(operation).into_response();
    }
    state.events.publish("operation.patch", &operation);
    let operation_for_thread = operation.clone();
    let thread_state = state.clone();
    let thread_result = std::thread::Builder::new()
        .name(format!("glr-web-{}", operation.operation_id))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    runtime.block_on(execute_operation(thread_state, operation_for_thread))
                }
                Err(error) => {
                    let message = format!("无法创建 Web 操作运行时：{error}");
                    if let Ok(operation) =
                        thread_state.operations.mark_start_failed(message.clone())
                    {
                        thread_state.events.publish("operation.patch", &operation);
                    }
                    eprintln!("✗ {message}");
                }
            }
        });
    match thread_result {
        Ok(worker) => {
            if let Err(error) = state.operations.register_worker(worker) {
                if let Ok(operation) = state
                    .operations
                    .mark_start_failed(format!("无法登记操作线程：{error}"))
                {
                    state.events.publish("operation.patch", &operation);
                }
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("无法登记操作线程：{error}"),
                );
            }
        }
        Err(error) => {
            if let Ok(operation) = state
                .operations
                .mark_start_failed(format!("无法启动操作线程：{error}"))
            {
                state.events.publish("operation.patch", &operation);
            }
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("无法启动操作线程：{error}"),
            );
        }
    }
    (StatusCode::ACCEPTED, Json(operation)).into_response()
}

async fn cancel_operation(
    State(state): State<WebState>,
    Path(operation_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !valid_csrf(&state, &headers) {
        return error_response(StatusCode::FORBIDDEN, "CSRF 校验失败");
    }
    match state.operations.request_cancel(&operation_id) {
        Ok(operation) => {
            state.events.publish("operation.patch", &operation);
            StatusCode::ACCEPTED.into_response()
        }
        Err(error) => error_response(StatusCode::CONFLICT, &error.to_string()),
    }
}

fn valid_csrf(state: &WebState, headers: &HeaderMap) -> bool {
    headers
        .get("x-getlatestrepo-csrf")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == state.csrf_token.as_ref())
}

fn has_active_operation(state: &WebState) -> bool {
    state
        .operations
        .current()
        .is_some_and(|operation| operation.state.is_active())
}

pub async fn publish_repository_snapshot(state: &WebState) -> anyhow::Result<()> {
    let revision = state.revision.fetch_add(1, Ordering::AcqRel) + 1;
    let (repositories, statistics) = tokio::task::spawn_blocking(move || {
        let repositories = Database::open()?.list_repositories()?;
        let statistics = StatisticsDto::from(repositories.as_slice());
        let repositories = repositories
            .into_iter()
            .map(|repository| RepositorySummaryDto::from_repository(repository, revision))
            .collect::<Vec<_>>();
        Ok::<_, anyhow::Error>((repositories, statistics))
    })
    .await??;
    let current_ids = repositories
        .iter()
        .map(|repository| repository.repo_id.clone())
        .collect();
    let removes = state.replace_repository_ids(current_ids);
    state.events.publish(
        "repositories.patch",
        &RepositoryPatchDto {
            upserts: repositories,
            removes,
        },
    );
    state.events.publish("statistics.replace", &statistics);
    Ok(())
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ErrorDto {
            message: message.to_string(),
        }),
    )
        .into_response()
}

fn open_local_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台不支持自动打开浏览器",
    ));
    if let Err(error) = result {
        eprintln!("⚠ 无法自动打开浏览器：{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_token_has_sufficient_entropy_material() {
        let state = WebState::new(38427, ProxyConfig::default(), true).unwrap();
        assert!(state.csrf_token.len() >= 64);
        assert_ne!(
            state.csrf_token.as_ref(),
            WebState::new(38427, ProxyConfig::default(), true)
                .unwrap()
                .csrf_token
                .as_ref()
        );
    }

    #[test]
    fn web_state_preserves_proxy_and_security_options() {
        let proxy = ProxyConfig {
            enabled: true,
            http_proxy: "http://127.0.0.1:19080".to_string(),
            https_proxy: "http://127.0.0.1:19443".to_string(),
        };

        let state = WebState::new(38427, proxy, false).unwrap();

        assert!(state.proxy.enabled);
        assert_eq!(state.proxy.https_proxy, "http://127.0.0.1:19443");
        assert!(!state.security_check);
    }

    #[test]
    fn operation_manager_rejects_parallel_top_level_operations() {
        let manager = OperationManager::new_for_test();
        manager
            .start(
                dto::OperationKind::Scan,
                2,
                "scan-request".to_string(),
                None,
            )
            .unwrap();
        assert!(
            manager
                .start(
                    dto::OperationKind::Fetch,
                    2,
                    "fetch-request".to_string(),
                    None,
                )
                .is_err()
        );
    }

    #[test]
    fn destructive_web_operations_require_confirmation() {
        assert!(dto::OperationKind::PullSafe.requires_confirmation());
        assert!(dto::OperationKind::PullForce.requires_confirmation());
        assert!(dto::OperationKind::PullBackup.requires_confirmation());
        assert!(!dto::OperationKind::Fetch.requires_confirmation());
    }

    #[test]
    fn operation_request_accepts_frontend_camel_case_request_id() {
        let request: dto::StartOperationRequest = serde_json::from_value(serde_json::json!({
            "kind": "fetch",
            "requestId": "browser-request",
            "confirmed": false
        }))
        .unwrap();

        assert_eq!(request.request_id, "browser-request");
    }

    #[test]
    fn repository_id_snapshot_reports_only_removed_ids() {
        let state = WebState::new(38427, ProxyConfig::default(), true).unwrap();
        assert!(
            state
                .replace_repository_ids(HashSet::from(["1".to_string(), "2".to_string()]))
                .is_empty()
        );

        let removed = state.replace_repository_ids(HashSet::from(["2".to_string()]));
        assert_eq!(removed, vec!["1"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_folder_dialog_parses_selected_path() {
        let selected =
            parse_macos_folder_dialog_result(true, b"/Users/example/Repositories/\n", b"").unwrap();

        assert_eq!(
            selected,
            Some(std::path::PathBuf::from("/Users/example/Repositories/"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_folder_dialog_treats_user_cancellation_as_no_selection() {
        let selected = parse_macos_folder_dialog_result(
            false,
            b"",
            b"execution error: User canceled. (-128)\n",
        )
        .unwrap();

        assert_eq!(selected, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_folder_dialog_reports_real_failures() {
        let error =
            parse_macos_folder_dialog_result(false, b"", "权限不足".as_bytes()).unwrap_err();

        assert!(error.to_string().contains("权限不足"));
    }
}
