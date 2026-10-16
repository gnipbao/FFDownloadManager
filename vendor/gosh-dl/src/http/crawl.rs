//! Recursive HTTP/HTTPS discovery scaffolding.
//!
//! The initial implementation keeps discovery separate from the existing
//! segmented file download path so recursive mirroring can be added without
//! changing the semantics of `add_http()`.

use super::connection::with_retry;
use super::{HttpDownloader, ACCEPT_ENCODING_IDENTITY};
use crate::error::{EngineError, Result};
use crate::types::{DownloadOptions, RecursiveEntry, RecursiveManifest, RecursiveOptions};
use futures::stream::{FuturesUnordered, StreamExt};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use url::Url;

const MAX_DISCOVERY_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_DISCOVERED_PAGES: usize = 1024;
const MAX_DISCOVERED_FILES: usize = 10_000;

#[derive(Debug)]
struct DiscoveryResponse {
    final_url: Url,
    html: Option<String>,
    size_hint: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct RedirectScope {
    root_url: Url,
    scope_prefix: String,
    same_host_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedRedirectScope {
    root_url: String,
    scope_prefix: String,
    same_host_only: bool,
}

fn normalize_url(candidate: &str, base: &Url) -> Result<Url> {
    let joined = base.join(candidate).map_err(|e| {
        EngineError::invalid_input(
            "root_url",
            format!("Failed to resolve URL '{candidate}': {e}"),
        )
    })?;

    match joined.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(EngineError::invalid_input(
                "root_url",
                format!("Unsupported scheme: {}", scheme),
            ));
        }
    }

    let mut normalized = joined;
    normalized.set_fragment(None);

    let trailing_slash = normalized.path().ends_with('/');
    let normalized_path = normalize_path(normalized.path(), trailing_slash)?;
    normalized.set_path(&normalized_path);

    Ok(normalized)
}

fn normalize_path(path: &str, trailing_slash: bool) -> Result<String> {
    let mut components = Vec::new();

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(EngineError::invalid_input(
                        "root_url",
                        "path escapes above the configured root",
                    ));
                }
            }
            segment => components.push(segment),
        }
    }

    let mut normalized = if components.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", components.join("/"))
    };

    if trailing_slash && normalized != "/" {
        normalized.push('/');
    }

    Ok(normalized)
}

fn normalized_scope_prefix(root: &Url, recursive: &RecursiveOptions) -> Result<String> {
    let prefix = if let Some(prefix) = recursive.allowed_prefix.as_deref() {
        if prefix.starts_with('/') {
            prefix.to_string()
        } else {
            format!("/{}", prefix)
        }
    } else if root.path().ends_with('/') {
        root.path().to_string()
    } else {
        match root.path().rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => format!("{}/", parent),
            _ => "/".to_string(),
        }
    };

    normalize_path(&prefix, true)
}

impl RedirectScope {
    pub(crate) fn new(root_url: &str, recursive: &RecursiveOptions) -> Result<Self> {
        let parsed_url = normalize_url(
            root_url,
            &Url::parse(root_url).map_err(|e| {
                EngineError::invalid_input("root_url", format!("Invalid URL: {}", e))
            })?,
        )?;

        Ok(Self {
            scope_prefix: normalized_scope_prefix(&parsed_url, recursive)?,
            root_url: parsed_url,
            same_host_only: recursive.same_host_only,
        })
    }

    fn contains(&self, url: &Url) -> bool {
        if self.same_host_only && url.host_str() != self.root_url.host_str() {
            return false;
        }

        path_within_prefix(url.path(), &self.scope_prefix)
    }

    pub(crate) fn to_persisted(&self) -> PersistedRedirectScope {
        PersistedRedirectScope {
            root_url: self.root_url.as_str().to_string(),
            scope_prefix: self.scope_prefix.clone(),
            same_host_only: self.same_host_only,
        }
    }

    pub(crate) fn from_persisted(persisted: PersistedRedirectScope) -> Result<Self> {
        let root_url = Url::parse(&persisted.root_url).map_err(|e| {
            EngineError::invalid_input(
                "runtime_metadata",
                format!("Invalid persisted recursive root URL: {}", e),
            )
        })?;

        let root_url = normalize_url(&persisted.root_url, &root_url)?;
        let scope_prefix = normalize_path(&persisted.scope_prefix, true)?;

        Ok(Self {
            root_url,
            scope_prefix,
            same_host_only: persisted.same_host_only,
        })
    }
}

