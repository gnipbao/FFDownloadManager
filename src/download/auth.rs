//! File credentials remain in memory while durable range checkpoints contain
//! only non-secret download state. This adapter is intentionally file-only.
use anyhow::{ensure, Result};
use gosh_dl::{
    DownloadId, DownloadState, DownloadStatus, EngineError, Segment, SegmentState, Storage,
};
use reqwest::{header, Client, RequestBuilder, Url};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

/// A single task's user-supplied request headers. Never serialize or debug-print
/// this type, including otherwise ordinary fields such as Referer or User-Agent.
#[derive(Clone)]
pub struct FileAuthorization {
    origin: String,
    headers: Vec<(header::HeaderName, header::HeaderValue)>,
    #[cfg(test)]
    local_test_certificate: bool,
}

impl FileAuthorization {
    pub fn new(url: &str, cookie: Option<&str>) -> Result<Option<Self>> {
        Self::with_headers(url, cookie, &[])
    }

    /// Credentials are scoped to this exact HTTPS origin, kept only in memory,
    /// and replaced as one set when a user retries an authorized download.
    pub fn with_headers(
        url: &str,
        cookie: Option<&str>,
        headers: &[(String, String)],
    ) -> Result<Option<Self>> {
        ensure!(
            cookie.is_none_or(
                |value| value.is_ascii() && !value.bytes().any(|byte| byte.is_ascii_control())
            ),
            "Cookie 必须为 ASCII 文本，不能包含换行或控制字符"
        );
        let cookie = cookie.filter(|value| !value.trim().is_empty());
        let count = headers.len().saturating_add(usize::from(cookie.is_some()));
        let size = headers
            .iter()
            .fold(0_usize, |size, (name, value)| {
                size.saturating_add(name.len()).saturating_add(value.len())
            })
            .saturating_add(cookie.map_or(0, |value| "Cookie".len().saturating_add(value.len())));
        ensure!(
            count <= 48 && size <= 16 * 1024,
            "自定义请求头最多 48 项，总大小不能超过 16 KiB"
        );
        let mut names = HashSet::new();
        let mut parsed = Vec::with_capacity(count);
        for (name, value) in headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .chain(cookie.map(|value| ("Cookie", value.trim())))
        {
            let name = header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| anyhow::anyhow!("自定义请求头名称无效"))?;
            ensure!(
                value.is_ascii() && !value.bytes().any(|byte| byte.is_ascii_control()),
                "自定义请求头内容必须是 ASCII 文本，不能包含换行或控制字符"
            );
            let value = value.trim();
            if name == header::COOKIE && value.is_empty() {
                continue;
            }
            ensure!(
                !matches!(
                    name.as_str(),
                    "host"
                        | "range"
                        | "if-range"
                        | "if-match"
                        | "if-none-match"
                        | "if-modified-since"
                        | "if-unmodified-since"
                        | "content-length"
                        | "transfer-encoding"
                        | "connection"
                        | "keep-alive"
                        | "te"
                        | "trailer"
                        | "upgrade"
                        | "accept-encoding"
                ) && !name.as_str().starts_with("proxy-"),
                "自定义请求头不能覆盖连接、代理或断点续传控制字段"
            );
            ensure!(
                names.insert(name.clone()),
                "自定义请求头不能重复；Cookie 快捷字段与请求头中不能同时填写 Cookie"
            );
            let mut value = header::HeaderValue::from_str(value)
                .map_err(|_| anyhow::anyhow!("自定义请求头内容无效"))?;
            value.set_sensitive(true);
            parsed.push((name, value));
        }
        if parsed.is_empty() {
            return Ok(None);
        }
        let url = crate::filename::parse_url(url)?;
        ensure!(
            url.scheme() == "https",
            "携带自定义请求头的文件下载仅支持 HTTPS"
        );
        Ok(Some(Self {
            origin: url.origin().ascii_serialization(),
            headers: parsed,
            #[cfg(test)]
            local_test_certificate: false,
        }))
    }

    pub(crate) fn validate_url(&self, raw: &str) -> Result<Url> {
        let url = crate::filename::parse_url(raw)?;
        ensure!(
            url.scheme() == "https" && url.origin().ascii_serialization() == self.origin,
            "自定义请求头仅能用于本次授权的 HTTPS 来源（包括端口）"
        );
        Ok(url)
    }

    pub(crate) fn client(&self, timeout: Duration) -> Result<Client> {
        // User headers may contain credentials even without an Authorization or
        // Cookie field. Never replay any of them through a redirect.
        let builder = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout);
        #[cfg(test)]
        let builder = builder.danger_accept_invalid_certs(self.local_test_certificate);
        Ok(builder.build()?)
    }

    pub(crate) fn request(&self, request: RequestBuilder) -> RequestBuilder {
        self.headers.iter().fold(request, |request, (name, value)| {
            request.header(name.clone(), value.clone())
        })
    }

    pub(super) async fn verify_resume_authorization(&self, raw: &str) -> Result<()> {
        let url = self.validate_url(raw)?;
        let client = self.client(Duration::from_secs(12))?;
        let response = self
            .request(client.get(url))
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::RANGE, "bytes=0-0")
            .send()
            .await?;
        ensure!(
            !response.status().is_redirection(),
            "授权下载不跟随重定向，请使用文件的最终 HTTPS 下载地址"
        );
        // Drop without reading a payload, even if the origin ignores Range.
        // Validate before a failed HEAD probe can make the engine replace the
        // saved segmented state with its single-connection fallback state.
        response.error_for_status()?;
        Ok(())
    }

    pub(super) fn headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_str().into(), value.to_str().unwrap().into()))
            .collect()
    }

    fn user_agent(&self) -> Option<String> {
        self.headers
            .iter()
            .find(|(name, _)| name == header::USER_AGENT)
            .map(|(_, value)| value.to_str().unwrap().into())
    }

    fn engine_headers(&self) -> Vec<(String, String)> {
        // gosh-dl sends its dedicated user_agent separately from custom headers.
        // Restoring it in both slots appends two User-Agent values.
        self.headers()
            .into_iter()
            .filter(|(name, _)| name != "user-agent")
            .collect()
    }

    pub(crate) fn safe_error(&self, error: &anyhow::Error) -> anyhow::Error {
        let message = format!("{error:#}");
        // Keep a numeric HTTP status recognizable even for very short cookie
        // values (e.g. session=1), without retaining source-provided error text.
        let status = error
            .chain()
            .find_map(|error| match error.downcast_ref::<EngineError>() {
                Some(EngineError::Network {
                    kind: gosh_dl::NetworkErrorKind::HttpStatus(status),
                    ..
                }) => Some(*status),
                _ => error
                    .downcast_ref::<reqwest::Error>()
                    .and_then(reqwest::Error::status)
                    .map(|status| status.as_u16()),
            });
        static HTTP_STATUS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r"(?i)(?:HTTP (?:error|status):?\s*|HEAD request returned:\s*)(\d{3})\b",
            )
            .unwrap()
        });
        let status = status.or_else(|| {
            HTTP_STATUS
                .captures(&message)?
                .get(1)?
                .as_str()
                .parse::<u16>()
                .ok()
        });
        if let Some(status) = status {
            return anyhow::anyhow!("HTTP status: {status}，授权文件请求失败");
        }
        let redirect = error.chain().any(|error| {
            error
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_redirect)
                || matches!(
                    error.downcast_ref::<EngineError>(),
                    Some(EngineError::Network {
                        kind: gosh_dl::NetworkErrorKind::TooManyRedirects,
                        ..
                    })
                )
        });
        if redirect || message.to_ascii_lowercase().contains("redirect") {
            return anyhow::anyhow!("授权下载不跟随重定向，请使用文件的最终 HTTPS 下载地址");
        }
        anyhow::anyhow!(self.redact(&message))
    }

    /// Filter an error before showing it or recording it in a task's history.
    pub fn redact_error(&self, message: &str) -> String {
        self.redact(message)
    }

    #[cfg(test)]
    pub(super) fn accepts_local_test_certificate(&self) -> bool {
        self.local_test_certificate
    }

    // Not compiled into library consumers, the CLI, desktop or release builds.
    #[cfg(test)]
    fn for_local_tls_test(url: &str, cookie: &str) -> Self {
        let parsed = Url::parse(url).unwrap();
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        assert!(parsed.port().is_some());
        let mut authorization = Self::new(url, Some(cookie)).unwrap().unwrap();
        authorization.local_test_certificate = true;
        authorization
    }

    #[cfg(test)]
    fn for_local_tls_headers(url: &str, headers: &[(String, String)]) -> Self {
        let parsed = Url::parse(url).unwrap();
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        assert!(parsed.port().is_some());
        let mut authorization = Self::with_headers(url, None, headers).unwrap().unwrap();
        authorization.local_test_certificate = true;
        authorization
    }

    fn redact(&self, text: &str) -> String {
        let mut secrets = Vec::new();
        for (name, value) in &self.headers {
            let raw = value.to_str().unwrap();
            secrets.push(raw);
            if name == header::COOKIE {
                for pair in raw.split(';') {
                    if let Some((_, value)) = pair.trim().split_once('=') {
                        secrets.push(value.trim());
                    }
                }
            }
            if name == header::AUTHORIZATION {
                if let Some((scheme, token)) = raw.split_once(char::is_whitespace) {
                    if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic")
                    {
                        secrets.push(token.trim());
                    }
                }
            }
        }
        secrets.retain(|value| !value.is_empty());
        secrets.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        secrets.into_iter().fold(text.to_owned(), |text, secret| {
            text.replace(secret, "[授权信息]")
        })
    }
}

