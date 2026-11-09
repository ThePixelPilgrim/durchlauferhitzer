//! Temp dirs and the configs written into them for the tests that run the
//! binary (`child.rs` includes this file and re-exports it), with the
//! seccomp kill switch: `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1` adds
//! `containment { landlock required; seccomp required; seccomp_action
//! kill; }` to every config written with [`Containment::Switch`], so an
//! unexpected system call ends the server (FR-PROC-06, FR-CFG-25). Since
//! the defaults of 2026-11-09 (`required`, `required`, `kill`) that block
//! equals the default, so every server test runs under kill with or
//! without the switch; it is kept for CI's existing step and a possible
//! future `fail`-mode run.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The switch that runs every server under `seccomp_action kill`.
pub const SECCOMP_KILL: &str = "DURCHLAUFERHITZER_TEST_SECCOMP_KILL";

/// The block the switch adds.
pub const KILL_BLOCK: &str =
    "containment { landlock required; seccomp required; seccomp_action kill; }\n";

/// Whether the switch is on.
pub fn seccomp_kill() -> bool {
    std::env::var_os(SECCOMP_KILL).is_some_and(|v| v == "1")
}

/// Whose `containment` block a config file carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    /// [`KILL_BLOCK`] when the switch is on, none otherwise; the text
    /// must have no block of its own.
    Switch,
    /// The text's own block, or none: the test is about containment
    /// itself (`containment.rs`), whatever the switch says.
    AsWritten,
    /// None: the config is invalid, so the server exits at load, before
    /// any containment; a block after it would move the error.
    Invalid,
}

/// A directory under the system temp dir, removed on drop; readable by
/// everyone, so a server running as `nobody` can read its config.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("dlh-cli-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        set_mode(&path, 0o755);
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `text` to `name`, readable by everyone. Configs (`.conf`)
    /// go through [`TempDir::config`].
    pub fn file(&self, name: &str, text: &str) -> PathBuf {
        assert!(
            !name.ends_with(".conf"),
            "write configs with TempDir::config: {name}"
        );
        self.write(name, text)
    }

    /// Writes the config `text` to `name` (ending in `.conf`), readable
    /// by everyone, with the `containment` block `containment` says.
    pub fn config(&self, name: &str, text: &str, containment: Containment) -> PathBuf {
        assert!(name.ends_with(".conf"), "a config is a .conf: {name}");
        match containment {
            Containment::AsWritten | Containment::Invalid => self.write(name, text),
            Containment::Switch => {
                let own_block = text.lines().any(|line| {
                    line.trim_start()
                        .strip_prefix("containment")
                        .is_some_and(|rest| rest.trim_start().starts_with('{'))
                });
                assert!(
                    !own_block,
                    "a config with its own containment block is Containment::AsWritten"
                );
                if seccomp_kill() {
                    self.write(name, &format!("{text}{KILL_BLOCK}"))
                } else {
                    self.write(name, text)
                }
            }
        }
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, text).unwrap();
        set_mode(&path, 0o644);
        path
    }

    /// A directory `name`, readable by everyone.
    pub fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        set_mode(&path, 0o755);
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A config with one listener and one host whose `/` route is `route`
/// (the inside of the `path` block), plus `extra` top-level lines.
pub fn config(listen: &str, extra: &str, route: &str) -> String {
    format!("listen {listen};\n{extra}\nhost example.test {{\n  path / {{ {route} }}\n}}\n")
}

/// A proxy route to an address nothing listens on (the tests that use it
/// never send a request there).
pub const PROXY_ROUTE: &str =
    "proxy 127.0.0.1:9; max_request_body 1MiB; max_response_body unlimited;";
