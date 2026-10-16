//! Metalink 4 (RFC 5854) support
//!
//! Parses `.meta4` XML documents and queues each described file as an HTTP
//! download, using the document's mirror URLs (ordered by `priority`) and
//! checksums (SHA-256 preferred over MD5) through the engine's existing
//! mirror-failover and checksum-verification machinery.
//!
//! Torrent `<metaurl>` elements, `<pieces>` (piece hashes), and unknown
//! elements are ignored. Namespaces are handled loosely: elements are matched
//! by local name, so both unprefixed and prefixed
//! (`urn:ietf:params:xml:ns:metalink`) documents parse.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::engine::DownloadEngine;
use crate::error::{EngineError, Result, StorageErrorKind};
use crate::http::validate_filename_components;
use crate::protocol::{DownloadId, DownloadOptions, ExpectedChecksum};

/// Default URL priority when the `priority` attribute is absent (RFC 5854
/// allows 1–999999; lower values are higher priority).
const DEFAULT_URL_PRIORITY: u64 = 999_999;

/// A single `<file>` entry parsed from a Metalink 4 document.
#[derive(Debug, Clone)]
pub struct MetalinkFile {
    /// Save name from the `name` attribute (validated: no absolute paths, no
    /// `..` components).
    pub name: String,
    /// Declared `<size>` in bytes, if present.
    pub size: Option<u64>,
    /// Download URLs, sorted by ascending `priority` (best first).
    pub urls: Vec<String>,
    /// Whole-file checksum: SHA-256 if the document provides one, else MD5.
    pub checksum: Option<ExpectedChecksum>,
}

fn parse_error(message: impl Into<String>) -> EngineError {
    EngineError::invalid_input("metalink", message)
}

fn xml_error(err: impl std::fmt::Display) -> EngineError {
    parse_error(format!("XML parse error: {}", err))
}

/// Extract the required, validated `name` attribute from a `<file>` element.
fn file_name_attr(e: &BytesStart<'_>) -> Result<String> {
    for attr in e.attributes() {
        let attr = attr.map_err(xml_error)?;
        if attr.key.local_name().as_ref() == b"name" {
            let name = attr.unescape_value().map_err(xml_error)?.into_owned();
            if name.is_empty() {
                return Err(parse_error("<file> name attribute is empty"));
            }
            // Reject absolute paths and `..` components (path traversal).
            validate_filename_components(&name)?;
            return Ok(name);
        }
    }
    Err(parse_error("<file> element is missing the name attribute"))
}

/// Find an attribute by local name, returning its unescaped value.
fn attr_by_local_name(e: &BytesStart<'_>, key: &[u8]) -> Result<Option<String>> {
    for attr in e.attributes() {
        let attr = attr.map_err(xml_error)?;
        if attr.key.local_name().as_ref() == key {
            return Ok(Some(attr.unescape_value().map_err(xml_error)?.into_owned()));
        }
    }
    Ok(None)
}

/// Read the text content of a leaf element (e.g. `<size>`, `<hash>`, `<url>`),
/// resolving character/entity references. Unexpected nested markup is skipped.
/// Consumes events up to and including the element's end tag.
fn read_leaf_text(reader: &mut Reader<&[u8]>) -> Result<String> {
    let mut out = String::new();
    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Text(t) => out.push_str(&t.decode().map_err(xml_error)?),
            Event::CData(t) => out.push_str(&t.decode().map_err(xml_error)?),
            Event::GeneralRef(r) => {
                if let Some(ch) = r.resolve_char_ref().map_err(xml_error)? {
                    out.push(ch);
                } else {
                    match r.decode().map_err(xml_error)?.as_ref() {
                        "amp" => out.push('&'),
                        "lt" => out.push('<'),
                        "gt" => out.push('>'),
                        "apos" => out.push('\''),
                        "quot" => out.push('"'),
                        other => {
                            return Err(parse_error(format!(
                                "unsupported entity reference: &{};",
                                other
                            )))
                        }
                    }
                }
            }
            // Leaf elements shouldn't contain children; skip any gracefully.
            Event::Start(child) => {
                reader.read_to_end(child.name()).map_err(xml_error)?;
            }
            Event::End(_) => break,
            Event::Eof => return Err(parse_error("unexpected end of document")),
            _ => {}
        }
    }
    Ok(out.trim().to_string())
}

/// Validate that a hash value is hex of the expected length.
fn validate_hex(value: &str, expected_len: usize, label: &str) -> Result<()> {
    if value.len() != expected_len || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(parse_error(format!(
            "invalid {} hash value: {:?}",
            label, value
        )));
    }
    Ok(())
}

