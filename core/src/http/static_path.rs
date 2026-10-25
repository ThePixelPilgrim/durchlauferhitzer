//! Decoding of validated request paths into relative file-name segments.

/// Why a path could not be turned into relative segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticPathError {
    /// A decoded segment starts with `.` (hidden files and traversal).
    DotSegment,
    /// A `%` escape is not followed by two hex digits. Paths that passed
    /// `parse_target` never contain one.
    BadEscape,
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
pub fn relative_segments(path: &str, skip: usize) -> Result<Vec<Vec<u8>>, StaticPathError> {
    let mut raw: Vec<&str> = path.split('/').skip(1).collect();
    if raw.last() == Some(&"") {
        raw.pop();
    }
    let mut out = Vec::new();
    for segment in raw.into_iter().skip(skip) {
        let decoded = decode(segment)?;
        if decoded.first() == Some(&b'.') {
            return Err(StaticPathError::DotSegment);
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
