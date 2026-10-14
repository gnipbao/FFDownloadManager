//! Shared limits for sources that document lower request/bandwidth budgets.
//!
//! These permits coordinate callers within this application process. Each
//! Wikimedia transfer must also apply [`limits`] to its connection count and
//! bandwidth; two transfers at 1.4 MB/s have a steady combined cap of 22.4 Mbps.
//! This leaves room for the HTTP engine's initial one-second token-bucket burst
//! while staying below Wikimedia's 25 Mbps limit over a ten-second interval.
use anyhow::{anyhow, bail, ensure, Context, Result};
use reqwest::{header, redirect::Policy, Client, Url};
use std::{
    sync::Mutex,
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{Semaphore, SemaphorePermit},
    time::Instant,
};

/// Conservative limits for one transfer from a restricted source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLimits {
    pub max_connections: usize,
    pub bytes_per_second: u64,
}

const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Identifying user-agent for direct Wikimedia downloads with no custom agent.
pub const USER_AGENT: &str =
    "FFDownload/0.3.7 (https://github.com/gnipbao/FFDownloadManager; public media resolver)";
static WIKIMEDIA: Gate = Gate::new();

/// Return a limit only for the exact Wikimedia media hostname.
pub fn limits(raw: &str) -> Option<SourceLimits> {
    let url = Url::parse(raw).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str() == Some("upload.wikimedia.org"))
        .then_some(SourceLimits {
            max_connections: 1,
            bytes_per_second: 1_400_000,
        })
}

/// Wait for the source's cooldown and acquire a shared transfer permit.
/// Keep it alive for the entire response/download, including its engine retries.
pub async fn acquire(raw: &str) -> Result<Option<SemaphorePermit<'static>>> {
    if limits(raw).is_none() {
        return Ok(None);
    }
    Ok(Some(WIKIMEDIA.acquire().await?))
}

/// Record the server's `Retry-After` when this source returns HTTP 429.
/// Supported values are decimal seconds and HTTP dates. Missing or malformed
/// headers receive a conservative 60-second cooldown; malformed values also
/// return an error so the caller can retain diagnostic information.
pub fn record_retry_after(raw: &str, value: Option<&str>) -> Result<()> {
    if limits(raw).is_none() {
        return Ok(());
    }
    record(&WIKIMEDIA, value, SystemTime::now())
}

/// Check for a source cooldown before handing a transfer to an HTTP engine
/// that cannot preserve `Retry-After`. The caller must already hold its source
/// permit. This reads response headers only and is not a media format probe.
pub async fn preflight(raw: &str, headers: &[(String, String)]) -> Result<()> {
    if limits(raw).is_none() {
        return Ok(());
    }
    let url = crate::validation::parse_url(raw)?;
    let headers = preflight_headers(headers)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(USER_AGENT)
        .redirect(Policy::custom(|attempt| {
            let target = attempt.url();
            let origin_changed = attempt
                .previous()
                .first()
                .is_some_and(|first| first.origin() != target.origin());
            if attempt.previous().len() >= 5
                || origin_changed
                || limits(target.as_str()).is_none()
                || !target.username().is_empty()
                || target.password().is_some()
            {
                attempt.error("source preflight redirect rejected")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .context("无法初始化 Wikimedia 请求检查")?;
    let mut request = client.head(url).header(header::ACCEPT_ENCODING, "identity");
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|_| anyhow!("Wikimedia 下载请求检查失败，请稍后重试"))?;
    check_preflight_status(
        &WIKIMEDIA,
        response.status(),
        response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
    )
}

fn preflight_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>> {
    // Ordinary file jobs may include headers outside the HLS contract. Apply
    // their shared transport validation and force one identity encoding, just
    // as the downloader does, rather than rejecting its added encoding header.
    crate::validation::validate_headers(headers)?;
    ensure!(
        headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
            .count()
            <= 1,
        "Wikimedia 下载检查只能使用一个 User-Agent"
    );
    Ok(headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("accept-encoding"))
        .cloned()
        .collect())
}

fn check_preflight_status(
    gate: &Gate,
    status: reqwest::StatusCode,
    retry_after: Option<&str>,
) -> Result<()> {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Even an invalid header records a conservative cooldown. Preserve
        // HTTP 429 as the actionable error, without exposing its media URL.
        let _ = record(gate, retry_after, SystemTime::now());
        bail!("Wikimedia 限制了请求（HTTP 429），请等待服务器冷却后继续下载");
    }
    if status.is_success() || status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        return Ok(());
    }
    bail!("Wikimedia 暂时无法读取该资源（HTTP {}）", status.as_u16())
}

