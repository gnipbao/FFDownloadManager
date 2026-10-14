//! A disposable browser for public share-page resolvers. The browser uses a
//! fresh profile and ordinary HTTPS; it has no capture proxy or certificate
//! exceptions and never reads an existing browser profile or saved cookies.
use anyhow::{bail, ensure, Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest_media::Url;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
    MaybeTlsStream, WebSocketStream,
};

const CONTROL_RESPONSE_LIMIT: usize = 1024 * 1024;
const EVENT_LIMIT: usize = 512;

pub(crate) struct HeadlessPage {
    cdp: Cdp,
    process: BrowserProcess,
}

impl HeadlessPage {
    pub async fn launch() -> Result<Self> {
        let binary = browser_binary()?;
        let profile = temporary_profile()?;
        let child = Command::new(binary)
            .args(page_browser_args(profile.path()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("无法启动媒体解析浏览器")?;
        // Install the cleanup guard before the first await. Cancelling launch
        // must kill the browser and keep its profile alive until it exits.
        let mut process = BrowserProcess {
            child: Some(child),
            profile: Some(profile),
        };
        let port_file = process
            .profile
            .as_ref()
            .unwrap()
            .path()
            .join("DevToolsActivePort");
        let result = tokio::select! {
            result = connect(&port_file) => result,
            _ = process.child.as_mut().unwrap().wait() => {
                Err(anyhow::anyhow!("媒体解析浏览器启动失败"))
            },
        };
        match result {
            Ok(cdp) => Ok(Self { cdp, process }),
            Err(error) => {
                process.stop().await;
                Err(error)
            }
        }
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.cdp.call(method, params).await
    }

    pub fn take_events(&mut self) -> Vec<Value> {
        self.cdp.events.drain(..).collect()
    }

    pub async fn close(mut self) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.cdp.call("Browser.close", json!({})),
        )
        .await;
        self.process.stop().await;
    }
}

struct BrowserProcess {
    child: Option<Child>,
    profile: Option<tempfile::TempDir>,
}

impl BrowserProcess {
    async fn stop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if !matches!(
                tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
                Ok(Ok(_))
            ) {
                let _ = child.kill().await;
            }
        }
        self.child.take();
        self.profile.take();
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let profile = self.profile.take();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                    drop(profile);
                });
            } else {
                // A resolver may be dropped after its runtime. Polling the
                // already-killed child needs no Tokio runtime and avoids
                // deleting the profile while Chrome is still using it.
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        if !matches!(child.try_wait(), Ok(None)) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    drop(child);
                    drop(profile);
                });
            }
        }
    }
}

fn page_browser_args(profile: &Path) -> Vec<OsString> {
    // Construct a single OS argument, preserving spaces and non-UTF8 paths.
    // Nothing here is passed through a shell.
    let mut profile_arg = OsString::from("--user-data-dir=");
    profile_arg.push(profile.as_os_str());
    let mut args = vec![profile_arg];
    args.extend(
        [
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--headless=new",
            "--autoplay-policy=no-user-gesture-required",
            "--mute-audio",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--window-size=1440,1000",
            "about:blank",
        ]
        .map(OsString::from),
    );
    args
}

fn temporary_profile() -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("ffdm-media-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    Ok(builder.tempdir()?)
}

fn browser_binary() -> Result<PathBuf> {
    let override_path = std::env::var_os("FFDM_BROWSER");
    let search_path = std::env::var_os("PATH");
    let candidates = browser_candidates();
    let binary = discover_browser(
        override_path.as_deref(),
        &candidates,
        search_path.as_deref(),
        browser_names(),
    );
    if let Some(binary) = binary {
        return Ok(binary);
    }
    if override_path.is_some() {
        bail!("FFDM_BROWSER 未指向可执行的 Chrome、Chromium 或 Edge 浏览器程序");
    }
    bail!("解析汽水音乐需要安装 Google Chrome 或 Microsoft Edge；也可用 FFDM_BROWSER 指定 Chromium 浏览器程序")
}

fn discover_browser(
    override_path: Option<&OsStr>,
    candidates: &[PathBuf],
    search_path: Option<&OsStr>,
    names: &[&str],
) -> Option<PathBuf> {
    if let Some(path) = override_path {
        let path = Path::new(path);
        // A bare command name can be resolved from PATH. Explicit paths are
        // interpreted as paths, never as command strings with arguments.
        return if path.components().count() == 1 && !path.is_absolute() {
            find_on_path(path.as_os_str(), search_path)
        } else {
            is_executable(path).then(|| path.to_path_buf())
        };
    }
    candidates
        .iter()
        .find(|path| is_executable(path))
        .cloned()
        .or_else(|| {
            names
                .iter()
                .find_map(|name| find_on_path(OsStr::new(name), search_path))
        })
}

