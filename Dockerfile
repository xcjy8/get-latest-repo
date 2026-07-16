# syntax=docker/dockerfile:1.7

ARG NODE_VERSION=24.18.0
ARG RUST_VERSION=1.97.0

FROM node:${NODE_VERSION}-alpine AS frontend-build
WORKDIR /build/frontend
RUN corepack enable
COPY frontend/package.json frontend/pnpm-lock.yaml frontend/pnpm-workspace.yaml ./
RUN --mount=type=cache,target=/root/.cache/pnpm \
    corepack pnpm install --frozen-lockfile
COPY frontend/ ./
RUN corepack pnpm build

FROM rust:${RUST_VERSION}-bookworm AS backend-build
RUN apt-get update \
    && apt-get install --yes --no-install-recommends libssl-dev libwayland-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ ./src/
COPY templates/ ./templates/
COPY --from=frontend-build /build/frontend/dist ./frontend/dist/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked \
    && cp /build/target/release/getlatestrepo /tmp/getlatestrepo

FROM debian:bookworm-slim AS backend
ARG APP_UID=1000
ARG APP_GID=1000
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates curl dumb-init git libwayland-client0 openssh-client tzdata \
    && rm -rf /var/lib/apt/lists/* \
    # macOS 的主组 GID 常与 Debian 内置组冲突；复用并重命名已有组，保持挂载目录权限一致。 \
    && existing_group="$(getent group "${APP_GID}" | cut -d: -f1 || true)" \
    && if [ -n "${existing_group}" ]; then \
         [ "${existing_group}" = "getlatestrepo" ] || groupmod --new-name getlatestrepo "${existing_group}"; \
       else \
         groupadd --gid "${APP_GID}" getlatestrepo; \
       fi \
    # 同理复用冲突 UID；容器内始终统一为无登录权限的 getlatestrepo 用户。 \
    && existing_user="$(getent passwd "${APP_UID}" | cut -d: -f1 || true)" \
    && if [ -n "${existing_user}" ]; then \
         if [ "${existing_user}" != "getlatestrepo" ]; then \
           usermod --login getlatestrepo --home /home/getlatestrepo --gid "${APP_GID}" --shell /usr/sbin/nologin "${existing_user}"; \
         fi; \
       else \
         useradd --uid "${APP_UID}" --gid "${APP_GID}" --create-home --shell /usr/sbin/nologin getlatestrepo; \
       fi \
    && git config --system --add safe.directory '*'
COPY --from=backend-build /tmp/getlatestrepo /usr/local/bin/getlatestrepo
COPY --chmod=0755 docker/backend-entrypoint.sh /usr/local/bin/backend-entrypoint.sh
RUN mkdir -p /data /repositories /home/getlatestrepo/.cache \
    && chown -R getlatestrepo:getlatestrepo /data /repositories /home/getlatestrepo
USER getlatestrepo:getlatestrepo
ENV HOME=/home/getlatestrepo \
    XDG_CACHE_HOME=/data/cache \
    GETLATESTREPO_CONFIG_DIR=/data \
    GETLATESTREPO_CONTAINER_SCAN_ROOT=/repositories \
    GETLATESTREPO_BACKEND_PORT=8615 \
    RUST_LOG=getlatestrepo=info,tower_http=warn
EXPOSE 8615
ENTRYPOINT ["/usr/bin/dumb-init", "--", "/usr/local/bin/backend-entrypoint.sh"]

FROM nginxinc/nginx-unprivileged:1.29-alpine AS frontend
COPY docker/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=frontend-build /build/frontend/dist/ /usr/share/nginx/html/
EXPOSE 8080
ENTRYPOINT ["/usr/sbin/nginx"]
CMD ["-g", "daemon off;"]
