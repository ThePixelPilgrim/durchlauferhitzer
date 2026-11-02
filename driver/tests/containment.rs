//! Containment at startup (FR-PROC-06) in the built binary, each test a
//! server run as non-root (as `nobody` when the tests run as root): the
//! startup log's `containment` line per layer, the seccomp filter and
//! `no_new_privs` in every thread, `required` against a layer hidden by a
//! test seccomp filter (exit 2) and `best_effort` against the same
//! (logged missing, serving), and reads under Landlock.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use child::{
    Containment, Filter, Lines, Running, Setup, TempDir, assert_exit, free_port, get, run,
    run_program, text, threads,
};
use durchlauferhitzer::sys::landlock::{Abi, abi};

const SECCOMP_ACTIVE: &str = "kind=containment layer=seccomp state=active action=fail log=kernel";
const SECCOMP_OFF: &str = "kind=containment layer=seccomp state=off";
const SECCOMP_MISSING: &str = "kind=containment layer=seccomp state=missing";
const LANDLOCK_OFF: &str = "kind=containment layer=landlock state=off";
const LANDLOCK_MISSING: &str = "kind=containment layer=landlock state=missing";

/// Whether the seccomp filter is installed when Landlock's line is
/// written (it covers device `ioctl` before ABI 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seccomp {
    Filtered,
    Unfiltered,
}

/// Landlock's line for this kernel's ABI (FR-LOG-05).
fn landlock_line(seccomp: Seccomp) -> String {
    let Abi::Version(abi) = abi() else {
        return LANDLOCK_MISSING.to_owned();
    };
    let head = "kind=containment layer=landlock";
    match (abi, seccomp) {
        (5.., _) => format!("{head} state=active abi={abi}"),
        (3 | 4, Seccomp::Filtered) => {
            format!("{head} state=active abi={abi} gaps=device_ioctl covered_by=seccomp")
        }
        (_, Seccomp::Filtered) => format!(
            "{head} state=active abi={abi} gaps=device_ioctl covered_by=seccomp \
             uncovered=truncate"
        ),
        (3 | 4, Seccomp::Unfiltered) => {
            format!("{head} state=partial abi={abi} uncovered=device_ioctl")
        }
        (_, Seccomp::Unfiltered) => {
            format!("{head} state=partial abi={abi} uncovered=truncate+device_ioctl")
        }
    }
}

/// A server's files: `www` (the `/` route) and `two` (the `/two`
/// route) with `inside.txt` each, `outside.txt` next to them, and
/// symlinks out of the roots.
struct Site {
    dir: TempDir,
    addr: SocketAddr,
    www: PathBuf,
    two: PathBuf,
}

