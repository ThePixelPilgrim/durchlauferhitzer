//! The path of an `OpenFile` (see the contract in `core/contract.rs`): a
//! relative path beneath a root, kept in one buffer so that a request's
//! decoded path costs one allocation however many segments it has, and
//! moves from the core to the driver without a copy (NFR-RES-09).

/// A relative path beneath a root: its segments (decoded bytes) joined by
/// `/`, in one buffer. No segment is empty or holds `/` or NUL, so the
/// join is unambiguous and the bytes are a relative file-system path as
/// they stand; no segment at all (an empty buffer) is the root itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelPath(Vec<u8>);

impl RelPath {
    /// A path built from `segments`, or `None` when one is empty or holds
    /// `/` or NUL. (The core builds its paths itself; this is for drivers'
    /// tests and simulations.)
    pub fn from_segments<S: AsRef<[u8]>>(segments: &[S]) -> Option<RelPath> {
        let mut path = RelPath::default();
        for segment in segments {
            let segment = segment.as_ref();
            if segment.is_empty() || segment.iter().any(|&b| b == b'/' || b == 0) {
                return None;
            }
            path.start_segment();
            path.0.extend_from_slice(segment);
        }
        Some(path)
    }

    /// An empty path (the root) with room for `capacity` bytes.
    pub(crate) fn with_capacity(capacity: usize) -> RelPath {
        RelPath(Vec::with_capacity(capacity))
    }

    /// Starts a new segment: a `/` unless the path is empty. Returns where
    /// the segment's bytes begin; the caller appends them through
    /// [`RelPath::buffer`] and checks them.
    pub(crate) fn start_segment(&mut self) -> usize {
        if !self.0.is_empty() {
            self.0.push(b'/');
        }
        self.0.len()
    }

    /// The buffer, for appending the segment begun by
    /// [`RelPath::start_segment`].
    pub(crate) fn buffer(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }

    /// Appends the segment `name` (non-empty, without `/` or NUL),
    /// growing the buffer by exactly what it needs.
    pub(crate) fn push(&mut self, name: &[u8]) {
        self.0.reserve_exact(name.len() + 1);
        self.start_segment();
        self.0.extend_from_slice(name);
    }

    /// The segments in order; none for the root.
    pub fn segments(&self) -> impl Iterator<Item = &[u8]> {
        self.0
            .split(|&b| b == b'/')
            .filter(|segment| !segment.is_empty())
    }

    /// The last segment, empty for the root.
    pub fn last(&self) -> &[u8] {
        self.segments().last().unwrap_or_default()
    }

    /// The joined bytes: empty for the root.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The joined bytes, moved out (the driver's `openat2` path).
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::RelPath;

    #[test]
    fn joins_segments_with_slashes() {
        let path = RelPath::from_segments(&[&b"a b"[..], b"c.css"]).unwrap_or_default();
        assert_eq!(path.as_bytes(), b"a b/c.css");
        assert_eq!(path.segments().collect::<Vec<_>>(), [&b"a b"[..], b"c.css"]);
        assert_eq!(path.last(), b"c.css");
    }

    #[test]
    fn the_root_is_empty() {
        let root = RelPath::from_segments::<&[u8]>(&[]);
        assert_eq!(root, Some(RelPath::default()));
        let root = root.unwrap_or_default();
        assert_eq!(root.as_bytes(), b"");
        assert_eq!(root.segments().count(), 0);
        assert_eq!(root.last(), b"");
    }

    #[test]
    fn segments_that_would_not_join_are_refused() {
        for bad in [&b""[..], b"a/b", b"a\0"] {
            assert_eq!(RelPath::from_segments(&[bad]), None, "{bad:?}");
        }
    }

    #[test]
    fn push_appends_one_segment_without_spare_room() {
        let mut path = RelPath::from_segments(&[b"docs"]).unwrap_or_default();
        path.push(b"index.html");
        assert_eq!(path.as_bytes(), b"docs/index.html");
        assert_eq!(path.into_bytes().capacity(), b"docs/index.html".len());
        let mut root = RelPath::default();
        root.push(b"index.html");
        assert_eq!(root.as_bytes(), b"index.html");
    }
}
