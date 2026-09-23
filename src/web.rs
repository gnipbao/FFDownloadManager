//! Loopback HTTP host. The task service can also be hosted by a desktop shell.
use crate::{
    service::{DownloadService, NewMediaTask, NewTask},
    test_server::{fixture_data, fixture_router, ServerConfig},
};
use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::{io::AsyncReadExt, net::TcpListener};

#[derive(Clone)]
struct WebState {
    service: Arc<DownloadService>,
    origin: String,
    demo_hash: String,
}

pub struct LocalWeb {
    pub service: Arc<DownloadService>,
    pub address: SocketAddr,
    pub router: Router,
    pub listener: TcpListener,
}

impl LocalWeb {
    pub async fn bind(port: u16, directory: PathBuf, state_directory: PathBuf) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .context("无法启动本地界面；请检查端口是否已被占用")?;
        let address = listener.local_addr()?;
        let service = DownloadService::open(&directory, &state_directory)?;
        let data = fixture_data(32 * 1024 * 1024);
        let demo_hash = format!("{:x}", Sha256::digest(&data));
        let mut config = ServerConfig::new(data);
        config.per_response_mbps = Some(6.0);
        let state = WebState {
            service: service.clone(),
            origin: format!("http://{address}"),
            demo_hash,
        };
        let router = Router::new()
            .route("/", get(index))
            .route("/app.js", get(app_js))
            .route("/api.js", get(api_js))
            .route("/style.css", get(style))
            .route("/favicon.svg", get(favicon))
            .route("/api/tasks", get(snapshot).post(create))
            .route("/api/filename", post(filename))
            .route("/api/media/resolve", post(resolve_media))
            .route("/api/media/tasks", post(create_media))
            .route("/api/demo", post(demo))
            .route("/api/pause-all", post(pause_all))
            .route("/api/tasks/{id}/pause", post(pause))
            .route("/api/tasks/{id}/resume", post(resume))
            .route("/api/tasks/{id}/reveal", post(reveal))
            .route("/api/tasks/{id}/file", get(file))
            .route("/api/tasks/{id}", axum::routing::delete(remove))
            .route("/api/folder/open", post(open_folder))
            .with_state(state.clone())
            .nest("/sample", fixture_router(config))
            .layer(DefaultBodyLimit::max(16 * 1024))
            .layer(middleware::from_fn_with_state(state, local_only));
        Ok(Self {
            service,
            address,
            router,
            listener,
        })
    }
}

pub async fn serve(port: u16, directory: PathBuf, state_directory: PathBuf) -> Result<()> {
    let web = LocalWeb::bind(port, directory, state_directory).await?;
    println!("FFDownload 已启动：http://{}", web.address);
    println!("下载位置：{}", web.service.directory().display());
    println!("按 Ctrl-C 保存下载进度并退出。");
    let service = web.service.clone();
    axum::serve(web.listener, web.router)
        .with_graceful_shutdown(async move {
            wait_for_exit().await;
            if let Err(error) = service.shutdown().await {
                eprintln!("保存进度失败：{error}");
            }
        })
        .await?;
    Ok(())
}

async fn wait_for_exit() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

async fn local_only(State(state): State<WebState>, request: Request, next: Next) -> Response {
    let port = state.origin.rsplit(':').next().unwrap();
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if host != format!("127.0.0.1:{port}") && host != format!("localhost:{port}") {
        return (StatusCode::FORBIDDEN, "Invalid local host").into_response();
    }
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok());
    if origin.is_some_and(|o| o != state.origin && o != format!("http://localhost:{port}"))
        || request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
    {
        return (
            StatusCode::FORBIDDEN,
            "Only the local UI can access this service",
        )
            .into_response();
    }
    // This header forces a preflight for cross-origin mutations. No CORS is enabled.
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        && request
            .headers()
            .get("x-ffdm-client")
            .is_none_or(|v| v != "local-ui")
    {
        return (StatusCode::FORBIDDEN, "Missing local client header").into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"));
    response
}