/// Parse one `<file>` element (the `Start` event has already been consumed).
fn parse_file(reader: &mut Reader<&[u8]>, start: &BytesStart<'_>) -> Result<MetalinkFile> {
    let name = file_name_attr(start)?;
    let mut size: Option<u64> = None;
    let mut sha256: Option<String> = None;
    let mut md5: Option<String> = None;
    let mut urls: Vec<(u64, String)> = Vec::new();

    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Start(e) => match e.local_name().as_ref() {
                b"size" => {
                    let text = read_leaf_text(reader)?;
                    size =
                        Some(text.parse::<u64>().map_err(|_| {
                            parse_error(format!("invalid <size> value: {:?}", text))
                        })?);
                }
                b"hash" => {
                    let hash_type =
                        attr_by_local_name(&e, b"type")?.map(|t| t.trim().to_ascii_lowercase());
                    let value = read_leaf_text(reader)?.to_ascii_lowercase();
                    match hash_type.as_deref() {
                        Some("sha-256") | Some("sha256") => {
                            validate_hex(&value, 64, "sha-256")?;
                            sha256 = Some(value);
                        }
                        Some("md5") => {
                            validate_hex(&value, 32, "md5")?;
                            md5 = Some(value);
                        }
                        // Other hash types (sha-1, sha-512, ...) are ignored.
                        _ => {}
                    }
                }
                b"url" => {
                    let priority = attr_by_local_name(&e, b"priority")?
                        .and_then(|p| p.trim().parse::<u64>().ok())
                        .unwrap_or(DEFAULT_URL_PRIORITY);
                    let url = read_leaf_text(reader)?;
                    if !url.is_empty() {
                        urls.push((priority, url));
                    }
                }
                // <metaurl> (torrent metalinks), <pieces>, and unknown
                // elements are skipped, children included.
                _ => {
                    reader.read_to_end(e.name()).map_err(xml_error)?;
                }
            },
            // Self-closing children (<url/>, <size/>, ...) carry no content.
            Event::Empty(_) => {}
            Event::End(_) => break,
            Event::Eof => return Err(parse_error("unexpected end of document inside <file>")),
            _ => {}
        }
    }

    // Stable sort: ties keep document order.
    urls.sort_by_key(|(priority, _)| *priority);

    let checksum = match (sha256, md5) {
        // SHA-256 preferred over MD5 when both are present.
        (Some(value), _) => Some(ExpectedChecksum::sha256(value)),
        (None, Some(value)) => Some(ExpectedChecksum::md5(value)),
        (None, None) => None,
    };

    Ok(MetalinkFile {
        name,
        size,
        urls: urls.into_iter().map(|(_, url)| url).collect(),
        checksum,
    })
}

/// Parse a Metalink 4 (RFC 5854) XML document.
///
/// Returns one [`MetalinkFile`] per `<file>` element. The whole document is
/// rejected with an error if it is not valid XML, is not rooted at
/// `<metalink>`, or contains a file whose `name` attribute is missing, empty,
/// absolute, or contains `..` components.
pub fn parse_metalink(xml: &[u8]) -> Result<Vec<MetalinkFile>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut files = Vec::new();
    let mut saw_root = false;
    let mut closed_root = false;

    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Start(e) => {
                if closed_root {
                    return Err(parse_error("content after </metalink>"));
                }
                if !saw_root {
                    if e.local_name().as_ref() != b"metalink" {
                        return Err(parse_error("root element is not <metalink>"));
                    }
                    saw_root = true;
                } else if e.local_name().as_ref() == b"file" {
                    files.push(parse_file(&mut reader, &e)?);
                } else {
                    // <published>, <generator>, <origin>, unknown elements...
                    reader.read_to_end(e.name()).map_err(xml_error)?;
                }
            }
            Event::Empty(e) => {
                if closed_root {
                    return Err(parse_error("content after </metalink>"));
                }
                if !saw_root {
                    if e.local_name().as_ref() != b"metalink" {
                        return Err(parse_error("root element is not <metalink>"));
                    }
                    saw_root = true;
                    closed_root = true;
                } else if e.local_name().as_ref() == b"file" {
                    // <file name="..."/> — valid name, but no URLs.
                    files.push(MetalinkFile {
                        name: file_name_attr(&e)?,
                        size: None,
                        urls: Vec::new(),
                        checksum: None,
                    });
                }
            }
            Event::End(_) => closed_root = true,
            Event::Text(t) if !t.decode().map_err(xml_error)?.trim().is_empty() => {
                return Err(parse_error("unexpected text outside a Metalink element"));
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !saw_root {
        return Err(parse_error("document has no <metalink> root element"));
    }
    if !closed_root {
        return Err(parse_error("unclosed <metalink> root element"));
    }
    Ok(files)
}

