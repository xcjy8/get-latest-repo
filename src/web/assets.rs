use axum::body::Body;
use axum::extract::Path;
use axum::http::{Response, StatusCode, header};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "frontend/dist/"]
struct FrontendAssets;

/// 仅无扩展名的前端路由回退到 index.html；API 与缺失静态资源绝不伪装成页面。
pub async fn serve_asset(Path(path): Path<String>) -> Response<Body> {
    let normalized = path.trim_start_matches('/');
    let requested = if normalized.is_empty() {
        "index.html"
    } else {
        normalized
    };
    // 同时记录最终命中的资源名；前端路由回退后必须按 HTML 设置响应头。
    let resolved = FrontendAssets::get(requested)
        .map(|asset| (requested, asset))
        .or_else(|| {
            if std::path::Path::new(requested).extension().is_none() {
                FrontendAssets::get("index.html").map(|asset| ("index.html", asset))
            } else {
                None
            }
        });

    let Some((resolved_name, asset)) = resolved else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from("未找到静态资源"))
            .expect("固定响应必须可构造");
    };

    let content_type = mime_guess::from_path(resolved_name)
        .first_or_octet_stream()
        .to_string();
    let cache_control = if resolved_name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(asset.data))
        .expect("固定响应必须可构造")
}

pub async fn serve_index() -> Response<Body> {
    serve_asset(Path(String::new())).await
}