pub(super) struct AuthorizedStorage {
    inner: Arc<dyn Storage>,
    auth: FileAuthorization,
}

impl AuthorizedStorage {
    pub fn new(inner: Arc<dyn Storage>, auth: FileAuthorization) -> Self {
        Self { inner, auth }
    }

    fn check(&self, status: &DownloadStatus) -> gosh_dl::Result<()> {
        let valid = status
            .metadata
            .url
            .as_deref()
            .is_some_and(|url| self.auth.validate_url(url).is_ok());
        if !valid || !status.metadata.mirrors.is_empty() {
            return Err(EngineError::InvalidInput {
                field: "authorization",
                message: "授权下载记录与 HTTPS 来源不一致".into(),
            });
        }
        Ok(())
    }

    fn restore(&self, mut status: DownloadStatus) -> gosh_dl::Result<DownloadStatus> {
        self.check(&status)?;
        // This adapter is opened only for an explicit authorized attempt after
        // the previous engine has shut down. gosh-dl resume() accepts Paused,
        // not Error, so a rejected/expired Cookie must become retryable here.
        // Preserve progress, validators and every saved segment unchanged.
        if matches!(status.state, DownloadState::Error { .. }) {
            status.state = DownloadState::Paused;
        }
        status.metadata.headers = self.auth.engine_headers();
        status.metadata.cookies.clear();
        status.metadata.user_agent = self.auth.user_agent();
        status.metadata.referer = None;
        Ok(status)
    }

    /// Only this credential-free copy may reach durable storage. The engine's
    /// current in-memory DownloadOptions are deliberately left untouched.
    fn runtime_for_persistence(&self, raw: &str) -> gosh_dl::Result<String> {
        let mut value = Self::runtime_document(raw)?;
        if let Some(options) = value.get_mut("options").and_then(Value::as_object_mut) {
            options.insert("headers".into(), json!([]));
            options.insert("cookies".into(), Value::Null);
            options.insert("user_agent".into(), Value::Null);
            options.insert("referer".into(), Value::Null);
        }
        Self::encode_runtime(value)
    }

    /// Restore credentials only in the value returned to the active engine.
    /// This value must never be passed to the underlying persistent storage.
    fn runtime_for_transfer(&self, raw: &str) -> gosh_dl::Result<String> {
        let mut value = Self::runtime_document(raw)?;
        if let Some(options) = value.get_mut("options").and_then(Value::as_object_mut) {
            options.insert("headers".into(), json!(self.auth.engine_headers()));
            options.insert("cookies".into(), Value::Null);
            options.insert("user_agent".into(), json!(self.auth.user_agent()));
            // Referer is restored only in the current generic header set. Clear
            // any legacy dedicated slot so replaced credentials cannot linger.
            options.insert("referer".into(), Value::Null);
        }
        Self::encode_runtime(value)
    }

    fn runtime_document(raw: &str) -> gosh_dl::Result<Value> {
        serde_json::from_str(raw).map_err(|_| EngineError::Internal("授权下载恢复信息无效".into()))
    }

    fn encode_runtime(value: Value) -> gosh_dl::Result<String> {
        serde_json::to_string(&value)
            .map_err(|_| EngineError::Internal("授权下载恢复信息无法保存".into()))
    }
}

#[async_trait::async_trait]
impl Storage for AuthorizedStorage {
    async fn save_download(&self, status: &DownloadStatus) -> gosh_dl::Result<()> {
        self.check(status)?;
        let mut safe = status.clone();
        safe.metadata.headers.clear();
        safe.metadata.cookies.clear();
        safe.metadata.user_agent = None;
        safe.metadata.referer = None;
        if let DownloadState::Error { kind, message, .. } = &mut safe.state {
            *kind = self.auth.redact(kind);
            *message = self.auth.redact(message);
        }
        self.inner.save_download(&safe).await
    }

    async fn load_download(&self, id: DownloadId) -> gosh_dl::Result<Option<DownloadStatus>> {
        self.inner
            .load_download(id)
            .await?
            .map(|status| self.restore(status))
            .transpose()
    }

    async fn load_all(&self) -> gosh_dl::Result<Vec<DownloadStatus>> {
        self.inner
            .load_all()
            .await?
            .into_iter()
            .map(|status| self.restore(status))
            .collect()
    }

    async fn delete_download(&self, id: DownloadId) -> gosh_dl::Result<()> {
        self.inner.delete_download(id).await
    }

    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> gosh_dl::Result<()> {
        let mut safe = segments.to_vec();
        for segment in &mut safe {
            if let SegmentState::Failed { error, .. } = &mut segment.state {
                *error = self.auth.redact(error);
            }
        }
        self.inner.save_segments(id, &safe).await
    }

    async fn load_segments(&self, id: DownloadId) -> gosh_dl::Result<Vec<Segment>> {
        self.inner.load_segments(id).await
    }