fn asset(content_type: &'static str, contents: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type)], contents)
}
async fn index() -> impl IntoResponse {
    asset(
        "text/html; charset=utf-8",
        include_str!("../web/index.html"),
    )
}
async fn app_js() -> impl IntoResponse {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../web/app.js"),
    )
}
async fn api_js() -> impl IntoResponse {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../web/api.js"),
    )
}
async fn style() -> impl IntoResponse {
    asset("text/css; charset=utf-8", include_str!("../web/style.css"))
}
async fn favicon() -> impl IntoResponse {
    asset("image/svg+xml", include_str!("../web/favicon.svg"))
}

struct ApiError(anyhow::Error);
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": self.0.to_string()})),
        )
            .into_response()
    }
}
type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

async fn snapshot(State(state): State<WebState>) -> Json<crate::service::Snapshot> {
    Json(state.service.snapshot())
}
async fn create(
    State(state): State<WebState>,
    Json(request): Json<NewTask>,
) -> ApiResult<crate::service::Task> {
    Ok(Json(state.service.create(request, false).await?))
}
#[derive(Deserialize)]
struct FilenameRequest {
    url: String,
}

async fn filename(
    State(state): State<WebState>,
    Json(request): Json<FilenameRequest>,
) -> ApiResult<crate::filename::FilenameInfo> {
    Ok(Json(state.service.suggest_filename(&request.url).await?))
}
async fn resolve_media(
    State(state): State<WebState>,
    Json(request): Json<FilenameRequest>,
) -> ApiResult<crate::media::MediaPreview> {
    Ok(Json(state.service.resolve_media(&request.url).await?))
}
async fn create_media(
    State(state): State<WebState>,
    Json(request): Json<NewMediaTask>,
) -> ApiResult<crate::service::Task> {
    Ok(Json(state.service.create_media(request).await?))
}
async fn demo(State(state): State<WebState>) -> ApiResult<crate::service::Task> {
    Ok(Json(
        state
            .service
            .create(
                NewTask {
                    url: format!("{}/sample/file.bin", state.origin),
                    filename: Some("FFDownload-体验文件.bin".into()),
                    connections: 4,
                    sha256: Some(state.demo_hash),
                },
                true,
            )
            .await?,
    ))
}
fn done() -> Json<serde_json::Value> {
    Json(json!({"ok":true}))
}
async fn pause(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state.service.pause(&id)?;
    Ok(done())
}
async fn pause_all(State(state): State<WebState>) -> ApiResult<serde_json::Value> {
    state.service.pause_all()?;
    Ok(done())
}
async fn resume(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state.service.resume(&id)?;
    Ok(done())
}
async fn remove(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    state.service.remove(&id)?;
    Ok(done())
}

async fn reveal_path(path: PathBuf, select: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut command = tokio::process::Command::new("/usr/bin/open");
        if select {
            command.arg("-R");
        }
        let status = command.arg(path).status().await?;
        anyhow::ensure!(status.success(), "无法打开 Finder");
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = tokio::process::Command::new("explorer.exe");
        if select {
            command.arg("/select,");
        }
        let status = command.arg(path).status().await?;
        anyhow::ensure!(status.success(), "无法打开资源管理器");
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (path, select);
        anyhow::bail!("当前平台不支持打开文件管理器")
    }
}
async fn reveal(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    reveal_path(state.service.output_path(&id)?, true).await?;
    Ok(done())
}
async fn open_folder(State(state): State<WebState>) -> ApiResult<serde_json::Value> {
    reveal_path(state.service.directory().into(), false).await?;
    Ok(done())
}
async fn file(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let path = state.service.output_path(&id)?;
    let file = tokio::fs::File::open(path)
        .await
        .context("无法打开下载文件")?;
    let size = file.metadata().await.context("无法读取下载文件")?.len();
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0; 256 * 1024];
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(read);
        Ok(Some((buffer, file)))
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_DISPOSITION, "attachment".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}
