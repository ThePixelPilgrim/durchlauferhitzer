//! The simulated file system behind the sim's file actions.
//!
//! Each static root (named by its path as written in the config) holds a
//! tree of entries keyed by their relative segments. A root always exists as
//! an (initially empty) directory. Paths are resolved like
//! `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`: a symlink anywhere in
//! the path fails, as does a denied entry on the way.

use std::collections::BTreeMap;

use durchlauferhitzer_core::{FileError, FileKind, FileMeta, FileStamp};

/// Path segments relative to a root.
pub(crate) type Key = Vec<Vec<u8>>;

#[derive(Debug, Clone)]
pub(crate) enum Node {
    File(SimFile),
    Dir,
    Symlink,
    /// A FIFO, socket or device: opens as `FileKind::Other`.
    Special,
}

#[derive(Debug, Clone)]
pub(crate) struct SimFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) stamp: FileStamp,
    /// `sendfile` hits end of file at this offset.
    pub(crate) shrink_at: Option<u64>,
    /// Chunks read after this many report this stamp instead.
    pub(crate) change: Option<(usize, FileStamp)>,
}

#[derive(Debug, Clone)]
struct Entry {
    node: Node,
    denied: Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Denied {
    No,
    Yes,
}

#[derive(Debug, Default)]
pub(crate) struct Fs {
    roots: BTreeMap<String, BTreeMap<Key, Entry>>,
    /// Source of distinct modification times.
    clock: u64,
}

/// `"a/b.css"` → `[b"a", b"b.css"]`; `""` is the root itself.
pub(crate) fn key(path: &str) -> Key {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.as_bytes().to_vec())
        .collect()
}

impl Fs {
    fn root_mut(&mut self, root: &str) -> &mut BTreeMap<Key, Entry> {
        self.roots.entry(root.to_owned()).or_default()
    }

    fn insert(&mut self, root: &str, path: &str, node: Node) {
        let key = key(path);
        assert!(!key.is_empty(), "the root itself is always a directory");
        let entries = self.root_mut(root);
        for depth in 1..key.len() {
            let parent = key[..depth].to_vec();
            let entry = entries.entry(parent).or_insert(Entry {
                node: Node::Dir,
                denied: Denied::No,
            });
            assert!(
                matches!(entry.node, Node::Dir),
                "parent of {path} in {root} is not a directory"
            );
        }
        entries.insert(
            key,
            Entry {
                node,
                denied: Denied::No,
            },
        );
    }

    pub(crate) fn add_file(&mut self, root: &str, path: &str, bytes: &[u8]) {
        self.clock += 1;
        let stamp = FileStamp {
            size: bytes.len() as u64,
            modified_ns: self.clock,
        };
        let file = SimFile {
            bytes: bytes.to_vec(),
            stamp,
            shrink_at: None,
            change: None,
        };
        self.insert(root, path, Node::File(file));
    }

    pub(crate) fn add_dir(&mut self, root: &str, path: &str) {
        self.insert(root, path, Node::Dir);
    }

    /// A symlink; where it points is irrelevant, it is never followed.
    pub(crate) fn add_symlink(&mut self, root: &str, path: &str) {
        self.insert(root, path, Node::Symlink);
    }

    /// A non-regular, non-directory entry (FIFO, socket, device).
    pub(crate) fn add_special(&mut self, root: &str, path: &str) {
        self.insert(root, path, Node::Special);
    }

    fn entry_mut(&mut self, root: &str, path: &str) -> &mut Entry {
        let key = key(path);
        match self.root_mut(root).get_mut(&key) {
            Some(e) => e,
            None => panic!("no {path} in sim root {root}"),
        }
    }

    pub(crate) fn deny(&mut self, root: &str, path: &str) {
        self.entry_mut(root, path).denied = Denied::Yes;
    }

    fn file_mut(&mut self, root: &str, path: &str) -> &mut SimFile {
        match &mut self.entry_mut(root, path).node {
            Node::File(f) => f,
            Node::Dir | Node::Symlink | Node::Special => {
                panic!("{path} in sim root {root} is not a file")
            }
        }
    }

    pub(crate) fn shrink_on_send(&mut self, root: &str, path: &str, at: u64) {
        self.file_mut(root, path).shrink_at = Some(at);
    }