    async fn delete_segments(&self, id: DownloadId) -> gosh_dl::Result<()> {
        self.inner.delete_segments(id).await
    }

    async fn save_torrent_data(&self, id: DownloadId, data: &[u8]) -> gosh_dl::Result<()> {
        self.inner.save_torrent_data(id, data).await
    }

    async fn load_torrent_data(&self, id: DownloadId) -> gosh_dl::Result<Option<Vec<u8>>> {
        self.inner.load_torrent_data(id).await
    }

    async fn save_runtime_metadata(&self, id: DownloadId, raw: &str) -> gosh_dl::Result<()> {
        self.inner
            .save_runtime_metadata(id, &self.runtime_for_persistence(raw)?)
            .await
    }

    async fn load_runtime_metadata(&self, id: DownloadId) -> gosh_dl::Result<Option<String>> {
        self.inner
            .load_runtime_metadata(id)
            .await?
            .map(|raw| self.runtime_for_transfer(&raw))
            .transpose()
    }

    async fn load_all_runtime_metadata(&self) -> gosh_dl::Result<HashMap<DownloadId, String>> {
        self.inner
            .load_all_runtime_metadata()
            .await?
            .into_iter()
            .map(|(id, raw)| Ok((id, self.runtime_for_transfer(&raw)?)))
            .collect()
    }

    async fn health_check(&self) -> gosh_dl::Result<()> {
        self.inner.health_check().await
    }