struct Gate {
    transfers: Semaphore,
    retry_at: Mutex<Option<Instant>>,
}

impl Gate {
    const fn new() -> Self {
        Self {
            transfers: Semaphore::const_new(2),
            retry_at: Mutex::new(None),
        }
    }

    fn deadline(&self) -> Result<Option<Instant>> {
        Ok(*self
            .retry_at
            .lock()
            .map_err(|_| anyhow!("source cooldown state is unavailable"))?)
    }

    fn record_delay(&self, delay: Duration) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(delay)
            .context("Retry-After exceeds the supported timer range")?;
        let mut current = self
            .retry_at
            .lock()
            .map_err(|_| anyhow!("source cooldown state is unavailable"))?;
        // A later response can extend a cooldown, but must never shorten one
        // that a simultaneous request has already recorded.
        if current.is_none_or(|old| deadline > old) {
            *current = Some(deadline);
        }
        Ok(())
    }

    async fn acquire(&self) -> Result<SemaphorePermit<'_>> {
        loop {
            if let Some(deadline) = self
                .deadline()?
                .filter(|deadline| *deadline > Instant::now())
            {
                tokio::time::sleep_until(deadline).await;
                // Another 429 received during this sleep can extend the wait.
                continue;
            }
            let permit = self
                .transfers
                .acquire()
                .await
                .context("source transfer gate is closed")?;
            // This waiter may have queued before a running transfer got 429.
            // Recheck after admission, releasing the permit during cooldown.
            if self
                .deadline()?
                .is_some_and(|deadline| deadline > Instant::now())
            {
                drop(permit);
                continue;
            }
            return Ok(permit);
        }
    }
}

fn retry_delay(value: Option<&str>, now: SystemTime) -> Result<Duration> {
    let Some(value) = value else {
        return Ok(DEFAULT_RETRY_DELAY);
    };
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(Duration::from_secs(
            value
                .parse()
                .context("Retry-After seconds are out of range")?,
        ));
    }
    let deadline = httpdate::parse_http_date(value).context("invalid Retry-After HTTP date")?;
    Ok(deadline.duration_since(now).unwrap_or(Duration::ZERO))
}

