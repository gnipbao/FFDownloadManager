//! Shared validation for parsers and downloader hosts.
use anyhow::{ensure, Context, Result};
use reqwest::Url;

pub fn parse_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw.trim()).context("请输入有效的 HTTP 或 HTTPS 链接")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host().is_some(),
        "仅支持 HTTP / HTTPS 下载链接"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "暂不支持链接内的用户名和密码"
    );
    Ok(url)
}

pub fn validate_headers(headers: &[(String, String)]) -> Result<()> {
    ensure!(
        headers.len() <= 48
            && headers
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
                <= 16 * 1024,
        "媒体请求头过大"
    );
    for (name, value) in headers {
        reqwest::header::HeaderName::from_bytes(name.as_bytes()).context("无效的媒体请求头名称")?;
        reqwest::header::HeaderValue::from_str(value).context("无效的媒体请求头内容")?;
        ensure!(
            !matches!(
                name.to_ascii_lowercase().as_str(),
                "host"
                    | "range"
                    | "if-range"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "proxy-authorization"
            ),
            "媒体请求头不能覆盖传输控制字段"
        );
    }
    Ok(())
}

/// Windows device names remain reserved when followed by an extension.
pub fn is_windows_reserved_name(name: &str) -> bool {
    let upper = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            upper.strip_prefix(prefix).is_some_and(|number| {
                matches!(
                    number,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        })
}