impl DownloadEngine {
    /// Add every file described by a Metalink 4 (RFC 5854) document as an
    /// HTTP download.
    ///
    /// For each `<file>`: the highest-priority HTTP/HTTPS URL becomes the primary
    /// download URL, the rest become mirrors (automatic failover), and the
    /// document's SHA-256 (or MD5) checksum is verified after download.
    /// `options` supplies the shared settings (save dir, headers, limits...);
    /// per-file fields (`filename`, `mirrors`, `checksum`) are overridden from
    /// the document. Other transports are ignored; files with no supported URL
    /// are skipped with a warning. Declared size is parsed but is not enforced
    /// as an independent integrity constraint. Torrent metaurls, piece hashes,
    /// and hash algorithms other than SHA-256/MD5 are not supported.
    ///
    /// All-or-nothing: if adding any file fails, the downloads already added
    /// from this document are cancelled and the error is returned.
    pub async fn add_metalink(
        &self,
        xml: &[u8],
        options: DownloadOptions,
    ) -> Result<Vec<DownloadId>> {
        let files = parse_metalink(xml)?;
        let mut added: Vec<DownloadId> = Vec::new();

        for mut file in files {
            // RFC 5854 permits other transports, but this engine queues HTTP.
            // An FTP URL with a better priority must not hide usable mirrors.
            file.urls.retain(|url| {
                url::Url::parse(url)
                    .map(|u| matches!(u.scheme(), "http" | "https"))
                    .unwrap_or(false)
            });
            if file.urls.is_empty() {
                tracing::warn!(
                    file = %file.name,
                    "Skipping metalink file with no supported HTTP/HTTPS URLs"
                );
                continue;
            }

            let mut per_file_options = options.clone();
            per_file_options.filename = Some(file.name.clone());
            per_file_options.mirrors = file.urls[1..].to_vec();
            per_file_options.checksum = file.checksum.clone();

            match self.add_http(&file.urls[0], per_file_options).await {
                Ok(id) => added.push(id),
                Err(err) => {
                    // All-or-nothing: roll back the downloads already added.
                    for id in added {
                        self.cancel(id, true).await.ok();
                    }
                    return Err(err);
                }
            }
        }

        Ok(added)
    }

    /// Read a `.meta4` / `.metalink` file from disk and add its downloads.
    ///
    /// See [`DownloadEngine::add_metalink`].
    pub async fn add_metalink_file(
        &self,
        path: &std::path::Path,
        options: DownloadOptions,
    ) -> Result<Vec<DownloadId>> {
        let xml = tokio::fs::read(path).await.map_err(|e| {
            EngineError::storage(
                StorageErrorKind::Io,
                path,
                format!("Failed to read metalink file: {}", e),
            )
        })?;
        self.add_metalink(&xml, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ChecksumAlgorithm;

    const SHA256_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MD5_A: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn representative_doc() -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <published>2010-05-01T12:15:21Z</published>
  <file name="example.ext">
    <size>14471447</size>
    <identity>Example</identity>
    <description>A description of the example file for download.</description>
    <hash type="sha-1">abcdef</hash>
    <hash type="md5">{md5}</hash>
    <hash type="sha-256">{sha}</hash>
    <pieces type="sha-256" length="1048576">
      <hash>0000000000000000000000000000000000000000000000000000000000000000</hash>
    </pieces>
    <metaurl mediatype="torrent">http://example.com/example.ext.torrent</metaurl>
    <url priority="20" location="us">http://mirror.example.com/example.ext?a=1&amp;b=2</url>
    <url priority="1" location="de">ftp://ftp.example.com/example.ext</url>
    <url>http://last.example.com/example.ext</url>
    <url priority="5">http://five.example.com/example.ext</url>
  </file>
  <file name="sub/other.bin">
    <size>42</size>
    <url>http://example.com/other.bin</url>
  </file>
</metalink>"#,
            md5 = MD5_A,
            sha = SHA256_A,
        )
    }