impl Site {
    fn new() -> Site {
        let dir = TempDir::new();
        let www = dir.dir("www");
        let two = dir.dir("two");
        dir.file("outside.txt", "outside\n");
        dir.file("www/inside.txt", "inside\n");
        dir.file("two/inside.txt", "two\n");
        std::os::unix::fs::symlink("../outside.txt", www.join("out.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path(), www.join("up")).unwrap();
        let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
        Site {
            dir,
            addr,
            www,
            two,
        }
    }

    /// A config with `containment` (the inside of the block, or no
    /// block when empty); returns its path.
    fn config(&self, containment: &str) -> String {
        let block = if containment.is_empty() {
            String::new()
        } else {
            format!("containment {{ {containment} }}\n")
        };
        let conf = format!(
            "listen {};\n{block}host example.test {{\n  path /two {{ static {}; }}\n  \
             path / {{ static {}; }}\n}}\n",
            self.addr,
            self.two.display(),
            self.www.display()
        );
        let path = self
            .dir
            .config("containment.conf", &conf, Containment::AsWritten);
        path.to_str().unwrap().to_owned()
    }

    fn start(&self, containment: &str, setup: Setup) -> (Running, Lines) {
        let mut server = Running::start(&[&self.config(containment)], setup, self.addr);
        let lines = server.lines();
        (server, lines)
    }
}

/// `key` of every thread's `/proc/<pid>/task/*/status` (the main thread
/// and the 4 disk workers).
fn status_of_threads(pid: u32, key: &str) -> Vec<String> {
    let tasks = threads(pid, 5);
    assert_eq!(tasks.len(), 5, "{tasks:?}");
    tasks
        .iter()
        .map(|task| {
            let status = fs::read_to_string(task.join("status")).unwrap();
            status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in {status}"))
                .trim()
                .to_owned()
        })
        .collect()
}

fn assert_every_thread(pid: u32, key: &str, value: &str) {
    for got in status_of_threads(pid, key) {
        assert_eq!(got, value, "{key}");
    }
}

/// The defaults: both layers on, the filter in every thread with
/// `no_new_privs` (TSYNC covers the disk workers started before it).
#[test]
fn by_default_both_layers_are_active_in_every_thread() {
    let site = Site::new();
    let (server, mut lines) = site.start("", Setup::non_root());
    lines.wait_for(&landlock_line(Seccomp::Filtered));
    lines.wait_for(SECCOMP_ACTIVE);
    assert_every_thread(server.pid(), "Seccomp:", "2");
    assert_every_thread(server.pid(), "NoNewPrivs:", "1");
    let response = get(site.addr, "/inside.txt");
    assert!(response.ends_with("\r\n\r\ninside\n"), "{response}");
}

#[test]
fn landlock_off_is_logged_off() {
    let site = Site::new();
    let (server, mut lines) = site.start("landlock off;", Setup::non_root());
    lines.wait_for(LANDLOCK_OFF);
    lines.wait_for(SECCOMP_ACTIVE);
    assert_every_thread(server.pid(), "Seccomp:", "2");
}

/// Without the filter Landlock still applies (it sets `no_new_privs`
/// itself); before ABI 5 it is partial then (device `ioctl` uncovered).
#[test]
fn seccomp_off_is_logged_off_and_installs_no_filter() {
    let site = Site::new();
    let (server, mut lines) = site.start("seccomp off;", Setup::non_root());
    lines.wait_for(&landlock_line(Seccomp::Unfiltered));
    lines.wait_for(SECCOMP_OFF);
    assert_every_thread(server.pid(), "Seccomp:", "0");
    let nnp = match abi() {
        Abi::Version(_) => "1",
        Abi::Missing => "0",
    };
    assert_every_thread(server.pid(), "NoNewPrivs:", nnp);
}

/// Both off: nothing is applied, not even `no_new_privs`.
#[test]
fn both_off_apply_nothing() {
    let site = Site::new();
    let (server, mut lines) = site.start("landlock off; seccomp off;", Setup::non_root());
    lines.wait_for(LANDLOCK_OFF);
    lines.wait_for(SECCOMP_OFF);
    assert_every_thread(server.pid(), "Seccomp:", "0");
    assert_every_thread(server.pid(), "NoNewPrivs:", "0");
}

/// FR-PROC-06: device `ioctl` before ABI 5 is covered by the filter
/// only; without it a required Landlock is exit 2 naming the gap. Every
/// kernel we test on (CI included) has ABI 5 or later, so this skips
/// there; the decision is pinned by the unit tests of `cover`
/// (`src/containment/tests.rs`) for every ABI before 5.
#[test]
fn seccomp_off_fails_a_required_landlock_before_abi_5() {
    let abi = match abi() {
        Abi::Version(abi @ 1..=4) => abi,
        other @ (Abi::Missing | Abi::Version(_)) => {
            println!("skipped: the kernel's Landlock is {other:?}, so it has no device_ioctl gap");
            return;
        }
    };
    let site = Site::new();
    let output = run(
        &[&site.config("landlock required; seccomp off;")],
        Setup::non_root(),
    );
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        format!(
            "durchlauferhitzer: landlock is required but the kernel's Landlock (ABI {abi}) \
             cannot restrict device_ioctl, and seccomp, which covers it, is off\n"
        )
    );
}

/// `seccomp(2)` answered `ENOSYS` by a test filter, as on a kernel
/// without seccomp.
fn seccomp_hidden() -> Setup {
    Setup {
        filter: Filter::SeccompEnosys,
        ..Setup::non_root()
    }
}

#[test]
fn a_required_seccomp_the_kernel_lacks_is_exit_2() {
    let site = Site::new();
    let output = run(&[&site.config("seccomp required;")], seccomp_hidden());
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: seccomp is required but the kernel has none\n"
    );
}

