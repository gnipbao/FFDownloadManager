//! Transactional macOS proxy configuration. Only UI POST actions mutate settings.
//! A durable journal is written before the first change; recovery only touches
//! values that still belong to us, so another proxy app keeps its newer settings.
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    Http,
    Https,
    Socks,
}
impl Kind {
    fn stem(self) -> &'static str {
        match self {
            Self::Http => "webproxy",
            Self::Https => "securewebproxy",
            Self::Socks => "socksfirewallproxy",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Setting {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
    pub authenticated: bool,
}
impl Setting {
    fn equivalent(&self, other: &Self) -> bool {
        self == other || (!self.enabled && !other.enabled && other.server.is_empty())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Network {
    pub name: String,
    pub settings: Vec<(Kind, Setting)>,
    pub pac: bool,
    pub discovery: bool,
}
impl Network {
    pub fn upstream(&self) -> Result<Option<String>> {
        ensure!(!self.pac && !self.discovery, "此网络启用了自动代理（PAC / 自动发现），请先在原代理工具中关闭自动代理，或手动设置应用代理");
        let mut upstream = None;
        for (kind, setting) in &self.settings {
            ensure!(
                !setting.authenticated,
                "此网络有需要认证的代理，MVP 不改写其配置；请手动为目标应用设置代理"
            );
            if *kind == Kind::Socks || !setting.enabled {
                continue;
            }
            ensure!(
                !setting.server.is_empty() && setting.port > 0,
                "已有代理地址无效"
            );
            let mut url = reqwest::Url::parse("http://localhost")?;
            url.set_host(Some(&setting.server))
                .map_err(|_| anyhow::anyhow!("已有代理地址无效"))?;
            url.set_port(Some(setting.port))
                .map_err(|_| anyhow::anyhow!("已有代理端口无效"))?;
            let value = url.as_str().trim_end_matches('/').to_owned();
            ensure!(
                upstream.as_ref().is_none_or(|old| old == &value),
                "HTTP 与 HTTPS 代理地址不同，MVP 不自动改写；请手动设置应用代理"
            );
            upstream = Some(value);
        }
        ensure!(
            upstream.is_some()
                || !self
                    .settings
                    .iter()
                    .any(|(k, s)| *k == Kind::Socks && s.enabled),
            "目前只配置了 SOCKS 代理，请先让原代理工具提供 HTTP 代理端口"
        );
        Ok(upstream)
    }
    fn captured(&self, port: u16) -> Self {
        let mut result = self.clone();
        for (kind, setting) in &mut result.settings {
            if *kind == Kind::Socks {
                setting.enabled = false;
            } else {
                *setting = Setting {
                    enabled: true,
                    server: "127.0.0.1".into(),
                    port,
                    authenticated: false,
                };
            }
        }
        result
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    schema: u8,
    before: Network,
    owned: Network,
}

#[async_trait]
pub(crate) trait Backend: Send + Sync {
    async fn services(&self) -> Result<Vec<String>>;
    async fn read(&self, name: &str) -> Result<Network>;
    async fn write(&self, name: &str, kind: Kind, setting: &Setting) -> Result<()>;
}
pub(crate) struct MacBackend;
pub(crate) struct SystemProxy<B = MacBackend> {
    pub backend: B,
    path: PathBuf,
    journal: Option<Journal>,
    health: Option<(std::time::Instant, Option<String>)>,
}
impl SystemProxy {
    pub fn new(path: PathBuf) -> Self {
        Self {
            backend: MacBackend,
            path,
            journal: None,
            health: None,
        }
    }
}
impl<B: Backend> SystemProxy<B> {
    pub async fn connection_issue(&mut self) -> Option<String> {
        let Some(journal) = &self.journal else {
            self.health = None;
            return None;
        };
        if let Some((at, value)) = &self.health {
            if at.elapsed() < Duration::from_secs(5) {
                return value.clone();
            }
        }
        let issue = match self.backend.read(&journal.before.name).await {
            Ok(network) if network == journal.owned => None,
            Ok(_) => Some("系统代理已被其他软件改写，应用流量可能未经过 FFDownload。请停止捕获，刷新状态后重新开启；不要让其他代理工具同时接管系统代理。".into()),
            Err(_) => Some("无法核实当前系统代理，请刷新网络状态后重试。".into()),
        };
        self.health = Some((std::time::Instant::now(), issue.clone()));
        issue
    }
    pub fn active_service(&self) -> Option<String> {
        self.journal.as_ref().map(|j| j.before.name.clone())
    }
    pub fn original_upstream(&self) -> Option<String> {
        self.journal
            .as_ref()
            .and_then(|j| j.before.upstream().ok().flatten())
    }
    pub fn recovery_pending(&self) -> bool {
        self.journal.is_some() || self.path.exists()
    }
    pub async fn prepare(&self, name: &str) -> Result<Network> {
        ensure!(!self.recovery_pending(), "请先恢复上一次应用抓包的代理配置");
        ensure!(
            self.backend.services().await?.iter().any(|n| n == name),
            "请选择实际存在的网络服务"
        );
        let before = self.backend.read(name).await?;
        before.upstream()?;
        Ok(before)
    }
    pub async fn enable(&mut self, before: Network, port: u16) -> Result<()> {
        self.health = None;
        ensure!(!self.recovery_pending(), "请先恢复上一次代理配置");
        ensure!(port > 0, "无效的抓包代理端口");
        before.upstream()?;
        ensure!(
            self.backend.read(&before.name).await? == before,
            "网络代理已被其他应用修改，请刷新配置后重试"
        );
        let owned = before.captured(port);
        let journal = Journal {
            schema: 1,
            before,
            owned,
        };
        crate::capture_ca::atomic_write(&self.path, &serde_json::to_vec(&journal)?)?;
        self.journal = Some(journal.clone());
        for (kind, setting) in &journal.owned.settings {
            let result = self
                .backend
                .write(&journal.before.name, *kind, setting)
                .await;
            if let Err(e) = result {
                let recovery = self.restore().await;
                return Err(match recovery {
                    Ok(()) => e.context("启用失败，已恢复原代理配置"),
                    Err(r) => anyhow::anyhow!("启用失败：{e}；恢复仍需处理：{r}。请点击恢复原代理"),
                });
            }
        }
        if !self
            .backend
            .read(&journal.before.name)
            .await
            .is_ok_and(|state| state == journal.owned)
        {
            self.restore().await?;
            bail!("系统未接受代理配置，已恢复；请检查 macOS 网络设置权限");
        }
        Ok(())
    }
    pub async fn restore(&mut self) -> Result<()> {
        self.health = None;
        if self.journal.is_none() && self.path.exists() {
            let journal: Journal = serde_json::from_slice(&std::fs::read(&self.path)?)
                .context("代理恢复记录损坏，请在系统网络设置中检查 HTTP / HTTPS 代理")?;
            ensure!(journal.schema == 1, "不支持的代理恢复记录");
            self.journal = Some(journal);
        }
        let Some(journal) = self.journal.clone() else {
            return Ok(());
        };
        let mut errors = vec![];
        for (kind, original) in &journal.before.settings {
            let current = self.backend.read(&journal.before.name).await?;
            let setting = &current
                .settings
                .iter()
                .find(|(k, _)| k == kind)
                .context("无法读取代理恢复状态")?
                .1;
            let ours = &journal
                .owned
                .settings
                .iter()
                .find(|(k, _)| k == kind)
                .context("代理恢复记录不完整")?
                .1;
            if setting.equivalent(original) {
                continue;
            }
            // A setter may have updated the endpoint before enabling it.
            let partial = *kind != Kind::Socks
                && setting.server == ours.server
                && setting.port == ours.port
                && !setting.authenticated;
            if setting != ours && !partial {
                continue;
            }
            if let Err(e) = self
                .backend
                .write(&journal.before.name, *kind, original)
                .await
            {
                errors.push(e.to_string());
            } else {
                let verified = self.backend.read(&journal.before.name).await?;
                if !verified
                    .settings
                    .iter()
                    .find(|(k, _)| k == kind)
                    .context("无法验证恢复状态")?
                    .1
                    .equivalent(original)
                {
                    errors.push("系统未接受恢复设置".into());
                }
            }
        }
        ensure!(errors.is_empty(), "恢复原代理失败：{}", errors.join("；"));
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        self.journal = None;
        Ok(())
    }
}

pub(crate) async fn run(program: &str, args: &[&str]) -> Result<String> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).env("LC_ALL", "C").kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .context("系统配置命令超时，请在系统设置中检查代理状态")??;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    ensure!(
        out.status.success()
            && !stdout.contains("Error:")
            && !stdout.contains("AuthorizationCreate")
            && !stdout.contains("You need administrator"),
        "系统未允许此操作，请在系统设置中检查权限或手动配置代理"
    );
    Ok(stdout)
}
fn parse_setting(raw: &str) -> Result<Setting> {
    let fields: std::collections::HashMap<_, _> = raw
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    Ok(Setting {
        enabled: match fields.get("Enabled") {
            Some(&"Yes") => true,
            Some(&"No") => false,
            _ => bail!("无法识别系统代理启用状态"),
        },
        server: fields
            .get("Server")
            .context("缺少代理服务器配置")?
            .to_string(),
        port: fields.get("Port").context("缺少代理端口")?.parse()?,
        authenticated: match fields.get("Authenticated Proxy Enabled") {
            Some(&"0") => false,
            Some(&"1") => true,
            _ => bail!("无法识别代理认证配置"),
        },
    })
}
#[async_trait]
impl Backend for MacBackend {
    async fn services(&self) -> Result<Vec<String>> {
        ensure!(
            cfg!(target_os = "macos"),
            "当前系统请手动设置应用 HTTP / HTTPS 代理"
        );
        let raw = run("/usr/sbin/networksetup", &["-listallnetworkservices"]).await?;
        Ok(raw
            .lines()
            .skip(1)
            .filter(|n| !n.starts_with('*') && !n.is_empty())
            .map(str::to_owned)
            .collect())
    }
    async fn read(&self, name: &str) -> Result<Network> {
        let mut settings = vec![];
        for kind in [Kind::Http, Kind::Https, Kind::Socks] {
            settings.push((
                kind,
                parse_setting(
                    &run(
                        "/usr/sbin/networksetup",
                        &[&format!("-get{}", kind.stem()), name],
                    )
                    .await?,
                )?,
            ));
        }
        let pac = run("/usr/sbin/networksetup", &["-getautoproxyurl", name]).await?;
        let discovery = run("/usr/sbin/networksetup", &["-getproxyautodiscovery", name]).await?;
        ensure!(
            pac.contains("Enabled: Yes") || pac.contains("Enabled: No"),
            "无法识别自动代理状态"
        );
        ensure!(
            discovery.contains(": On") || discovery.contains(": Off"),
            "无法识别代理自动发现状态"
        );
        Ok(Network {
            name: name.into(),
            settings,
            pac: pac.contains("Enabled: Yes"),
            discovery: discovery.contains(": On"),
        })
    }
    async fn write(&self, name: &str, kind: Kind, setting: &Setting) -> Result<()> {
        if !setting.server.is_empty() && setting.port > 0 {
            run(
                "/usr/sbin/networksetup",
                &[
                    &format!("-set{}", kind.stem()),
                    name,
                    &setting.server,
                    &setting.port.to_string(),
                    "off",
                ],
            )
            .await?;
        }
        run(
            "/usr/sbin/networksetup",
            &[
                &format!("-set{}state", kind.stem()),
                name,
                if setting.enabled { "on" } else { "off" },
            ],
        )
        .await?;
        Ok(())
    }
}
pub(crate) async fn certificate_trusted(path: &Path) -> bool {
    cfg!(target_os = "macos")
        && run(
            "/usr/bin/security",
            &[
                "verify-cert",
                "-c",
                &path.to_string_lossy(),
                "-p",
                "ssl",
                "-q",
            ],
        )
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Fake {
        network: Arc<Mutex<Network>>,
        fail: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Backend for Fake {
        async fn services(&self) -> Result<Vec<String>> {
            Ok(vec!["Wi-Fi".into()])
        }
        async fn read(&self, _: &str) -> Result<Network> {
            Ok(self.network.lock().unwrap().clone())
        }
        async fn write(&self, _: &str, kind: Kind, setting: &Setting) -> Result<()> {
            if kind == Kind::Https && *self.fail.lock().unwrap() {
                *self.fail.lock().unwrap() = false;
                bail!("simulated failure");
            }
            self.network
                .lock()
                .unwrap()
                .settings
                .iter_mut()
                .find(|(k, _)| *k == kind)
                .unwrap()
                .1 = setting.clone();
            Ok(())
        }
    }
    fn setup(path: PathBuf) -> (SystemProxy<Fake>, Network) {
        let network = Network {
            name: "Wi-Fi".into(),
            settings: [Kind::Http, Kind::Https, Kind::Socks]
                .into_iter()
                .map(|kind| {
                    (
                        kind,
                        Setting {
                            enabled: true,
                            server: "127.0.0.1".into(),
                            port: 7897,
                            authenticated: false,
                        },
                    )
                })
                .collect(),
            pac: false,
            discovery: false,
        };
        let fake = Fake {
            network: Arc::new(Mutex::new(network.clone())),
            fail: Arc::new(Mutex::new(false)),
        };
        (
            SystemProxy {
                backend: fake,
                path,
                journal: None,
                health: None,
            },
            network,
        )
    }
    #[tokio::test]
    async fn restores_after_restart_and_keeps_existing_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, before) = setup(dir.path().join("proxy.json"));
        assert_eq!(
            before.upstream().unwrap().as_deref(),
            Some("http://127.0.0.1:7897")
        );
        manager.enable(before.clone(), 18899).await.unwrap();
        assert!(
            !manager.backend.read("Wi-Fi").await.unwrap().settings[2]
                .1
                .enabled
        );
        manager.journal = None; // Simulate a new process reading the durable journal.
        manager.restore().await.unwrap();
        assert_eq!(manager.backend.read("Wi-Fi").await.unwrap(), before);
        assert!(!manager.recovery_pending());
    }
    #[tokio::test]
    async fn partial_failure_rolls_back_and_external_changes_win() {
        let dir = tempfile::tempdir().unwrap();
        let (mut manager, before) = setup(dir.path().join("proxy.json"));
        *manager.backend.fail.lock().unwrap() = true;
        assert!(manager.enable(before.clone(), 18899).await.is_err());
        assert_eq!(manager.backend.read("Wi-Fi").await.unwrap(), before);
        manager.enable(before.clone(), 18899).await.unwrap();
        let changed = Setting {
            port: 9999,
            ..before.settings[0].1.clone()
        };
        manager
            .backend
            .write("Wi-Fi", Kind::Http, &changed)
            .await
            .unwrap();
        assert!(manager.connection_issue().await.is_some());
        manager.restore().await.unwrap();
        assert!(manager.connection_issue().await.is_none());
        let after = manager.backend.read("Wi-Fi").await.unwrap();
        assert_eq!(after.settings[0].1, changed);
        assert_eq!(after.settings[1], before.settings[1]);
        assert_eq!(after.settings[2], before.settings[2]);
    }
    #[test]
    fn rejects_unrecoverable_auth_and_ambiguous_proxy_settings() {
        let (_, mut network) = setup("unused".into());
        network.settings[0].1.authenticated = true;
        assert!(network.upstream().is_err());
        network.settings[0].1.authenticated = false;
        network.pac = true;
        assert!(network.upstream().is_err());
        network.pac = false;
        network.settings[1].1.port = 9999;
        assert!(network.upstream().is_err());
        assert!(parse_setting("AuthorizationCreate() failed").is_err());
    }
}
