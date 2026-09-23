//! Filename hints from response headers and URLs; never reads the file payload.
use anyhow::{ensure, Context, Result};
use percent_encoding::percent_decode_str;
use reqwest::{header, Client, Url};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
pub struct FilenameInfo {
    pub filename: String,
    pub extension: Option<String>,
    pub source: &'static str,
}

pub struct FilenameResolver {
    client: Client,
    cache: Mutex<HashMap<String, (Instant, FilenameInfo)>>,
}

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

impl FilenameResolver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(4)).build()?,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub async fn resolve(&self, raw: &str) -> Result<FilenameInfo> {
        let url = parse_url(raw)?;
        if let Some((time, info)) = self.cache.lock().unwrap().get(url.as_str()) {
            if time.elapsed() < Duration::from_secs(120) {
                return Ok(info.clone());
            }
        }
        let mut info = infer(&url, &url, None, None);
        let head = self
            .client
            .head(url.clone())
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await;
        let mut useful_headers = false;
        if let Ok(response) = head {
            if response.status().is_success() {
                info = response_info(&url, &response);
                useful_headers = info.source != "url" || info.extension.is_some();
            }
        }
        if !useful_headers {
            // Some origins reject HEAD or expose download headers only on GET.
            // Dropping the response without reading bytes also bounds servers
            // that ignore Range and return 200 with a full-length body.
            if let Ok(response) = self
                .client
                .get(url.clone())
                .header(header::ACCEPT_ENCODING, "identity")
                .header(header::RANGE, "bytes=0-0")
                .send()
                .await
            {
                if response.status().is_success() {
                    info = response_info(&url, &response);
                }
            }
        }
        let mut cache = self.cache.lock().unwrap();
        cache.retain(|_, (time, _)| time.elapsed() < Duration::from_secs(120));
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(url.into(), (Instant::now(), info.clone()));
        Ok(info)
    }
}

fn response_info(original: &Url, response: &reqwest::Response) -> FilenameInfo {
    let text = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
    };
    infer(
        original,
        response.url(),
        text(header::CONTENT_DISPOSITION),
        text(header::CONTENT_TYPE),
    )
}

/// Preserve a custom extension; supply one when the user enters just a name.
pub fn complete_name(custom: Option<&str>, info: &FilenameInfo) -> String {
    let Some(name) = custom.map(str::trim).filter(|s| !s.is_empty()) else {
        return info.filename.clone();
    };
    match (&info.extension, extension(name)) {
        (Some(ext), None) => format!("{name}.{ext}"),
        _ => name.to_owned(),
    }
}

pub fn extension(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    for compound in [
        "tar.gz", "tar.bz2", "tar.xz", "tar.zst", "tar.lz", "tar.lzma",
    ] {
        if lower.ends_with(&format!(".{compound}")) {
            return Some(compound.into());
        }
    }
    let (stem, suffix) = name.rsplit_once('.')?;
    (!stem.is_empty()
        && !suffix.is_empty()
        && suffix.len() <= 12
        && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        && suffix.bytes().any(|b| b.is_ascii_alphabetic()))
    .then(|| suffix.to_ascii_lowercase())
}

/// Windows device names are reserved even when followed by an extension.
pub fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            upper.strip_prefix(prefix).is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                    || matches!(number, "¹" | "²" | "³")
            })
        })
}

fn from_url(url: &Url) -> Option<String> {
    // A filename query is common on signed download endpoints.
    let query = url.query_pairs().find_map(|(key, value)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "filename" | "file_name" | "download" | "file"
        )
        .then(|| clean_name(&value))
        .flatten()
        .filter(|name| extension(name).is_some())
    });
    query.or_else(|| {
        let last = url.path_segments()?.next_back()?;
        clean_name(&percent_decode_str(last).decode_utf8_lossy())
    })
}