#[test]
fn a_seccomp_the_kernel_lacks_is_logged_missing_under_best_effort() {
    let site = Site::new();
    let (_server, mut lines) = site.start("", seccomp_hidden());
    lines.wait_for(&landlock_line(Seccomp::Unfiltered));
    lines.wait_for(SECCOMP_MISSING);
    let response = get(site.addr, "/inside.txt");
    assert!(response.ends_with("\r\n\r\ninside\n"), "{response}");
}

/// `landlock_create_ruleset` answered `ENOSYS`, as on a kernel without
/// Landlock.
fn landlock_hidden() -> Setup {
    Setup {
        filter: Filter::LandlockEnosys,
        ..Setup::non_root()
    }
}

#[test]
fn a_required_landlock_the_kernel_lacks_is_exit_2() {
    let site = Site::new();
    let output = run(&[&site.config("landlock required;")], landlock_hidden());
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: landlock is required but the kernel has none\n"
    );
}

#[test]
fn a_landlock_the_kernel_lacks_is_logged_missing_under_best_effort() {
    let site = Site::new();
    let (_server, mut lines) = site.start("", landlock_hidden());
    lines.wait_for(LANDLOCK_MISSING);
    lines.wait_for(SECCOMP_ACTIVE);
    let response = get(site.addr, "/inside.txt");
    assert!(response.ends_with("\r\n\r\ninside\n"), "{response}");
}

/// Owner decision 2026-11-01 (FR-PROC-06): a layer the kernel offers
/// but refuses to apply is exit 2 in every mode, also under the default
/// `best_effort`: `landlock_restrict_self` refused with `EPERM`.
#[test]
fn a_landlock_the_kernel_refuses_to_apply_is_exit_2_under_best_effort() {
    if abi() == Abi::Missing {
        println!("skipped: no Landlock here, so nothing to refuse");
        return;
    }
    let site = Site::new();
    let setup = Setup {
        filter: Filter::LandlockRestrictEperm,
        ..Setup::non_root()
    };
    let output = run(&[&site.config("")], setup);
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: landlock: landlock_restrict_self: Operation not permitted \
         (os error 1)\n"
    );
}

/// The same for seccomp: `SECCOMP_GET_ACTION_AVAIL` answers, then
/// `SECCOMP_SET_MODE_FILTER` is refused with `EINVAL`.
#[test]
fn a_seccomp_the_kernel_refuses_to_apply_is_exit_2_under_best_effort() {
    let site = Site::new();
    let setup = Setup {
        filter: Filter::SeccompInstallEinval,
        ..Setup::non_root()
    };
    let output = run(&[&site.config("")], setup);
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: seccomp: Invalid argument (os error 22)\n"
    );
}

/// Landlock does not get in the way: with it required, a file beneath
/// each root is served, every path out of the roots through the file
/// system is still 404 (dot and empty segments are 400, refused before
/// it).
/// The 404s come from the server's own `openat2` beneath the root
/// (`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`), which Landlock backs up
/// unseen; Landlock's enforcement itself is proven in `landlock_self.rs`.
#[test]
fn with_landlock_files_beneath_the_roots_are_served_and_paths_out_are_404() {
    let site = Site::new();
    let (_server, mut lines) = site.start("landlock required;", Setup::non_root());
    lines.wait_for(SECCOMP_ACTIVE);
    for (path, body) in [("/inside.txt", "inside\n"), ("/two/inside.txt", "two\n")] {
        let response = get(site.addr, path);
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "{path}: {response}"
        );
        assert!(response.ends_with(&format!("\r\n\r\n{body}")), "{response}");
    }
    // Paths that reach the file system: refused beneath the root.
    for path in ["/out.txt", "/up/outside.txt"] {
        let response = get(site.addr, path);
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{path}: {response}"
        );
    }
    // Dot segments, plain or encoded, and an empty segment: refused by
    // the target parser.
    for path in [
        "//outside.txt",
        "/two/../outside.txt",
        "/%2e%2e/outside.txt",
        "/two/%2e%2e/%2e%2e/outside.txt",
    ] {
        let response = get(site.addr, path);
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{path}: {response}"
        );
    }
}

