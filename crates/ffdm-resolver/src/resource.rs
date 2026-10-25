//! Inspect an explicit resource request without downloading its complete body.
use crate::{generic::GenericCandidate, validation::parse_url};
use anyhow::{ensure, Context, Result};
use percent_encoding::percent_decode_str;
use reqwest::{header, Url};
use std::time::Duration;

pub(crate) enum ResourceKind {
    Page(Url),
    Direct {
        title: String,
        candidate: GenericCandidate,
        manifest: bool,
    },
}

pub(crate) const USER_AGENT: &str = concat!("FFDM-Resolver/", env!("CARGO_PKG_VERSION"));

pub(crate) async fn inspect(input: &Url) -> Result<ResourceKind> {
    inspect_with_headers(input, &[]).await
}

pub(crate) async fn inspect_with_headers(
    input: &Url,
    headers: &[(String, String)],
) -> Result<ResourceKind> {
    crate::hls::validate_headers(headers)?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(USER_AGENT);
    if input.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }) {
        builder = builder.no_proxy();
    }
    let client = builder.build()?;
    let request = || {
        let mut request = client.get(input.clone());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request
    };
    let mut response = request()
        .header(header::RANGE, "bytes=0-511")
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("资源响应检查失败")?;
    // Some origins reject Range despite supporting normal GET.
    if matches!(response.status().as_u16(), 400 | 403 | 416) {
        response = request()
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .context("资源响应检查失败")?;
    }
    ensure!(
        response.status().is_success(),
        "资源服务器拒绝访问（HTTP {}）；请确认链接及访问权限",
        response.status().as_u16()
    );
    let final_url = parse_url(response.url().as_str())?;
    let mime = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let disposition = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = response.content_length();
    let size = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse().ok())
    } else {
        bytes
    };
    let chunk = response.chunk().await?.unwrap_or_default();
    let prefix = String::from_utf8_lossy(&chunk[..chunk.len().min(512)]);
    let head = prefix
        .trim_start_matches('\u{feff}')
        .trim_start()
        .to_ascii_lowercase();
    // Downloadable HTML attachments still belong to a file downloader. An
    // ordinary HTML response is a page requiring actual media extraction.
    let attachment = disposition.to_ascii_lowercase().starts_with("attachment");
    if !attachment
        && (mime == "text/html"
            || mime == "application/xhtml+xml"
            || head.starts_with("<!doctype html")
            || head.starts_with("<html"))
    {
        return Ok(ResourceKind::Page(final_url));
    }
    let manifest = mime.contains("mpegurl")
        || head.starts_with("#extm3u")
        || final_url.path().to_ascii_lowercase().ends_with(".m3u8");
    ensure!(
        !mime.contains("dash+xml") && !final_url.path().to_ascii_lowercase().ends_with(".mpd"),
        "暂不支持通用 DASH 清单，请使用平台视频页面或完整媒体文件地址"
    );
    let name = disposition_name(&disposition)
        .or_else(|| {
            final_url
                .path_segments()?
                .next_back()
                .filter(|s| !s.is_empty())
                .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
        })
        .unwrap_or_else(|| "download".into());
    let suffix = safe_extension(&name);
    let extension = if manifest {
        "m3u8".into()
    } else {
        if suffix.as_deref() == Some("tar.gz") && mime == "application/gzip" {
            "tar.gz".into()
        } else if mime.starts_with("video/") || mime.starts_with("audio/") {
            mime_extension(&mime)
                .map(str::to_owned)
                .or(suffix.clone())
                .unwrap_or_else(|| "bin".into())
        } else {
            suffix
                .clone()
                .or_else(|| mime_extension(&mime).map(str::to_owned))
                .unwrap_or_else(|| "bin".into())
        }
    };
    let title = suffix
        .as_ref()
        .map(|suffix| &name[..name.len() - suffix.len() - 1])
        .unwrap_or(&name)
        .to_owned();
    let id = format!("{:x}", sha2::Sha256::digest(final_url.as_str().as_bytes()))[..16].to_owned();
    let candidate = GenericCandidate {
        transform: None,
        id,
        label: if manifest {
            "HLS".into()
        } else {
            extension.to_ascii_uppercase()
        },
        height: 0,
        url: final_url.to_string(),
        extension: extension.clone(),
        audio_only: mime.starts_with("audio/")
            || (!mime.starts_with("video/")
                && matches!(
                    extension.as_str(),
                    "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "ape"
                )),
        headers: vec![("User-Agent".into(), USER_AGENT.into())],
        size: if manifest { None } else { size },
    };
    Ok(ResourceKind::Direct {
        title,
        candidate,
        manifest,
    })
}