fn infer(
    original: &Url,
    final_url: &Url,
    disposition: Option<&str>,
    content_type: Option<&str>,
) -> FilenameInfo {
    let server = disposition
        .and_then(disposition_filename)
        .and_then(|name| clean_name(&name));
    let source = if server.is_some() { "server" } else { "url" };
    let mut name = server
        .or_else(|| from_url(final_url))
        .or_else(|| from_url(original))
        .unwrap_or_else(|| "download".into());
    let inferred_extension = content_type
        .and_then(mime_extension)
        .map(str::to_owned)
        .or_else(|| from_url(final_url).and_then(|name| extension(&name)))
        .or_else(|| from_url(original).and_then(|name| extension(&name)));
    let old_ext = extension(&name);
    let replace_endpoint = old_ext.as_deref().is_some_and(|ext| {
        matches!(
            ext,
            "php" | "asp" | "aspx" | "jsp" | "cgi" | "do" | "action" | "bin"
        )
    });
    let mut source = source;
    if let Some(ext) = inferred_extension {
        if old_ext.is_none() || replace_endpoint {
            if replace_endpoint {
                name.truncate(name.rfind('.').unwrap());
            }
            name = format!("{name}.{ext}");
            if source != "server" && content_type.and_then(mime_extension).is_some() {
                source = "content-type";
            }
        }
    }
    let name = fit_name(name);
    FilenameInfo {
        extension: extension(&name),
        filename: name,
        source,
    }
}

fn clean_name(name: &str) -> Option<String> {
    // A server-provided name is advisory: strip paths and non-display controls.
    let leaf = name.rsplit(['/', '\\']).next()?;
    let clean: String = leaf
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .map(|c| if ":*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let clean = clean.trim_matches(['.', ' ']);
    (!clean.is_empty()).then(|| {
        let portable = if is_windows_reserved_name(clean) {
            format!("_{clean}")
        } else {
            clean.to_owned()
        };
        fit_name(portable)
    })
}

fn fit_name(name: String) -> String {
    if name.len() <= 180 {
        return name;
    }
    let suffix = extension(&name)
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    let mut end = 180 - suffix.len();
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &name[..end], suffix)
}

fn disposition_filename(header: &str) -> Option<String> {
    // Split parameters while respecting semicolons and escapes inside quotes.
    let mut parts = Vec::new();
    let mut part = String::new();
    let (mut quoted, mut escaped) = (false, false);
    for c in header.chars() {
        if escaped {
            part.push(c);
            escaped = false;
        } else if quoted && c == '\\' {
            part.push(c);
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
            part.push(c);
        } else if c == ';' && !quoted {
            parts.push(std::mem::take(&mut part));
        } else {
            part.push(c);
        }
    }
    parts.push(part);
    let (mut plain, mut extended) = (None, None);
    for part in parts.into_iter().skip(1) {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let value = raw.trim().trim_matches('"');
        if key.trim().eq_ignore_ascii_case("filename*") {
            let mut parts = value.splitn(3, '\'');
            let (charset, _language, encoded) = (parts.next(), parts.next(), parts.next());
            if let (Some(charset), Some(encoded)) = (charset, encoded) {
                if charset.eq_ignore_ascii_case("utf-8") {
                    extended = percent_decode_str(encoded)
                        .decode_utf8()
                        .ok()
                        .map(|v| v.into_owned());
                } else if charset.eq_ignore_ascii_case("iso-8859-1") {
                    extended = Some(percent_decode_str(encoded).map(char::from).collect());
                }
            }
        } else if key.trim().eq_ignore_ascii_case("filename") {
            plain = Some(value.replace("\\\"", "\"").replace("\\\\", "\\"));
        }
    }
    extended.filter(|s| !s.is_empty()).or(plain)
}

