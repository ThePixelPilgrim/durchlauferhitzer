//! `sys::landlock::restrict_to_reading` on the real kernel: a child
//! process (this binary again, no harness, no new `unsafe`) opens a root
//! `O_PATH`, restricts itself and checks what it can still reach; the
//! parent builds the tree and removes it afterwards (the child no longer
//! can). Run twice: with a ruleset of the kernel's ABI and of ABI 1.
//!
//! Without Landlock (`abi()` is `Missing`) the test skips with a printed
//! reason, unless `DURCHLAUFERHITZER_REQUIRE_LANDLOCK=1` is set (CI sets
//! it): then it fails.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test binary; clippy.toml allows these in tests"
)]

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use durchlauferhitzer::sys::file::{open_dir_path, openat2_beneath};
use durchlauferhitzer::sys::landlock::{Abi, abi, restrict_to_reading};

/// Set in the child: the temp dir holding the tree.
const CHILD: &str = "DURCHLAUFERHITZER_LANDLOCK_CHILD";

/// Set in the child: the ABI of the ruleset it restricts itself with.
const RULESET: &str = "DURCHLAUFERHITZER_LANDLOCK_RULESET_ABI";

/// Turns a skip into a failure.
const REQUIRE: &str = "DURCHLAUFERHITZER_REQUIRE_LANDLOCK";

/// `dir/root` (opened as the root, then moved to `dir/moved`) with a file
/// and two directories, and a file outside it.
fn build(dir: &Path) {
    fs::create_dir_all(dir.join("root/sub")).unwrap();
    fs::create_dir(dir.join("root/empty")).unwrap();
    fs::write(dir.join("root/file.txt"), "inside").unwrap();
    fs::write(dir.join("root/sub/index.html"), "index").unwrap();
    fs::write(dir.join("outside.txt"), "outside").unwrap();
}

fn read_fd(fd: OwnedFd) -> io::Result<String> {
    let mut text = String::new();
    File::from(fd).read_to_string(&mut text)?;
    Ok(text)
}

/// `Ok` if `result` failed with `EACCES`.
fn eacces<T: std::fmt::Debug>(what: &str, result: io::Result<T>) -> Result<(), String> {
    match result {
        Err(err) if err.raw_os_error() == Some(libc::EACCES) => Ok(()),
        other => Err(format!("{what}: {other:?}, expected EACCES")),
    }
}

fn ok<T>(what: &str, result: io::Result<T>) -> Result<T, String> {
    result.map_err(|err| format!("{what}: {err}"))
}

/// The child's checks, after the restriction, by name.
type Check<'a> = (&'static str, Box<dyn Fn() -> Result<(), String> + 'a>);

