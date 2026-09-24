//! Opt-in loopback resource capture. Media bodies are forwarded without buffering;
//! only bounded video metadata is inspected. Application proxy changes are opt-in.
use crate::media::{Assembly, MediaPlan, MediaStream};
use crate::{
    capture_ca::CaptureCa,
    capture_system::{Backend, SystemProxy},
    capture_transport::DownloadForwarder,
};
use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{header, HeaderMap, Method, Request, Response, StatusCode},
    rustls::crypto::aws_lc_rs,
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use reqwest::Url;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{oneshot, Mutex as AsyncMutex, Semaphore},
    task::JoinHandle,
};

const MAX_RESOURCES: usize = 200;
const MAX_METADATA: usize = 1024 * 1024;
const TTL: u64 = 30 * 60 * 1000;

#[derive(Clone, Serialize)]
pub struct Resource {
    pub id: String,
    pub filename: String,
    pub host: String,
    pub mime: String,
    pub kind: String,
    pub platform: String,
    pub bytes: Option<u64>,
    pub supports_range: bool,
    pub encrypted: bool,
    pub has_key: bool,
    pub downloadable: bool,
    pub note: String,
    pub captured_at: u64,
    pub first_seen_at: u64,
    pub request_count: u64,
    pub detail_at: Option<u64>,
    pub title: Option<String>,
}
#[derive(Clone)]
struct Entry {
    public: Resource,
    url: String,
    headers: Vec<(String, String)>,
    key: Option<u64>,
    extension: String,
}
#[derive(Default)]
struct Catalog {
    entries: Vec<Entry>,
    keys: HashMap<String, u64>,
    generation: u64,
    active: bool,
    observed: u64,
    connections: u64,
    errors: u64,
    merged: u64,
    wechat_requests: u64,
    wechat_pages: u64,
    adapted_scripts: u64,
    bridge_reports: u64,
    wechat_diagnostics: crate::capture_wechat::Diagnostics,
    bridge_token: String,
}
#[derive(Serialize)]
pub struct CaptureSnapshot {
    pub running: bool,
    pub proxy: Option<String>,
    pub browser_available: bool,
    pub browser: Option<crate::capture_browser::BrowserStatus>,
    pub resources: Vec<Resource>,
    pub observed: u64,
    pub connections: u64,
    pub errors: u64,
    pub system_proxy: Option<String>,
    pub recovery_pending: bool,
    pub upstream: Option<String>,
    pub proxy_issue: Option<String>,
    pub merged: u64,
    pub wechat_requests: u64,
    pub wechat_pages: u64,
    pub adapted_scripts: u64,
    pub bridge_reports: u64,
    pub wechat_diagnostics: crate::capture_wechat::Diagnostics,
}
struct Runtime {
    address: String,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
    spki: String,
    upstream: Option<String>,
    browser: Option<crate::capture_browser::CaptureBrowser>,
}
pub struct CaptureService {
    catalog: Arc<Mutex<Catalog>>,
    runtime: AsyncMutex<Option<Runtime>>,
    control: AsyncMutex<()>,
    system: Arc<AsyncMutex<SystemProxy>>,
    forwarders: AsyncMutex<Vec<DownloadForwarder>>,
    ca_directory: PathBuf,
    profile: PathBuf,
    local_origin: String,
}
impl CaptureService {
    pub fn new(profile: PathBuf, local_origin: String) -> Arc<Self> {
        let directory = profile.parent().unwrap_or(std::path::Path::new("."));
        Arc::new(Self {
            catalog: Arc::new(Mutex::new(Catalog::default())),
            runtime: AsyncMutex::new(None),
            control: AsyncMutex::new(()),
            system: Arc::new(AsyncMutex::new(SystemProxy::new(
                directory.join("capture-system-proxy.json"),
            ))),
            forwarders: AsyncMutex::new(Vec::new()),
            ca_directory: directory.join("capture-ca"),
            profile,
            local_origin,
        })
    }
    pub async fn snapshot(&self) -> CaptureSnapshot {
        let (system_proxy, recovery_pending, proxy_issue) = {
            let mut system = self.system.lock().await;
            (
                system.active_service(),
                system.recovery_pending(),
                system.connection_issue().await,
            )
        };
        let runtime = self.runtime.lock().await;
        let mut catalog = self.catalog.lock().unwrap();
        catalog
            .entries
            .retain(|e| e.public.captured_at + TTL > crate::service::now_ms());
        CaptureSnapshot {
            running: runtime.as_ref().is_some_and(|r| !r.task.is_finished()),
            proxy: runtime.as_ref().map(|r| r.address.clone()),
            browser_available: browser_binary().is_some(),
            browser: runtime
                .as_ref()
                .and_then(|r| r.browser.as_ref().map(|b| b.status())),
            resources: catalog
                .entries
                .iter()
                .rev()
                .map(|e| e.public.clone())
                .collect(),
            observed: catalog.observed,
            connections: catalog.connections,
            errors: catalog.errors,
            system_proxy,
            recovery_pending,
            upstream: runtime.as_ref().and_then(|r| r.upstream.clone()),
            proxy_issue,
            merged: catalog.merged,
            wechat_requests: catalog.wechat_requests,
            wechat_pages: catalog.wechat_pages,
            adapted_scripts: catalog.adapted_scripts,
            bridge_reports: catalog.bridge_reports,
            wechat_diagnostics: catalog.wechat_diagnostics.clone(),
        }
    }
    pub async fn start(&self) -> Result<()> {
        let _control = self.control.lock().await;
        self.start_inner(None).await
    }
    async fn start_inner(&self, upstream: Option<String>) -> Result<()> {
        let mut runtime = self.runtime.lock().await;
        if runtime.as_ref().is_some_and(|r| !r.task.is_finished()) {
            return Ok(());
        }
        ensure!(runtime.is_none(), "捕获服务已退出，请先停止后重试");
        let identity = CaptureCa::load_or_create(&self.ca_directory)?;
        let spki = identity.spki;
        let ca = RcgenAuthority::new(identity.issuer, 128, aws_lc_rs::default_provider());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("无法启动捕获代理")?;
        let address = listener.local_addr()?.to_string();
        let (stop, wait) = oneshot::channel();
        let bridge_token = format!(
            "{:x}",
            Sha256::digest(rcgen::KeyPair::generate()?.serialize_der())
        );
        let handler = CaptureHandler {
            catalog: self.catalog.clone(),
            local_origin: self.local_origin.clone(),
            request: None,
            bridge_token: bridge_token.clone(),
            slots: Arc::new(Semaphore::new(4)),
            script_slots: Arc::new(Semaphore::new(4)),
            upstream: upstream
                .as_deref()
                .map(|url| {
                    Ok::<_, anyhow::Error>((
                        crate::capture_transport::client(Some(url))?,
                        crate::capture_transport::websocket_client(url)?,
                    ))
                })
                .transpose()?,
            upstream_url: upstream.clone(),
        };
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(ca)
            .with_rustls_connector(aws_lc_rs::default_provider())
            .with_http_handler(handler)
            .with_graceful_shutdown(async {
                let _ = wait.await;
            })
            .build()
            .context("无法配置捕获代理")?;
        {
            let mut catalog = self.catalog.lock().unwrap();
            // Starting a new session must not show old browser/preload results.
            let generation = catalog.generation + 1;
            *catalog = Catalog {
                active: true,
                generation,
                bridge_token,
                ..Default::default()
            };
        }
        let catalog = self.catalog.clone();
        let system = self.system.clone();
        let task = tokio::spawn(async move {
            if proxy.start().await.is_err() {
                catalog.lock().unwrap().errors += 1;
            }
            catalog.lock().unwrap().active = false;
            // An unexpected listener failure must not strand the OS on a dead port.
            if let Err(error) = system.lock().await.restore().await {
                eprintln!("捕获退出后恢复代理失败：{error}");
            }
        });
        *runtime = Some(Runtime {
            address,
            stop,
            task,
            spki,
            upstream,
            browser: None,
        });
        Ok(())
    }
    pub async fn stop(&self) -> Result<()> {
        let _control = self.control.lock().await;
        self.system.lock().await.restore().await?;
        crate::capture_transport::set_download_proxy(None);
        self.stop_inner().await;
        Ok(())
    }
    async fn stop_inner(&self) {
        let mut runtime = self.runtime.lock().await;
        {
            let mut c = self.catalog.lock().unwrap();
            c.active = false;
            c.generation += 1;
        }
        if let Some(mut r) = runtime.take() {
            if let Some(browser) = r.browser.take() {
                browser.stop().await;
            }
            let _ = r.stop.send(());
            if tokio::time::timeout(Duration::from_secs(2), &mut r.task)
                .await
                .is_err()
            {
                r.task.abort();
                let _ = r.task.await;
            }
        }
    }
    pub fn clear(&self) {
        let mut c = self.catalog.lock().unwrap();
        c.entries.clear();
        c.keys.clear();
        c.observed = 0;
        c.connections = 0;
        c.errors = 0;
        c.merged = 0;
        c.wechat_requests = 0;
        c.wechat_pages = 0;
        c.adapted_scripts = 0;
        c.bridge_reports = 0;
        c.wechat_diagnostics = Default::default();
        c.generation += 1;
    }
    pub async fn certificate(&self) -> Result<String> {
        let _control = self.control.lock().await;
        Ok(CaptureCa::load_or_create(&self.ca_directory)?.certificate)
    }
    pub async fn recover_system_proxy(&self) -> Result<()> {
        let _control = self.control.lock().await;
        self.system.lock().await.restore().await?;
        crate::capture_transport::set_download_proxy(None);
        Ok(())
    }
    pub async fn application_setup(&self, selected: Option<&str>) -> Result<serde_json::Value> {
        let _control = self.control.lock().await;
        let ca = CaptureCa::load_or_create(&self.ca_directory)?;
        let supported = cfg!(target_os = "macos");
        let trusted = crate::capture_system::certificate_trusted(&ca.path).await;
        let system = self.system.lock().await;
        let services = if supported {
            system.backend.services().await?
        } else {
            vec![]
        };
        let service = system
            .active_service()
            .or_else(|| {
                selected
                    .filter(|s| services.iter().any(|n| n == s))
                    .map(str::to_owned)
            })
            .or_else(|| services.iter().find(|s| s.as_str() == "Wi-Fi").cloned())
            .or_else(|| services.first().cloned());
        let (upstream, error) = if system.active_service().is_some() {
            (system.original_upstream(), None)
        } else if let Some(service) = &service {
            match system
                .backend
                .read(service)
                .await
                .and_then(|n| n.upstream())
            {
                Ok(upstream) => (upstream, None),
                Err(e) => (None, Some(e.to_string())),
            }
        } else {
            (None, None)
        };
        Ok(
            serde_json::json!({"supported":supported, "services":services, "service":service,
            "trusted":trusted, "certificate_name":ca.name, "fingerprint":ca.fingerprint,
            "upstream":upstream, "error":error, "recovery_pending":system.recovery_pending()}),
        )
    }
    pub async fn open_certificate(&self) -> Result<()> {
        let _control = self.control.lock().await;
        ensure!(
            cfg!(target_os = "macos"),
            "请下载证书并在系统证书管理器中导入"
        );
        let ca = CaptureCa::load_or_create(&self.ca_directory)?;
        crate::capture_system::run("/usr/bin/open", &[&ca.path.to_string_lossy()]).await?;
        Ok(())
    }
    pub async fn start_application(&self, service: &str) -> Result<()> {
        let _control = self.control.lock().await;
        let ca = CaptureCa::load_or_create(&self.ca_directory)?;
        ensure!(
            crate::capture_system::certificate_trusted(&ca.path).await,
            "请先在钥匙串中信任本机 FFDownload 捕获证书，再刷新状态"
        );
        let before = self.system.lock().await.prepare(service).await?;
        let upstream = before.upstream()?;
        self.stop_inner().await;
        self.start_inner(upstream.clone()).await?;
        let port = self
            .runtime
            .lock()
            .await
            .as_ref()
            .context("代理启动失败")?
            .address
            .parse::<std::net::SocketAddr>()?
            .port();
        let mut forwarders = self.forwarders.lock().await;
        let download_route =
            if let Some(existing) = forwarders.iter().find(|f| f.upstream == upstream) {
                existing.url.clone()
            } else {
                let forwarder = DownloadForwarder::start(upstream).await?;
                let url = forwarder.url.clone();
                // Keep old forwarders alive for downloads already in flight after Stop.
                forwarders.push(forwarder);
                url
            };
        drop(forwarders);
        crate::capture_transport::set_download_proxy(Some(download_route));
        let result = self.system.lock().await.enable(before, port).await;
        if result.is_err() {
            crate::capture_transport::set_download_proxy(None);
            if !self.system.lock().await.recovery_pending() {
                self.stop_inner().await;
            }
        }
        result
    }
    pub async fn open_browser(&self, raw: &str) -> Result<()> {
        self.open_browser_mode(raw, false).await
    }
    pub async fn open_browser_mode(&self, raw: &str, visible: bool) -> Result<()> {
        let _control = self.control.lock().await;
        ensure!(
            !self.system.lock().await.recovery_pending(),
            "请先停止应用抓包并恢复原代理"
        );
        let mut url = crate::filename::parse_url(raw)?;
        if url
            .host_str()
            .is_some_and(|h| h == "douyin.com" || h.ends_with(".douyin.com"))
        {
            if let Some(id) = url
                .query_pairs()
                .find(|(k, v)| {
                    k == "modal_id" && !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())
                })
                .map(|(_, v)| v.into_owned())
            {
                url = Url::parse(&format!("https://www.douyin.com/video/{id}"))?;
            }
        }
        ensure!(
            allowed_target(&url, &self.local_origin),
            "捕获浏览器只支持公网网页和本机捕获测试页"
        );
        let binary = browser_binary()
            .context("需要安装 Google Chrome 或 Microsoft Edge 才能捕获网页媒体")?;
        self.start_inner(None).await?;
        let mut runtime = self.runtime.lock().await;
        let r = runtime.as_mut().context("请先启动资源捕获")?;
        ensure!(!r.task.is_finished(), "捕获服务已停止，请重新启动");
        if let Some(browser) = r.browser.take() {
            browser.stop().await;
        }
        r.browser = Some(
            crate::capture_browser::CaptureBrowser::launch(
                &binary,
                &self.profile,
                &r.address,
                &r.spki,
                url.as_str(),
                visible,
            )
            .await?,
        );
        Ok(())
    }
    pub fn select(&self, id: &str, key: Option<&str>) -> Result<MediaPlan> {
        let c = self.catalog.lock().unwrap();
        let e = c
            .entries
            .iter()
            .find(|e| e.public.id == id && e.public.captured_at + TTL > crate::service::now_ms())
            .context("捕获结果已过期，请重新播放视频")?;
        ensure!(e.public.downloadable, "{}", e.public.note);
        let key = match key.map(str::trim).filter(|k| !k.is_empty()) {
            Some(k) => {
                ensure!(e.public.encrypted, "此资源不需要视频号解密密钥");
                Some(
                    k.parse::<u64>()
                        .context("decodeKey 必须是完整的无符号整数")?,
                )
            }
            None => e.key,
        }
        .filter(|k| *k != 0);
        ensure!(
            !e.public.encrypted || key.is_some(),
            "尚未取得此视频的解密信息。请在微信重新打开视频详情并播放，等待自动关联完成"
        );
        let plan = MediaPlan {
            source_url: {
                let mut url = Url::parse(&e.url)?;
                url.set_query(None);
                url.set_fragment(None);
                url.into()
            },
            source_id: id.into(),
            platform: if e.public.encrypted {
                "WeChat Channels"
            } else {
                "Rust Capture"
            }
            .into(),
            title: e
                .public
                .filename
                .trim_end_matches(&format!(".{}", e.extension))
                .into(),
            format_id: id.into(),
            label: format!("捕获 · {}", e.extension.to_uppercase()),
            extension: e.extension.clone(),
            assembly: Assembly::Direct,
            streams: vec![MediaStream {
                url: e.url.clone(),
                headers: e.headers.clone(),
                size: e.public.bytes,
                decrypt_key: key,
            }],
            extracted_at: e.public.captured_at,
        };
        plan.validate()?;
        Ok(plan)
    }
}
fn browser_binary() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let candidates: Vec<PathBuf> = vec![
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into(),
        "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
    ];
    #[cfg(target_os = "windows")]
    let candidates: Vec<PathBuf> = ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(std::env::var_os)
        .flat_map(|base| {
            [
                PathBuf::from(&base).join("Google/Chrome/Application/chrome.exe"),
                PathBuf::from(&base).join("Microsoft/Edge/Application/msedge.exe"),
            ]
        })
        .collect();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: Vec<PathBuf> = [
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/microsoft-edge",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    candidates.into_iter().find(|p| p.is_file())
}
fn allowed_target(url: &Url, local: &str) -> bool {
    if url.origin().ascii_serialization() == local {
        return url.path().starts_with("/capture-demo") || url.path().starts_with("/sample/");
    }
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        let restricted = match ip {
            IpAddr::V4(ip) => {
                ip.is_loopback()
                    || ip.is_private()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || ip.is_broadcast()
                    || ip.is_multicast()
            }
            IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.to_ipv4_mapped().is_some()
            }
        };
        if restricted {
            return false;
        }
    }
    matches!(url.scheme(), "http" | "https")
        && matches!(url.port_or_known_default(), Some(80 | 443))
}
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}
fn replay_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    ["user-agent", "referer", "origin", "cookie", "authorization"]
        .iter()
        .filter_map(|name| {
            let value = header_text(headers, name);
            (!value.is_empty() && value.len() <= 8192)
                .then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}
fn wechat_media(url: &Url) -> bool {
    crate::capture_wechat::media_host(url.host_str().unwrap_or(""))
}
fn identity(url: &str) -> String {
    format!("{:x}", Sha256::digest(url.as_bytes()))[..20].into()
}
fn key_identity(raw: &str) -> String {
    let Ok(u) = Url::parse(raw) else {
        return raw.into();
    };
    crate::capture_wechat::file_identity(&u)
        .unwrap_or_else(|| crate::capture_wechat::resource_identity(&u))
}
fn observe(
    catalog: &mut Catalog,
    raw: &str,
    headers: &HeaderMap,
    response: &HeaderMap,
    generation: u64,
) {
    observe_inner(catalog, raw, headers, response, generation, true);
}
fn observe_inner(
    catalog: &mut Catalog,
    raw: &str,
    headers: &HeaderMap,
    response: &HeaderMap,
    generation: u64,
    network: bool,
) {
    if !catalog.active || generation != catalog.generation || raw.len() > 8192 {
        return;
    }
    let Ok(url) = Url::parse(raw) else {
        return;
    };
    let mime = header_text(response, "content-type")
        .split(';')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let path = url.path().to_ascii_lowercase();
    let suffix = path.rsplit('.').next().unwrap_or("");
    let key = catalog.keys.get(&key_identity(raw)).copied();
    let manifest = mime.contains("mpegurl")
        || mime.contains("dash+xml")
        || path.ends_with(".m3u8")
        || path.ends_with(".mpd");
    let fragment = path.ends_with(".m4s") || path.ends_with(".ts") || mime == "video/mp2t";
    // The same WeChat CDN also serves covers. Never label images as encrypted
    // MP4s merely because their host is the video CDN.
    let encrypted = wechat_media(&url)
        && (key.is_some() || url.query_pairs().any(|(k, _)| k == "encfilekey"))
        && (mime.starts_with("video/") || matches!(mime.as_str(), "" | "application/octet-stream"));
    let audio_suffix = matches!(
        suffix,
        "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "ape"
    );
    let file_suffix = audio_suffix || matches!(suffix, "mp4" | "webm" | "mkv" | "flv" | "mov");
    let protected_music = suffix.starts_with("qmc")
        || matches!(
            suffix,
            "kgm" | "kgma" | "kgg" | "vpr" | "mflac" | "mgg" | "bkc" | "tkm"
        );
    let generic = matches!(
        mime.as_str(),
        "" | "application/octet-stream" | "binary/octet-stream" | "application/x-download"
    );
    if !(mime.starts_with("video/")
        || mime.starts_with("audio/")
        || manifest
        || encrypted
        || generic && (file_suffix || protected_music))
    {
        return;
    }
    let extension = if manifest {
        if mime.contains("dash+xml") || path.ends_with(".mpd") {
            "mpd"
        } else {
            "m3u8"
        }
    } else if fragment {
        "ts"
    } else if protected_music {
        suffix
    } else if mime.contains("webm") {
        "webm"
    } else if mime == "audio/mpeg" {
        "mp3"
    } else if matches!(mime.as_str(), "audio/flac" | "audio/x-flac") {
        "flac"
    } else if matches!(
        mime.as_str(),
        "audio/wav" | "audio/wave" | "audio/x-wav" | "audio/vnd.wave"
    ) {
        "wav"
    } else if matches!(mime.as_str(), "audio/aac" | "audio/aacp" | "audio/x-aac") {
        "aac"
    } else if mime == "audio/opus" {
        "opus"
    } else if matches!(
        mime.as_str(),
        "audio/ape" | "audio/x-ape" | "audio/x-monkeys-audio"
    ) {
        "ape"
    } else if mime.contains("ogg") {
        "ogg"
    } else if mime == "video/quicktime" {
        "mov"
    } else if mime == "video/x-matroska" {
        "mkv"
    } else if file_suffix && (generic || mime.starts_with("audio/")) {
        suffix
    } else if mime.starts_with("audio/") {
        "m4a"
    } else if mime.contains("flv") {
        "flv"
    } else {
        "mp4"
    };
    let id = identity(&crate::capture_wechat::resource_identity(&url));
    let size = if let Some((_, size)) = header_text(response, "content-range").rsplit_once('/') {
        size.parse().ok()
    } else {
        header_text(response, "content-length").parse().ok()
    };
    let filename = crate::media::media_filename(
        &format!("{}-{}", url.host_str().unwrap_or("video"), &id[..8]),
        extension,
    );
    let now = crate::service::now_ms();
    let mut entry = Entry {
        public: Resource {
            id: id.clone(),
            filename,
            host: url.host_str().unwrap_or("").into(),
            kind: if mime.starts_with("audio/") || audio_suffix || protected_music {
                "audio"
            } else {
                "video"
            }
            .into(),
            platform: media_platform(&url, headers).into(),
            mime,
            bytes: size,
            supports_range: header_text(response, "accept-ranges") == "bytes"
                || response.contains_key("content-range"),
            encrypted,
            has_key: key.is_some(),
            downloadable: !manifest && !fragment && !protected_music,
            note: if protected_music {
                "平台加密音频：不能直接保存为可播放歌曲；请捕获播放器实际使用的普通音频流"
            } else if manifest {
                "流媒体清单：本轮 MVP 支持完整媒体文件，暂不下载 HLS / DASH"
            } else if fragment {
                "媒体分片：不是完整视频，请选择完整媒体资源"
            } else if encrypted && key.is_none() {
                "等待自动获取解密信息，请在微信打开此视频的详情并播放"
            } else {
                "可交给 Rust 分段下载"
            }
            .into(),
            captured_at: now,
            first_seen_at: now,
            request_count: u64::from(network),
            detail_at: None,
            title: None,
        },
        url: raw.into(),
        headers: replay_headers(headers),
        key,
        extension: extension.into(),
    };
    if crate::download::validate_headers(&entry.headers).is_err() {
        return;
    }
    if let Some(old) = catalog.entries.iter_mut().find(|e| e.public.id == id) {
        entry.public.first_seen_at = old.public.first_seen_at;
        entry.public.request_count += old.public.request_count;
        entry.public.detail_at = old.public.detail_at;
        entry.public.title = old.public.title.clone();
        if entry.public.title.is_some() {
            entry.public.filename = old.public.filename.clone();
        }
        entry.key = entry.key.or(old.key);
        entry.public.has_key = entry.key.is_some();
        entry.public.encrypted |= old.public.encrypted;
        entry.public.bytes = entry.public.bytes.or(old.public.bytes);
        if entry.public.encrypted && entry.public.has_key {
            entry.public.note = "视频号密钥已关联 · 下载后自动解密".into();
        }
        // Metadata carries page headers, not the CDN's authentication. Retain
        // the actual media request headers whenever they have been observed.
        if !network && old.public.request_count > 0 {
            entry.headers = old.headers.clone();
        }
        *old = entry;
        if network {
            catalog.merged += 1;
        }
    } else {
        if catalog.entries.len() >= MAX_RESOURCES {
            catalog.entries.remove(0);
        }
        catalog.entries.push(entry);
    }
}
fn observe_wechat_metadata(
    catalog: &mut Catalog,
    body: &[u8],
    request_headers: &HeaderMap,
    generation: u64,
) {
    if !catalog.active || catalog.generation != generation {
        return;
    }
    if catalog.wechat_diagnostics.status(body) {
        catalog.wechat_diagnostics.last_issue = None;
        return;
    }
    let media = crate::capture_wechat::parse_media(body);
    if media.is_empty() {
        catalog.wechat_diagnostics.invalid_media += 1;
        catalog.wechat_diagnostics.last_issue =
            Some("已收到页面回传，但未识别到可用的视频地址；需要适配此播放器的数据结构。".into());
        return;
    }
    catalog.bridge_reports += 1;
    catalog.wechat_diagnostics.last_issue = None;
    for item in media {
        let Ok(url) = Url::parse(&item.url) else {
            continue;
        };
        let id = identity(&crate::capture_wechat::resource_identity(&url));
        if let Some(key) = item.key {
            if catalog.keys.len() < MAX_RESOURCES
                || catalog.keys.contains_key(&key_identity(&item.url))
            {
                catalog.keys.insert(key_identity(&item.url), key);
            }
        }
        let mut headers = HeaderMap::from_iter([(
            header::REFERER,
            "https://channels.weixin.qq.com/".parse().unwrap(),
        )]);
        if let Some(agent) = request_headers.get(header::USER_AGENT) {
            headers.insert(header::USER_AGENT, agent.clone());
        }
        let mut response =
            HeaderMap::from_iter([(header::CONTENT_TYPE, "video/mp4".parse().unwrap())]);
        if let Some(size) = item.bytes {
            response.insert(header::CONTENT_LENGTH, size.to_string().parse().unwrap());
        }
        observe_inner(catalog, &item.url, &headers, &response, generation, false);
        if let Some(entry) = catalog.entries.iter_mut().find(|e| e.public.id == id) {
            if let Some(key) = item.key {
                entry.key = Some(key);
                entry.public.encrypted = true;
                entry.public.has_key = true;
                entry.public.note = "视频号密钥已关联 · 下载后自动解密".into();
            }
            if !item.title.is_empty() {
                entry.public.filename = crate::media::media_filename(&item.title, &entry.extension);
                entry.public.title = Some(item.title);
            }
            if item.detail {
                entry.public.detail_at = Some(crate::service::now_ms());
            }
        }
    }
}
fn domain(host: &str, suffix: &str) -> bool {
    host == suffix || host.ends_with(&format!(".{suffix}"))
}
fn media_platform(url: &Url, headers: &HeaderMap) -> &'static str {
    let host = url.host_str().unwrap_or("");
    let referer = Url::parse(header_text(headers, "referer"))
        .ok()
        .or_else(|| Url::parse(header_text(headers, "origin")).ok());
    let page = referer.as_ref().and_then(Url::host_str).unwrap_or("");
    if domain(page, "servicewechat.com") || domain(host, "servicewechat.com") {
        return "小程序";
    }
    if domain(page, "mp.weixin.qq.com") || domain(host, "mp.weixin.qq.com") {
        return "公众号";
    }
    for (name, domains) in [
        (
            "抖音",
            &["douyin.com", "douyinvod.com", "iesdouyin.com", "amemv.com"][..],
        ),
        (
            "快手",
            &[
                "kuaishou.com",
                "kwaicdn.com",
                "kwai.net",
                "ks-cdn.com",
                "gifshow.com",
                "ksapisrv.com",
            ][..],
        ),
        (
            "小红书",
            &["xiaohongshu.com", "xhscdn.com", "xhslink.com"][..],
        ),
        ("酷狗音乐", &["kugou.com", "kugou.net"][..]),
        (
            "QQ 音乐",
            &[
                "y.qq.com",
                "qqmusic.qq.com",
                "qqmusic.com",
                "music.tc.qq.com",
                "aqqmusic.tc.qq.com",
            ][..],
        ),
    ] {
        if domains.iter().any(|d| domain(host, d) || domain(page, d)) {
            return name;
        }
    }
    if wechat_media(url) || domain(page, "channels.weixin.qq.com") {
        return "视频号";
    }
    if domain(host, "weixin.qq.com")
        || domain(host, "weixinbridge.com")
        || domain(host, "wx.qq.com")
    {
        return "微信媒体";
    }
    "其他来源"
}
// Metadata is consumed only for the WeChat playback API, never generic page or form bodies.
fn metadata_target(url: &Url) -> bool {
    let h = url.host_str().unwrap_or("");
    (h == "channels.weixin.qq.com" || h.ends_with(".weixin.qq.com"))
        && (url.path().contains("feed")
            || url.path().contains("finder")
            || url.path().contains("object"))
}
fn extract_keys(
    value: &serde_json::Value,
    inherited: Option<u64>,
    pairs: &mut Vec<(String, u64)>,
    depth: usize,
) {
    if depth > 32 || pairs.len() >= MAX_RESOURCES {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            let key = map
                .get("decodeKey")
                .or_else(|| map.get("decode_key"))
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .filter(|k| *k != 0)
                .or(inherited);
            if let Some(key) = key {
                for field in ["url", "videoUrl", "originVideoUrl", "playUrl"] {
                    if let Some(raw) = map.get(field).and_then(|v| v.as_str()) {
                        if Url::parse(raw).is_ok_and(|u| wechat_media(&u)) {
                            pairs.push((key_identity(raw), key));
                            if let Some(token) = map.get("urlToken").and_then(|v| v.as_str()) {
                                if token.len() < 8192 && token.starts_with(['?', '&']) {
                                    pairs.push((key_identity(&format!("{raw}{token}")), key));
                                }
                            }
                        }
                    }
                }
            }
            for child in map.values() {
                extract_keys(child, key, pairs, depth + 1);
            }
        }
        serde_json::Value::Array(values) => {
            for v in values {
                extract_keys(v, inherited, pairs, depth + 1);
            }
        }
        _ => {}
    }
}
#[derive(Clone)]
struct RequestInfo {
    url: String,
    headers: HeaderMap,
    generation: u64,
    metadata: bool,
    script: bool,
    page: bool,
}
#[derive(Clone)]
struct CaptureHandler {
    catalog: Arc<Mutex<Catalog>>,
    local_origin: String,
    request: Option<RequestInfo>,
    slots: Arc<Semaphore>,
    script_slots: Arc<Semaphore>,
    upstream: Option<(reqwest::Client, reqwest::Client)>,
    upstream_url: Option<String>,
    bridge_token: String,
}
impl HttpHandler for CaptureHandler {
    async fn handle_request(
        &mut self,
        ctx: &HttpContext,
        mut req: Request<Body>,
    ) -> RequestOrResponse {
        let raw = if req.method() == Method::CONNECT {
            format!("https://{}/", req.uri())
        } else {
            req.uri().to_string()
        };
        if Url::parse(&raw).is_ok_and(|u| u.host_str() == Some(crate::capture_wechat::BRIDGE_HOST))
        {
            // This reserved origin is entirely local; even invalid tokens and
            // stale scripts must never be forwarded to an upstream proxy.
            if req.method() == Method::CONNECT {
                self.catalog.lock().unwrap().wechat_diagnostics.connections += 1;
                return req.into();
            }
            let url = Url::parse(&raw).unwrap();
            let trusted =
                crate::capture_wechat::trusted_origin(header_text(req.headers(), "origin"));
            if req.method() == Method::GET && url.scheme() == "https" && url.path() == "/session" {
                let mut c = self.catalog.lock().unwrap();
                c.wechat_diagnostics.session_requests += 1;
                if trusted && c.active {
                    c.wechat_diagnostics.sessions += 1;
                    return crate::capture_wechat::bridge_response(
                        StatusCode::OK,
                        true,
                        Body::from(serde_json::json!({"token": c.bridge_token}).to_string()),
                    )
                    .into();
                }
                c.wechat_diagnostics.rejected += 1;
                c.wechat_diagnostics.last_issue = Some(
                    if !trusted {
                        "视频号会话更新未通过 Origin 来源校验"
                    } else {
                        "抓包会话已经停止"
                    }
                    .into(),
                );
                return crate::capture_wechat::bridge_response(
                    StatusCode::FORBIDDEN,
                    trusted,
                    Body::empty(),
                )
                .into();
            }
            let generation = {
                let mut c = self.catalog.lock().unwrap();
                c.wechat_diagnostics.requests += 1;
                // Browser TLS connections may outlive a capture session.
                // Always authenticate against the current active capability.
                let rejection = crate::capture_wechat::bridge_rejection(
                    req.method().as_str(),
                    &url,
                    header_text(req.headers(), "origin"),
                    &c.bridge_token,
                )
                .or_else(|| (!c.active).then_some("抓包会话已经停止"));
                if let Some(reason) = rejection {
                    c.wechat_diagnostics.rejected += 1;
                    c.wechat_diagnostics.last_issue = Some(reason.into());
                }
                rejection.is_none().then_some(c.generation)
            };
            let Some(generation) = generation else {
                return crate::capture_wechat::bridge_response(
                    StatusCode::FORBIDDEN,
                    trusted,
                    Body::empty(),
                )
                .into();
            };
            let (parts, body) = req.into_parts();
            let body = tokio::time::timeout(
                Duration::from_secs(3),
                http_body_util::Limited::new(body, crate::capture_wechat::MAX_BRIDGE_BODY)
                    .collect(),
            )
            .await;
            let status = if let Ok(Ok(body)) = body {
                observe_wechat_metadata(
                    &mut self.catalog.lock().unwrap(),
                    &body.to_bytes(),
                    &parts.headers,
                    generation,
                );
                StatusCode::NO_CONTENT
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            };
            return crate::capture_wechat::bridge_response(status, trusted, Body::empty()).into();
        }
        if !Url::parse(&raw).is_ok_and(|u| allowed_target(&u, &self.local_origin)) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from("Capture proxy: target not allowed"))
                .unwrap()
                .into();
        }
        if req.method() == Method::CONNECT {
            self.catalog.lock().unwrap().connections += 1;
            if Url::parse(&raw)
                .is_ok_and(|u| crate::capture_wechat::control_host(u.host_str().unwrap_or("")))
            {
                self.catalog
                    .lock()
                    .unwrap()
                    .wechat_diagnostics
                    .control_tunnels += 1;
                return match crate::capture_transport::passthrough_connect(
                    req,
                    self.upstream_url.as_deref(),
                )
                .await
                {
                    Ok(response) => response.into(),
                    Err(_) => {
                        self.catalog.lock().unwrap().errors += 1;
                        Response::builder()
                            .status(StatusCode::BAD_GATEWAY)
                            .body(Body::from("Capture tunnel connection failed"))
                            .unwrap()
                            .into()
                    }
                };
            }
        }
        if req.method() != Method::CONNECT {
            let url = Url::parse(&raw).unwrap();
            let metadata = metadata_target(&url);
            let script = req.method() == Method::GET && crate::capture_wechat::script_target(&url);
            let page = req.method() == Method::GET && crate::capture_wechat::page_target(&url);
            if metadata || script || page {
                req.headers_mut()
                    .insert(header::ACCEPT_ENCODING, "identity".parse().unwrap());
            }
            if script || page {
                req.headers_mut().remove(header::IF_NONE_MATCH);
                req.headers_mut().remove(header::IF_MODIFIED_SINCE);
                req.headers_mut()
                    .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
            }
            let generation = {
                let mut catalog = self.catalog.lock().unwrap();
                catalog.observed += 1;
                if metadata || script || page || wechat_media(&url) {
                    catalog.wechat_requests += 1;
                }
                if page {
                    catalog.wechat_pages += 1;
                }
                catalog.generation
            };
            self.request = Some(RequestInfo {
                url: raw,
                headers: req.headers().clone(),
                generation,
                metadata,
                script,
                page,
            });
            if let Some((client, websocket_client)) = &self.upstream {
                let result =
                    if header_text(req.headers(), "upgrade").eq_ignore_ascii_case("websocket") {
                        crate::capture_transport::forward_websocket(websocket_client, req).await
                    } else {
                        crate::capture_transport::forward(client, req).await
                    };
                return match result {
                    Ok(response) => self.handle_response(ctx, response).await.into(),
                    Err(_) => {
                        self.catalog.lock().unwrap().errors += 1;
                        Response::builder()
                            .status(StatusCode::BAD_GATEWAY)
                            .body(Body::from("Capture upstream connection failed"))
                            .unwrap()
                            .into()
                    }
                };
            }
        }
        req.into()
    }
    async fn handle_response(
        &mut self,
        _ctx: &HttpContext,
        mut response: Response<Body>,
    ) -> Response<Body> {
        let Some(info) = self.request.take() else {
            return response;
        };
        if !response.status().is_success() {
            return response;
        }
        {
            let c = self.catalog.lock().unwrap();
            if !c.active || c.bridge_token != self.bridge_token || c.generation != info.generation {
                return response;
            }
        }
        let mime = header_text(response.headers(), "content-type").to_ascii_lowercase();
        if (info.script
            && (mime.contains("javascript")
                || mime.contains("ecmascript")
                || mime.contains("text/plain")))
            || (info.page && mime.contains("text/html"))
        {
            // Partially rewriting a module graph creates duplicate shared
            // modules. Queue script work instead of silently skipping it.
            // Streaming metadata has a separate limit and cannot starve JS.
            if let Ok(_permit) = self.script_slots.clone().acquire_owned().await {
                let (adapted, hooked) =
                    crate::capture_wechat::adapt_response(response, &self.bridge_token, info.page)
                        .await;
                if hooked {
                    let mut c = self.catalog.lock().unwrap();
                    if c.active && c.generation == info.generation {
                        c.adapted_scripts += 1;
                        c.wechat_diagnostics.asset(&info.url);
                    }
                }
                response = adapted;
            }
        }
        observe(
            &mut self.catalog.lock().unwrap(),
            &info.url,
            &info.headers,
            response.headers(),
            info.generation,
        );
        if !info.metadata
            || !header_text(response.headers(), "content-type").contains("json")
            || !matches!(
                header_text(response.headers(), "content-encoding"),
                "" | "identity"
            )
        {
            return response;
        }
        let Ok(permit) = self.slots.clone().try_acquire_owned() else {
            return response;
        };
        let (parts, body) = response.into_parts();
        let catalog = self.catalog.clone();
        let state = (
            body.into_data_stream(),
            Vec::new(),
            false,
            catalog,
            info.generation,
            permit,
        );
        let stream = futures_util::stream::unfold(
            state,
            |(mut body, mut buffer, mut overflow, catalog, generation, permit)| async move {
                if let Some(chunk) = body.next().await {
                    match &chunk {
                        Ok(bytes) if !overflow && buffer.len() + bytes.len() <= MAX_METADATA => {
                            buffer.extend_from_slice(bytes)
                        }
                        _ => {
                            overflow = true;
                            buffer.clear();
                        }
                    }
                    Some((chunk, (body, buffer, overflow, catalog, generation, permit)))
                } else {
                    if !overflow {
                        if let Ok(value) = serde_json::from_slice(&buffer) {
                            let mut pairs = Vec::new();
                            extract_keys(&value, None, &mut pairs, 0);
                            let mut c = catalog.lock().unwrap();
                            if c.active && c.generation == generation {
                                for (url, key) in pairs {
                                    for entry in &mut c.entries {
                                        if key_identity(&entry.url) == url {
                                            entry.key = Some(key);
                                            entry.public.encrypted = true;
                                            entry.public.has_key = true;
                                            entry.public.note =
                                                "视频号密钥已捕获 · 下载后自动解密".into();
                                        }
                                    }
                                    if c.keys.len() < MAX_RESOURCES {
                                        c.keys.insert(url, key);
                                    }
                                }
                            }
                        }
                    }
                    None
                }
            },
        );
        Response::from_parts(parts, Body::from_stream(stream))
    }
    async fn handle_error(
        &mut self,
        _ctx: &HttpContext,
        _error: hudsucker::hyper_util::client::legacy::Error,
    ) -> Response<Body> {
        self.catalog.lock().unwrap().errors += 1;
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("Capture upstream connection failed"))
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn wechat_token_churn_collapses_187_requests_without_merging_other_videos() {
        let mut c = Catalog {
            active: true,
            ..Default::default()
        };
        let headers = HeaderMap::from_iter([(header::COOKIE, "cdn=private".parse().unwrap())]);
        let response = HeaderMap::from_iter([
            (header::CONTENT_TYPE, "video/mp4".parse().unwrap()),
            (header::CONTENT_RANGE, "bytes 0-3/36700160".parse().unwrap()),
        ]);
        for i in 0..187 {
            observe(
                &mut c,
                &format!(
                    "https://finder.video.qq.com/251/stodownload?encfilekey=one&token={i}&idx={i}"
                ),
                &headers,
                &response,
                0,
            );
        }
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].public.request_count, 187);
        assert_eq!(c.merged, 186);
        assert!(
            c.entries[0].url.contains("token=186"),
            "keep the latest usable URL"
        );
        let body = json!({"source":"detail", "description":"正在查看的视频", "media":[{"url":"https://wxapp.tc.qq.com/video?encfilekey=one", "urlToken":"&token=metadata", "decodeKey":"18446744073709551614"}]});
        observe_wechat_metadata(
            &mut c,
            &serde_json::to_vec(&body).unwrap(),
            &HeaderMap::new(),
            0,
        );
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].public.filename, "正在查看的视频.mp4");
        assert!(c.entries[0].public.detail_at.is_some());
        assert_eq!(c.entries[0].public.request_count, 187);
        observe(
            &mut c,
            "https://finder.video.qq.com/251/stodownload?encfilekey=one&token=final",
            &headers,
            &response,
            0,
        );
        assert_eq!(c.entries[0].key, Some(18446744073709551614));
        assert!(c.entries[0].public.has_key);
        assert_eq!(c.entries[0].public.filename, "正在查看的视频.mp4");
        observe(
            &mut c,
            "https://finder.video.qq.com/251/stodownload?encfilekey=two",
            &headers,
            &response,
            0,
        );
        assert_eq!(c.entries.len(), 2, "same-size videos must not be merged");
        assert!(c.entries[1].public.detail_at.is_none());
        let public =
            serde_json::to_string(&c.entries.iter().map(|e| &e.public).collect::<Vec<_>>())
                .unwrap();
        assert!(!public.contains("private"));
        assert!(!public.contains("encfilekey"));
        assert!(!public.contains("18446744073709551614"));
    }
    #[tokio::test]
    async fn wechat_local_tls_bridge_is_authenticated_bounded_and_never_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let capture =
            CaptureService::new(dir.path().join("browser"), "http://127.0.0.1:17890".into());
        capture.start().await.unwrap();
        let token = capture.catalog.lock().unwrap().bridge_token.clone();
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(
                reqwest::Proxy::all(format!(
                    "http://{}",
                    capture.snapshot().await.proxy.unwrap()
                ))
                .unwrap(),
            )
            .add_root_certificate(
                reqwest::Certificate::from_pem(capture.certificate().await.unwrap().as_bytes())
                    .unwrap(),
            )
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!(
            "https://{}/{token}/media",
            crate::capture_wechat::BRIDGE_HOST
        );
        let body = json!({"source":"detail","description":"本地回传验证","media":[{"url":"https://finder.video.qq.com/v?encfilekey=one","decodeKey":"123456789"}]});
        let discovery = format!("https://{}/session", crate::capture_wechat::BRIDGE_HOST);
        for origin in [None, Some("null"), Some("https://evil.test")] {
            let mut request = client.get(&discovery);
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 403);
            assert!(!response
                .headers()
                .contains_key("access-control-allow-origin"));
            assert!(!response.text().await.unwrap().contains(&token));
        }
        let response = client
            .get(&discovery)
            .header("origin", "https://channels.weixin.qq.com")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "https://channels.weixin.qq.com"
        );
        assert!(!response
            .headers()
            .contains_key("access-control-allow-credentials"));
        let session: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(session["token"], token);
        assert_eq!(
            client
                .post(&url)
                .body(body.to_string())
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .post(&url)
                .header("origin", "https://evil.test")
                .body(body.to_string())
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .post(&url)
                .header("origin", "https://channels.weixin.qq.com")
                .body(vec![b'x'; crate::capture_wechat::MAX_BRIDGE_BODY + 1])
                .send()
                .await
                .unwrap()
                .status(),
            413
        );
        assert_eq!(
            client
                .post(&url)
                .header("origin", "https://channels.weixin.qq.com")
                .body(body.to_string())
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
        let snapshot = capture.snapshot().await;
        assert_eq!(snapshot.resources.len(), 1);
        assert_eq!(snapshot.bridge_reports, 1);
        assert!(snapshot.resources[0].detail_at.is_some());
        assert!(snapshot.resources[0].has_key);
        let plan = capture.select(&snapshot.resources[0].id, None).unwrap();
        assert_eq!(plan.streams[0].decrypt_key, Some(123456789));
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains(&token));
        assert!(!serialized.contains("123456789"));
        // Keep the existing TLS client/handler alive while rotating state.
        let rotated = "0".repeat(64);
        capture.catalog.lock().unwrap().bridge_token = rotated.clone();
        let response = client
            .get(&discovery)
            .header("origin", "https://channels.weixin.qq.com")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let current: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(current["token"], rotated);
        let response = client
            .post(format!(
                "https://{}/{rotated}/media",
                crate::capture_wechat::BRIDGE_HOST
            ))
            .header("origin", "https://channels.weixin.qq.com")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        capture.stop().await.unwrap();
        capture.start().await.unwrap();
        assert!(
            capture.snapshot().await.resources.is_empty(),
            "new session must not retain unrelated results"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(
                reqwest::Proxy::all(format!(
                    "http://{}",
                    capture.snapshot().await.proxy.unwrap()
                ))
                .unwrap(),
            )
            .add_root_certificate(
                reqwest::Certificate::from_pem(capture.certificate().await.unwrap().as_bytes())
                    .unwrap(),
            )
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        assert_eq!(
            client
                .post(&url)
                .header("origin", "https://channels.weixin.qq.com")
                .body(body.to_string())
                .send()
                .await
                .unwrap()
                .status(),
            403,
            "old pages cannot report into a different capture session"
        );
        let response = client
            .get(&discovery)
            .header("origin", "https://channels.weixin.qq.com")
            .send()
            .await
            .unwrap();
        let session: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        let fresh = session["token"].as_str().unwrap();
        assert_ne!(fresh, token);
        let renewed_url = format!(
            "https://{}/{fresh}/media",
            crate::capture_wechat::BRIDGE_HOST
        );
        let status = client
            .post(&renewed_url)
            .header("origin", "https://channels.weixin.qq.com")
            .body(r#"{"source":"status","event":"getter"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), 204);
        assert!(
            capture
                .snapshot()
                .await
                .wechat_diagnostics
                .last_issue
                .is_none(),
            "a recovered bridge must not keep telling the user to reopen the page"
        );
        let response = client
            .post(renewed_url)
            .header("origin", "https://channels.weixin.qq.com")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "https://channels.weixin.qq.com"
        );
        assert_eq!(capture.snapshot().await.resources.len(), 1);
        capture.stop().await.unwrap();
    }
    #[tokio::test]
    async fn wechat_account_tls_is_relayed_unchanged_through_original_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(5), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let mut head = vec![];
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                    assert!(head.len() < 8192);
                }
                assert!(head.starts_with(b"CONNECT login.weixin.qq.com:443 HTTP/1.1\r\n"));
                socket.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
                let mut opaque = [0u8; 8];
                socket.read_exact(&mut opaque).await.unwrap();
                socket.write_all(&opaque).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let capture = CaptureService::new(dir.path().join("browser"), "tauri://localhost".into());
            capture.start_inner(Some(format!("http://{address}"))).await.unwrap();
            let mut socket = tokio::net::TcpStream::connect(capture.snapshot().await.proxy.unwrap()).await.unwrap();
            socket.write_all(b"CONNECT login.weixin.qq.com:443 HTTP/1.1\r\nHost: login.weixin.qq.com:443\r\n\r\n").await.unwrap();
            let mut head = vec![];
            while !head.ends_with(b"\r\n\r\n") { head.push(socket.read_u8().await.unwrap()); }
            assert!(head.starts_with(b"HTTP/1.1 200"));
            let opaque = [0x16, 0x03, 0x01, 0x00, 0x03, 0x91, 0x22, 0x73];
            socket.write_all(&opaque).await.unwrap();
            let mut echoed = [0u8;8]; socket.read_exact(&mut echoed).await.unwrap();
            assert_eq!(echoed, opaque);
            server.await.unwrap();
            let snapshot = capture.snapshot().await;
            assert_eq!(snapshot.wechat_diagnostics.control_tunnels, 1);
            assert_eq!(snapshot.observed, 0, "account TLS must never enter HTTP inspection");
            assert!(snapshot.resources.is_empty());
            capture.stop().await.unwrap();
        }).await.unwrap();
    }
    #[tokio::test]
    async fn simultaneous_player_modules_are_all_adapted() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const COUNT: usize = 12;
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(COUNT));
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..COUNT {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let barrier = barrier.clone();
                tasks.spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        head.push(socket.read_u8().await.unwrap());
                        assert!(head.len() < 16384);
                    }
                    let body = b"import './shared.js';export class Player{get media(){return this.objectDesc.media}}";
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    barrier.wait().await;
                    socket.write_all(body).await.unwrap();
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let capture =
            CaptureService::new(dir.path().join("browser"), "http://127.0.0.1:17890".into());
        capture
            .start_inner(Some(format!("http://{address}")))
            .await
            .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(
                reqwest::Proxy::all(format!(
                    "http://{}",
                    capture.snapshot().await.proxy.unwrap()
                ))
                .unwrap(),
            )
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for n in 0..COUNT {
            let client = client.clone();
            tasks.spawn(async move {
                let body = client
                    .get(format!(
                        "http://res.wx.qq.com/t/web-finder/res/js/player-{n}.js"
                    ))
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
                assert!(body.contains("function __ffdmCaptureMedia033("));
                assert!(body.contains("shared.js?__ffdm="));
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        server.await.unwrap();
        assert_eq!(capture.snapshot().await.adapted_scripts, COUNT as u64);
        capture.stop().await.unwrap();
    }

    #[tokio::test]
    async fn application_upstream_keeps_websocket_upgrades_working() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn head(socket: &mut tokio::net::TcpStream) -> String {
            let mut data = vec![];
            while !data.ends_with(b"\r\n\r\n") {
                assert!(data.len() < 8192);
                data.push(socket.read_u8().await.unwrap());
            }
            String::from_utf8(data).unwrap()
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream = format!("http://{}", listener.local_addr().unwrap());
            let echo = tokio::spawn(async move {
                let (mut socket,_) = listener.accept().await.unwrap();
                let request = head(&mut socket).await;
                assert!(request.to_lowercase().contains("upgrade: websocket"));
                socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n").await.unwrap();
                let mut bytes = [0;4]; socket.read_exact(&mut bytes).await.unwrap();
                socket.write_all(&bytes).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let capture = CaptureService::new(dir.path().join("browser"), "http://127.0.0.1:17890".into());
            capture.start_inner(Some(upstream)).await.unwrap();
            let mut socket = tokio::net::TcpStream::connect(capture.snapshot().await.proxy.unwrap()).await.unwrap();
            socket.write_all(b"GET http://music.kugou.com/socket HTTP/1.1\r\nHost: music.kugou.com\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n").await.unwrap();
            assert!(head(&mut socket).await.starts_with("HTTP/1.1 101"));
            socket.write_all(b"ping").await.unwrap();
            let mut data = [0;4]; socket.read_exact(&mut data).await.unwrap(); assert_eq!(&data, b"ping");
            echo.await.unwrap();
            drop(socket);
            capture.stop().await.unwrap();
        }).await.unwrap();
    }
    #[tokio::test]
    async fn upstream_forwarding_preserves_media_ranges_cookies_and_redirects() {
        use axum::{routing::any, Router};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().fallback(any(|req: axum::extract::Request| async move {
            assert_eq!(req.headers().get("cookie").unwrap(), "playback=private");
            assert!(!req.headers().contains_key("proxy-authorization"));
            assert!(
                !req.headers().contains_key("transfer-encoding"),
                "bodyless GET must stay bodyless"
            );
            if req.uri().path() == "/redirect" {
                return axum::http::Response::builder()
                    .status(302)
                    .header("location", "http://audio.kugou.com/track.flac")
                    .body(axum::body::Body::empty())
                    .unwrap();
            }
            assert_eq!(req.uri().host(), Some("audio.kugou.com"));
            assert_eq!(req.headers().get("range").unwrap(), "bytes=0-3");
            axum::http::Response::builder()
                .status(206)
                .header("content-type", "audio/flac")
                .header("content-range", "bytes 0-3/100")
                .header("set-cookie", "player=1")
                .body(axum::body::Body::from("fLaC"))
                .unwrap()
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let capture =
            CaptureService::new(dir.path().join("browser"), "http://127.0.0.1:17890".into());
        capture.start_inner(Some(upstream)).await.unwrap();
        let address = capture.snapshot().await.proxy.unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .proxy(reqwest::Proxy::all(format!("http://{address}")).unwrap())
            .build()
            .unwrap();
        let redirect = client
            .get("http://audio.kugou.com/redirect")
            .header("cookie", "playback=private")
            .send()
            .await
            .unwrap();
        assert_eq!(redirect.status(), 302);
        let media = client
            .get("http://audio.kugou.com/track.flac")
            .header("cookie", "playback=private")
            .header("range", "bytes=0-3")
            .send()
            .await
            .unwrap();
        assert_eq!(media.status(), 206);
        assert_eq!(media.headers()["set-cookie"], "player=1");
        assert_eq!(media.bytes().await.unwrap().as_ref(), b"fLaC");
        let snapshot = capture.snapshot().await;
        assert_eq!(snapshot.resources.len(), 1);
        assert_eq!(snapshot.resources[0].platform, "酷狗音乐");
        assert_eq!(snapshot.resources[0].bytes, Some(100));
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("private"));
        capture.stop().await.unwrap();
        server.abort();
    }
    #[test]
    fn music_types_and_encrypted_music_are_not_mislabeled_as_m4a() {
        let dir = tempfile::tempdir().unwrap();
        let service =
            CaptureService::new(dir.path().join("browser"), "http://127.0.0.1:17890".into());
        let mut c = service.catalog.lock().unwrap();
        c.active = true;
        for (index, (mime, path, expected, downloadable)) in [
            ("audio/flac", "track", "flac", true),
            ("application/octet-stream", "track.flac", "flac", true),
            ("audio/x-wav", "track", "wav", true),
            ("audio/aac", "track", "aac", true),
            ("audio/mpeg", "track", "mp3", true),
            ("audio/mp4", "track", "m4a", true),
            ("audio/ogg", "track", "ogg", true),
            (
                "application/octet-stream",
                "track.qmcflac",
                "qmcflac",
                false,
            ),
            ("audio/mpeg", "track.kgm", "kgm", false),
        ]
        .into_iter()
        .enumerate()
        {
            observe(
                &mut c,
                &format!("https://stream.qqmusic.qq.com/{index}/{path}"),
                &HeaderMap::new(),
                &HeaderMap::from_iter([(header::CONTENT_TYPE, mime.parse().unwrap())]),
                0,
            );
            let e = c.entries.last().unwrap();
            assert_eq!(e.extension, expected);
            assert_eq!(e.public.kind, "audio");
            assert_eq!(e.public.platform, "QQ 音乐");
            assert_eq!(e.public.downloadable, downloadable);
            if downloadable {
                let id = e.public.id.clone();
                drop(c);
                assert_eq!(service.select(&id, None).unwrap().extension, expected);
                c = service.catalog.lock().unwrap();
            }
        }
    }
    #[test]
    fn wechat_context_distinguishes_mini_apps_and_official_accounts() {
        for (page, expected) in [
            (
                "https://servicewechat.com/wx123/1/page-frame.html",
                "小程序",
            ),
            ("https://mp.weixin.qq.com/s/article", "公众号"),
            ("https://channels.weixin.qq.com/web/pages/feed", "视频号"),
        ] {
            let headers = HeaderMap::from_iter([(header::REFERER, page.parse().unwrap())]);
            assert_eq!(
                media_platform(
                    &Url::parse("https://finder.video.qq.com/video.mp4").unwrap(),
                    &headers
                ),
                expected
            );
        }
        let mut catalog = Catalog {
            active: true,
            ..Default::default()
        };
        let response = HeaderMap::from_iter([(header::CONTENT_TYPE, "video/mp4".parse().unwrap())]);
        observe(
            &mut catalog,
            "https://finder.video.qq.com/plain.mp4",
            &HeaderMap::new(),
            &response,
            0,
        );
        assert!(
            !catalog.entries[0].public.encrypted,
            "a shared CDN alone does not prove encryption"
        );
        observe(
            &mut catalog,
            "https://finder.video.qq.com/video?encfilekey=secret",
            &HeaderMap::new(),
            &response,
            0,
        );
        assert!(catalog.entries[1].public.encrypted);
    }
    #[test]
    fn media_catalog_deduplicates_ranges_and_never_serializes_credentials() {
        let mut c = Catalog {
            active: true,
            ..Default::default()
        };
        let req = HeaderMap::from_iter([
            (header::COOKIE, "session=secret".parse().unwrap()),
            (header::RANGE, "bytes=20-30".parse().unwrap()),
        ]);
        let res = HeaderMap::from_iter([
            (header::CONTENT_TYPE, "video/mp4".parse().unwrap()),
            (header::CONTENT_RANGE, "bytes 20-30/8192".parse().unwrap()),
        ]);
        let url = "https://cdn.example/movie.mp4?token=secret";
        observe(&mut c, url, &req, &res, 0);
        observe(&mut c, url, &req, &res, 0);
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].public.bytes, Some(8192));
        assert_eq!(
            c.entries[0].headers,
            vec![("cookie".into(), "session=secret".into())]
        );
        let displayed = serde_json::to_string(&c.entries[0].public).unwrap();
        assert!(!displayed.contains("secret"));
        assert!(!displayed.contains("token"));
        c.generation += 1;
        observe(&mut c, "https://cdn.example/old.mp4", &req, &res, 0);
        assert_eq!(
            c.entries.len(),
            1,
            "in-flight responses must not repopulate a cleared catalog"
        );
    }
    #[test]
    fn manifests_and_fragments_are_not_downloadable_as_full_videos() {
        let mut c = Catalog {
            active: true,
            ..Default::default()
        };
        let res = HeaderMap::from_iter([(
            header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl".parse().unwrap(),
        )]);
        observe(
            &mut c,
            "https://cdn.example/video.m3u8",
            &HeaderMap::new(),
            &res,
            0,
        );
        assert!(!c.entries[0].public.downloadable);
        let cover = HeaderMap::from_iter([(header::CONTENT_TYPE, "image/jpg".parse().unwrap())]);
        observe(
            &mut c,
            "https://finder.video.qq.com/cover",
            &HeaderMap::new(),
            &cover,
            0,
        );
        assert_eq!(c.entries.len(), 1, "video CDN covers are not videos");
    }
    #[test]
    fn wechat_keys_are_bound_to_the_file_and_preserve_large_integer_precision() {
        let value = json!({"feedInfo":{"decodeKey":"18446744073709551614", "h264VideoInfo":{"videoUrl":"https://finder.video.qq.com/251/file?encfilekey=one&token=a"}}});
        let mut pairs = vec![];
        extract_keys(&value, None, &mut pairs, 0);
        assert_eq!(pairs[0].1, 18446744073709551614);
        assert_eq!(
            pairs[0].0,
            key_identity("https://finder.video.qq.com/251/file?encfilekey=one&token=b")
        );
        assert_ne!(
            pairs[0].0,
            key_identity("https://finder.video.qq.com/251/file?encfilekey=two&token=a")
        );
    }
    #[test]
    fn proxy_cannot_reach_local_controls_or_private_targets() {
        let origin = "http://127.0.0.1:17890";
        for raw in [
            "http://127.0.0.1:17890/api/tasks",
            "http://localhost/",
            "http://192.168.1.1/",
            "http://[::ffff:127.0.0.1]/",
            "http://169.254.169.254/",
            "https://example.com:8443/",
            "https://secret@example.com/",
        ] {
            assert!(!allowed_target(&Url::parse(raw).unwrap(), origin), "{raw}");
        }
        assert!(allowed_target(
            &Url::parse("https://www.douyin.com/video/123").unwrap(),
            origin
        ));
        assert!(allowed_target(
            &Url::parse("http://127.0.0.1:17890/capture-demo/sample.mp4").unwrap(),
            origin
        ));
    }
}
