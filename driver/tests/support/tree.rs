//! A static root for the file tests: a fresh directory under the system
//! temp dir (removed on drop), deterministic file contents, symlinks and
//! FIFOs (made with the `mkfifo` command: tests may not use `unsafe`,
//! R3). Shared by `tests/static_files.rs` and `tests/static_fd_leak.rs`
//! (`#[path]` include).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code,
    reason = "test helpers outside #[test] functions; each test binary uses part"
)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A directory under the system temp dir, removed on drop.
pub struct Tree(PathBuf);

impl Tree {
    pub fn new() -> Tree {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("dlh-static-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Tree(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `bytes` to `name` (creating parent directories).
    pub fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, bytes).unwrap();
        path
    }

    pub fn dir(&self, name: &str) {
        fs::create_dir_all(self.0.join(name)).unwrap();
    }

    pub fn symlink(&self, name: &str, target: &str) {
        symlink(target, self.0.join(name)).unwrap();
    }

    pub fn fifo(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        let status = Command::new("mkfifo").arg(&path).status().unwrap();
        assert!(status.success(), "mkfifo {}", path.display());
        path
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `len` deterministic bytes that differ per `seed` (xorshift), so a
/// misplaced or repeated range is never mistaken for the right one.
pub fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A config with one host, `example.test`, whose `/` is the static root
/// `root`.
pub fn static_config(root: &Path) -> String {
    format!(
        "listen 127.0.0.1:8080;
host example.test {{
  path / {{ static {}; }}
}}",
        root.display()
    )
}