fn checks<'a>(dir: &'a Path, root: &'a OwnedFd, kernel: u32) -> Vec<Check<'a>> {
    let moved = dir.join("moved");
    let moved2 = moved.clone();
    let moved3 = moved.clone();
    vec![
        (
            "a_file_beneath_the_root_opens",
            Box::new(move || {
                let fd = ok("open", openat2_beneath(root.as_fd(), &["file.txt"]))?;
                match ok("read", read_fd(fd))?.as_str() {
                    "inside" => Ok(()),
                    other => Err(format!("read {other:?}")),
                }
            }),
        ),
        (
            "a_directory_beneath_the_root_opens",
            Box::new(move || {
                ok("open .", openat2_beneath::<&str>(root.as_fd(), &[]))?;
                ok("open sub", openat2_beneath(root.as_fd(), &["sub"]))?;
                ok(
                    "open sub/index.html",
                    openat2_beneath(root.as_fd(), &["sub", "index.html"]),
                )?;
                let names: Vec<_> = ok("read_dir", fs::read_dir(moved.join("sub")))?
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                if names == ["index.html"] {
                    Ok(())
                } else {
                    Err(format!("sub lists {names:?}"))
                }
            }),
        ),
        (
            "the_root_is_its_descriptor_not_its_path",
            Box::new(move || {
                // The tree moved after the open: reachable by its new
                // path, the new directory at the old path is not.
                ok("read moved", fs::read(moved2.join("file.txt")))?;
                eacces("read decoy", fs::read(dir.join("root/decoy.txt")))
            }),
        ),
        (
            "a_file_outside_the_root_is_eacces",
            Box::new(move || {
                eacces("outside.txt", fs::read(dir.join("outside.txt")))?;
                eacces("/etc/passwd", fs::read("/etc/passwd"))
            }),
        ),
        (
            "nothing_beneath_the_root_can_be_written",
            Box::new(move || {
                let file = moved3.join("file.txt");
                eacces("write", OpenOptions::new().write(true).open(&file))?;
                eacces("create", File::create(moved3.join("new.txt")))?;
                eacces("mkdir", fs::create_dir(moved3.join("newdir")))?;
                eacces("unlink", fs::remove_file(&file))?;
                eacces("rmdir", fs::remove_dir(moved3.join("empty")))?;
                match fs::read_to_string(&file) {
                    Ok(text) if text == "inside" => Ok(()),
                    other => Err(format!("file afterwards: {other:?}")),
                }
            }),
        ),
        (
            "nothing_can_be_executed",
            Box::new(move || eacces("exec /bin/true", Command::new("/bin/true").status())),
        ),
        (
            "the_version_is_still_there",
            Box::new(move || match abi() {
                Abi::Version(now) if now == kernel => Ok(()),
                other @ (Abi::Missing | Abi::Version(_)) => {
                    Err(format!("abi() afterwards: {other:?}"))
                }
            }),
        ),
    ]
}

/// The child: opens the root, moves it, restricts itself with a ruleset
/// of ABI `ruleset`, runs the checks.
fn child(dir: &Path, ruleset: u32) -> ExitCode {
    let Abi::Version(kernel) = abi() else {
        println!("test landlock_self ... FAILED: no Landlock in the child");
        return ExitCode::FAILURE;
    };
    let root = open_dir_path(&dir.join("root")).unwrap();
    fs::rename(dir.join("root"), dir.join("moved")).unwrap();
    fs::create_dir(dir.join("root")).unwrap();
    fs::write(dir.join("root/decoy.txt"), "decoy").unwrap();
    if let Err(err) = restrict_to_reading(&[root.as_fd()], ruleset) {
        println!("test restrict_to_reading ... FAILED: {err}");
        return ExitCode::FAILURE;
    }
    println!("kernel landlock ABI {kernel}, ruleset ABI {ruleset}");
    let mut failed = false;
    for (name, check) in checks(dir, &root, kernel) {
        match check() {
            Ok(()) => println!("test {name} ... ok"),
            Err(err) => {
                println!("test {name} ... FAILED: {err}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Runs a child on a fresh tree with a ruleset of ABI `ruleset`.
fn run(ruleset: u32) -> bool {
    let dir: PathBuf =
        env::temp_dir().join(format!("dlh-landlock-{}-{ruleset}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    build(&dir);
    let status = Command::new(env::current_exe().unwrap())
        .env(CHILD, &dir)
        .env(RULESET, ruleset.to_string())
        .status()
        .unwrap();
    fs::remove_dir_all(&dir).unwrap();
    if !status.success() {
        println!("test landlock_self ... FAILED: child (ruleset ABI {ruleset}) {status}");
    }
    status.success()
}

fn main() -> ExitCode {
    if let Some(dir) = env::var_os(CHILD) {
        let ruleset = env::var(RULESET).unwrap().parse().unwrap();
        return child(Path::new(&dir), ruleset);
    }
    let kernel = match abi() {
        Abi::Version(kernel) => kernel,
        Abi::Missing => {
            if env::var_os(REQUIRE).is_some_and(|v| v == "1") {
                println!("test landlock_self ... FAILED: no Landlock here, but {REQUIRE}=1");
                return ExitCode::FAILURE;
            }
            println!("test landlock_self ... skipped: no Landlock here");
            return ExitCode::SUCCESS;
        }
    };
    // The kernel's own ABI, and ABI 1 (the oldest supported, FR-PROC-06).
    let mut ok = run(kernel);
    if kernel > 1 {
        ok &= run(1);
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
