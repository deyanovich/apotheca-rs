// Name validation per SPEC §4.1.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    ContainsSlash,
    ContainsNul,
    DotOrDotDot,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => f.write_str("name is empty"),
            NameError::TooLong => f.write_str("name exceeds 255 octets"),
            NameError::ContainsSlash => f.write_str("name contains '/'"),
            NameError::ContainsNul => f.write_str("name contains NUL"),
            NameError::DotOrDotDot => f.write_str("name is '.' or '..'"),
        }
    }
}

impl std::error::Error for NameError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Name<'a>(&'a [u8]);

impl<'a> Name<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, NameError> {
        if bytes.is_empty() {
            return Err(NameError::Empty);
        }
        if bytes.len() > 255 {
            return Err(NameError::TooLong);
        }
        if bytes.contains(&b'/') {
            return Err(NameError::ContainsSlash);
        }
        if bytes.contains(&0u8) {
            return Err(NameError::ContainsNul);
        }
        if bytes == b"." || bytes == b".." {
            return Err(NameError::DotOrDotDot);
        }
        Ok(Name(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert_eq!(Name::new(b""), Err(NameError::Empty));
    }

    #[test]
    fn rejects_too_long() {
        let long = vec![b'a'; 256];
        assert_eq!(Name::new(&long), Err(NameError::TooLong));
    }

    #[test]
    fn accepts_max_length() {
        let just_right = vec![b'a'; 255];
        assert!(Name::new(&just_right).is_ok());
    }

    #[test]
    fn rejects_slash() {
        assert_eq!(Name::new(b"a/b"), Err(NameError::ContainsSlash));
    }

    #[test]
    fn rejects_nul() {
        assert_eq!(Name::new(b"a\0b"), Err(NameError::ContainsNul));
    }

    #[test]
    fn rejects_dot_dotdot() {
        assert_eq!(Name::new(b"."), Err(NameError::DotOrDotDot));
        assert_eq!(Name::new(b".."), Err(NameError::DotOrDotDot));
    }

    #[test]
    fn accepts_dotted_names() {
        assert!(Name::new(b"...").is_ok());
        assert!(Name::new(b".hidden").is_ok());
        assert!(Name::new(b"foo.txt").is_ok());
    }

    #[test]
    fn accepts_arbitrary_octets() {
        assert!(Name::new(&[0xff, 0xfe, 0xfd]).is_ok());
    }
}