fn mime_extension(raw: &str) -> Option<&'static str> {
    Some(
        match raw.split(';').next()?.trim().to_ascii_lowercase().as_str() {
            "video/mp4" => "mp4",
            "video/webm" => "webm",
            "video/x-matroska" => "mkv",
            "video/quicktime" => "mov",
            "video/x-msvideo" => "avi",
            "video/mp2t" => "ts",
            "audio/mpeg" => "mp3",
            "audio/mp4" | "audio/x-m4a" => "m4a",
            "audio/aac" => "aac",
            "audio/ogg" => "ogg",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/wav" | "audio/x-wav" | "audio/vnd.wave" => "wav",
            "application/pdf" => "pdf",
            "application/zip" | "application/x-zip-compressed" => "zip",
            "application/gzip" | "application/x-gzip" => "gz",
            "application/x-tar" => "tar",
            "application/x-xz" => "xz",
            "application/x-bzip2" => "bz2",
            "application/zstd" => "zst",
            "application/x-7z-compressed" => "7z",
            "application/vnd.rar" | "application/x-rar-compressed" => "rar",
            "application/x-apple-diskimage" => "dmg",
            "application/x-iso9660-image" => "iso",
            "application/vnd.android.package-archive" => "apk",
            "application/epub+zip" => "epub",
            "application/msword" => "doc",
            "application/vnd.ms-excel" => "xls",
            "application/vnd.ms-powerpoint" => "ppt",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
            "application/json" => "json",
            "application/xml" | "text/xml" => "xml",
            "text/plain" => "txt",
            "text/csv" => "csv",
            "text/html" => "html",
            "image/jpeg" => "jpg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/svg+xml" => "svg",
            "image/avif" => "avif",
            "image/heic" => "heic",
            "font/woff" => "woff",
            "font/woff2" => "woff2",
            "font/ttf" => "ttf",
            "font/otf" => "otf",
            // Generic binary data does not identify the real file format.
            _ => return None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://example.test/tunnel?id=1").unwrap()
    }

    #[test]
    fn server_and_url_names_remain_usable_on_windows() {
        assert!(is_windows_reserved_name("con.mp4"));
        assert!(is_windows_reserved_name("Lpt9.txt"));
        assert!(is_windows_reserved_name("COM¹.log"));
        assert!(!is_windows_reserved_name("COM0.txt"));
        assert!(!is_windows_reserved_name("console.mp4"));
        assert_eq!(clean_name("CON.mp4").as_deref(), Some("_CON.mp4"));
        assert_eq!(
            clean_name("clip<1080>?.mp4").as_deref(),
            Some("clip_1080__.mp4")
        );
    }

    #[test]
    fn international_server_filename_wins_over_ascii_fallback() {
        let info = infer(
            &url(),
            &url(),
            Some("attachment; filename=\"video.bin\"; filename*=UTF-8''%E8%AF%BE%E7%A8%8B.mp4"),
            Some("application/octet-stream"),
        );
        assert_eq!(info.filename, "课程.mp4");
        assert_eq!(complete_name(Some("我的课程"), &info), "我的课程.mp4");
        assert_eq!(complete_name(Some("我的课程.MP4"), &info), "我的课程.MP4");
        assert_eq!(complete_name(Some("自己选的.mkv"), &info), "自己选的.mkv");
        assert_eq!(
            disposition_filename("attachment; FILENAME=\"lesson; notes.pdf\"").as_deref(),
            Some("lesson; notes.pdf")
        );
    }

    #[test]
    fn mime_fills_missing_suffix_and_preserves_compound_archives() {
        assert_eq!(
            infer(&url(), &url(), None, Some("video/mp4")).filename,
            "tunnel.mp4"
        );
        let endpoint = Url::parse("https://example.test/download.php").unwrap();
        assert_eq!(
            infer(
                &endpoint,
                &endpoint,
                None,
                Some("application/pdf; charset=utf-8")
            )
            .filename,
            "download.pdf"
        );
        let archive = Url::parse("https://example.test/rust%20sources.tar.xz").unwrap();
        let info = infer(&url(), &archive, None, Some("application/octet-stream"));
        assert_eq!(info.filename, "rust sources.tar.xz");
        assert_eq!(complete_name(Some("源码"), &info), "源码.tar.xz");
        assert!(
            infer(&url(), &url(), None, Some("application/octet-stream"))
                .extension
                .is_none()
        );
        assert_eq!(
            infer(&url(), &archive, Some("attachment; filename=source"), None).filename,
            "source.tar.xz"
        );
    }

    #[test]
    fn remote_names_cannot_escape_and_truncation_preserves_suffix() {
        let info = infer(
            &url(),
            &url(),
            Some("attachment; filename*=UTF-8''..%2F..%2Fnotes.pdf"),
            None,
        );
        assert_eq!(info.filename, "notes.pdf");
        let filename = format!("{}.tar.gz", "中".repeat(100));
        let header = format!("attachment; filename=\"{filename}\"");
        let info = infer(&url(), &url(), Some(&header), None);
        assert!(info.filename.len() <= 180);
        assert!(info.filename.ends_with(".tar.gz"));
    }
}
