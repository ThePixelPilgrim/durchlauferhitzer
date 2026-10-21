//! Strict validation of the HTTP request target (origin-form only).
//!
//! Stage 2 percent-decodes accepted paths exactly once, so anything that
//! could decode into traversal is rejected here.

/// A validated origin-form request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: String,
    pub query: Option<String>,
}

/// Why a request target was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetError {
    NotOriginForm,
    Empty,
    DotSegment,
    EmptySegment,
    EncodedDot,
    EncodedSlash,
    EncodedBackslash,
    EncodedNul,
    BadPercentEncoding,
    Backslash,
    NonAscii,
    ControlOrSpace,
    Fragment,
}

/// Parses an origin-form request target (`/path?query`), splitting at the
/// first `?`.
pub fn parse_target(raw: &[u8]) -> Result<Target, TargetError> {
    let (path, query) = match raw.iter().position(|&b| b == b'?') {
        Some(i) => (raw.get(..i).unwrap_or_default(), raw.get(i + 1..)),
        None => (raw, None),
    };
    check_path(path)?;
    if let Some(query) = query {
        check_bytes(query)?;
    }
    Ok(Target {
        path: to_string(path),
        query: query.map(to_string),
    })
}

/// Validates a path (no query part), e.g. a configured `path` block argument.
pub fn validate_path(path: &str) -> Result<(), TargetError> {
    check_path(path.as_bytes())
}

