//! Strict validation of the HTTP request target (origin-form only).
//!
//! Stage 2 percent-decodes accepted paths exactly once, so anything that
//! could decode into traversal is rejected here. Escapes of unreserved
//! characters are rejected too, so they have exactly one spelling (defence
//! in depth: routing compares decoded segments anyway, since reserved
//! characters and hex case still allow several spellings). Forwarding uses
//! the raw path as received.

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
    /// An escape for an unreserved character other than `.` (ALPHA, DIGIT,
    /// `-`, `_`, `~`): it must be written plainly, so each path has one
    /// spelling.
    EncodedUnreserved,
    EncodedSlash,
    EncodedBackslash,
    EncodedNul,
    BadPercentEncoding,
    Backslash,
    NonAscii,
    ControlOrSpace,
    Fragment,
    Semicolon,
    EncodedControl,
    QueryInPath,
}

/// Parses an origin-form request target (`/path?query`), splitting at the
/// first `?`.
///
/// Accepted paths may still percent-decode to non-ASCII bytes (`%80`-`%FF`,
/// e.g. UTF-8 file names); the raw path, not the decoded one, is what gets
/// logged.
pub fn parse_target(raw: &[u8]) -> Result<Target, TargetError> {
    let (path, query) = match raw.iter().position(|&b| b == b'?') {
        Some(i) => (raw.get(..i).unwrap_or_default(), raw.get(i + 1..)),
        None => (raw, None),
    };
    check_path(path)?;
    if let Some(query) = query {
        check_bytes(query, false)?;
    }
    Ok(Target {
        path: to_string(path),
        query: query.map(to_string),
    })
}