pub(crate) fn validate_redirect_scope(url: &Url, scope: &RedirectScope) -> Result<()> {
    if scope.contains(url) {
        Ok(())
    } else {
        Err(EngineError::invalid_input(
            "url",
            format!("redirect escaped recursive scope: {}", url),
        ))
    }
}

fn is_url_in_scope(url: &Url, root: &Url, recursive: &RecursiveOptions, depth: usize) -> bool {
    if depth > recursive.max_depth {
        return false;
    }

    if recursive.same_host_only && url.host_str() != root.host_str() {
        return false;
    }

    let prefix = match normalized_scope_prefix(root, recursive) {
        Ok(prefix) => prefix,
        Err(_) => return false,
    };

    path_within_prefix(url.path(), &prefix)
}

fn path_within_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return path.starts_with('/');
    }

    let trimmed_prefix = prefix.trim_end_matches('/');
    path == trimmed_prefix || path.starts_with(prefix)
}

fn build_relative_path(url: &Url, root: &Url, recursive: &RecursiveOptions) -> Result<PathBuf> {
    let prefix = normalized_scope_prefix(root, recursive)?;
    let path = url.path();
    let stripped = if prefix == "/" {
        path.trim_start_matches('/')
    } else {
        let trimmed_prefix = prefix.trim_end_matches('/');
        if path == trimmed_prefix {
            ""
        } else {
            path.strip_prefix(&prefix).unwrap_or_default()
        }
    };

    let candidate = if stripped.is_empty() {
        path.rsplit('/').next().unwrap_or_default()
    } else {
        stripped
    };

    if candidate.is_empty() || candidate.ends_with('/') {
        return Err(EngineError::invalid_input(
            "root_url",
            format!("URL does not resolve to a file path: {}", url),
        ));
    }

    let relative = PathBuf::from(candidate);
    for component in relative.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(EngineError::invalid_input(
                    "root_url",
                    format!("Discovered invalid path for URL: {}", url),
                ));
            }
        }
    }

    Ok(relative)
}

fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.as_bytes();
    let candidate = candidate.as_bytes();
    let mut pattern_idx = 0usize;
    let mut candidate_idx = 0usize;
    let mut star_idx = None;
    let mut backtrack_idx = 0usize;

    while candidate_idx < candidate.len() {
        if pattern_idx < pattern.len()
            && (pattern[pattern_idx] == b'?' || pattern[pattern_idx] == candidate[candidate_idx])
        {
            pattern_idx += 1;
            candidate_idx += 1;
        } else if pattern_idx < pattern.len() && pattern[pattern_idx] == b'*' {
            star_idx = Some(pattern_idx);
            pattern_idx += 1;
            backtrack_idx = candidate_idx;
        } else if let Some(star_pos) = star_idx {
            pattern_idx = star_pos + 1;
            backtrack_idx += 1;
            candidate_idx = backtrack_idx;
        } else {
            return false;
        }
    }

    while pattern_idx < pattern.len() && pattern[pattern_idx] == b'*' {
        pattern_idx += 1;
    }

    pattern_idx == pattern.len()
}

fn path_is_selected(relative_path: &Path, recursive: &RecursiveOptions) -> bool {
    let candidate = relative_path.to_string_lossy().replace('\\', "/");

    if recursive
        .exclude_patterns
        .iter()
        .any(|pattern| pattern_matches(pattern, &candidate))
    {
        return false;
    }

    if recursive.include_patterns.is_empty() {
        return true;
    }

    recursive
        .include_patterns
        .iter()
        .any(|pattern| pattern_matches(pattern, &candidate))
}

fn insert_entry(
    discovered: &mut BTreeMap<String, RecursiveEntry>,
    entry: RecursiveEntry,
    recursive: &RecursiveOptions,
) -> Result<()> {
    let key = entry.relative_path.to_string_lossy().to_string();
    match discovered.get(&key).map(|existing| existing.url.clone()) {
        Some(existing_url) if existing_url == entry.url => Ok(()),
        Some(existing_url) if recursive.overwrite_existing => {
            tracing::debug!(
                "Overwriting recursive entry mapping {} -> {} (was {})",
                key,
                entry.url,
                existing_url
            );
            discovered.insert(key, entry);
            Ok(())
        }
        Some(existing_url) => Err(EngineError::AlreadyExists(format!(
            "{} maps to both {} and {}",
            key, existing_url, entry.url
        ))),
        None => {
            discovered.insert(key, entry);
            Ok(())
        }
    }
}

