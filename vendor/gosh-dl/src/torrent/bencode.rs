//! Bencode Parser
//!
//! This module provides a custom bencode parser that preserves raw bytes
//! for info_hash calculation. While serde_bencode exists, we need raw
//! byte access to calculate SHA-1 hashes of the info dictionary.
//!
//! Bencode format:
//! - Integers:   `i<number>e`        Example: `i42e`
//! - Strings:    `<length>:<data>`   Example: `4:spam`
//! - Lists:      `l<items>e`         Example: `l4:spami42ee`
//! - Dicts:      `d<pairs>e`         Example: `d3:cow3:moo4:spam4:eggse`

use std::collections::BTreeMap;

/// Maximum allowed length for a bencode string (100 MiB)
/// This prevents malicious torrents from causing memory exhaustion
const MAX_STRING_LENGTH: usize = 100 * 1024 * 1024;

/// Maximum nesting depth for lists/dicts. Real BitTorrent structures nest a
/// handful of levels; deeply nested input is a stack-overflow attack.
const MAX_PARSE_DEPTH: usize = 64;
use std::fmt;

use crate::error::{EngineError, ProtocolErrorKind, Result};

/// A bencode value
#[derive(Clone, PartialEq, Eq)]
pub enum BencodeValue {
    /// Integer value (can be negative)
    Integer(i64),
    /// Byte string (not necessarily valid UTF-8)
    Bytes(Vec<u8>),
    /// List of values
    List(Vec<BencodeValue>),
    /// Dictionary with byte string keys (sorted by key)
    Dict(BTreeMap<Vec<u8>, BencodeValue>),
}

impl fmt::Debug for BencodeValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(n) => write!(f, "Integer({})", n),
            Self::Bytes(b) => {
                // Try to display as UTF-8 if valid, otherwise show hex
                if let Ok(s) = std::str::from_utf8(b) {
                    if s.len() <= 50 {
                        write!(f, "Bytes(\"{}\")", s)
                    } else {
                        // Truncate on a char boundary; byte 50 may fall inside
                        // a multi-byte character in untrusted input.
                        let cut = (0..=50).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
                        write!(f, "Bytes(\"{}...\" [{} bytes])", &s[..cut], b.len())
                    }
                } else {
                    write!(f, "Bytes([{} bytes])", b.len())
                }
            }
            Self::List(l) => f.debug_tuple("List").field(l).finish(),
            Self::Dict(d) => {
                let readable: BTreeMap<String, &BencodeValue> = d
                    .iter()
                    .map(|(k, v)| {
                        let key = String::from_utf8_lossy(k).to_string();
                        (key, v)
                    })
                    .collect();
                f.debug_tuple("Dict").field(&readable).finish()
            }
        }
    }
}

/// Result of parsing bencode, includes the remaining unparsed bytes
pub struct ParseResult<'a> {
    /// The parsed value
    pub value: BencodeValue,
    /// The remaining unparsed bytes
    pub remaining: &'a [u8],
}