    #[test]
    fn parses_representative_document() {
        let files = parse_metalink(representative_doc().as_bytes()).unwrap();
        assert_eq!(files.len(), 2);

        let f = &files[0];
        assert_eq!(f.name, "example.ext");
        assert_eq!(f.size, Some(14_471_447));
        // URLs ordered by priority: 1, 5, 20, then default (999999).
        assert_eq!(
            f.urls,
            vec![
                "ftp://ftp.example.com/example.ext",
                "http://five.example.com/example.ext",
                "http://mirror.example.com/example.ext?a=1&b=2",
                "http://last.example.com/example.ext",
            ]
        );
        // SHA-256 preferred over MD5.
        let checksum = f.checksum.as_ref().unwrap();
        assert_eq!(checksum.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(checksum.value, SHA256_A);

        let g = &files[1];
        assert_eq!(g.name, "sub/other.bin");
        assert_eq!(g.size, Some(42));
        assert_eq!(g.urls, vec!["http://example.com/other.bin"]);
        assert!(g.checksum.is_none());
    }

    #[test]
    fn md5_used_when_no_sha256() {
        let xml = format!(
            r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink">
                 <file name="f.bin">
                   <hash type="md5">{}</hash>
                   <url>http://example.com/f.bin</url>
                 </file>
               </metalink>"#,
            MD5_A
        );
        let files = parse_metalink(xml.as_bytes()).unwrap();
        let checksum = files[0].checksum.as_ref().unwrap();
        assert_eq!(checksum.algorithm, ChecksumAlgorithm::Md5);
        assert_eq!(checksum.value, MD5_A);
    }

    #[test]
    fn parses_namespace_prefixed_document() {
        let xml = format!(
            r#"<?xml version="1.0"?>
<m:metalink xmlns:m="urn:ietf:params:xml:ns:metalink">
  <m:file name="pre.bin">
    <m:size>7</m:size>
    <m:hash type="sha-256">{}</m:hash>
    <m:url m:priority="2">http://b.example.com/pre.bin</m:url>
    <m:url priority="1">http://a.example.com/pre.bin</m:url>
  </m:file>
</m:metalink>"#,
            SHA256_A
        );
        let files = parse_metalink(xml.as_bytes()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "pre.bin");
        assert_eq!(files[0].size, Some(7));
        assert_eq!(
            files[0].urls,
            vec![
                "http://a.example.com/pre.bin",
                "http://b.example.com/pre.bin",
            ]
        );
        assert!(files[0].checksum.is_some());
    }

    #[test]
    fn rejects_path_traversal_names() {
        for name in ["../evil", "a/../../evil", "/etc/passwd"] {
            let xml = format!(
                r#"<metalink><file name="{}"><url>http://example.com/x</url></file></metalink>"#,
                name
            );
            let err = parse_metalink(xml.as_bytes());
            assert!(err.is_err(), "name {:?} should be rejected", name);
        }
    }

    #[test]
    fn rejects_missing_or_empty_name() {
        assert!(parse_metalink(
            br#"<metalink><file><url>http://example.com/x</url></file></metalink>"#
        )
        .is_err());
        assert!(parse_metalink(
            br#"<metalink><file name=""><url>http://example.com/x</url></file></metalink>"#
        )
        .is_err());
    }

    #[test]
    fn rejects_empty_and_invalid_xml() {
        assert!(parse_metalink(b"").is_err());
        assert!(parse_metalink(b"not xml at all <<<").is_err());
        assert!(parse_metalink(b"<metalink><file name=\"x\"").is_err());
        // Wrong root element.
        assert!(parse_metalink(b"<rss><file name=\"x\"/></rss>").is_err());
    }

    #[test]
    fn rejects_invalid_size_and_hash_values() {
        assert!(
            parse_metalink(br#"<metalink><file name="x"><size>big</size></file></metalink>"#)
                .is_err()
        );
        assert!(parse_metalink(
            br#"<metalink><file name="x"><hash type="sha-256">zz</hash></file></metalink>"#
        )
        .is_err());
    }

    #[test]
    fn file_without_urls_parses_as_empty() {
        let files = parse_metalink(
            br#"<metalink><file name="nourl.bin"><size>3</size></file><file name="selfclosed"/></metalink>"#,
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[0].urls.is_empty());
        assert!(files[1].urls.is_empty());
    }

    #[test]
    fn unknown_hash_type_is_ignored() {
        let files = parse_metalink(
            br#"<metalink><file name="x"><hash type="sha-512">nothex</hash><url>http://example.com/x</url></file></metalink>"#,
        )
        .unwrap();
        assert!(files[0].checksum.is_none());
    }
}
