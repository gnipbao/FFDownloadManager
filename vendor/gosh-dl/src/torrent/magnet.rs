//! Magnet URI Parser
//!
//! This module parses magnet URIs as defined in the BitTorrent specification.
//! Magnet URIs allow downloading torrents without having the .torrent file,
//! using only the info_hash and optionally tracker URLs.
//!
//! Format: `magnet:?xt=urn:btih:<hash>&dn=<name>&tr=<tracker>`

use crate::error::{EngineError, ProtocolErrorKind, Result};

use super::metainfo::Sha1Hash;

/// Parsed magnet URI
#[derive(Debug, Clone)]
pub struct MagnetUri {
    /// Info hash (20 bytes)
    pub info_hash: Sha1Hash,
    /// Display name (optional)
    pub display_name: Option<String>,
    /// Tracker URLs
    pub trackers: Vec<String>,
    /// Web seed URLs (BEP 19)
    pub web_seeds: Vec<String>,
    /// Exact length (optional, rarely used)
    pub exact_length: Option<u64>,
    /// Exact source (URL to .torrent file)
    pub exact_source: Option<String>,
    /// Keyword topic (search terms)
    pub keyword_topic: Option<String>,
    /// Acceptable sources (for web seeding)
    pub acceptable_sources: Vec<String>,
    /// Manifest topic (URL to file with list of links)
    pub manifest_topic: Option<String>,
    /// Original URI string
    pub original_uri: String,
}

impl MagnetUri {
    /// Parse a magnet URI string
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let uri = "magnet:?xt=urn:btih:HASH&dn=Name&tr=http://tracker.example.com/announce";
    /// let magnet = MagnetUri::parse(uri)?;
    /// ```
    pub fn parse(uri: &str) -> Result<Self> {
        // Validate scheme
        if !uri.starts_with("magnet:?") {
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidMagnet,
                "URI must start with 'magnet:?'",
            ));
        }

        let query = &uri[8..]; // Skip "magnet:?"

        let mut info_hash: Option<Sha1Hash> = None;
        let mut display_name: Option<String> = None;
        let mut trackers = Vec::new();
        let mut web_seeds = Vec::new();
        let mut exact_length: Option<u64> = None;
        let mut exact_source: Option<String> = None;
        let mut keyword_topic: Option<String> = None;
        let mut acceptable_sources = Vec::new();
        let mut manifest_topic: Option<String> = None;

        // Parse query parameters
        for param in query.split('&') {
            if param.is_empty() {
                continue;
            }

            let (key, value) = match param.split_once('=') {
                Some((k, v)) => (k, v),
                None => continue,
            };

            // URL-decode the value
            let value = url_decode(value);

            match key {
                // Exact Topic (required) - urn:btih:<hash>
                "xt" => {
                    if let Some(hash) = parse_btih(&value) {
                        info_hash = Some(hash);
                    }
                }

                // Display Name
                "dn" => {
                    display_name = Some(value);
                }

                // Tracker URL
                "tr" if !value.is_empty() => {
                    trackers.push(value);
                }

                // Web Seed (BEP 19)
                "ws" if !value.is_empty() => {
                    web_seeds.push(value);
                }

                // Exact Length
                "xl" => {
                    exact_length = value.parse().ok();
                }

                // Exact Source (URL to .torrent file)
                "xs" => {
                    exact_source = Some(value);
                }

                // Keyword Topic
                "kt" => {
                    keyword_topic = Some(value);
                }

                // Acceptable Source
                "as" if !value.is_empty() => {
                    acceptable_sources.push(value);
                }

                // Manifest Topic
                "mt" => {
                    manifest_topic = Some(value);
                }

                // Ignore unknown parameters
                _ => {}
            }
        }

        // info_hash is required
        let info_hash = info_hash.ok_or_else(|| {
            EngineError::protocol(
                ProtocolErrorKind::InvalidMagnet,
                "Missing or invalid 'xt' parameter (info hash)",
            )
        })?;

        Ok(MagnetUri {
            info_hash,
            display_name,
            trackers,
            web_seeds,
            exact_length,
            exact_source,
            keyword_topic,
            acceptable_sources,
            manifest_topic,
            original_uri: uri.to_string(),
        })
    }

    /// Get the info_hash as a hex string
    pub fn info_hash_hex(&self) -> String {
        self.info_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    /// Get the info_hash URL-encoded
    pub fn info_hash_urlencoded(&self) -> String {
        self.info_hash
            .iter()
            .map(|b| format!("%{:02X}", b))
            .collect()
    }

    /// Get display name or a default based on info_hash
    pub fn name(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| self.info_hash_hex())
    }

    /// Convert back to a magnet URI string
    pub fn to_uri(&self) -> String {
        let mut uri = String::from("magnet:?xt=urn:btih:");
        uri.push_str(&self.info_hash_hex());

        if let Some(ref name) = self.display_name {
            uri.push_str("&dn=");
            uri.push_str(&url_encode(name));
        }

        for tracker in &self.trackers {
            uri.push_str("&tr=");
            uri.push_str(&url_encode(tracker));
        }

        for ws in &self.web_seeds {
            uri.push_str("&ws=");
            uri.push_str(&url_encode(ws));
        }

        if let Some(len) = self.exact_length {
            uri.push_str("&xl=");
            uri.push_str(&len.to_string());
        }

        if let Some(ref src) = self.exact_source {
            uri.push_str("&xs=");
            uri.push_str(&url_encode(src));
        }

        uri
    }

    /// Check if this magnet has any trackers
    pub fn has_trackers(&self) -> bool {
        !self.trackers.is_empty()
    }

    /// Check if this is a "trackerless" magnet (relies on DHT)
    pub fn is_trackerless(&self) -> bool {
        self.trackers.is_empty()
    }
}