impl BencodeValue {
    /// Parse bencode from bytes
    ///
    /// Returns the parsed value and remaining unparsed bytes.
    pub fn parse(data: &[u8]) -> Result<ParseResult<'_>> {
        Self::parse_at_depth(data, 0)
    }

    /// Parse bencode with nesting-depth tracking.
    ///
    /// Bencode arrives from untrusted sources (trackers, peers, .torrent
    /// files); without a depth limit, input like `"l" * 100_000` overflows
    /// the stack and aborts the process.
    fn parse_at_depth(data: &[u8], depth: usize) -> Result<ParseResult<'_>> {
        if depth > MAX_PARSE_DEPTH {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                format!("Nesting depth exceeds maximum of {MAX_PARSE_DEPTH}"),
            ));
        }
        if data.is_empty() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Empty input",
            ));
        }

        match data[0] {
            b'i' => Self::parse_integer(data),
            b'l' => Self::parse_list(data, depth),
            b'd' => Self::parse_dict(data, depth),
            b'0'..=b'9' => Self::parse_bytes(data),
            c => Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                format!("Invalid bencode type marker: {:?}", c as char),
            )),
        }
    }

    /// Parse a complete bencode value (ensuring no trailing data)
    pub fn parse_exact(data: &[u8]) -> Result<Self> {
        let result = Self::parse(data)?;
        if !result.remaining.is_empty() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                format!("Trailing data: {} bytes", result.remaining.len()),
            ));
        }
        Ok(result.value)
    }

    /// Parse an integer: i<number>e
    fn parse_integer(data: &[u8]) -> Result<ParseResult<'_>> {
        if data.is_empty() || data[0] != b'i' {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Expected integer",
            ));
        }

        let end = data[1..].iter().position(|&c| c == b'e').ok_or_else(|| {
            EngineError::protocol(ProtocolErrorKind::BencodeParse, "Unterminated integer")
        })? + 1; // Add 1 because we started at index 1

        let num_str = std::str::from_utf8(&data[1..end]).map_err(|_| {
            EngineError::protocol(ProtocolErrorKind::BencodeParse, "Invalid integer encoding")
        })?;

        // Check for invalid formats: leading zeros (except for "0"), negative zero
        if num_str.len() > 1 && num_str.starts_with('0') {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Invalid integer: leading zero",
            ));
        }
        if num_str == "-0" {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Invalid integer: negative zero",
            ));
        }
        if num_str.starts_with("-0") && num_str.len() > 2 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Invalid integer: leading zero after minus",
            ));
        }

        let value = num_str.parse::<i64>().map_err(|_| {
            EngineError::protocol(ProtocolErrorKind::BencodeParse, "Integer parse error")
        })?;

        Ok(ParseResult {
            value: BencodeValue::Integer(value),
            remaining: &data[end + 1..], // Skip the 'e'
        })
    }

    /// Parse a byte string: <length>:<data>
    fn parse_bytes(data: &[u8]) -> Result<ParseResult<'_>> {
        let colon = data.iter().position(|&c| c == b':').ok_or_else(|| {
            EngineError::protocol(ProtocolErrorKind::BencodeParse, "Expected colon in string")
        })?;

        let len_str = std::str::from_utf8(&data[..colon]).map_err(|_| {
            EngineError::protocol(ProtocolErrorKind::BencodeParse, "Invalid string length")
        })?;

        let len = len_str.parse::<usize>().map_err(|_| {
            EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Invalid string length number",
            )
        })?;

        // Prevent memory exhaustion attacks with extremely large strings
        if len > MAX_STRING_LENGTH {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                format!(
                    "String length {} exceeds maximum allowed {} bytes",
                    len, MAX_STRING_LENGTH
                ),
            ));
        }

        let start = colon + 1;
        let end = start + len;

        if end > data.len() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                format!(
                    "String length {} exceeds available data {}",
                    len,
                    data.len() - start
                ),
            ));
        }

        Ok(ParseResult {
            value: BencodeValue::Bytes(data[start..end].to_vec()),
            remaining: &data[end..],
        })
    }

    /// Parse a list: l<items>e
    fn parse_list(data: &[u8], depth: usize) -> Result<ParseResult<'_>> {
        if data.is_empty() || data[0] != b'l' {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Expected list",
            ));
        }

        let mut items = Vec::new();
        let mut remaining = &data[1..]; // Skip 'l'

        while !remaining.is_empty() && remaining[0] != b'e' {
            let result = Self::parse_at_depth(remaining, depth + 1)?;
            items.push(result.value);
            remaining = result.remaining;
        }

        if remaining.is_empty() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Unterminated list",
            ));
        }

        Ok(ParseResult {
            value: BencodeValue::List(items),
            remaining: &remaining[1..], // Skip 'e'
        })
    }

    /// Parse a dictionary: d<pairs>e
    fn parse_dict(data: &[u8], depth: usize) -> Result<ParseResult<'_>> {
        if data.is_empty() || data[0] != b'd' {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Expected dict",
            ));
        }

        let mut items = BTreeMap::new();
        let mut remaining = &data[1..]; // Skip 'd'
        let mut last_key: Option<Vec<u8>> = None;

        while !remaining.is_empty() && remaining[0] != b'e' {
            // Parse key (must be a string)
            let key_result = Self::parse_bytes(remaining)?;
            let key = match key_result.value {
                BencodeValue::Bytes(k) => k,
                _ => {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::BencodeParse,
                        "Dict key must be a string",
                    ))
                }
            };

            // Keys must be in sorted order
            if let Some(ref lk) = last_key {
                if &key <= lk {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::BencodeParse,
                        "Dict keys not in sorted order",
                    ));
                }
            }
            last_key = Some(key.clone());

            remaining = key_result.remaining;

            // Parse value
            let value_result = Self::parse_at_depth(remaining, depth + 1)?;
            items.insert(key, value_result.value);
            remaining = value_result.remaining;
        }

        if remaining.is_empty() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::BencodeParse,
                "Unterminated dict",
            ));
        }

        Ok(ParseResult {
            value: BencodeValue::Dict(items),
            remaining: &remaining[1..], // Skip 'e'
        })
    }

    /// Encode to bencode bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.encode_to(&mut buf);
        buf
    }

    /// Encode to an existing buffer
    pub fn encode_to(&self, buf: &mut Vec<u8>) {
        match self {
            Self::Integer(n) => {
                buf.push(b'i');
                buf.extend_from_slice(n.to_string().as_bytes());
                buf.push(b'e');
            }
            Self::Bytes(b) => {
                buf.extend_from_slice(b.len().to_string().as_bytes());
                buf.push(b':');
                buf.extend_from_slice(b);
            }
            Self::List(l) => {
                buf.push(b'l');
                for item in l {
                    item.encode_to(buf);
                }
                buf.push(b'e');
            }
            Self::Dict(d) => {
                buf.push(b'd');
                for (k, v) in d {
                    // Encode key as string
                    buf.extend_from_slice(k.len().to_string().as_bytes());
                    buf.push(b':');
                    buf.extend_from_slice(k);
                    // Encode value
                    v.encode_to(buf);
                }
                buf.push(b'e');
            }
        }
    }

    // Accessor methods

    /// Get as string (UTF-8)
    pub fn as_string(&self) -> Option<&str> {
        match self {
            Self::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    /// Get as integer
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Integer(n) => Some(*n),
            _ => None,
        }
    }

    /// Get as unsigned integer
    pub fn as_uint(&self) -> Option<u64> {
        match self {
            Self::Integer(n) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }

    /// Get as bytes
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Get as list
    pub fn as_list(&self) -> Option<&[BencodeValue]> {
        match self {
            Self::List(l) => Some(l),
            _ => None,
        }
    }

    /// Get as mutable list
    pub fn as_list_mut(&mut self) -> Option<&mut Vec<BencodeValue>> {
        match self {
            Self::List(l) => Some(l),
            _ => None,
        }
    }

    /// Get as dict
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, BencodeValue>> {
        match self {
            Self::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Get as mutable dict
    pub fn as_dict_mut(&mut self) -> Option<&mut BTreeMap<Vec<u8>, BencodeValue>> {
        match self {
            Self::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Get dict value by key
    pub fn get(&self, key: &str) -> Option<&BencodeValue> {
        match self {
            Self::Dict(d) => d.get(key.as_bytes()),
            _ => None,
        }
    }

    /// Get dict value by key (byte key)
    pub fn get_bytes(&self, key: &[u8]) -> Option<&BencodeValue> {
        match self {
            Self::Dict(d) => d.get(key),
            _ => None,
        }
    }

    /// Check if this is a dict
    pub fn is_dict(&self) -> bool {
        matches!(self, Self::Dict(_))
    }

    /// Check if this is a list
    pub fn is_list(&self) -> bool {
        matches!(self, Self::List(_))
    }

    /// Check if this is a string/bytes
    pub fn is_bytes(&self) -> bool {
        matches!(self, Self::Bytes(_))
    }

    /// Check if this is an integer
    pub fn is_int(&self) -> bool {
        matches!(self, Self::Integer(_))
    }
}

/// Find the raw bytes of the "info" dictionary in a torrent file
///
/// This is needed to calculate the info_hash (SHA-1 of the raw info dict)
pub fn find_info_dict_bytes(data: &[u8]) -> Result<&[u8]> {
    // First, parse to validate the structure
    let root = BencodeValue::parse_exact(data)?;
    let dict = root.as_dict().ok_or_else(|| {
        EngineError::protocol(ProtocolErrorKind::InvalidTorrent, "Root is not a dict")
    })?;

    if !dict.contains_key(b"info".as_slice()) {
        return Err(EngineError::protocol(
            ProtocolErrorKind::InvalidTorrent,
            "Missing 'info' key",
        ));
    }

    // Now find the raw bytes of the info dict
    // We need to find "4:info" followed by a dict
    let info_key = b"4:info";

    // Find the position of "4:info" in the data
    let mut pos = 0;
    while pos < data.len() {
        if data[pos..].starts_with(info_key) {
            let info_start = pos + info_key.len();
            if info_start < data.len() && data[info_start] == b'd' {
                // Parse from here to find the end
                let result = BencodeValue::parse(&data[info_start..])?;
                let info_len = data.len() - info_start - result.remaining.len();
                return Ok(&data[info_start..info_start + info_len]);
            }
        }
        pos += 1;
    }

    Err(EngineError::protocol(
        ProtocolErrorKind::InvalidTorrent,
        "Could not locate info dict bytes",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_depth_limit() {
        // Deeply nested lists must error, not overflow the stack.
        let mut evil = vec![b'l'; 100_000];
        evil.extend(vec![b'e'; 100_000]);
        assert!(BencodeValue::parse(&evil).is_err());

        // Same for dicts nested inside lists.
        let mut evil = Vec::new();
        for _ in 0..50_000 {
            evil.extend_from_slice(b"d1:a");
        }
        assert!(BencodeValue::parse(&evil).is_err());

        // Nesting within the limit still parses.
        let mut ok = vec![b'l'; 32];
        ok.extend(vec![b'e'; 32]);
        assert!(BencodeValue::parse(&ok).is_ok());
    }

    #[test]
    fn test_debug_truncates_on_char_boundary() {
        // 48 ASCII bytes then a multi-byte char straddling byte 50.
        let mut s = "a".repeat(48);
        s.push('€'); // 3 bytes: 48..51
        let value = BencodeValue::Bytes(s.into_bytes());
        let _ = format!("{value:?}"); // must not panic
    }

    #[test]
    fn test_parse_integer() {
        let result = BencodeValue::parse(b"i42e").unwrap();
        assert_eq!(result.value, BencodeValue::Integer(42));
        assert!(result.remaining.is_empty());

        let result = BencodeValue::parse(b"i-42e").unwrap();
        assert_eq!(result.value, BencodeValue::Integer(-42));

        let result = BencodeValue::parse(b"i0e").unwrap();
        assert_eq!(result.value, BencodeValue::Integer(0));

        // Invalid: leading zero
        assert!(BencodeValue::parse(b"i03e").is_err());

        // Invalid: negative zero
        assert!(BencodeValue::parse(b"i-0e").is_err());
    }

    #[test]
    fn test_parse_bytes() {
        let result = BencodeValue::parse(b"4:spam").unwrap();
        assert_eq!(result.value, BencodeValue::Bytes(b"spam".to_vec()));
        assert!(result.remaining.is_empty());

        let result = BencodeValue::parse(b"0:").unwrap();
        assert_eq!(result.value, BencodeValue::Bytes(vec![]));

        // Binary data
        let data = b"5:\x00\x01\x02\x03\x04";
        let result = BencodeValue::parse(data).unwrap();
        assert_eq!(result.value, BencodeValue::Bytes(vec![0, 1, 2, 3, 4]));
    }

    #[test]
    fn test_parse_list() {
        let result = BencodeValue::parse(b"l4:spami42ee").unwrap();
        if let BencodeValue::List(items) = result.value {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0], BencodeValue::Bytes(b"spam".to_vec()));
            assert_eq!(items[1], BencodeValue::Integer(42));
        } else {
            panic!("Expected list");
        }

        // Empty list
        let result = BencodeValue::parse(b"le").unwrap();
        assert_eq!(result.value, BencodeValue::List(vec![]));

        // Nested list
        let result = BencodeValue::parse(b"ll4:spamee").unwrap();
        if let BencodeValue::List(items) = result.value {
            assert_eq!(items.len(), 1);
            assert!(matches!(&items[0], BencodeValue::List(_)));
        } else {
            panic!("Expected list");
        }
    }

    #[test]
    fn test_parse_dict() {
        let result = BencodeValue::parse(b"d3:cow3:moo4:spam4:eggse").unwrap();
        if let BencodeValue::Dict(d) = &result.value {
            assert_eq!(d.len(), 2);
            assert_eq!(
                d.get(b"cow".as_slice()),
                Some(&BencodeValue::Bytes(b"moo".to_vec()))
            );
            assert_eq!(
                d.get(b"spam".as_slice()),
                Some(&BencodeValue::Bytes(b"eggs".to_vec()))
            );
        } else {
            panic!("Expected dict");
        }

        // Empty dict
        let result = BencodeValue::parse(b"de").unwrap();
        assert_eq!(result.value, BencodeValue::Dict(BTreeMap::new()));
    }

    #[test]
    fn test_encode() {
        // Integer
        let v = BencodeValue::Integer(42);
        assert_eq!(v.encode(), b"i42e");

        // String
        let v = BencodeValue::Bytes(b"spam".to_vec());
        assert_eq!(v.encode(), b"4:spam");

        // List
        let v = BencodeValue::List(vec![
            BencodeValue::Bytes(b"spam".to_vec()),
            BencodeValue::Integer(42),
        ]);
        assert_eq!(v.encode(), b"l4:spami42ee");

        // Dict
        let mut d = BTreeMap::new();
        d.insert(b"cow".to_vec(), BencodeValue::Bytes(b"moo".to_vec()));
        d.insert(b"spam".to_vec(), BencodeValue::Bytes(b"eggs".to_vec()));
        let v = BencodeValue::Dict(d);
        assert_eq!(v.encode(), b"d3:cow3:moo4:spam4:eggse");
    }

    #[test]
    fn test_roundtrip() {
        // Simple roundtrip test with nested structures
        // Dict with: "items" (list), "name" (string), "value" (integer)
        let original = b"d5:itemsli1ei2ei3ee4:name4:test5:valuei42ee";

        let value = BencodeValue::parse_exact(original).unwrap();
        let encoded = value.encode();
        assert_eq!(encoded, original.to_vec());

        // Verify the parsed structure
        assert_eq!(value.get("name").and_then(|v| v.as_string()), Some("test"));
        assert_eq!(value.get("value").and_then(|v| v.as_int()), Some(42));
        assert_eq!(
            value
                .get("items")
                .and_then(|v| v.as_list())
                .map(|l| l.len()),
            Some(3)
        );
    }

    #[test]
    fn test_accessor_methods() {
        // Keys must be in sorted byte order: "list" < "name" < "num"
        let data = b"d4:listli1ei2ei3ee4:name4:test3:numi42ee";
        let value = BencodeValue::parse_exact(data).unwrap();

        assert_eq!(value.get("num").and_then(|v| v.as_int()), Some(42));
        assert_eq!(value.get("name").and_then(|v| v.as_string()), Some("test"));
        assert_eq!(
            value.get("list").and_then(|v| v.as_list()).map(|l| l.len()),
            Some(3)
        );
        assert!(value.get("missing").is_none());
    }
}