fn find_on_path(name: &OsStr, search_path: Option<&OsStr>) -> Option<PathBuf> {
    // Empty PATH entries mean the working directory. Ignore them so an
    // accidentally downloaded executable cannot replace the browser.
    std::env::split_paths(search_path?)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn browser_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let suffixes = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Chromium.app/Contents/MacOS/Chromium",
        ];
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Applications"));
        }
        roots
            .into_iter()
            .flat_map(|root| suffixes.map(|suffix| root.join(suffix)))
            .collect()
    }
    #[cfg(target_os = "windows")]
    {
        ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"]
            .iter()
            .filter_map(std::env::var_os)
            .flat_map(|base| {
                [
                    PathBuf::from(&base).join("Google/Chrome/Application/chrome.exe"),
                    PathBuf::from(&base).join("Microsoft/Edge/Application/msedge.exe"),
                    PathBuf::from(&base).join("Chromium/Application/chrome.exe"),
                ]
            })
            .collect()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/microsoft-edge",
            "/usr/bin/microsoft-edge-stable",
            "/snap/bin/chromium",
        ]
        .map(PathBuf::from)
        .into()
    }
}

fn browser_names() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &["chrome.exe", "msedge.exe", "chromium.exe"]
    }
    #[cfg(not(target_os = "windows"))]
    {
        &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "microsoft-edge-stable",
        ]
    }
}

struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    next_id: u64,
    events: VecDeque<Value>,
}

impl Cdp {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("浏览器命令编号已耗尽")?;
        let id = self.next_id;
        tokio::time::timeout(Duration::from_secs(4), async {
            self.socket
                .send(Message::Text(
                    json!({"id":id,"method":method,"params":params})
                        .to_string()
                        .into(),
                ))
                .await?;
            while let Some(message) = self.socket.next().await {
                let message = message?;
                if let Message::Ping(data) = message {
                    self.socket.send(Message::Pong(data)).await?;
                    continue;
                }
                let Message::Text(text) = message else {
                    continue;
                };
                ensure!(text.len() <= CONTROL_RESPONSE_LIMIT, "浏览器控制响应过大");
                let response: Value = serde_json::from_str(&text)?;
                if response["id"].as_u64() == Some(id) {
                    ensure!(response.get("error").is_none(), "浏览器未能执行媒体检测");
                    return Ok(response["result"].clone());
                }
                if matches!(
                    response["method"].as_str(),
                    Some("Network.requestWillBeSent" | "Network.responseReceived")
                ) {
                    if self.events.len() >= EVENT_LIMIT {
                        self.events.pop_front();
                    }
                    self.events.push_back(response);
                }
            }
            bail!("媒体解析浏览器已关闭")
        })
        .await
        .context("浏览器响应超时")?
    }
}

fn debugger_endpoint(endpoint: &str, port: u16) -> Result<Url> {
    let url = Url::parse(endpoint)?;
    ensure!(
        url.scheme() == "ws"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(port)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.path().starts_with("/devtools/page/"),
        "浏览器控制地址不是本机页面地址"
    );
    Ok(url)
}

