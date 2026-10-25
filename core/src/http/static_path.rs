//! Decoding of validated request paths into relative file-name segments.

/// Why a path could not be turned into relative segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticPathError {
    /// A decoded segment starts with `.` (hidden files and traversal).
    DotSegment,
    /// A `%` escape is not followed by two hex digits. Paths that passed
    /// `parse_target` never contain one.
    BadEscape,
    /// The path is not usable as a file path: it does not start with `/`,
    /// has an empty non-trailing segment, or a decoded segment contains `/`
    /// or NUL.
    BadSegment,
}

/// Whether a path ends in `/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailingSlash {
    Present,
    Absent,
}

/// Returns whether `path` ends in a slash.
pub fn trailing_slash(path: &str) -> TrailingSlash {
    if path.ends_with('/') {
        TrailingSlash::Present
    } else {
        TrailingSlash::Absent
    }
}

/// Skips the route's first `skip` segments, percent-decodes each remaining
/// segment exactly once and drops the empty trailing segment.
///
/// Safe on arbitrary input: the path must start with `/`, may have only a
/// trailing empty segment, and decoded segments must not start with `.` or
/// contain `/` or NUL. The only precondition is that `path` is a path
/// without a query (`?` is treated as an ordinary byte).
pub fn relative_segments(path: &str, skip: usize) -> Result<Vec<Vec<u8>>, StaticPathError> {
    let rest = path.strip_prefix('/').ok_or(StaticPathError::BadSegment)?;
    let mut out = Vec::new();
    let mut segments = rest.split('/').enumerate().peekable();
    while let Some((index, segment)) = segments.next() {
        if segment.is_empty() {
            if segments.peek().is_some() {
                return Err(StaticPathError::BadSegment);
            }
            break;
        }
        if index < skip {
            continue;
        }
        let decoded = decode(segment)?;
        if decoded.first() == Some(&b'.') {
            return Err(StaticPathError::DotSegment);
        }
        if decoded.iter().any(|&b| b == b'/' || b == 0) {
            return Err(StaticPathError::BadSegment);
        }
        out.push(decoded);
    }
    Ok(out)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode(segment: &str) -> Result<Vec<u8>, StaticPathError> {
    let mut out = Vec::with_capacity(segment.len());
    let mut iter = segment.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = iter.next().and_then(hex_value);
            let lo = iter.next().and_then(hex_value);
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(StaticPathError::BadEscape);
            };
            out.push(hi * 16 + lo);
        } else {
            out.push(b);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_skips() {
        assert_eq!(
            relative_segments("/assets/a%20b/c.css", 1),
            Ok(vec![b"a b".to_vec(), b"c.css".to_vec()])
        );
        assert_eq!(relative_segments("/", 0), Ok(vec![]));
        assert_eq!(relative_segments("/a/", 0), Ok(vec![b"a".to_vec()]));
        assert_eq!(relative_segments("/%C3%A4", 0), Ok(vec![vec![0xC3, 0xA4]]));
    }

    #[test]
    fn decodes_exactly_once() {
        assert_eq!(relative_segments("/%252e", 0), Ok(vec![b"%2e".to_vec()]));
    }

    #[test]
    fn skip_beyond_segments_is_empty() {
        assert_eq!(relative_segments("/a/b", 5), Ok(vec![]));
        assert_eq!(relative_segments("/a/", 1), Ok(vec![]));
        assert_eq!(relative_segments("/", 3), Ok(vec![]));
    }

    #[test]
    fn rejects_dot_segments_after_skip_only() {
        assert_eq!(
            relative_segments("/.hidden", 0),
            Err(StaticPathError::DotSegment)
        );
        assert_eq!(
            relative_segments("/a/.git/config", 0),
            Err(StaticPathError::DotSegment)
        );
        assert_eq!(
            relative_segments("/x/.y", 1),
            Err(StaticPathError::DotSegment)
        );
        assert_eq!(relative_segments("/.x/y", 1), Ok(vec![b"y".to_vec()]));
    }

    #[test]
    fn rejects_dot_after_decoding() {
        for (path, skip) in [("/%2ehidden", 0), ("/a/%2E%2e", 0), ("/x/%2Ey", 1)] {
            assert_eq!(
                relative_segments(path, skip),
                Err(StaticPathError::DotSegment),
                "{path}"
            );
        }
    }

    #[test]
    fn bad_segments_are_errors() {
        for path in [
            "", "a", "a/b", "/a//b", "//", "///", "/%2F", "/a%2Fb", "/%00", "/a/b%00",
        ] {
            assert_eq!(
                relative_segments(path, 0),
                Err(StaticPathError::BadSegment),
                "{path}"
            );
        }
        // Skipped segments are checked too.
        assert_eq!(
            relative_segments("/a//b", 2),
            Err(StaticPathError::BadSegment)
        );
    }

    #[test]
    fn bad_escape_is_an_error() {
        for path in ["/%zz", "/%4", "/%", "/a/%4g"] {
            assert_eq!(
                relative_segments(path, 0),
                Err(StaticPathError::BadEscape),
                "{path}"
            );
        }
    }

    #[test]
    fn trailing_slash_detection() {
        assert_eq!(trailing_slash("/a/"), TrailingSlash::Present);
        assert_eq!(trailing_slash("/a"), TrailingSlash::Absent);
        assert_eq!(trailing_slash("/"), TrailingSlash::Present);
    }
}