    pub(crate) fn change_during_send(
        &mut self,
        root: &str,
        path: &str,
        after_chunks: usize,
        new: FileStamp,
    ) {
        self.file_mut(root, path).change = Some((after_chunks, new));
    }

    /// Resolve `key` beneath `root` the way the production driver does.
    pub(crate) fn open(&self, root: &str, key: &Key) -> Result<FileMeta, FileError> {
        let directory = FileMeta {
            kind: FileKind::Directory,
            stamp: FileStamp {
                size: 0,
                modified_ns: 0,
            },
        };
        let Some(entries) = self.roots.get(root) else {
            return if key.is_empty() {
                Ok(directory)
            } else {
                Err(FileError::NotFound)
            };
        };
        if key.is_empty() {
            return Ok(directory);
        }
        for depth in 1..=key.len() {
            let Some(entry) = entries.get(&key[..depth]) else {
                return Err(FileError::NotFound);
            };
            if entry.denied == Denied::Yes {
                return Err(FileError::PermissionDenied);
            }
            let last = depth == key.len();
            match (&entry.node, last) {
                (Node::Symlink, _) => return Err(FileError::Symlink),
                (Node::Dir, false) => {}
                (Node::File(_) | Node::Special, false) => return Err(FileError::NotFound),
                (Node::Special, true) => {
                    return Ok(FileMeta {
                        kind: FileKind::Other,
                        stamp: FileStamp {
                            size: 0,
                            modified_ns: 0,
                        },
                    });
                }
                (Node::Dir, true) => return Ok(directory),
                (Node::File(f), true) => {
                    return Ok(FileMeta {
                        kind: FileKind::Regular,
                        stamp: f.stamp,
                    });
                }
            }
        }
        Err(FileError::NotFound)
    }

    /// The file at `key`, as it is now.
    pub(crate) fn file(&self, root: &str, key: &Key) -> Option<&SimFile> {
        match &self.roots.get(root)?.get(key)?.node {
            Node::File(f) => Some(f),
            Node::Dir | Node::Symlink | Node::Special => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(fs: &Fs, path: &str) -> Result<FileKind, FileError> {
        fs.open("/r", &key(path)).map(|m| m.kind)
    }

    #[test]
    fn resolves_like_openat2_beneath_without_symlinks() {
        let mut fs = Fs::default();
        fs.add_file("/r", "d/f.txt", b"abc");
        fs.add_dir("/r", "empty");
        fs.add_symlink("/r", "link");
        fs.add_file("/r", "link2/x", b"");
        fs.add_symlink("/r", "link2");
        fs.add_file("/r", "secret/x", b"");
        fs.deny("/r", "secret");
        fs.add_special("/r", "d/fifo");
        assert_eq!(kind(&fs, ""), Ok(FileKind::Directory));
        assert_eq!(kind(&fs, "d/fifo"), Ok(FileKind::Other));
        assert_eq!(kind(&fs, "d/fifo/x"), Err(FileError::NotFound));
        assert_eq!(kind(&fs, "d"), Ok(FileKind::Directory));
        assert_eq!(kind(&fs, "empty"), Ok(FileKind::Directory));
        assert_eq!(kind(&fs, "d/f.txt"), Ok(FileKind::Regular));
        assert_eq!(kind(&fs, "d/f.txt/x"), Err(FileError::NotFound));
        assert_eq!(kind(&fs, "missing"), Err(FileError::NotFound));
        assert_eq!(kind(&fs, "link"), Err(FileError::Symlink));
        assert_eq!(kind(&fs, "link2/x"), Err(FileError::Symlink));
        assert_eq!(kind(&fs, "secret"), Err(FileError::PermissionDenied));
        assert_eq!(kind(&fs, "secret/x"), Err(FileError::PermissionDenied));
        assert_eq!(
            fs.open("/other", &key("")).map(|m| m.kind),
            Ok(FileKind::Directory)
        );
        assert_eq!(fs.open("/other", &key("f")), Err(FileError::NotFound));
        let meta = fs.open("/r", &key("d/f.txt")).unwrap();
        assert_eq!(meta.stamp.size, 3);
    }
}