fn extract_links(html: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a[href]").expect("static selector should parse");

    document
        .select(&selector)
        .filter_map(|node| node.value().attr("href"))
        .map(ToOwned::to_owned)
        .collect()
}

async fn fetch_discovery_response(
    http: &HttpDownloader,
    url: &Url,
    options: &DownloadOptions,
) -> Result<DiscoveryResponse> {
    let user_agent = options
        .user_agent
        .as_deref()
        .unwrap_or(&http.config.default_user_agent);
    let referer = options.referer.as_deref();
    let headers = options.headers.clone();
    let cookies = options.cookies.clone();

    with_retry(&http.pool, http.retry_policy(), || {
        let client = http.client().clone();
        let url = url.clone();
        let headers = headers.clone();
        let cookies = cookies.clone();
        async move {
            let mut request = client.get(url).header("User-Agent", user_agent);
            request = request.header("Accept-Encoding", ACCEPT_ENCODING_IDENTITY);

            if let Some(referer) = referer {
                request = request.header("Referer", referer);
            }

            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }

            if let Some(cookie_list) = cookies.as_ref() {
                if !cookie_list.is_empty() {
                    request = request.header("Cookie", cookie_list.join("; "));
                }
            }

            let mut response = request.send().await?.error_for_status()?;
            let final_url = response.url().clone();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.to_ascii_lowercase());
            let size_hint = response.content_length();

            let is_html = final_url.path().ends_with('/')
                || content_type
                    .as_deref()
                    .map(|value| {
                        value.starts_with("text/html") || value.starts_with("application/xhtml+xml")
                    })
                    .unwrap_or(false);

            if !is_html {
                return Ok(DiscoveryResponse {
                    final_url,
                    html: None,
                    size_hint,
                });
            }

            if let Some(length) = size_hint {
                if length > MAX_DISCOVERY_HTML_BYTES as u64 {
                    return Err(EngineError::ResourceLimit {
                        resource: "recursive_html_bytes",
                        limit: MAX_DISCOVERY_HTML_BYTES,
                    });
                }
            }

            // Stream the body so a chunked (or lying) response cannot buffer
            // more than the discovery HTML cap in memory.
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if body.len() + chunk.len() > MAX_DISCOVERY_HTML_BYTES {
                    return Err(EngineError::ResourceLimit {
                        resource: "recursive_html_bytes",
                        limit: MAX_DISCOVERY_HTML_BYTES,
                    });
                }
                body.extend_from_slice(&chunk);
            }

            Ok(DiscoveryResponse {
                final_url,
                html: Some(String::from_utf8_lossy(&body).into_owned()),
                size_hint,
            })
        }
    })
    .await
}