/// Validates a path (no query part), e.g. a configured `path` block argument.
pub fn validate_path(path: &str) -> Result<(), TargetError> {
    check_path(path.as_bytes())?;
    if path.contains('?') {
        return Err(TargetError::QueryInPath);
    }
    Ok(())
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
    check_bytes(path, true)?;
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

pub(crate) fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Byte-class and well-formed-escape rules for path and query. Only the path
/// additionally rejects `;` (raw or as `%3B`), escapes that decode to dangerous bytes and
/// escapes of unreserved characters (RFC 3986 section 2.3).
fn check_bytes(bytes: &[u8], is_path: bool) -> Result<(), TargetError> {
    let mut iter = bytes.iter();
    while let Some(&b) = iter.next() {
        match b {
            0x80..=0xFF => return Err(TargetError::NonAscii),
            0x00..=0x20 | 0x7F => return Err(TargetError::ControlOrSpace),
            b'\\' => return Err(TargetError::Backslash),
            b'#' => return Err(TargetError::Fragment),
            b';' if is_path => return Err(TargetError::Semicolon),
            b'%' => {
                let hi = iter.next().and_then(|&h| hex_value(h));
                let lo = iter.next().and_then(|&l| hex_value(l));
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err(TargetError::BadPercentEncoding);
                };
                if !is_path {
                    continue;
                }
                match hi * 16 + lo {
                    0x01..=0x1F | 0x7F => return Err(TargetError::EncodedControl),
                    0x2E => return Err(TargetError::EncodedDot),
                    0x3B => return Err(TargetError::Semicolon),
                    b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' | b'_' | b'~' => {
                        return Err(TargetError::EncodedUnreserved);
                    }
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
            (b"/public/..;/admin", TargetError::Semicolon),
            (b"/a;b", TargetError::Semicolon),
            // An encoded `;` is a `;` once decoded: same defence as `..;/`.
            (b"/a%3Bb", TargetError::Semicolon),
            (b"/a%3bb", TargetError::Semicolon),
            (b"/public/..%3B/admin", TargetError::Semicolon),
            (b"/a%0a", TargetError::EncodedControl),
            (b"/a%1B", TargetError::EncodedControl),
            (b"/a%1b", TargetError::EncodedControl),
            (b"/a%01", TargetError::EncodedControl),
            (b"/a%7f", TargetError::EncodedControl),
            (b"/a%7F", TargetError::EncodedControl),
            // Unreserved characters have exactly one spelling in a path
            // (defence in depth: routing compares decoded segments).
            (b"/%61ssets/a.css", TargetError::EncodedUnreserved),
            (b"/%41", TargetError::EncodedUnreserved),
            (b"/a%7a", TargetError::EncodedUnreserved),
            (b"/a%7A", TargetError::EncodedUnreserved),
            (b"/%7E", TargetError::EncodedUnreserved),
            (b"/%7e", TargetError::EncodedUnreserved),
            (b"/%2D", TargetError::EncodedUnreserved),
            (b"/%2d", TargetError::EncodedUnreserved),
            (b"/%5F", TargetError::EncodedUnreserved),
            (b"/%5f", TargetError::EncodedUnreserved),
            (b"/%30", TargetError::EncodedUnreserved),
            (b"/%39", TargetError::EncodedUnreserved),
            (b"/x/%30/y", TargetError::EncodedUnreserved),
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
        // The query only gets byte-class and well-formed-escape rules.
        for ok in [
            &b"/a?next=%2Fhome"[..],
            b"/a?x=%2e%00%5C",
            b"/a?x=1;y=2",
            b"/a?x=%3B",
            b"/a?x=%3b;",
            b"/%C3%A4",
            b"/%80%ff",
            // Reserved and other characters may stay encoded.
            b"/a%20b",
            b"/a%3Fb",
            b"/a%25",
            b"/a%40%21%2C",
            // The query is not canonicalised.
            b"/a?x=%61%7E%2D%5F%30",
        ] {
            assert!(
                parse_target(ok).is_ok(),
                "{:?}",
                String::from_utf8_lossy(ok)
            );
        }
    }

    #[test]
    fn validate_path_checks_path_only() {
        assert_eq!(validate_path("/a/../"), Err(TargetError::DotSegment));
        assert_eq!(validate_path("x"), Err(TargetError::NotOriginForm));
        assert_eq!(validate_path("/a?b"), Err(TargetError::QueryInPath));
        assert_eq!(validate_path("/a;b"), Err(TargetError::Semicolon));
        assert_eq!(validate_path("/a%3Bb"), Err(TargetError::Semicolon));
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

    fn is_unreserved(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
    }

    fn check_accepted(raw: &[u8]) {
        if let Ok(t) = parse_target(raw) {
            // No escape decodes to an unreserved character. Routing compares
            // decoded segments, so this is defence in depth, not what makes
            // routing sound.
            let p = t.path.as_bytes();
            for (i, _) in p.iter().enumerate().filter(|&(_, &c)| c == b'%') {
                let hex = t.path.get(i + 1..i + 3).unwrap_or("");
                let decoded = u8::from_str_radix(hex, 16).unwrap_or(b'%');
                assert!(!is_unreserved(decoded), "{raw:?}");
            }
            let d = decode(&t.path);
            assert!(!d.contains(&b'\\') && !d.contains(&b';'), "{raw:?}");
            assert!(d.iter().all(|&c| c > 0x1F && c != 0x7F), "{raw:?}");
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
                    vec![b'/', b'.', b'%', b'2', b'e', b'E', b'f', b'F', b'5', b'c', b'C',
                         b'0', b'a', b'?', b';', b'6', b'7', b'D', b'3']
                ),
                0..24,
            )
        ) {
            let mut v = vec![b'/'];
            v.extend(raw);
            check_accepted(&v);
        }

        #[test]
        fn accepted_paths_from_mixed_fragments(
            parts in proptest::collection::vec(
                prop_oneof![
                    proptest::sample::select(vec![
                        "/", ".", "..", "a", "b.c", "%2e", "%2E", "%2f", "%5c", "%00", "%25",
                        "%0a", "%7f", "%C3%A4", "%41", ";", "?q=1", "%61", "%7E", "%7e",
                        "%2D", "%5F", "%30", "%20", "%3f", "%3B", "%3b", "%40",
                    ])
                    .prop_map(str::to_string),
                    "[ -~]{0,3}",
                ],
                0..10,
            )
        ) {
            let mut v = b"/".to_vec();
            for p in parts {
                v.extend(p.bytes());
            }
            check_accepted(&v);
        }
    }
}