async fn connect(port_file: &Path) -> Result<Cdp> {
    let client = reqwest_media::Client::builder()
        .no_proxy()
        .redirect(reqwest_media::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(port_file).await {
                if let Some(port) = text
                    .lines()
                    .next()
                    .and_then(|value| value.parse::<u16>().ok())
                    .filter(|port| *port > 0)
                {
                    if let Ok(mut response) = client
                        .get(format!("http://127.0.0.1:{port}/json/list"))
                        .send()
                        .await
                    {
                        ensure!(response.status().is_success(), "浏览器页面列表请求失败");
                        let mut bytes = Vec::new();
                        while let Some(chunk) = response.chunk().await? {
                            ensure!(
                                bytes.len() + chunk.len() <= CONTROL_RESPONSE_LIMIT,
                                "浏览器页面列表过大"
                            );
                            bytes.extend_from_slice(&chunk);
                        }
                        let targets: Vec<Value> = serde_json::from_slice(&bytes)?;
                        if let Some(endpoint) = targets
                            .iter()
                            .find(|target| target["type"] == "page")
                            .and_then(|target| target["webSocketDebuggerUrl"].as_str())
                        {
                            let url = debugger_endpoint(endpoint, port)?;
                            let config = WebSocketConfig::default()
                                .max_message_size(Some(CONTROL_RESPONSE_LIMIT))
                                .max_frame_size(Some(CONTROL_RESPONSE_LIMIT));
                            let (socket, _) =
                                connect_async_with_config(url.as_str(), Some(config), false)
                                    .await?;
                            return Ok(Cdp {
                                socket,
                                next_id: 0,
                                events: VecDeque::new(),
                            });
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .context("媒体解析浏览器启动超时")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_has_isolated_profile_and_no_capture_flags() {
        let args = page_browser_args(Path::new("/tmp/ffdm profile $special"));
        assert_eq!(args[0], "--user-data-dir=/tmp/ffdm profile $special");
        assert!(args.iter().any(|arg| arg == "--headless=new"));
        assert!(args
            .iter()
            .any(|arg| arg == "--remote-debugging-address=127.0.0.1"));
        assert!(args.iter().any(|arg| arg == "--remote-debugging-port=0"));
        assert_eq!(args.last().unwrap(), "about:blank");
        for arg in args {
            let arg = arg.to_string_lossy();
            assert!(!arg.contains("proxy-server"));
            assert!(!arg.contains("certificate-errors"));
            assert!(!arg.contains("no-sandbox"));
            assert!(!arg.contains("remote-allow-origins"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn temporary_profile_is_private_and_removed_after_use() {
        use std::os::unix::fs::PermissionsExt;
        let profile = temporary_profile().unwrap();
        let path = profile.path().to_owned();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(profile);
        assert!(!path.exists());
    }

    #[test]
    fn control_endpoint_cannot_escape_the_assigned_loopback_port() {
        let valid = "ws://127.0.0.1:12345/devtools/page/session";
        assert!(debugger_endpoint(valid, 12345).is_ok());
        for endpoint in [
            "ws://127.0.0.1:12346/devtools/page/session",
            "ws://localhost:12345/devtools/page/session",
            "ws://192.168.1.1:12345/devtools/page/session",
            "wss://127.0.0.1:12345/devtools/page/session",
            "ws://user:password@127.0.0.1:12345/devtools/page/session",
            "ws://127.0.0.1:12345/devtools/browser/session",
            "ws://127.0.0.1:12345/devtools/page/session#fragment",
        ] {
            assert!(debugger_endpoint(endpoint, 12345).is_err(), "{endpoint}");
        }
    }

    fn executable(path: &Path) {
        std::fs::write(path, b"test browser").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn explicit_browser_override_is_an_executable_path_not_shell_text() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("Browser with spaces");
        executable(&binary);
        assert_eq!(
            discover_browser(Some(binary.as_os_str()), &[], None, &[]),
            Some(binary.clone())
        );
        assert!(discover_browser(
            Some(OsStr::new(
                "Browser with spaces --remote-debugging-port=9222"
            )),
            &[binary],
            None,
            &[],
        )
        .is_none());
    }

    #[test]
    fn discovery_uses_known_paths_then_absolute_path_entries() {
        let directory = tempfile::tempdir().unwrap();
        let known = directory.path().join("known-browser");
        let path_browser = directory.path().join("chrome-test");
        executable(&known);
        executable(&path_browser);
        let search_path = std::env::join_paths([Path::new(""), directory.path()]).unwrap();
        assert_eq!(
            discover_browser(
                None,
                std::slice::from_ref(&known),
                Some(&search_path),
                &["chrome-test"]
            ),
            Some(known)
        );
        assert_eq!(
            discover_browser(None, &[], Some(&search_path), &["chrome-test"]),
            Some(path_browser.clone())
        );
        assert_eq!(
            discover_browser(
                Some(OsStr::new("chrome-test")),
                &[],
                Some(&search_path),
                &[]
            ),
            Some(path_browser)
        );
        assert!(discover_browser(None, &[], Some(OsStr::new("")), &["chrome-test"]).is_none());
        assert!(discover_browser(None, &[directory.path().into()], None, &[]).is_none());
    }

    #[tokio::test]
    async fn control_retains_only_bounded_network_events() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = socket.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
            socket.send(Message::Ping(vec![1, 2].into())).await.unwrap();
            assert!(matches!(
                socket.next().await.unwrap().unwrap(),
                Message::Pong(_)
            ));
            for index in 0..EVENT_LIMIT + 2 {
                let event =
                    json!({"method":"Network.requestWillBeSent","params":{"requestId":index}});
                socket
                    .send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
            for response in [
                json!({"method":"Page.loadEventFired","params":{}}),
                json!({"method":"Network.responseReceived","params":{"requestId":"audio"}}),
                json!({"id":request["id"],"result":{"ok":true}}),
            ] {
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let mut cdp = Cdp {
            socket,
            next_id: 0,
            events: VecDeque::new(),
        };
        assert_eq!(
            cdp.call("Runtime.evaluate", json!({})).await.unwrap(),
            json!({"ok":true})
        );
        assert_eq!(cdp.events.len(), EVENT_LIMIT);
        assert_eq!(cdp.events[0]["params"]["requestId"], 3);
        assert_eq!(
            cdp.events.back().unwrap()["method"],
            "Network.responseReceived"
        );
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_launch_guard_reaps_child_before_deleting_profile() {
        let profile = tempfile::tempdir().unwrap();
        let profile_path = profile.path().to_owned();
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        drop(BrowserProcess {
            child: Some(child),
            profile: Some(profile),
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while profile_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