/// Discover file candidates reachable from a recursive HTTP/HTTPS root.
pub(crate) async fn discover(
    http: &HttpDownloader,
    root_url: &str,
    options: &DownloadOptions,
    recursive: &RecursiveOptions,
) -> Result<RecursiveManifest> {
    let parsed_url = normalize_url(
        root_url,
        &Url::parse(root_url)
            .map_err(|e| EngineError::invalid_input("root_url", format!("Invalid URL: {}", e)))?,
    )?;

    if recursive.max_discovery_concurrency == 0 {
        return Err(EngineError::invalid_input(
            "recursive.max_discovery_concurrency",
            "must be greater than 0",
        ));
    }

    let _scope_prefix = normalized_scope_prefix(&parsed_url, recursive)?;
    if !is_url_in_scope(&parsed_url, &parsed_url, recursive, 0) {
        return Err(EngineError::invalid_input(
            "root_url",
            "root URL is outside the configured recursive scope",
        ));
    }

    let mut queue = VecDeque::from([(parsed_url.clone(), 0usize)]);
    let mut visited_pages = HashSet::new();
    let mut discovered: BTreeMap<String, RecursiveEntry> = BTreeMap::new();
    let mut pages_truncated = false;
    // Fetches run with bounded concurrency; responses are processed (and new
    // pages enqueued) one at a time on this task, so manifest construction
    // stays deterministic. On error the unfinished fetches are dropped.
    let mut in_flight = FuturesUnordered::new();

    'crawl: loop {
        // Fill available fetch slots from the BFS queue.
        while in_flight.len() < recursive.max_discovery_concurrency {
            let Some((current_url, depth)) = queue.pop_front() else {
                break;
            };

            if !visited_pages.insert(current_url.as_str().to_string()) {
                continue;
            }

            if visited_pages.len() > MAX_DISCOVERED_PAGES {
                tracing::warn!(
                    "Recursive discovery reached the page limit ({}); results are truncated",
                    MAX_DISCOVERED_PAGES
                );
                pages_truncated = true;
                queue.clear();
                break;
            }

            in_flight.push(async move {
                let response = fetch_discovery_response(http, &current_url, options).await;
                (current_url, depth, response)
            });
        }

        let Some((current_url, depth, response)) = in_flight.next().await else {
            break; // queue drained and no fetches in flight
        };
        let response = match response {
            Ok(response) => response,
            // A dead root is a real error; a failing nested page only loses
            // its own subtree, so log it and keep crawling.
            Err(err) if depth == 0 => return Err(err),
            Err(err) => {
                tracing::warn!("Skipping recursive discovery page {}: {}", current_url, err);
                continue;
            }
        };

        if !is_url_in_scope(&response.final_url, &parsed_url, recursive, depth) {
            return Err(EngineError::invalid_input(
                "root_url",
                format!("redirect escaped recursive scope: {}", response.final_url),
            ));
        }

        if let Some(html) = response.html {
            if depth >= recursive.max_depth {
                continue;
            }

            for link in extract_links(&html) {
                let normalized = match normalize_url(&link, &response.final_url) {
                    Ok(url) => url,
                    Err(err) => {
                        tracing::debug!("Skipping malformed discovery link '{}': {}", link, err);
                        continue;
                    }
                };

                if !is_url_in_scope(&normalized, &parsed_url, recursive, depth + 1) {
                    continue;
                }

                if normalized.path().ends_with('/') {
                    // Autoindex sort links (`?C=N;O=D`, ...) address the same
                    // directory page, so strip the query before dedup/enqueue.
                    // File URLs keep their query: it may be load-bearing.
                    let mut normalized = normalized;
                    normalized.set_query(None);
                    if !pages_truncated && !visited_pages.contains(normalized.as_str()) {
                        queue.push_back((normalized, depth + 1));
                    }
                    continue;
                }

                let relative_path = build_relative_path(&normalized, &parsed_url, recursive)?;
                if !path_is_selected(&relative_path, recursive) {
                    continue;
                }

                if discovered.len() >= MAX_DISCOVERED_FILES {
                    tracing::warn!(
                        "Recursive discovery reached the file limit ({}); results are truncated",
                        MAX_DISCOVERED_FILES
                    );
                    break 'crawl;
                }

                insert_entry(
                    &mut discovered,
                    RecursiveEntry {
                        url: normalized.to_string(),
                        relative_path,
                        size_hint: None,
                    },
                    recursive,
                )?;
            }
        } else {
            let relative_path = build_relative_path(&response.final_url, &parsed_url, recursive)?;
            if !path_is_selected(&relative_path, recursive) {
                continue;
            }

            if discovered.len() >= MAX_DISCOVERED_FILES {
                tracing::warn!(
                    "Recursive discovery reached the file limit ({}); results are truncated",
                    MAX_DISCOVERED_FILES
                );
                break 'crawl;
            }

            insert_entry(
                &mut discovered,
                RecursiveEntry {
                    url: response.final_url.to_string(),
                    relative_path,
                    size_hint: response.size_hint,
                },
                recursive,
            )?;
        }
    }

    Ok(RecursiveManifest {
        root_url: parsed_url.to_string(),
        entries: discovered.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_relative_path, insert_entry, is_url_in_scope, normalize_path, normalize_url,
        normalized_scope_prefix, path_is_selected, pattern_matches, MAX_DISCOVERED_FILES,
        MAX_DISCOVERED_PAGES,
    };
    use crate::types::RecursiveOptions;
    use crate::{DownloadEngine, DownloadOptions, EngineConfig, EngineError, RecursiveEntry};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;
    use url::Url;
    use wiremock::matchers::{header, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn normalize_url_resolves_relative_paths_and_strips_fragments() {
        let base = Url::parse("https://example.com/pub/index.html").unwrap();

        let normalized = normalize_url("./releases/app.tar.gz#download", &base).unwrap();

        assert_eq!(
            normalized.as_str(),
            "https://example.com/pub/releases/app.tar.gz"
        );
    }

    #[test]
    fn normalize_url_collapses_parent_segments() {
        let base = Url::parse("https://example.com/pub/").unwrap();

        let normalized = normalize_url("../../etc/passwd", &base).unwrap();
        assert_eq!(normalized.as_str(), "https://example.com/etc/passwd");
    }

    #[test]
    fn normalize_url_rejects_unsupported_schemes() {
        let base = Url::parse("https://example.com/pub/").unwrap();

        let err = normalize_url("javascript:alert(1)", &base).unwrap_err();
        assert!(err.to_string().contains("Unsupported scheme"));
    }

    #[test]
    fn normalize_path_preserves_directory_suffix() {
        let normalized = normalize_path("/pub/releases/./v1/", true).unwrap();
        assert_eq!(normalized, "/pub/releases/v1/");
    }

    #[test]
    fn normalized_scope_prefix_uses_parent_for_index_pages() {
        let root = Url::parse("https://example.com/pub/index.html").unwrap();
        let recursive = RecursiveOptions::default();

        let prefix = normalized_scope_prefix(&root, &recursive).unwrap();
        assert_eq!(prefix, "/pub/");
    }

    #[test]
    fn normalized_scope_prefix_honors_explicit_prefix() {
        let root = Url::parse("https://example.com/pub/index.html").unwrap();
        let recursive = RecursiveOptions {
            allowed_prefix: Some("pub/releases".to_string()),
            ..RecursiveOptions::default()
        };

        let prefix = normalized_scope_prefix(&root, &recursive).unwrap();
        assert_eq!(prefix, "/pub/releases/");
    }

    #[test]
    fn scope_rejects_cross_host_links() {
        let root = Url::parse("https://example.com/pub/").unwrap();
        let other = Url::parse("https://cdn.example.com/pub/file.tar.gz").unwrap();

        assert!(!is_url_in_scope(
            &other,
            &root,
            &RecursiveOptions::default(),
            1
        ));
    }

    #[test]
    fn scope_rejects_sibling_directories() {
        let root = Url::parse("https://example.com/pub/").unwrap();
        let sibling = Url::parse("https://example.com/public/file.tar.gz").unwrap();

        assert!(!is_url_in_scope(
            &sibling,
            &root,
            &RecursiveOptions::default(),
            1
        ));
    }

    #[test]
    fn scope_rejects_paths_that_escape_the_root_prefix() {
        let root = Url::parse("https://example.com/pub/").unwrap();
        let escaped = Url::parse("https://example.com/etc/passwd").unwrap();

        assert!(!is_url_in_scope(
            &escaped,
            &root,
            &RecursiveOptions::default(),
            1
        ));
    }

    #[test]
    fn scope_accepts_nested_paths_under_root_prefix() {
        let root = Url::parse("https://example.com/pub/").unwrap();
        let nested = Url::parse("https://example.com/pub/releases/v1/app.tar.gz").unwrap();

        assert!(is_url_in_scope(
            &nested,
            &root,
            &RecursiveOptions::default(),
            2
        ));
    }

    #[test]
    fn scope_rejects_urls_beyond_max_depth() {
        let root = Url::parse("https://example.com/pub/").unwrap();
        let nested = Url::parse("https://example.com/pub/releases/v1/app.tar.gz").unwrap();
        let recursive = RecursiveOptions {
            max_depth: 1,
            ..RecursiveOptions::default()
        };

        assert!(!is_url_in_scope(&nested, &root, &recursive, 2));
    }

    #[test]
    fn build_relative_path_preserves_nested_structure() {
        let root = Url::parse("https://example.com/pub/").unwrap();
        let url = Url::parse("https://example.com/pub/releases/v1/app.tar.gz").unwrap();

        let relative = build_relative_path(&url, &root, &RecursiveOptions::default()).unwrap();
        assert_eq!(relative, PathBuf::from("releases/v1/app.tar.gz"));
    }

    #[test]
    fn pattern_matches_supports_wildcards() {
        assert!(pattern_matches("*.txt", "readme.txt"));
        assert!(pattern_matches("releases/*", "releases/app.tar.gz"));
        assert!(pattern_matches("release-?.txt", "release-a.txt"));
        assert!(!pattern_matches("*.txt", "archive.tar.gz"));
    }

    #[test]
    fn path_selection_uses_exclude_precedence() {
        let recursive = RecursiveOptions {
            include_patterns: vec!["*.txt".to_string()],
            exclude_patterns: vec!["readme.txt".to_string()],
            ..RecursiveOptions::default()
        };

        assert!(!path_is_selected(&PathBuf::from("readme.txt"), &recursive));
        assert!(path_is_selected(&PathBuf::from("notes.txt"), &recursive));
        assert!(!path_is_selected(
            &PathBuf::from("archive.tar.gz"),
            &recursive
        ));
    }

    #[test]
    fn insert_entry_rejects_colliding_relative_paths() {
        let mut discovered = BTreeMap::new();
        let recursive = RecursiveOptions::default();

        insert_entry(
            &mut discovered,
            RecursiveEntry {
                url: "https://example.com/pub/file.txt?one".to_string(),
                relative_path: PathBuf::from("file.txt"),
                size_hint: None,
            },
            &recursive,
        )
        .unwrap();

        let err = insert_entry(
            &mut discovered,
            RecursiveEntry {
                url: "https://example.com/pub/file.txt?two".to_string(),
                relative_path: PathBuf::from("file.txt"),
                size_hint: None,
            },
            &recursive,
        )
        .unwrap_err();

        assert!(matches!(err, EngineError::AlreadyExists(_)));
    }

    async fn create_engine(temp_dir: &TempDir) -> std::sync::Arc<DownloadEngine> {
        let config = EngineConfig {
            download_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };

        DownloadEngine::new(config).await.unwrap()
    }

    #[tokio::test]
    async fn discover_builds_manifest_for_nested_directory_indexes() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .and(header(
                "user-agent",
                format!("gosh-dl/{}", env!("CARGO_PKG_VERSION")).as_str(),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="releases/">releases/</a>
                            <a href="readme.txt">readme.txt</a>
                            <a href="https://other.example.com/file.txt">external</a>
                        </body></html>
                        "#,
                    ),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/pub/releases/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="app.tar.gz">app.tar.gz</a>
                            <a href="../ignore.txt">ignore.txt</a>
                        </body></html>
                        "#,
                    ),
            )
            .mount(&server)
            .await;

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/pub/", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions::default(),
            )
            .await
            .unwrap();

        let paths: Vec<_> = manifest
            .entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            paths,
            vec!["ignore.txt", "readme.txt", "releases/app.tar.gz"]
        );
    }

    #[tokio::test]
    async fn discover_concurrent_matches_sequential_manifest() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        let dir_page = |links: &[&str]| {
            let body = links
                .iter()
                .map(|link| format!(r#"<a href="{link}">{link}</a>"#))
                .collect::<String>();
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(format!("<html><body>{body}</body></html>"))
        };

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .respond_with(dir_page(&["a/", "b/", "c/", "top.txt"]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/pub/a/"))
            .respond_with(dir_page(&["one.bin", "nested/"]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/pub/a/nested/"))
            .respond_with(dir_page(&["deep.bin"]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/pub/b/"))
            .respond_with(dir_page(&["two.bin"]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/pub/c/"))
            .respond_with(dir_page(&[]))
            .mount(&server)
            .await;

        let root = format!("{}/pub/", server.uri());
        let sequential = engine
            .discover_http_recursive(
                &root,
                &DownloadOptions::default(),
                &RecursiveOptions {
                    max_discovery_concurrency: 1,
                    ..RecursiveOptions::default()
                },
            )
            .await
            .unwrap();
        let concurrent = engine
            .discover_http_recursive(
                &root,
                &DownloadOptions::default(),
                &RecursiveOptions {
                    max_discovery_concurrency: 4,
                    ..RecursiveOptions::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(sequential, concurrent);
        let paths: Vec<_> = concurrent
            .entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            paths,
            vec!["a/nested/deep.bin", "a/one.bin", "b/two.bin", "top.txt"]
        );
    }

    #[tokio::test]
    async fn discover_treats_non_html_root_as_single_file() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/pub/archive.tar.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/gzip")
                    .insert_header("Content-Length", "12")
                    .set_body_bytes(b"hello world!".to_vec()),
            )
            .mount(&server)
            .await;

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/pub/archive.tar.gz", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(
            manifest.entries[0].relative_path,
            PathBuf::from("archive.tar.gz")
        );
        assert_eq!(manifest.entries[0].size_hint, Some(12));
    }

    #[tokio::test]
    async fn discover_propagates_headers_and_cookies() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/private/"))
            .and(header("authorization", "Bearer token"))
            .and(header("cookie", "session=abc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"<html><body><a href="file.bin">file.bin</a></body></html>"#,
                    ),
            )
            .mount(&server)
            .await;

        let options = DownloadOptions {
            headers: vec![("Authorization".to_string(), "Bearer token".to_string())],
            cookies: Some(vec!["session=abc".to_string()]),
            ..DownloadOptions::default()
        };

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/private/", server.uri()),
                &options,
                &RecursiveOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].relative_path, PathBuf::from("file.bin"));
    }

    #[tokio::test]
    async fn add_http_recursive_downloads_child_files() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="readme.txt">readme.txt</a>
                            <a href="releases/">releases/</a>
                        </body></html>
                        "#,
                    ),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/pub/releases/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"<html><body><a href="app.tar.gz">app.tar.gz</a></body></html>"#,
                    ),
            )
            .mount(&server)
            .await;

        for file_path in ["/pub/readme.txt", "/pub/releases/app.tar.gz"] {
            Mock::given(method("HEAD"))
                .and(path(file_path))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("Content-Length", "11")
                        .insert_header("Accept-Ranges", "bytes"),
                )
                .mount(&server)
                .await;

            Mock::given(method("GET"))
                .and(path(file_path))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("Content-Length", "11")
                        .set_body_bytes(b"hello world".to_vec()),
                )
                .mount(&server)
                .await;
        }

        let job = engine
            .add_http_recursive(
                &format!("{}/pub/", server.uri()),
                DownloadOptions::default(),
                RecursiveOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(job.child_ids.len(), 2);

        timeout(Duration::from_secs(10), async {
            loop {
                let statuses = job
                    .child_ids
                    .iter()
                    .filter_map(|id| engine.status(*id))
                    .collect::<Vec<_>>();
                if statuses
                    .iter()
                    .all(|status| matches!(status.state, crate::DownloadState::Completed))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(temp_dir.path().join("readme.txt"))
                .await
                .unwrap(),
            "hello world"
        );
        assert_eq!(
            tokio::fs::read_to_string(temp_dir.path().join("releases").join("app.tar.gz"))
                .await
                .unwrap(),
            "hello world"
        );
    }

    #[tokio::test]
    async fn discover_applies_include_and_exclude_patterns_to_files() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="readme.txt">readme.txt</a>
                            <a href="archive.tar.gz">archive.tar.gz</a>
                            <a href="releases/">releases/</a>
                        </body></html>
                        "#,
                    ),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/pub/releases/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="notes.txt">notes.txt</a>
                            <a href="app.tar.gz">app.tar.gz</a>
                        </body></html>
                        "#,
                    ),
            )
            .mount(&server)
            .await;

        let recursive = RecursiveOptions {
            include_patterns: vec!["*.txt".to_string()],
            exclude_patterns: vec!["readme.txt".to_string()],
            ..RecursiveOptions::default()
        };

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/pub/", server.uri()),
                &DownloadOptions::default(),
                &recursive,
            )
            .await
            .unwrap();

        let paths: Vec<_> = manifest
            .entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().to_string())
            .collect();
        assert_eq!(paths, vec!["releases/notes.txt"]);
    }

    #[tokio::test]
    async fn discover_strips_autoindex_queries_from_directory_links() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="?C=N;O=D">Name</a>
                            <a href="?C=M;O=A">Last modified</a>
                            <a href="?C=S;O=A">Size</a>
                            <a href="releases/">releases/</a>
                            <a href="releases/?C=N;O=D">releases sorted</a>
                            <a href="report.bin?version=2">report.bin</a>
                        </body></html>
                        "#,
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/pub/releases/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="?C=M;O=D">Last modified</a>
                            <a href="app.tar.gz">app.tar.gz</a>
                        </body></html>
                        "#,
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/pub/", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions::default(),
            )
            .await
            .unwrap();

        let paths: Vec<_> = manifest
            .entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().to_string())
            .collect();
        assert_eq!(paths, vec!["releases/app.tar.gz", "report.bin"]);

        // File URLs keep their query string; it may be load-bearing.
        let report = manifest
            .entries
            .iter()
            .find(|entry| entry.relative_path == Path::new("report.bin"))
            .unwrap();
        assert!(
            report.url.ends_with("/pub/report.bin?version=2"),
            "unexpected file URL: {}",
            report.url
        );

        // MockServer verifies the .expect(1) counters on drop: each directory
        // page is fetched exactly once despite the sort-link variants.
    }

    #[tokio::test]
    async fn discover_truncates_at_page_limit() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        let dir_count = MAX_DISCOVERED_PAGES + 100;
        let root_body = (0..dir_count)
            .map(|idx| format!(r#"<a href="d{idx}/">d{idx}/</a>"#))
            .collect::<String>();

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(format!("<html><body>{root_body}</body></html>")),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex(r"^/pub/d\d+/$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"<html><body><a href="file.bin">file.bin</a></body></html>"#,
                    ),
            )
            .mount(&server)
            .await;

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/pub/", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions {
                    max_discovery_concurrency: 1,
                    ..RecursiveOptions::default()
                },
            )
            .await
            .unwrap();

        // The root page plus MAX_DISCOVERED_PAGES - 1 directory pages are
        // fetched before the limit stops the crawl; each directory
        // contributes one file to the truncated manifest.
        assert_eq!(manifest.entries.len(), MAX_DISCOVERED_PAGES - 1);
    }

    #[tokio::test]
    async fn discover_truncates_at_file_limit() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        let file_count = MAX_DISCOVERED_FILES + 50;
        let body = (0..file_count)
            .map(|idx| format!(r#"<a href="f{idx:05}.bin">f{idx:05}.bin</a>"#))
            .collect::<String>();

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(format!("<html><body>{body}</body></html>")),
            )
            .mount(&server)
            .await;

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/pub/", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(manifest.entries.len(), MAX_DISCOVERED_FILES);
    }

    #[tokio::test]
    async fn discover_skips_pages_that_fail_to_fetch() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="broken/">broken/</a>
                            <a href="good/">good/</a>
                            <a href="top.txt">top.txt</a>
                        </body></html>
                        "#,
                    ),
            )
            .mount(&server)
            .await;

        // "/pub/broken/" has no mock and returns 404; the crawl should keep
        // the rest of the discovery instead of aborting.
        Mock::given(method("GET"))
            .and(path("/pub/good/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"<html><body><a href="file.bin">file.bin</a></body></html>"#,
                    ),
            )
            .mount(&server)
            .await;

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/pub/", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions::default(),
            )
            .await
            .unwrap();

        let paths: Vec<_> = manifest
            .entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().to_string())
            .collect();
        assert_eq!(paths, vec!["good/file.bin", "top.txt"]);
    }

    #[tokio::test]
    async fn discover_fails_when_root_fetch_fails() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        // No mock for the root: wiremock answers 404.
        let err = engine
            .discover_http_recursive(
                &format!("{}/pub/", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions::default(),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(err, EngineError::Network { .. }),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn discover_skips_oversized_nested_page() {
        let temp_dir = TempDir::new().unwrap();
        let engine = create_engine(&temp_dir).await;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/pub/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(
                        r#"
                        <html><body>
                            <a href="huge/">huge/</a>
                            <a href="ok.txt">ok.txt</a>
                        </body></html>
                        "#,
                    ),
            )
            .mount(&server)
            .await;

        let oversized = "x".repeat(super::MAX_DISCOVERY_HTML_BYTES + 1);
        Mock::given(method("GET"))
            .and(path("/pub/huge/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/html")
                    .set_body_string(oversized),
            )
            .mount(&server)
            .await;

        let manifest = engine
            .discover_http_recursive(
                &format!("{}/pub/", server.uri()),
                &DownloadOptions::default(),
                &RecursiveOptions::default(),
            )
            .await
            .unwrap();

        let paths: Vec<_> = manifest
            .entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().to_string())
            .collect();
        assert_eq!(paths, vec!["ok.txt"]);
    }
}
