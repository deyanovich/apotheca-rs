// Meta file format per SPEC §6.3:
//
//     size <decimal>\n
//     sha256 <hex>\n
//
// "Implementations MUST reject meta files that do not match this grammar
// exactly." We enforce: single-space separators, lowercase hex, no leading
// zeros on the size, no extra trailing content.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub size: u64,
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaParseError;

impl fmt::Display for MetaParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed meta file")
    }
}

impl std::error::Error for MetaParseError {}

impl Meta {
    pub fn format(&self) -> String {
        let mut s = String::with_capacity(8 + 20 + 8 + 64 + 2);
        s.push_str("size ");
        s.push_str(&self.size.to_string());
        s.push('\n');
        s.push_str("sha256 ");
        s.push_str(&hex::encode(self.sha256));
        s.push('\n');
        s
    }

    pub fn parse(text: &str) -> Result<Self, MetaParseError> {
        let mut iter = text.split('\n');
        let size_line = iter.next().ok_or(MetaParseError)?;
        let sha_line = iter.next().ok_or(MetaParseError)?;
        let trailing = iter.next().ok_or(MetaParseError)?;
        if !trailing.is_empty() || iter.next().is_some() {
            return Err(MetaParseError);
        }

        let size_str = size_line.strip_prefix("size ").ok_or(MetaParseError)?;
        if !valid_decimal(size_str) {
            return Err(MetaParseError);
        }
        let size: u64 = size_str.parse().map_err(|_| MetaParseError)?;

        let sha_str = sha_line.strip_prefix("sha256 ").ok_or(MetaParseError)?;
        if sha_str.len() != 64 || !sha_str.bytes().all(is_lower_hex) {
            return Err(MetaParseError);
        }
        let mut sha = [0u8; 32];
        hex::decode_to_slice(sha_str, &mut sha).map_err(|_| MetaParseError)?;

        Ok(Meta { size, sha256: sha })
    }
}

fn is_lower_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

fn valid_decimal(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    // Reject leading zeros (except for "0" itself).
    if s.len() > 1 && s.starts_with('0') {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Meta {
        let mut sha = [0u8; 32];
        for (i, b) in sha.iter_mut().enumerate() {
            *b = i as u8;
        }
        Meta { size: 12345, sha256: sha }
    }

    #[test]
    fn round_trip() {
        let m = fixture();
        let s = m.format();
        let parsed = Meta::parse(&s).unwrap();
        assert_eq!(m, parsed);
    }

    #[test]
    fn format_shape() {
        let m = fixture();
        let s = m.format();
        assert!(s.starts_with("size 12345\n"));
        assert!(s.contains("\nsha256 "));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn rejects_missing_trailing_newline() {
        let s = "size 5\nsha256 0000000000000000000000000000000000000000000000000000000000000000";
        assert!(Meta::parse(s).is_err());
    }

    #[test]
    fn rejects_extra_trailing_content() {
        let s = "size 5\nsha256 0000000000000000000000000000000000000000000000000000000000000000\nextra\n";
        assert!(Meta::parse(s).is_err());
    }

    #[test]
    fn rejects_uppercase_hex() {
        let s = "size 5\nsha256 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n";
        assert!(Meta::parse(s).is_err());
    }

    #[test]
    fn rejects_short_hex() {
        let s = "size 5\nsha256 abcdef\n";
        assert!(Meta::parse(s).is_err());
    }

    #[test]
    fn rejects_double_space() {
        let s = "size  5\nsha256 0000000000000000000000000000000000000000000000000000000000000000\n";
        assert!(Meta::parse(s).is_err());
    }

    #[test]
    fn rejects_leading_zero_size() {
        let s = "size 05\nsha256 0000000000000000000000000000000000000000000000000000000000000000\n";
        assert!(Meta::parse(s).is_err());
    }

    #[test]
    fn accepts_zero_size() {
        let s = "size 0\nsha256 0000000000000000000000000000000000000000000000000000000000000000\n";
        let m = Meta::parse(s).unwrap();
        assert_eq!(m.size, 0);
    }

    #[test]
    fn rejects_swapped_order() {
        let s = "sha256 0000000000000000000000000000000000000000000000000000000000000000\nsize 5\n";
        assert!(Meta::parse(s).is_err());
    }

    #[test]
    fn rejects_crlf() {
        let s = "size 5\r\nsha256 0000000000000000000000000000000000000000000000000000000000000000\r\n";
        assert!(Meta::parse(s).is_err());
    }
}
