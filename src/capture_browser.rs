//! A browser owned by the capture session. CDP is loopback-only and uses a
//! dedicated profile, never the user's everyday browser or saved credentials.
use anyhow::{ensure, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    sync::oneshot,
    task::JoinHandle,
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

#[derive(Clone, Serialize)]
pub struct BrowserStatus {
    pub visible: bool,
    pub phase: &'static str,
    pub started_at: u64,
}

pub struct CaptureBrowser {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
    status: Arc<Mutex<BrowserStatus>>,
}

impl CaptureBrowser {
    pub async fn launch(
        binary: &Path,
        profile: &Path,
        proxy: &str,
        spki: &str,
        url: &str,
        visible: bool,
    ) -> Result<Self> {
        tokio::fs::create_dir_all(profile).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let port_file = profile.join("DevToolsActivePort");
        match tokio::fs::remove_file(&port_file).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut command = Command::new(binary);
        command
            .args(browser_args(profile, proxy, spki, url, visible))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = command.spawn().context("无法启动捕获浏览器")?;
        let status = Arc::new(Mutex::new(BrowserStatus {
            visible,
            phase: "starting",
            started_at: crate::service::now_ms(),
        }));
        let state = status.clone();
        let (stop, wait) = oneshot::channel();
        let task = tokio::spawn(async move {
            run(child, port_file, visible, wait, state).await;
        });
        Ok(Self {
            stop: Some(stop),
            task,
            status,
        })
    }

    pub fn status(&self) -> BrowserStatus {
        self.status.lock().unwrap().clone()
    }

    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}

impl Drop for CaptureBrowser {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn browser_args(profile: &Path, proxy: &str, spki: &str, url: &str, visible: bool) -> Vec<String> {
    let mut args = vec![
        format!("--user-data-dir={}", profile.display()),
        format!("--proxy-server=http://{proxy}"),
        "--proxy-bypass-list=<-loopback>".into(),
        format!("--ignore-certificate-errors-spki-list={spki}"),
        "--remote-debugging-address=127.0.0.1".into(),
        "--remote-debugging-port=0".into(),
        "--disable-quic".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
    ];
    if visible {
        args.push("--new-window".into());
    } else {
        args.extend(
            [
                "--headless=new",
                "--autoplay-policy=no-user-gesture-required",
                "--mute-audio",
                "--window-size=1440,1000",
            ]
            .map(String::from),
        );
    }
    args.push(url.into());
    args
}

async fn run(
    mut child: Child,
    port_file: std::path::PathBuf,
    visible: bool,
    mut stop: oneshot::Receiver<()>,
    state: Arc<Mutex<BrowserStatus>>,
) {
    let connected = tokio::select! {
        _ = &mut stop => None,
        _ = child.wait() => { state.lock().unwrap().phase = "failed"; None },
        result = connect(&port_file) => Some(result),
    };
    if let Some(Ok(mut cdp)) = connected {
        state.lock().unwrap().phase = if visible { "manual" } else { "scanning" };
        let phase = tokio::select! {
            _ = &mut stop => "stopped",
            _ = child.wait() => if visible { "closed" } else { "finished" },
            result = inspect(&mut cdp, visible) => if result.is_ok() { "finished" } else { "failed" },
        };
        // Browser.close flushes this profile's login state before a mode switch.
        let _ = tokio::time::timeout(Duration::from_secs(2), cdp.call("Browser.close", json!({})))
            .await;
        state.lock().unwrap().phase = phase;
    } else if connected.is_some() {
        state.lock().unwrap().phase = "failed";
    } else if state.lock().unwrap().phase != "failed" {
        state.lock().unwrap().phase = "stopped";
    }
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    next_id: u64,
}

impl Cdp {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
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
                ensure!(text.len() <= 1024 * 1024, "浏览器控制响应过大");
                let response: Value = serde_json::from_str(&text)?;
                if response["id"].as_u64() == Some(id) {
                    ensure!(response.get("error").is_none(), "浏览器未能执行媒体检测");
                    return Ok(response["result"].clone());
                }
            }
            anyhow::bail!("捕获浏览器已关闭")
        })
        .await
        .context("浏览器响应超时")?
    }
}

async fn connect(port_file: &Path) -> Result<Cdp> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(port_file).await {
                if let Some(port) = text
                    .lines()
                    .next()
                    .and_then(|p| p.parse::<u16>().ok())
                    .filter(|p| *p > 0)
                {
                    if let Ok(response) = client
                        .get(format!("http://127.0.0.1:{port}/json/list"))
                        .send()
                        .await
                    {
                        let bytes = response.bytes().await?;
                        ensure!(bytes.len() <= 1024 * 1024, "浏览器页面列表过大");
                        let targets: Vec<Value> = serde_json::from_slice(&bytes)?;
                        if let Some(endpoint) = targets
                            .iter()
                            .find(|t| t["type"] == "page")
                            .and_then(|t| t["webSocketDebuggerUrl"].as_str())
                        {
                            let url = reqwest::Url::parse(endpoint)?;
                            ensure!(
                                url.scheme() == "ws"
                                    && url.host_str() == Some("127.0.0.1")
                                    && url.port() == Some(port),
                                "浏览器控制地址不是本机地址"
                            );
                            let (socket, _) = connect_async(endpoint).await?;
                            return Ok(Cdp { socket, next_id: 0 });
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .context("捕获浏览器启动超时，可尝试打开窗口处理")?
}

async fn inspect(cdp: &mut Cdp, visible: bool) -> Result<()> {
    if visible {
        return std::future::pending().await;
    }
    // Only activate media elements. Never click login, purchase or consent UI.
    // Some sites require human interaction; those keep the explicit window fallback.
    for _ in 0..30 {
        cdp.call("Runtime.evaluate", json!({
            "expression":"(() => { const media = [...document.querySelectorAll('video,audio')]; for (const m of media) { m.muted = true; m.playsInline = true; if (m.paused) m.play().catch(() => {}); } return media.length; })()",
            "returnByValue":true, "userGesture":true,
        })).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_is_default_and_interactive_profile_stays_isolated() {
        let args = browser_args(
            Path::new("/tmp/ffdm profile"),
            "127.0.0.1:12345",
            "test-key",
            "https://example.com/",
            false,
        );
        assert!(args.iter().any(|a| a == "--headless=new"));
        assert!(args
            .iter()
            .any(|a| a == "--remote-debugging-address=127.0.0.1"));
        assert!(args
            .iter()
            .any(|a| a == "--user-data-dir=/tmp/ffdm profile"));
        assert!(!args
            .iter()
            .any(|a| a == "--ignore-certificate-errors" || a == "--no-sandbox"));
        let visible = browser_args(
            Path::new("/tmp/ffdm profile"),
            "127.0.0.1:12345",
            "test-key",
            "https://example.com/",
            true,
        );
        assert!(visible.iter().any(|a| a == "--new-window"));
        assert!(!visible.iter().any(|a| a.starts_with("--headless")));
    }
}