/// Checked bytes are ASCII, so this never loses data.
fn to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn check_path(path: &[u8]) -> Result<(), TargetError> {
    match path.first() {
        None => return Err(TargetError::Empty),
        Some(b'/') => {}
        Some(_) => return Err(TargetError::NotOriginForm),
    }
    check_bytes(path)?;
    // Skip the part before the leading slash; a trailing slash leaves a
    // final empty segment, which is fine.
    let mut segments = path.split(|&b| b == b'/').skip(1).peekable();
    while let Some(segment) = segments.next() {
        if segment.is_empty() {
            if segments.peek().is_some() {
                return Err(TargetError::EmptySegment);
            }
        } else if segment == b"." || segment == b".." {
            return Err(TargetError::DotSegment);
        }
    }
    Ok(())
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Byte-class and percent-escape rules shared by path and query.
fn check_bytes(bytes: &[u8]) -> Result<(), TargetError> {
    let mut iter = bytes.iter();
    while let Some(&b) = iter.next() {
        match b {
            0x80..=0xFF => return Err(TargetError::NonAscii),
            0x00..=0x20 | 0x7F => return Err(TargetError::ControlOrSpace),
            b'\\' => return Err(TargetError::Backslash),
            b'#' => return Err(TargetError::Fragment),
            b'%' => {
                let hi = iter.next().and_then(|&h| hex_value(h));
                let lo = iter.next().and_then(|&l| hex_value(l));
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err(TargetError::BadPercentEncoding);
                };
                match hi * 16 + lo {
                    0x2E => return Err(TargetError::EncodedDot),
                    0x2F => return Err(TargetError::EncodedSlash),
                    0x5C => return Err(TargetError::EncodedBackslash),
                    0x00 => return Err(TargetError::EncodedNul),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn rejects_hostile_targets() {
        let rows: &[(&[u8], TargetError)] = &[
            (b"", TargetError::Empty),
            (b"http://x/", TargetError::NotOriginForm),
            (b"example.com:80", TargetError::NotOriginForm),
            (b"*", TargetError::NotOriginForm),
            (b"/a/../b", TargetError::DotSegment),
            (b"/a/.", TargetError::DotSegment),
            (b"/..", TargetError::DotSegment),
            (b"/a//b", TargetError::EmptySegment),
            (b"//", TargetError::EmptySegment),
            (b"/a/%2e%2E/b", TargetError::EncodedDot),
            (b"/a/%2E", TargetError::EncodedDot),
            (b"/%2f", TargetError::EncodedSlash),
            (b"/%2F", TargetError::EncodedSlash),
            (b"/%5c", TargetError::EncodedBackslash),
            (b"/%5C", TargetError::EncodedBackslash),
            (b"/%00", TargetError::EncodedNul),
            (b"/%zz", TargetError::BadPercentEncoding),
            (b"/%4", TargetError::BadPercentEncoding),
            (b"/%", TargetError::BadPercentEncoding),
            (b"/a\\b", TargetError::Backslash),
            (b"/\xc3\xa4", TargetError::NonAscii),
            (b"/a b", TargetError::ControlOrSpace),
            (b"/a\x00b", TargetError::ControlOrSpace),
            (b"/a\x7fb", TargetError::ControlOrSpace),
            (b"/a#frag", TargetError::Fragment),
            (b"/a?q=1#frag", TargetError::Fragment),
            (b"/a?q=\xc3\xa4", TargetError::NonAscii),
            (b"/a?q=a b", TargetError::ControlOrSpace),
            (b"/a?q=a\\b", TargetError::Backslash),
            (b"/a?q=%zz", TargetError::BadPercentEncoding),
        ];
        for (raw, want) in rows {
            assert_eq!(
                parse_target(raw),
                Err(*want),
                "{:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn accepts_ordinary_targets() {
        assert_eq!(
            parse_target(b"/"),
            Ok(Target {
                path: "/".into(),
                query: None
            })
        );
        assert_eq!(
            parse_target(b"/api/users?id=1&x=%20"),
            Ok(Target {
                path: "/api/users".into(),
                query: Some("id=1&x=%20".into())
            })
        );
        assert_eq!(
            parse_target(b"/api/"),
            Ok(Target {
                path: "/api/".into(),
                query: None
            })
        );
        assert_eq!(
            parse_target(b"/a.b/..c/c.."),
            Ok(Target {
                path: "/a.b/..c/c..".into(),
                query: None
            })
        );
        assert_eq!(
            parse_target(b"/a?"),
            Ok(Target {
                path: "/a".into(),
                query: Some(String::new())
            })
        );
        // Encoded percent is decoded once downstream, so it is harmless.
        assert!(parse_target(b"/%252e%252e/").is_ok());
        assert!(parse_target(b"/a?q=../..//").is_ok());
    }

    #[test]
    fn validate_path_checks_path_only() {
        assert_eq!(validate_path("/a/../"), Err(TargetError::DotSegment));
        assert_eq!(validate_path("x"), Err(TargetError::NotOriginForm));
        assert_eq!(validate_path("/a?b"), Ok(()));
    }

    fn decode(path: &str) -> Vec<u8> {
        let b = path.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            if c == b'%' {
                let hex = path.get(i + 1..i + 3).unwrap_or("");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                i += 3;
            } else {
                out.push(c);
                i += 1;
            }
        }
        out
    }

    fn check_accepted(raw: &[u8]) {
        if let Ok(t) = parse_target(raw) {
            let d = decode(&t.path);
            assert!(!d.contains(&0) && !d.contains(&b'\\'), "{raw:?}");
            assert!(!d.windows(2).any(|w| w == b"//"), "{raw:?}");
            for seg in d.split(|&c| c == b'/') {
                assert!(seg != b"." && seg != b"..", "{raw:?}");
            }
        }
    }

    proptest! {
        #[test]
        fn accepted_paths_never_contain_dot_segments_after_decoding(
            raw in proptest::collection::vec(any::<u8>(), 0..64)
        ) {
            check_accepted(&raw);
        }

        #[test]
        fn accepted_paths_from_hostile_alphabet(
            raw in proptest::collection::vec(
                proptest::sample::select(
                    vec![b'/', b'.', b'%', b'2', b'e', b'E', b'f', b'5', b'c', b'0', b'a', b'?']
                ),
                0..24,
            )
        ) {
            let mut v = vec![b'/'];
            v.extend(raw);
            check_accepted(&v);
        }
    }
}