fn record(gate: &Gate, value: Option<&str>, now: SystemTime) -> Result<()> {
    match retry_delay(value, now) {
        Ok(delay) => {
            if let Err(error) = gate.record_delay(delay) {
                gate.record_delay(DEFAULT_RETRY_DELAY)?;
                return Err(error);
            }
            Ok(())
        }
        Err(error) => {
            gate.record_delay(DEFAULT_RETRY_DELAY)?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_matching_does_not_restrict_other_sources_or_lookalikes() {
        let expected = Some(SourceLimits {
            max_connections: 1,
            bytes_per_second: 1_400_000,
        });
        assert_eq!(limits("https://upload.wikimedia.org/clip.webm"), expected);
        assert_eq!(limits("http://UPLOAD.wikimedia.org/clip.webm"), expected);
        for url in [
            "https://upload.wikimedia.org.evil.test/clip.webm",
            "https://evil.test/?url=https://upload.wikimedia.org/clip.webm",
            "https://commons.wikimedia.org/w/api.php",
            "https://example.test/clip.webm",
            "file:///upload.wikimedia.org/clip.webm",
            "invalid URL",
        ] {
            assert_eq!(limits(url), None);
        }
        // Irrelevant Retry-After headers must not delay other platforms.
        record_retry_after("https://example.test/clip.webm", Some("malformed")).unwrap();
    }

    #[test]
    fn retry_after_parses_seconds_dates_defaults_and_past_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            retry_delay(Some("120"), now).unwrap(),
            Duration::from_secs(120)
        );
        assert_eq!(retry_delay(Some(" 0 "), now).unwrap(), Duration::ZERO);
        assert_eq!(retry_delay(None, now).unwrap(), DEFAULT_RETRY_DELAY);
        let future = httpdate::fmt_http_date(now + Duration::from_secs(90));
        assert_eq!(
            retry_delay(Some(&future), now).unwrap(),
            Duration::from_secs(90)
        );
        let past = httpdate::fmt_http_date(now - Duration::from_secs(1));
        assert_eq!(retry_delay(Some(&past), now).unwrap(), Duration::ZERO);
        for value in ["", "-1", "+1", "1.5", "tomorrow", "18446744073709551616"] {
            assert!(retry_delay(Some(value), now).is_err());
        }
    }

    #[test]
    fn malformed_retry_after_still_records_a_safe_cooldown() {
        let gate = Gate::new();
        let before = Instant::now();
        assert!(record(&gate, Some("invalid"), SystemTime::now()).is_err());
        assert!(gate.deadline().unwrap().unwrap() >= before + DEFAULT_RETRY_DELAY);
        let first = gate.deadline().unwrap().unwrap();
        gate.record_delay(Duration::ZERO).unwrap();
        assert_eq!(gate.deadline().unwrap().unwrap(), first);
    }

    #[test]
    fn head_preflight_records_real_delay_and_accepts_servers_without_head() {
        let gate = Gate::new();
        let before = Instant::now();
        let error =
            check_preflight_status(&gate, reqwest::StatusCode::TOO_MANY_REQUESTS, Some("120"))
                .unwrap_err();
        assert!(error.to_string().contains("HTTP 429"));
        assert!(gate.deadline().unwrap().unwrap() >= before + Duration::from_secs(120));
        let separate_gate = Gate::new();
        check_preflight_status(&separate_gate, reqwest::StatusCode::OK, None).unwrap();
        check_preflight_status(
            &separate_gate,
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            None,
        )
        .unwrap();
        assert!(separate_gate.deadline().unwrap().is_none());
        assert!(
            check_preflight_status(&separate_gate, reqwest::StatusCode::FORBIDDEN, None).is_err()
        );
    }

    #[test]
    fn preflight_accepts_ordinary_headers_and_forces_one_identity_encoding() {
        let headers = vec![
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            (
                "Referer".to_owned(),
                "https://commons.wikimedia.org/".to_owned(),
            ),
            ("Accept-Encoding".to_owned(), "identity".to_owned()),
            ("Accept".to_owned(), "*/*".to_owned()),
            ("X-File-Option".to_owned(), "original".to_owned()),
        ];
        let prepared = preflight_headers(&headers).unwrap();
        assert_eq!(prepared.len(), 4);
        assert!(prepared
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("accept-encoding")));
        assert_eq!(prepared[0], headers[0]);
        assert_eq!(prepared[1], headers[1]);
        for (name, value) in [("Host", "evil.test"), ("Range", "bytes=0-1")] {
            assert!(preflight_headers(&[(name.to_owned(), value.to_owned())]).is_err());
        }
        let mut duplicated = headers;
        duplicated.push(("user-agent".to_owned(), "second".to_owned()));
        assert!(preflight_headers(&duplicated).is_err());
    }

    #[tokio::test]
    async fn gate_limits_concurrency_and_queued_waiter_rechecks_new_cooldown() {
        let gate = Gate::new();
        let first = gate.acquire().await.unwrap();
        let second = gate.acquire().await.unwrap();
        assert_eq!(gate.transfers.available_permits(), 0);
        let mut queued = Box::pin(gate.acquire());
        // Poll once to deterministically queue after checking the old
        // cooldown, rather than relying on a spawned task being scheduled.
        assert!(futures_util::poll!(&mut queued).is_pending());
        let delay = Duration::from_millis(30);
        let recorded = Instant::now();
        gate.record_delay(delay).unwrap();
        drop(first);
        assert!(futures_util::poll!(&mut queued).is_pending());
        let permit = tokio::time::timeout(Duration::from_secs(1), queued)
            .await
            .unwrap()
            .unwrap();
        assert!(Instant::now() >= recorded + delay);
        drop(permit);
        drop(second);
        assert_eq!(gate.transfers.available_permits(), 2);
    }

    #[tokio::test]
    async fn extended_cooldown_is_honored_without_consuming_a_permit() {
        let gate = Gate::new();
        gate.record_delay(Duration::from_millis(5)).unwrap();
        let mut waiting = Box::pin(gate.acquire());
        assert!(futures_util::poll!(&mut waiting).is_pending());
        assert_eq!(gate.transfers.available_permits(), 2);
        let recorded = Instant::now();
        let delay = Duration::from_millis(30);
        gate.record_delay(delay).unwrap();
        let _permit = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(Instant::now() >= recorded + delay);
    }

    #[tokio::test]
    async fn unrestricted_sources_do_not_use_the_global_gate() {
        assert!(acquire("https://example.test/file.zip")
            .await
            .unwrap()
            .is_none());
        preflight("https://example.test/file.zip", &[])
            .await
            .unwrap();
    }
}