    async fn compact(&self) -> gosh_dl::Result<()> {
        self.inner.compact().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gosh_dl::{DownloadEngine, DownloadOptions, EngineConfig, SqliteStorage};
    use std::sync::Mutex;

    const URL: &str = "https://files.example.test/private.zip";
    const COOKIE: &str = "session=ffdm-test-cookie-secret-one";

    #[test]
    fn credentials_require_exact_https_origin_and_safe_header_values() {
        let auth = FileAuthorization::new(URL, Some(COOKIE)).unwrap().unwrap();
        assert!(auth
            .validate_url("https://files.example.test:443/other")
            .is_ok());
        for url in [
            "http://files.example.test/private.zip",
            "https://files.example.test:444/private.zip",
            "https://cdn.files.example.test/private.zip",
            "https://files.example.test.evil.test/private.zip",
            "https://user:password@files.example.test/private.zip",
        ] {
            assert!(auth.validate_url(url).is_err());
        }
        for cookie in [
            "session=abc\r\nInjected: yes",
            "session=abc\n",
            "session=abc\t",
            "session=中文",
        ] {
            let error = FileAuthorization::new(URL, Some(cookie)).err().unwrap();
            assert!(!error.to_string().contains("abc"));
        }
        assert!(FileAuthorization::new(URL, Some(&"x".repeat(20 * 1024))).is_err());
        assert!(FileAuthorization::new("http://files.example.test/file", Some(COOKIE)).is_err());
        assert!(FileAuthorization::new(URL, None).unwrap().is_none());
        assert!(FileAuthorization::new(URL, Some("  ")).unwrap().is_none());
        let request = auth.request(Client::new().head(URL)).build().unwrap();
        assert!(request.headers()[header::COOKIE].is_sensitive());
        assert!(!format!("{request:?}").contains("ffdm-test-cookie-secret-one"));
        let error = auth.safe_error(&anyhow::anyhow!("HTTP error: 401 {COOKIE}"));
        assert!(error.to_string().contains("401"));
        assert!(!error.to_string().contains("ffdm-test-cookie-secret-one"));
    }

    fn custom_headers(suffix: &str) -> Vec<(String, String)> {
        vec![
            (
                "Authorization".into(),
                format!("Bearer authorization-token-{suffix}"),
            ),
            ("X-API-Key".into(), format!("custom-api-key-{suffix}")),
            (
                "Referer".into(),
                format!("https://member.example.test/private-referrer-{suffix}"),
            ),
            ("User-Agent".into(), format!("PrivateDesktopAgent-{suffix}")),
        ]
    }

    #[test]
    fn custom_headers_validate_limits_duplicates_controls_and_engine_owned_fields() {
        let fields = custom_headers("validation-secret");
        let auth = FileAuthorization::with_headers(URL, None, &fields)
            .unwrap()
            .unwrap();
        let request = auth.request(Client::new().head(URL)).build().unwrap();
        for (name, value) in &fields {
            assert_eq!(request.headers().get(name).unwrap(), value.as_str());
            assert!(request.headers().get(name).unwrap().is_sensitive());
            assert!(!format!("{request:?}").contains(value));
        }
        assert!(
            FileAuthorization::with_headers("http://files.example.test/file", None, &fields)
                .is_err()
        );
        for name in [
            "Host",
            "Range",
            "If-Range",
            "If-Match",
            "If-None-Match",
            "If-Modified-Since",
            "If-Unmodified-Since",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Keep-Alive",
            "TE",
            "Trailer",
            "Upgrade",
            "Proxy-Authorization",
            "Proxy-Connection",
            "Proxy-Anything",
            "Accept-Encoding",
            ":authority",
            "bad name",
            "",
        ] {
            let error = FileAuthorization::with_headers(
                URL,
                None,
                &[(name.into(), "private-value".into())],
            )
            .err()
            .unwrap();
            assert!(!format!("{error:#}").contains("private-value"));
        }
        for value in [
            "abc\r\nInjected: value",
            "abc\n",
            "abc\t",
            "abc\0",
            "abc\u{7f}",
            "中文",
        ] {
            let error =
                FileAuthorization::with_headers(URL, None, &[("X-Test".into(), value.into())])
                    .err()
                    .unwrap();
            assert!(!format!("{error:#}").contains("abc"));
        }
        assert!(FileAuthorization::with_headers(
            URL,
            None,
            &[
                ("X-Test".into(), "one".into()),
                ("x-test".into(), "two".into())
            ]
        )
        .is_err());
        assert!(FileAuthorization::with_headers(
            URL,
            Some(COOKIE),
            &[("cookie".into(), "session=another".into())]
        )
        .is_err());
        assert!(FileAuthorization::with_headers(
            URL,
            Some(COOKIE),
            &[("Cookie".into(), "  ".into())]
        )
        .is_ok());
        assert!(
            FileAuthorization::with_headers(URL, None, &[("Cookie".into(), "  ".into())])
                .unwrap()
                .is_none()
        );
        assert!(
            FileAuthorization::with_headers(URL, None, &[("X-Test".into(), " ".into())])
                .unwrap()
                .is_some()
        );
        let fields: Vec<_> = (0..48).map(|i| (format!("X-{i}"), "v".into())).collect();
        assert!(FileAuthorization::with_headers(URL, None, &fields).is_ok());
        assert!(FileAuthorization::with_headers(URL, Some(COOKIE), &fields).is_err());
        assert!(FileAuthorization::with_headers(
            URL,
            None,
            &[("X".into(), "a".repeat(16 * 1024 - 1))]
        )
        .is_ok());
        assert!(
            FileAuthorization::with_headers(URL, None, &[("X".into(), "a".repeat(16 * 1024))])
                .is_err()
        );
        let trimmed = FileAuthorization::with_headers(
            URL,
            None,
            &[("X-Test".into(), "  keep internal space  ".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            trimmed.headers(),
            vec![("x-test".into(), "keep internal space".into())]
        );
    }

    #[test]
    fn all_custom_header_values_and_authorization_tokens_are_redacted() {
        for authorization in [
            "Bearer synthetic-bearer-token",
            "Basic dXNlcjpzeW50aGV0aWMtcGFzc3dvcmQ=",
        ] {
            let fields = vec![
                ("Authorization".into(), authorization.into()),
                (
                    "Cookie".into(),
                    "session=synthetic-cookie; another=second-cookie".into(),
                ),
                ("X-API-Key".into(), "synthetic-api-secret".into()),
                ("User-Agent".into(), "private-user-agent".into()),
            ];
            let auth = FileAuthorization::with_headers(URL, None, &fields)
                .unwrap()
                .unwrap();
            let token = authorization.split_once(' ').unwrap().1;
            let raw = format!("{authorization} {token} synthetic-cookie second-cookie synthetic-api-secret private-user-agent");
            for message in [
                auth.redact_error(&raw),
                auth.safe_error(&anyhow::anyhow!(raw)).to_string(),
            ] {
                for secret in [
                    token,
                    "synthetic-cookie",
                    "second-cookie",
                    "synthetic-api-secret",
                    "private-user-agent",
                ] {
                    assert!(!message.contains(secret));
                }
            }
        }
    }

    #[tokio::test]
    async fn sqlite_checkpoints_never_contain_headers_and_restore_uses_new_authorization() {
        let directory = tempfile::tempdir().unwrap();
        let sqlite = Arc::new(
            SqliteStorage::new(directory.path().join("engine.sqlite"))
                .await
                .unwrap(),
        );
        let fields = custom_headers("stored-header-one");
        let auth = FileAuthorization::with_headers(URL, Some(COOKIE), &fields)
            .unwrap()
            .unwrap();
        let storage = Arc::new(AuthorizedStorage::new(sqlite.clone(), auth.clone()));
        let engine = DownloadEngine::with_storage(EngineConfig::default(), storage.clone())
            .await
            .unwrap();
        let id = engine
            .add_http(
                URL,
                DownloadOptions {
                    start_paused: true,
                    save_dir: Some(directory.path().to_owned()),
                    filename: Some("private.zip.ffdm.part".into()),
                    headers: auth.engine_headers(),
                    user_agent: auth.user_agent(),
                    referer: Some(fields[2].1.clone()),
                    cookies: Some(vec![COOKIE.into()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut status = engine.status(id).unwrap();
        engine.shutdown().await.unwrap();
        drop(engine);
        status.progress.completed_size = 1024;
        status.state = DownloadState::Error {
            kind: format!("Network {{ message: {COOKIE:?} {fields:?} }}"),
            message: format!("HTTP error: 401 {COOKIE} {fields:?}"),
            retryable: false,
        };
        storage.save_download(&status).await.unwrap();
        let mut range = Segment::new(0, 0, 4095);
        range.downloaded = 1024;
        storage.save_segments(id, &[range]).await.unwrap();
        let stored = sqlite.load_download(id).await.unwrap().unwrap();
        assert!(stored.metadata.headers.is_empty());
        assert!(stored.metadata.cookies.is_empty());
        assert!(stored.metadata.user_agent.is_none());
        assert!(stored.metadata.referer.is_none());
        let DownloadState::Error { kind, message, .. } = &stored.state else {
            panic!("stored failure must remain a failure until an explicit retry");
        };
        assert!(!kind.contains("ffdm-test-cookie-secret-one"));
        assert!(!message.contains("ffdm-test-cookie-secret-one"));
        for (_, secret) in &fields {
            assert!(!kind.contains(secret));
            assert!(!message.contains(secret));
        }
        let runtime = sqlite.load_runtime_metadata(id).await.unwrap().unwrap();
        assert!(!runtime.contains("ffdm-test-cookie-secret-one"));
        assert_eq!(
            serde_json::from_str::<Value>(&runtime).unwrap()["options"]["headers"],
            json!([])
        );
        assert!(
            serde_json::from_str::<Value>(&runtime).unwrap()["options"]["user_agent"].is_null()
        );
        assert!(serde_json::from_str::<Value>(&runtime).unwrap()["options"]["referer"].is_null());
        assert_state_has_no_secrets(
            directory.path(),
            &fields
                .iter()
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
        );
        // Check SQLite itself and its live WAL/SHM, not just the API readback.
        for entry in std::fs::read_dir(directory.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                assert!(!bytes
                    .windows(COOKIE.len())
                    .any(|window| window == COOKIE.as_bytes()));
                let secret = b"ffdm-test-cookie-secret-one";
                assert!(!bytes.windows(secret.len()).any(|window| window == secret));
            }
        }
        let replacement = "session=ffdm-test-cookie-secret-two";
        let next_fields = custom_headers("stored-header-two");
        let updated = FileAuthorization::with_headers(URL, Some(replacement), &next_fields)
            .unwrap()
            .unwrap();
        let reopened = Arc::new(AuthorizedStorage::new(sqlite.clone(), updated));
        let restored = DownloadEngine::with_storage(EngineConfig::default(), reopened.clone())
            .await
            .unwrap();
        let restored_status = restored.status(id).unwrap();
        assert_eq!(restored_status.state, DownloadState::Paused);
        assert_eq!(restored_status.progress.completed_size, 1024);
        assert_eq!(
            restored_status
                .metadata
                .headers
                .iter()
                .find(|(name, _)| name == "cookie"),
            Some(&("cookie".into(), replacement.into()))
        );
        assert_eq!(
            restored_status.metadata.user_agent,
            Some(next_fields[3].1.clone())
        );
        assert!(restored_status.metadata.referer.is_none());
        assert!(restored_status
            .metadata
            .headers
            .iter()
            .all(|(name, _)| name != "user-agent"));
        assert!(restored_status.metadata.cookies.is_empty());
        assert_eq!(
            reopened.load_segments(id).await.unwrap()[0].downloaded,
            1024
        );
        let restored_runtime = reopened.load_runtime_metadata(id).await.unwrap().unwrap();
        assert!(restored_runtime.contains("ffdm-test-cookie-secret-two"));
        assert!(!restored_runtime.contains("ffdm-test-cookie-secret-one"));
        assert!(restored_runtime.contains(&next_fields[3].1));
        for (_, secret) in &fields {
            assert!(!restored_runtime.contains(secret));
        }
        restored.shutdown().await.unwrap();
        let stored_runtime = sqlite.load_runtime_metadata(id).await.unwrap().unwrap();
        assert!(!stored_runtime.contains("ffdm-test-cookie-secret"));
        assert_state_has_no_secrets(
            directory.path(),
            &next_fields
                .iter()
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
        );

        // Removing a UA on retry must restore the engine default, rather than
        // retaining a preceding credential set's private User-Agent slot.
        let without_ua = AuthorizedStorage::new(
            sqlite,
            FileAuthorization::new(URL, Some(replacement))
                .unwrap()
                .unwrap(),
        );
        let restored = without_ua.load_download(id).await.unwrap().unwrap();
        assert!(restored.metadata.user_agent.is_none());
        let runtime: Value =
            serde_json::from_str(&without_ua.load_runtime_metadata(id).await.unwrap().unwrap())
                .unwrap();
        assert!(runtime["options"]["user_agent"].is_null());
    }

    #[tokio::test]
    async fn restart_without_credentials_cannot_resume_an_authorized_job() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("private.zip");
        let paths = crate::download::paths(&output).unwrap();
        std::fs::create_dir_all(&paths.state).unwrap();
        crate::download::save_job(
            &paths.job,
            &crate::download::Job {
                schema: 1,
                id: DownloadId::new(),
                url: URL.into(),
                output: paths.output,
                connections: 4,
                expected_sha256: None,
                auth_required: true,
            },
            false,
        )
        .unwrap();
        let error = crate::download::download(crate::download::DownloadRequest {
            url: Some(URL.into()),
            output,
            connections: 4,
            resume: true,
            expected_sha256: None,
            pause_after: None,
            timeout: Duration::from_secs(1),
            interactive: false,
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("重新提供"));
        assert!(!paths.state.join("engine.sqlite").exists());
    }

    type ReceivedRequests = Arc<Mutex<Vec<(String, String, Option<String>)>>>;
    type RequiredHeaders = Arc<Mutex<Option<Vec<(String, String)>>>>;
    type ReceivedHeaders = Arc<Mutex<Vec<(String, header::HeaderMap)>>>;

    struct HttpsFixture {
        url: String,
        cookie: Arc<Mutex<String>>,
        required_headers: RequiredHeaders,
        received_headers: ReceivedHeaders,
        requests: ReceivedRequests,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for HttpsFixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl HttpsFixture {
        async fn start(data: bytes::Bytes, redirect: Option<String>) -> Self {
            Self::start_with_chunk_delay(data, redirect, Duration::from_millis(40)).await
        }

        async fn start_with_chunk_delay(
            data: bytes::Bytes,
            redirect: Option<String>,
            chunk_delay: Duration,
        ) -> Self {
            use axum::body::Body;
            use hudsucker::{hyper, hyper_util::rt::TokioIo, rustls};
            use rustls::pki_types::PrivatePkcs8KeyDer;
            let key = rcgen::KeyPair::generate().unwrap();
            let certificate = rcgen::CertificateParams::new(vec!["127.0.0.1".into()])
                .unwrap()
                .self_signed(&key)
                .unwrap();
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("https://{}/file", listener.local_addr().unwrap());
            let cookie = Arc::new(Mutex::new(COOKIE.to_owned()));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let required_headers: RequiredHeaders = Arc::new(Mutex::new(None));
            let received_headers = Arc::new(Mutex::new(Vec::new()));
            let expected_cookie = cookie.clone();
            let received = requests.clone();
            let required = required_headers.clone();
            let received_fields = received_headers.clone();
            let server = tokio::spawn(async move {
                let mut clients = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            let (acceptor, data, redirect, expected_cookie, received, required, received_fields) =
                                (acceptor.clone(), data.clone(), redirect.clone(), expected_cookie.clone(), received.clone(), required.clone(), received_fields.clone());
                            clients.spawn(async move {
                                let Ok(stream) = acceptor.accept(stream).await else { return };
                                let service = hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                                    let (data, redirect, expected_cookie, received, required, received_fields) =
                                        (data.clone(), redirect.clone(), expected_cookie.clone(), received.clone(), required.clone(), received_fields.clone());
                                    async move {
                                        let cookie = request.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
                                        let range = request.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned);
                                        received.lock().unwrap().push((request.method().to_string(), cookie.clone(), range.clone()));
                                        received_fields.lock().unwrap().push((request.method().to_string(), request.headers().clone()));
                                        let authorized = required.lock().unwrap().as_ref().map_or_else(
                                            || cookie == *expected_cookie.lock().unwrap(),
                                            |fields| fields.iter().all(|(name, value)| {
                                                request.headers().get_all(name).iter().count() == 1
                                                    && request.headers().get(name).is_some_and(|actual| actual == value.as_str())
                                            })
                                        );
                                        let response = if !authorized {
                                            hyper::Response::builder().status(401).body(Body::empty()).unwrap()
                                        } else if let Some(redirect) = redirect {
                                            hyper::Response::builder().status(302).header(header::LOCATION, redirect).body(Body::empty()).unwrap()
                                        } else {
                                            let total = data.len();
                                            let bounds = range.as_deref().and_then(|r| r.strip_prefix("bytes="))
                                                .and_then(|r| r.split_once('-')).and_then(|(start, end)| {
                                                    Some((start.parse::<usize>().ok()?, if end.is_empty() { total - 1 } else { end.parse::<usize>().ok()? }))
                                                });
                                            let (start, end) = bounds.unwrap_or((0, total - 1));
                                            let mut response = hyper::Response::builder()
                                                .status(if bounds.is_some() { 206 } else { 200 })
                                                .header(header::CONTENT_TYPE, "application/zip")
                                                .header(header::CONTENT_LENGTH, end - start + 1)
                                                .header(header::ACCEPT_RANGES, "bytes")
                                                .header(header::ETAG, "\"unchanged-test-file\"")
                                                .header(header::CONTENT_DISPOSITION, "attachment; filename=private.zip");
                                            if bounds.is_some() {
                                                response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
                                            }
                                            let body = if request.method() == hyper::Method::HEAD { Body::empty() } else {
                                                let data = data.slice(start..=end);
                                                Body::from_stream(futures_util::stream::unfold((data, 0_usize), move |(data, at)| async move {
                                                    if at == data.len() { return None }
                                                    tokio::time::sleep(chunk_delay).await;
                                                    let next = (at + 32 * 1024).min(data.len());
                                                    Some((Ok::<_, std::convert::Infallible>(data.slice(at..next)), (data, next)))
                                                }))
                                            };
                                            response.body(body).unwrap()
                                        };
                                        Ok::<_, std::convert::Infallible>(response)
                                    }
                                });
                                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                            });
                        }
                        _ = clients.join_next(), if !clients.is_empty() => {}
                    }
                }
            });
            Self {
                url,
                cookie,
                required_headers,
                received_headers,
                requests,
                server,
            }
        }
    }

    fn file_request(url: &str, output: &std::path::Path) -> crate::download::DownloadRequest {
        crate::download::DownloadRequest {
            url: Some(url.into()),
            output: output.into(),
            connections: 4,
            resume: false,
            expected_sha256: None,
            pause_after: None,
            timeout: Duration::from_secs(15),
            interactive: false,
        }
    }

    fn assert_state_has_no_secrets(directory: &std::path::Path, secrets: &[&str]) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                assert_state_has_no_secrets(&entry.path(), secrets);
            } else {
                let bytes = std::fs::read(entry.path()).unwrap();
                for secret in secrets {
                    assert!(
                        !bytes
                            .windows(secret.len())
                            .any(|window| window == secret.as_bytes()),
                        "credential reached a disk file"
                    );
                }
            }
        }
    }

    async fn assert_fresh_authorized_download(
        connections: usize,
        cookie: Option<&str>,
        fields: &[(String, String)],
    ) {
        let data = crate::test_server::fixture_data(8 * 1024 * 1024);
        let fixture =
            HttpsFixture::start_with_chunk_delay(data.clone(), None, Duration::from_millis(10))
                .await;
        let expected: Vec<_> = fields
            .iter()
            .cloned()
            .chain(cookie.map(|value| ("Cookie".into(), value.into())))
            .collect();
        *fixture.required_headers.lock().unwrap() = Some(expected.clone());
        let mut auth = FileAuthorization::with_headers(&fixture.url, cookie, fields)
            .unwrap()
            .unwrap();
        auth.local_test_certificate = true;
        let secrets: Vec<_> = expected.iter().map(|(_, value)| value.as_str()).collect();
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("fresh-private.zip");
        let mut request = file_request(&fixture.url, &output);
        request.connections = connections;
        // No earlier anonymous attempt, failure, pause or resume may supply the
        // headers. This exercises the first active transfer's actual requests.
        assert!(!request.resume);
        let (_pause, pause_rx) = tokio::sync::watch::channel(false);
        let (progress, mut progress_rx) =
            tokio::sync::watch::channel(crate::download::DownloadSnapshot::default());
        let transfer = tokio::spawn(crate::download::download_with_authorization(
            request,
            Some(crate::download::DownloadControl {
                pause: pause_rx,
                progress,
            }),
            auth,
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while progress_rx.borrow().completed_bytes == 0 {
                progress_rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        // Include the live SQLite WAL/SHM whenever present, before shutdown
        // checkpoints and closes the connection.
        assert_state_has_no_secrets(directory.path(), &secrets);
        let report = transfer.await.unwrap().unwrap();
        assert_eq!(report.outcome, "completed");
        assert_eq!(report.starting_progress_bytes, 0);
        assert_eq!(report.connections, connections);
        assert_eq!(std::fs::read(&output).unwrap(), data);

        {
            let received = fixture.received_headers.lock().unwrap();
            assert!(received.iter().any(|(method, _)| method == "HEAD"));
            assert_eq!(
                received
                    .iter()
                    .filter(|(method, _)| method == "GET")
                    .count(),
                connections,
                "the initial transfer must use the requested number of GETs"
            );
            for (method, headers) in received.iter() {
                assert!(method == "HEAD" || method == "GET");
                for (name, expected) in &expected {
                    assert_eq!(
                        headers.get_all(name).iter().count(),
                        1,
                        "first-transfer authorization field was missing or duplicated"
                    );
                    assert_eq!(headers.get(name).unwrap(), expected.as_str());
                }
                assert_eq!(headers.get_all(header::USER_AGENT).iter().count(), 1);
                assert_eq!(headers.get_all(header::ACCEPT_ENCODING).iter().count(), 1);
                assert_eq!(headers.get(header::ACCEPT_ENCODING).unwrap(), "identity");
                if cookie.is_none() {
                    assert!(!headers.contains_key(header::COOKIE));
                }
                if connections > 1 && method == "GET" {
                    assert!(headers.contains_key(header::RANGE));
                }
            }
        }

        let paths = crate::download::paths(&output).unwrap();
        let job: crate::download::Job =
            serde_json::from_slice(&std::fs::read(&paths.job).unwrap()).unwrap();
        assert!(job.auth_required);
        let sqlite = SqliteStorage::new(paths.state.join("engine.sqlite"))
            .await
            .unwrap();
        let status = sqlite.load_download(job.id).await.unwrap().unwrap();
        assert!(status.metadata.headers.is_empty());
        assert!(status.metadata.cookies.is_empty());
        assert!(status.metadata.user_agent.is_none());
        assert!(status.metadata.referer.is_none());
        let runtime: Value =
            serde_json::from_str(&sqlite.load_runtime_metadata(job.id).await.unwrap().unwrap())
                .unwrap();
        assert_eq!(runtime["options"]["headers"], json!([]));
        assert!(runtime["options"]["cookies"].is_null());
        assert!(runtime["options"]["user_agent"].is_null());
        assert!(runtime["options"]["referer"].is_null());
        assert_state_has_no_secrets(directory.path(), &secrets);
    }

    #[tokio::test]
    async fn https_fresh_cookie_download_sends_credentials_once_with_one_and_four_connections() {
        for connections in [1, 4] {
            assert_fresh_authorized_download(connections, Some(COOKIE), &[]).await;
        }
    }

    #[tokio::test]
    async fn https_fresh_custom_headers_download_sends_credentials_once_with_one_and_four_connections(
    ) {
        for connections in [1, 4] {
            let fields = custom_headers(&format!("fresh-headers-{connections}"));
            assert_fresh_authorized_download(connections, None, &fields).await;
        }
    }

    #[tokio::test]
    async fn https_custom_headers_reauthenticate_failed_and_paused_ranges_without_persisting_headers(
    ) {
        let data = crate::test_server::fixture_data(8 * 1024 * 1024);
        let fixture = HttpsFixture::start(data.clone(), None).await;
        let fields = custom_headers("tls-first");
        *fixture.required_headers.lock().unwrap() = Some(fields.clone());
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("custom-auth.zip");

        // A failure before any data arrives remains the same retryable job.
        let mut invalid = fields.clone();
        invalid[0].1 = "Bearer invalid-header-token".into();
        let error = crate::download::download_with_authorization(
            file_request(&fixture.url, &output),
            None,
            FileAuthorization::for_local_tls_headers(&fixture.url, &invalid),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("401"));
        assert!(!error.to_string().contains("invalid-header-token"));
        let paths = crate::download::paths(&output).unwrap();
        let original: crate::download::Job =
            serde_json::from_slice(&std::fs::read(&paths.job).unwrap()).unwrap();
        assert!(original.auth_required);

        let auth = FileAuthorization::for_local_tls_headers(&fixture.url, &fields);
        let filename = crate::filename::FilenameResolver::new()
            .unwrap()
            .resolve_authorized(&fixture.url, &auth)
            .await
            .unwrap();
        assert_eq!(filename.filename, "private.zip");
        let mut request = file_request(&fixture.url, &output);
        request.resume = true;
        let (pause, pause_rx) = tokio::sync::watch::channel(false);
        let (progress, mut progress_rx) =
            tokio::sync::watch::channel(crate::download::DownloadSnapshot::default());
        let transfer = tokio::spawn(crate::download::download_with_authorization(
            request,
            Some(crate::download::DownloadControl {
                pause: pause_rx,
                progress,
            }),
            auth,
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while progress_rx.borrow().completed_bytes == 0 {
                progress_rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        pause.send_replace(true);
        let paused = transfer.await.unwrap().unwrap();
        assert_eq!(paused.outcome, "paused");
        assert!(paused.bytes > 0 && paused.bytes < data.len() as u64);
        let retained: crate::download::Job =
            serde_json::from_slice(&std::fs::read(&paths.job).unwrap()).unwrap();
        assert_eq!(retained.id, original.id);
        let sqlite = SqliteStorage::new(paths.state.join("engine.sqlite"))
            .await
            .unwrap();
        let checkpoint = sqlite.load_download(original.id).await.unwrap().unwrap();
        let segments =
            serde_json::to_value(sqlite.load_segments(original.id).await.unwrap()).unwrap();
        assert!(checkpoint.progress.completed_size > 0);
        assert!(checkpoint.metadata.headers.is_empty());
        assert!(checkpoint.metadata.user_agent.is_none());
        drop(sqlite);

        // Reopening without credentials must not send any source request or
        // rewrite the partial range checkpoint as an anonymous transfer.
        let requests_before = fixture.requests.lock().unwrap().len();
        let mut request = file_request(&fixture.url, &output);
        request.resume = true;
        let error = crate::download::download(request).await.unwrap_err();
        assert!(error.to_string().contains("重新提供"));
        assert_eq!(fixture.requests.lock().unwrap().len(), requests_before);

        let next_fields = custom_headers("tls-replaced");
        *fixture.required_headers.lock().unwrap() = Some(next_fields.clone());
        let mut request = file_request(&fixture.url, &output);
        request.resume = true;
        let error = crate::download::download_with_authorization(
            request,
            None,
            FileAuthorization::for_local_tls_headers(&fixture.url, &fields),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("401"));
        let sqlite = SqliteStorage::new(paths.state.join("engine.sqlite"))
            .await
            .unwrap();
        assert_eq!(
            sqlite
                .load_download(original.id)
                .await
                .unwrap()
                .unwrap()
                .progress
                .completed_size,
            checkpoint.progress.completed_size
        );
        assert_eq!(
            serde_json::to_value(sqlite.load_segments(original.id).await.unwrap()).unwrap(),
            segments
        );
        drop(sqlite);
        fixture.requests.lock().unwrap().clear();
        fixture.received_headers.lock().unwrap().clear();
        let mut request = file_request(&fixture.url, &output);
        request.resume = true;
        let resumed = crate::download::download_with_authorization(
            request,
            None,
            FileAuthorization::for_local_tls_headers(&fixture.url, &next_fields),
        )
        .await
        .unwrap();
        assert_eq!(resumed.outcome, "completed");
        assert_eq!(
            resumed.starting_progress_bytes,
            checkpoint.progress.completed_size
        );
        assert_eq!(std::fs::read(&output).unwrap(), data);
        let received = fixture.received_headers.lock().unwrap();
        assert!(received.iter().any(|(method, _)| method == "HEAD"));
        assert!(received.iter().any(|(method, _)| method == "GET"));
        for (_, headers) in received.iter() {
            for (name, expected) in &next_fields {
                assert_eq!(
                    headers.get_all(name).iter().count(),
                    1,
                    "authorization field was duplicated"
                );
                assert_eq!(headers.get(name).unwrap(), expected.as_str());
            }
            assert_eq!(headers.get(header::ACCEPT_ENCODING).unwrap(), "identity");
            assert_eq!(
                headers.get_all(header::ACCEPT_ENCODING).iter().count(),
                1,
                "identity encoding must not be duplicated when resuming"
            );
            assert!(!headers.contains_key(header::COOKIE));
        }
        drop(received);
        assert!(fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, range)| range
                .as_deref()
                .is_some_and(|range| !range.starts_with("bytes=0-"))));
        let secrets: Vec<&str> = fields
            .iter()
            .chain(&next_fields)
            .map(|(_, value)| value.as_str())
            .chain(["invalid-header-token"])
            .collect();
        assert_state_has_no_secrets(directory.path(), &secrets);
    }

    #[tokio::test]
    async fn https_redirect_never_replays_any_custom_header_to_same_or_other_origin() {
        let target = HttpsFixture::start(bytes::Bytes::from_static(b"target"), None).await;
        let source = HttpsFixture::start(
            bytes::Bytes::from_static(b"source"),
            Some(target.url.clone()),
        )
        .await;
        let fields = custom_headers("tls-redirect");
        *source.required_headers.lock().unwrap() = Some(fields.clone());
        let directory = tempfile::tempdir().unwrap();
        let auth = FileAuthorization::for_local_tls_headers(&source.url, &fields);
        let error = crate::filename::FilenameResolver::new()
            .unwrap()
            .resolve_authorized(&source.url, &auth)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("重定向"));
        let error = crate::download::download_with_authorization(
            file_request(&source.url, &directory.path().join("redirect.zip")),
            None,
            auth,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("重定向"));
        assert!(target.requests.lock().unwrap().is_empty());

        // Policy::none also rejects an ordinary relative same-origin redirect.
        let same = HttpsFixture::start(
            bytes::Bytes::from_static(b"same"),
            Some("/different-file".into()),
        )
        .await;
        *same.required_headers.lock().unwrap() = Some(fields.clone());
        let error = crate::filename::FilenameResolver::new()
            .unwrap()
            .resolve_authorized(
                &same.url,
                &FileAuthorization::for_local_tls_headers(&same.url, &fields),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("重定向"));
        assert_eq!(same.requests.lock().unwrap().len(), 1);
        assert_state_has_no_secrets(
            directory.path(),
            &fields
                .iter()
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn https_authorized_download_pauses_and_resumes_with_replaced_cookie_without_persisting_it(
    ) {
        let data = crate::test_server::fixture_data(8 * 1024 * 1024);
        let fixture = HttpsFixture::start(data.clone(), None).await;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("private.zip");
        let auth = FileAuthorization::for_local_tls_test(&fixture.url, COOKIE);
        let filename = crate::filename::FilenameResolver::new()
            .unwrap()
            .resolve_authorized(&fixture.url, &auth)
            .await
            .unwrap();
        assert_eq!(filename.filename, "private.zip");
        let request = file_request(&fixture.url, &output);
        let (pause, pause_rx) = tokio::sync::watch::channel(false);
        let (progress, mut progress_rx) =
            tokio::sync::watch::channel(crate::download::DownloadSnapshot::default());
        let transfer = tokio::spawn(crate::download::download_with_authorization(
            request,
            Some(crate::download::DownloadControl {
                pause: pause_rx,
                progress,
            }),
            auth,
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while progress_rx.borrow().completed_bytes == 0 {
                progress_rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        pause.send_replace(true);
        let paused = transfer.await.unwrap().unwrap();
        assert_eq!(paused.outcome, "paused");
        assert!(paused.bytes > 0 && paused.bytes < data.len() as u64);
        assert!(!output.exists());
        assert_state_has_no_secrets(directory.path(), &["ffdm-test-cookie-secret-one"]);

        // shutdown checkpoints before joining in-flight workers, so the report's
        // final in-memory count may exceed the durable checkpoint. Compare the
        // checkpoint itself across an expired-credential attempt.
        let paths = crate::download::paths(&output).unwrap();
        let job: crate::download::Job =
            serde_json::from_slice(&std::fs::read(&paths.job).unwrap()).unwrap();
        let sqlite = SqliteStorage::new(paths.state.join("engine.sqlite"))
            .await
            .unwrap();
        let checkpoint = sqlite.load_download(job.id).await.unwrap().unwrap();
        let segments = sqlite.load_segments(job.id).await.unwrap();
        assert!(checkpoint.progress.completed_size > 0);
        assert!(!segments.is_empty());
        let serialized_segments = serde_json::to_value(&segments).unwrap();
        drop(sqlite);

        let replacement = "session=ffdm-test-cookie-secret-two";
        *fixture.cookie.lock().unwrap() = replacement.into();
        // An expired Cookie may fail after the task already has real ranges.
        // Supplying fresh authorization must preserve those same checkpoints.
        let mut expired_request = file_request(&fixture.url, &output);
        expired_request.resume = true;
        let expired = crate::download::download_with_authorization(
            expired_request,
            None,
            FileAuthorization::for_local_tls_test(&fixture.url, COOKIE),
        )
        .await
        .unwrap_err();
        assert!(expired.to_string().contains("401"));
        let sqlite = SqliteStorage::new(paths.state.join("engine.sqlite"))
            .await
            .unwrap();
        assert_eq!(
            sqlite
                .load_download(job.id)
                .await
                .unwrap()
                .unwrap()
                .progress
                .completed_size,
            checkpoint.progress.completed_size
        );
        assert_eq!(
            serde_json::to_value(sqlite.load_segments(job.id).await.unwrap()).unwrap(),
            serialized_segments
        );
        drop(sqlite);
        fixture.requests.lock().unwrap().clear();
        let mut request = file_request(&fixture.url, &output);
        request.resume = true;
        let resumed = crate::download::download_with_authorization(
            request,
            None,
            FileAuthorization::for_local_tls_test(&fixture.url, replacement),
        )
        .await
        .unwrap();
        assert_eq!(resumed.outcome, "completed");
        assert_eq!(
            resumed.starting_progress_bytes,
            checkpoint.progress.completed_size
        );
        assert_eq!(std::fs::read(&output).unwrap(), data);
        let requests = fixture.requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|(_, cookie, _)| cookie == replacement));
        assert!(requests
            .iter()
            .any(|(_, _, range)| range.as_deref().is_some_and(|r| !r.starts_with("bytes=0-"))));
        drop(requests);
        assert_state_has_no_secrets(
            directory.path(),
            &["ffdm-test-cookie-secret-one", "ffdm-test-cookie-secret-two"],
        );
    }

    #[tokio::test]
    async fn https_invalid_cookie_and_cross_origin_redirect_never_publish_or_leak_credentials() {
        let target = HttpsFixture::start(bytes::Bytes::from_static(b"target"), None).await;
        let source = HttpsFixture::start(
            bytes::Bytes::from_static(b"source"),
            Some(target.url.clone()),
        )
        .await;
        let directory = tempfile::tempdir().unwrap();
        let bad_output = directory.path().join("invalid.zip");
        let error = crate::download::download_with_authorization(
            file_request(&source.url, &bad_output),
            None,
            FileAuthorization::for_local_tls_test(&source.url, "session=invalid-secret-value"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("401"));
        assert!(!error.to_string().contains("invalid-secret-value"));
        assert!(!bad_output.exists());
        let redirect_output = directory.path().join("redirect.zip");
        let auth = FileAuthorization::for_local_tls_test(&source.url, COOKIE);
        let error = crate::filename::FilenameResolver::new()
            .unwrap()
            .resolve_authorized(&source.url, &auth)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("重定向"));
        let error = crate::download::download_with_authorization(
            file_request(&source.url, &redirect_output),
            None,
            auth,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("重定向"));
        assert!(!redirect_output.exists());
        assert!(target.requests.lock().unwrap().is_empty());
        assert_state_has_no_secrets(
            directory.path(),
            &["ffdm-test-cookie-secret-one", "invalid-secret-value"],
        );
    }

    #[tokio::test]
    async fn https_failed_cookie_job_retries_the_same_output_with_correct_authorization() {
        let data = crate::test_server::fixture_data(64 * 1024);
        let fixture = HttpsFixture::start(data.clone(), None).await;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("retry.zip");
        let error = crate::download::download_with_authorization(
            file_request(&fixture.url, &output),
            None,
            FileAuthorization::for_local_tls_test(&fixture.url, "session=invalid-secret-value"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("401"));
        let paths = crate::download::paths(&output).unwrap();
        let original: crate::download::Job =
            serde_json::from_slice(&std::fs::read(&paths.job).unwrap()).unwrap();
        assert!(original.auth_required);
        let mut retry = file_request(&fixture.url, &output);
        retry.resume = true;
        let report = crate::download::download_with_authorization(
            retry,
            None,
            FileAuthorization::for_local_tls_test(&fixture.url, COOKIE),
        )
        .await
        .unwrap();
        assert_eq!(report.outcome, "completed");
        assert_eq!(std::fs::read(&output).unwrap(), data);
        let retained: crate::download::Job =
            serde_json::from_slice(&std::fs::read(&paths.job).unwrap()).unwrap();
        assert_eq!(retained.id, original.id);
        assert_state_has_no_secrets(
            directory.path(),
            &["ffdm-test-cookie-secret-one", "invalid-secret-value"],
        );
    }

    #[tokio::test]
    async fn https_anonymous_401_checkpoint_upgrades_to_authorized_retry_without_recreating_job() {
        let data = crate::test_server::fixture_data(64 * 1024);
        let fixture = HttpsFixture::start(data.clone(), None).await;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("anonymous.zip");
        let paths = crate::download::paths(&output).unwrap();
        std::fs::create_dir_all(&paths.state).unwrap();
        // Create a legacy anonymous failure with the actual unwrapped engine.
        // Only this localhost test accepts the fixture's self-signed certificate.
        let config = EngineConfig {
            download_dir: directory.path().to_owned(),
            database_path: Some(paths.state.join("engine.sqlite")),
            http: gosh_dl::HttpConfig {
                accept_invalid_certs: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = DownloadEngine::new(config).await.unwrap();
        let mut events = engine.subscribe();
        let id = engine
            .add_http(
                &fixture.url,
                DownloadOptions {
                    start_paused: true,
                    save_dir: Some(directory.path().to_owned()),
                    filename: Some(paths.part.file_name().unwrap().to_str().unwrap().into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        crate::download::save_job(
            &paths.job,
            &crate::download::Job {
                schema: 1,
                id,
                url: fixture.url.clone(),
                output: paths.output.clone(),
                connections: 4,
                expected_sha256: None,
                auth_required: false,
            },
            false,
        )
        .unwrap();
        engine.resume(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let gosh_dl::DownloadEvent::Failed {
                    id: failed, error, ..
                } = events.recv().await.unwrap()
                {
                    assert_eq!(failed, id);
                    assert!(error.contains("401"));
                    break;
                }
            }
        })
        .await
        .unwrap();
        engine.shutdown().await.unwrap();
        assert!(matches!(
            engine.status(id).unwrap().state,
            DownloadState::Error { .. }
        ));
        drop(events);
        drop(engine);
        assert!(fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, cookie, _)| cookie.is_empty()));
        let mut retry = file_request(&fixture.url, &output);
        retry.resume = true;
        let report = crate::download::download_with_authorization(
            retry,
            None,
            FileAuthorization::for_local_tls_test(&fixture.url, COOKIE),
        )
        .await
        .unwrap();
        assert_eq!(report.outcome, "completed");
        assert_eq!(std::fs::read(&output).unwrap(), data);
        let upgraded: crate::download::Job =
            serde_json::from_slice(&std::fs::read(&paths.job).unwrap()).unwrap();
        assert_eq!(upgraded.id, id);
        assert!(upgraded.auth_required);
        assert_state_has_no_secrets(directory.path(), &["ffdm-test-cookie-secret-one"]);
    }
}