/// Parse a BitTorrent info hash from an xt parameter
///
/// Supports both hex (40 chars) and base32 (32 chars) formats
fn parse_btih(xt: &str) -> Option<Sha1Hash> {
    // Format: urn:btih:<hash>
    let hash_str = xt.strip_prefix("urn:btih:")?;

    // Valid hashes are ASCII hex/base32; multi-byte UTF-8 would panic the
    // byte-indexed slicing below (found by fuzz_magnet).
    if !hash_str.is_ascii() {
        return None;
    }

    match hash_str.len() {
        // 40-char hex encoding
        40 => {
            let bytes: Vec<u8> = (0..40)
                .step_by(2)
                .filter_map(|i| u8::from_str_radix(&hash_str[i..i + 2], 16).ok())
                .collect();

            if bytes.len() == 20 {
                let mut hash = [0u8; 20];
                hash.copy_from_slice(&bytes);
                Some(hash)
            } else {
                None
            }
        }

        // 32-char base32 encoding
        32 => base32_decode(hash_str),

        _ => None,
    }
}

/// Decode base32 (RFC 4648) to bytes
fn base32_decode(input: &str) -> Option<Sha1Hash> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

    let input = input.to_uppercase();
    let input = input.as_bytes();

    if input.len() != 32 {
        return None;
    }

    let mut bits = 0u64;
    let mut bit_count = 0u32;
    let mut output = Vec::with_capacity(20);

    for &c in input {
        let val = ALPHABET.iter().position(|&x| x == c)? as u64;
        bits = (bits << 5) | val;
        bit_count += 5;

        while bit_count >= 8 {
            bit_count -= 8;
            output.push((bits >> bit_count) as u8);
            bits &= (1 << bit_count) - 1;
        }
    }

    if output.len() == 20 {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&output);
        Some(hash)
    } else {
        None
    }
}

/// URL-decode a string with proper UTF-8 handling
fn url_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            // Try to parse hex escape
            let mut hex = String::with_capacity(2);
            if let Some(&h1) = chars.peek() {
                if h1.is_ascii_hexdigit() {
                    hex.push(chars.next().unwrap());
                    if let Some(&h2) = chars.peek() {
                        if h2.is_ascii_hexdigit() {
                            hex.push(chars.next().unwrap());
                        }
                    }
                }
            }
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    bytes.push(byte);
                    continue;
                }
            }
            // Invalid escape, keep as-is
            bytes.push(b'%');
            bytes.extend(hex.as_bytes());
        } else if c == '+' {
            bytes.push(b' ');
        } else {
            // Non-ASCII chars are multi-byte, so we need to encode them properly
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            bytes.extend(encoded.as_bytes());
        }
    }

    // Convert bytes to string, using lossy conversion for invalid UTF-8
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string())
}

/// URL-encode a string
fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);

    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                result.push(c);
            }
            _ => {
                for byte in c.to_string().as_bytes() {
                    result.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_magnet() {
        let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";
        let magnet = MagnetUri::parse(uri).unwrap();

        assert_eq!(
            magnet.info_hash_hex(),
            "0123456789abcdef0123456789abcdef01234567"
        );
        assert!(magnet.display_name.is_none());
        assert!(magnet.trackers.is_empty());
    }

    #[test]
    fn test_parse_full_magnet() {
        let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567\
                   &dn=Test+File\
                   &tr=http%3A%2F%2Ftracker.example.com%2Fannounce\
                   &tr=udp%3A%2F%2Ftracker2.example.com%3A6969";

        let magnet = MagnetUri::parse(uri).unwrap();

        assert_eq!(magnet.display_name, Some("Test File".to_string()));
        assert_eq!(magnet.trackers.len(), 2);
        assert_eq!(magnet.trackers[0], "http://tracker.example.com/announce");
        assert_eq!(magnet.trackers[1], "udp://tracker2.example.com:6969");
    }

    #[test]
    fn test_parse_base32_hash() {
        // Base32-encoded info hash (32 chars)
        let uri = "magnet:?xt=urn:btih:AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQT";
        let magnet = MagnetUri::parse(uri).unwrap();

        // Verify we got a valid hash
        assert_eq!(magnet.info_hash.len(), 20);
    }

    #[test]
    fn test_invalid_magnet() {
        // Missing scheme
        assert!(MagnetUri::parse("http://example.com").is_err());

        // Missing xt parameter
        assert!(MagnetUri::parse("magnet:?dn=Test").is_err());

        // Invalid hash length
        assert!(MagnetUri::parse("magnet:?xt=urn:btih:invalid").is_err());
    }

    #[test]
    fn test_to_uri() {
        let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Test";
        let magnet = MagnetUri::parse(uri).unwrap();

        let regenerated = magnet.to_uri();
        assert!(regenerated.contains("xt=urn:btih:0123456789abcdef0123456789abcdef01234567"));
        assert!(regenerated.contains("dn=Test"));
    }

    #[test]
    fn test_url_decode() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("test+test"), "test test");
        assert_eq!(url_decode("http%3A%2F%2Fexample.com"), "http://example.com");
    }

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("http://example.com"), "http%3A%2F%2Fexample.com");
        assert_eq!(url_encode("test-file_name.txt"), "test-file_name.txt");
    }

    #[test]
    fn test_name() {
        let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=MyFile";
        let magnet = MagnetUri::parse(uri).unwrap();
        assert_eq!(magnet.name(), "MyFile");

        let uri2 = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";
        let magnet2 = MagnetUri::parse(uri2).unwrap();
        assert_eq!(magnet2.name(), "0123456789abcdef0123456789abcdef01234567");
    }
}