use sha2::Digest;

pub(crate) fn safe_extension(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    for suffix in ["tar.gz", "tar.bz2", "tar.xz", "tar.zst"] {
        if name.ends_with(&format!(".{suffix}")) {
            return Some(suffix.into());
        }
    }
    let (stem, suffix) = name.rsplit_once('.')?;
    (!stem.is_empty()
        && !suffix.is_empty()
        && suffix.len() <= 16
        && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        && !matches!(
            suffix,
            "php" | "aspx" | "asp" | "jsp" | "cgi" | "do" | "action"
        ))
    .then(|| suffix.into())
}

pub(crate) fn mime_extension(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "video/x-matroska" => "mkv",
        "audio/mp4" => "m4a",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/aac" => "aac",
        "audio/opus" => "opus",
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "application/gzip" => "gz",
        "application/x-7z-compressed" => "7z",
        _ => return None,
    })
}

fn disposition_name(raw: &str) -> Option<String> {
    let mut ordinary = None;
    for part in raw.split(';').skip(1) {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if key.eq_ignore_ascii_case("filename*") {
            if let Some(value) = value
                .strip_prefix("UTF-8''")
                .or_else(|| value.strip_prefix("utf-8''"))
            {
                return percent_decode_str(value)
                    .decode_utf8()
                    .ok()
                    .map(|v| v.into_owned());
            }
        } else if key.eq_ignore_ascii_case("filename") {
            ordinary = Some(value.to_owned());
        }
    }
    ordinary
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_files_keep_safe_compound_suffixes_and_disposition_names() {
        assert_eq!(safe_extension("backup.tar.gz").as_deref(), Some("tar.gz"));
        assert_eq!(safe_extension("report.XLSX").as_deref(), Some("xlsx"));
        assert_eq!(safe_extension("download.php"), None);
        assert_eq!(
            disposition_name("attachment; filename*=UTF-8''%E6%B5%8B%E8%AF%95.zip"),
            Some("测试.zip".into())
        );
        assert_eq!(mime_extension("application/pdf"), Some("pdf"));
    }

    #[tokio::test]
    async fn response_metadata_preserves_archives_case_and_audio_and_retries_range_rejection() {
        use axum::{
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
            Router,
        };
        let app = Router::new()
            .route(
                "/backup.tar.gz",
                get(|| async { ([("content-type", "application/gzip")], vec![0x1f, 0x8b, 0]) }),
            )
            .route(
                "/report.XLSX",
                get(|| async {
                    (
                        [("content-type", "application/octet-stream")],
                        vec![1, 2, 3],
                    )
                }),
            )
            .route(
                "/song.mp3",
                get(|| async {
                    (
                        [("content-type", "application/octet-stream")],
                        vec![1, 2, 3],
                    )
                }),
            )
            .route(
                "/range-refused",
                get(|headers: HeaderMap| async move {
                    if headers.contains_key("range") {
                        StatusCode::FORBIDDEN.into_response()
                    } else {
                        (
                            [
                                ("content-type", "application/zip"),
                                ("content-disposition", "attachment; filename=report.zip"),
                            ],
                            vec![0x50, 0x4b, 5, 6],
                        )
                            .into_response()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for (path, title, extension, audio) in [
            ("backup.tar.gz", "backup", "tar.gz", false),
            ("report.XLSX", "report", "xlsx", false),
            ("song.mp3", "song", "mp3", true),
            ("range-refused", "report", "zip", false),
        ] {
            let ResourceKind::Direct {
                title: actual,
                candidate,
                ..
            } = inspect(&Url::parse(&format!("{base}/{path}")).unwrap())
                .await
                .unwrap()
            else {
                panic!("expected a file")
            };
            assert_eq!(actual, title);
            assert_eq!(candidate.extension, extension);
            assert_eq!(candidate.audio_only, audio);
        }
        server.abort();
    }
}
