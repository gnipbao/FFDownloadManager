//! A user-supplied YouTube session, scoped to one extraction and one HTTPS origin.
use anyhow::{ensure, Result};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use ytdown::HttpRequest;

pub(super) const ORIGIN: &str = "https://www.youtube.com";
pub(super) const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
pub(super) const TV_UA: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/25.lts.30.1034943-gold (unlike Gecko), Unknown_TV_Unknown_0/Unknown (Unknown, Unknown)";

// No Debug or Serialize: neither a task nor a parser cache should own a session.
pub(super) struct YoutubeSession {
    cookie: String,
    values: HashMap<String, String>,
}

impl YoutubeSession {
    pub fn new(raw: &str) -> Result<Self> {
        ensure!(
            raw.len() <= 16 * 1024 - 64,
            "YouTube Cookie 不能超过 16 KiB"
        );
        ensure!(
            raw.is_ascii() && !raw.bytes().any(|b| b.is_ascii_control()),
            "请粘贴 YouTube 请求标头中 Cookie 的单行值，不要粘贴整段请求头或 Cookie 文件"
        );
        let mut values = HashMap::new();
        for pair in raw
            .trim()
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let (name, value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("YouTube Cookie 格式无效"))?;
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
                "YouTube Cookie 格式无效"
            );
            values.insert(name.to_owned(), value.to_owned());
        }
        ensure!(!values.is_empty(), "请填写 YouTube 网页 Cookie");
        Ok(Self {
            cookie: raw.trim().to_owned(),
            values,
        })
    }

    fn authorization(&self, timestamp: u64) -> Option<String> {
        // YouTube's HTTPS session proof. SHA-1 is required by this protocol;
        // this is not a file-integrity or password-hashing algorithm choice.
        let third_party = self.values.get("__Secure-3PAPISID");
        let schemes = [
            ("SAPISIDHASH", self.values.get("SAPISID").or(third_party)),
            ("SAPISID1PHASH", self.values.get("__Secure-1PAPISID")),
            ("SAPISID3PHASH", third_party),
        ];
        let parts: Vec<_> = schemes
            .into_iter()
            .filter_map(|(scheme, sid)| {
                let sid = sid.filter(|s| !s.is_empty())?;
                let hash = Sha1::digest(format!("{timestamp} {sid} {ORIGIN}"));
                Some(format!("{scheme} {timestamp}_{hash:x}"))
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    pub fn apply(&self, request: &mut HttpRequest) {
        // The raw header has no browser Domain/Path metadata. Do not attach it
        // to scripts, shortened URLs, media CDN requests or other subdomains.
        let allowed = reqwest::Url::parse(&request.url).is_ok_and(|url| {
            url.origin().ascii_serialization() == ORIGIN
                && url.username().is_empty()
                && url.password().is_none()
                && matches!(
                    url.path(),
                    "/youtubei/v1/player" | "/youtubei/v1/visitor_id"
                )
        });
        if !allowed {
            return;
        }
        request.headers.retain(|(name, _)| {
            !matches!(
                name.to_ascii_lowercase().as_str(),
                "cookie" | "authorization" | "x-origin" | "x-goog-authuser"
            )
        });
        request.headers.push(("Cookie".into(), self.cookie.clone()));
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Some(value) = self.authorization(timestamp) {
            request.headers.push(("Authorization".into(), value));
            request.headers.push(("X-Origin".into(), ORIGIN.into()));
            request.headers.push(("X-Goog-AuthUser".into(), "0".into()));
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut result = text.replace(&self.cookie, "[授权信息]");
        for value in self.values.values().filter(|s| !s.is_empty()) {
            result = result.replace(value, "[授权信息]");
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_and_session_proof_are_scoped_to_exact_https_api_origin() {
        let session =
            YoutubeSession::new("SAPISID=sample-session-secret; LOGIN_INFO=sample-login-secret")
                .unwrap();
        for url in [
            "http://www.youtube.com/youtubei/v1/player",
            "https://www.youtube.com:444/youtubei/v1/player",
            "https://www.youtube.com.evil.test/youtubei/v1/player",
            "https://www.youtube.com@evil.test/youtubei/v1/player",
            "https://rr1.googlevideo.com/videoplayback",
            "https://www.youtube.com/s/player/code/base.js",
        ] {
            let mut request = HttpRequest::get("test", url);
            session.apply(&mut request);
            assert!(request.headers.is_empty(), "{url}");
        }
        let mut request = HttpRequest::post("test", format!("{ORIGIN}/youtubei/v1/player"));
        session.apply(&mut request);
        assert!(request
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v.starts_with("SAPISIDHASH ")));
        assert!(request
            .headers
            .iter()
            .any(|(k, v)| k == "Cookie" && v.contains("sample-session-secret")));
        // Fixed independent SHA-1 vector for timestamp + session + HTTPS origin.
        assert_eq!(
            session.authorization(123).unwrap(),
            "SAPISIDHASH 123_75f6b8fa22ad832ab3ada02c1848c09e54c8ed73"
        );
    }

    #[test]
    fn rejects_header_injection_and_redacts_echoed_credentials() {
        for value in [
            "",
            "Cookie: SID=value",
            "SID=x\r\nX-Test: y",
            "SID=中文",
            "not-a-cookie",
        ] {
            assert!(YoutubeSession::new(value).is_err());
        }
        assert!(YoutubeSession::new(&format!("SID={}", "x".repeat(16384))).is_err());
        let session =
            YoutubeSession::new("SAPISID=test-session-secret; LOGIN_INFO=test-login-secret")
                .unwrap();
        let error = session.redact("denied test-session-secret / test-login-secret");
        assert!(!error.contains("test-session-secret"));
        assert!(!error.contains("test-login-secret"));
    }
}