/// Skips the test with `reason`, or fails it when
/// `DURCHLAUFERHITZER_REQUIRE_READ_IMPLIES_EXEC=1` (for a host known to
/// keep the flag; CI sets it nowhere, see below).
fn skip_read_implies_exec(reason: &str) {
    const REQUIRE: &str = "DURCHLAUFERHITZER_REQUIRE_READ_IMPLIES_EXEC";
    assert!(
        std::env::var_os(REQUIRE).is_none_or(|v| v != "1"),
        "{reason}, but {REQUIRE}=1 requires the test"
    );
    println!("skipped: {reason}");
}

/// `READ_IMPLIES_EXEC` (`setarch -X`) defeats the `PROT_EXEC` check:
/// exit 2 under `seccomp required`. Only where the kernel keeps the flag
/// across `exec`: both x86_64 and aarch64 clear it for a 64-bit binary
/// (seen on the CI runners, 2026-11-01), so this skips there and the
/// decision is pinned by the unit tests of `calls_plan`; a skip is a
/// failure with `DURCHLAUFERHITZER_REQUIRE_READ_IMPLIES_EXEC=1`.
#[test]
fn read_implies_exec_fails_a_required_seccomp() {
    const SETARCH: &str = "/usr/bin/setarch";
    let arch = std::env::consts::ARCH;
    if !Path::new(SETARCH).exists() {
        skip_read_implies_exec(&format!("no {SETARCH}"));
        return;
    }
    let probe = run_program(
        SETARCH,
        &[arch, "-X", "cat", "/proc/self/personality"],
        Setup::non_root(),
    );
    let persona = u32::from_str_radix(text(&probe.stdout).trim(), 16).unwrap_or(0);
    if !probe.status.success() || persona & 0x0040_0000 == 0 {
        skip_read_implies_exec(&format!(
            "`setarch {arch} -X` gives no READ_IMPLIES_EXEC across exec here"
        ));
        return;
    }
    let site = Site::new();
    let output = run_program(
        SETARCH,
        &[arch, "-X", child::exe(), &site.config("seccomp required;")],
        Setup::non_root(),
    );
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: seccomp is required but the personality has \
         READ_IMPLIES_EXEC, which defeats its PROT_EXEC check\n"
    );
}

/// Builds `support/clock_syscall.rs`, the test-only `LD_PRELOAD` library
/// whose `clock_gettime` always makes the system call, into `dir` with
/// the toolchain's `rustc` (it is no cargo target, so it never reaches
/// the product's dependency tree); returns its path.
fn clock_syscall_library(dir: &TempDir) -> String {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/clock_syscall.rs");
    let toolchain = Path::new(env!("CARGO")).with_file_name("rustc");
    let rustc = if toolchain.exists() {
        toolchain
    } else {
        PathBuf::from("rustc")
    };
    let out = dir.path().join("libclock_syscall.so");
    let output = Command::new(&rustc)
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "cdylib",
            "-C",
            "panic=abort",
            "-o",
        ])
        .arg(&out)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} failed: {}",
        rustc.display(),
        text(&output.stderr)
    );
    out.to_str().unwrap().to_owned()
}

/// The vDSO falls back to the `clock_gettime` system call on hosts whose
/// clocksource has no vDSO mode (hpet, acpi_pm, Xen, arm64 errata): the
/// server forced onto that path still serves and shuts down cleanly
/// under `seccomp_action kill` (an unlisted call would end it by
/// SIGSYS before it listens).
#[test]
fn the_clock_system_call_passes_the_filter_under_kill() {
    let site = Site::new();
    let library = clock_syscall_library(&site.dir);
    let setup = Setup {
        env: vec![("LD_PRELOAD", library)],
        ..Setup::non_root()
    };
    let (mut server, mut lines) = site.start("seccomp required; seccomp_action kill;", setup);
    lines.wait_for("kind=containment layer=seccomp state=active action=kill log=kernel");
    let response = get(site.addr, "/inside.txt");
    assert!(response.ends_with("\r\n\r\ninside\n"), "{response}");
    server.signal("TERM");
    let status = server.wait_exit(child::PATIENCE);
    assert_eq!(status.code(), Some(0), "{status}");
}
