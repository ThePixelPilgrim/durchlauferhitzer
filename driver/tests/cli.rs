//! The command line (FR-PROC-01, FR-PROC-05a): usage for bad arguments,
//! `--check` (it loads the config and touches neither the network nor the
//! roots; it warns about softer containment and prints the memory
//! backstop, FR-PROC-08), a `--ready-fd` that cannot be used (exit 2 before the config is
//! read, FR-PROC-07). Each test runs the built binary in a subprocess.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

use std::net::TcpListener;
use std::os::fd::OwnedFd;

use child::{
    Containment, PROXY_ROUTE, Setup, TempDir, USAGE, assert_exit, config, memory_backstop_bytes,
    run, text,
};

fn valid(dir: &TempDir) -> String {
    let path = dir.config(
        "ok.conf",
        &config("127.0.0.1:8080", "", PROXY_ROUTE),
        Containment::Switch,
    );
    path.to_str().unwrap().to_owned()
}

#[test]
fn bad_arguments_print_the_usage_and_exit_1() {
    let dir = TempDir::new();
    let path = valid(&dir);
    let cases: [&[&str]; 16] = [
        &[],
        &["--frobnicate"],
        &["--frobnicate", &path],
        &["--check"],
        &["--check", &path, &path],
        &[&path, &path],
        &["-x"],
        // `--ready-fd` without a number, or with anything else.
        &["--ready-fd"],
        &["--ready-fd", &path],
        &["--ready-fd", "3"],
        &["--ready-fd", "x", &path],
        &["--ready-fd", "-1", &path],
        &["--ready-fd", "+3", &path],
        &["--ready-fd", "99999999999", &path],
        &["--ready-fd", "3", "--check", &path],
        &["--check", "--ready-fd", "3", &path],
    ];
    for args in cases {
        let output = run(args, Setup::default());
        assert_exit(&output, 1);
        assert_eq!(text(&output.stderr), USAGE, "{args:?}");
        assert_eq!(text(&output.stdout), "", "{args:?}");
    }
}

/// A valid config with the default containment: `ok`, and on stderr
/// only the memory backstop it would set (FR-PROC-08).
#[test]
fn check_on_a_valid_config_prints_ok() {
    let dir = TempDir::new();
    let path = valid(&dir);
    let output = run(&["--check", &path], Setup::default());
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "ok\n");
    assert_eq!(
        text(&output.stderr),
        format!(
            "{path}: memory backstop: {} bytes\n",
            memory_backstop_bytes(&path)
        )
    );
}

/// FR-PROC-08: one warning per softer containment choice, in config
/// order, above the backstop line; still `ok` and exit 0.
#[test]
fn check_warns_about_each_softening() {
    let dir = TempDir::new();
    let conf = config(
        "127.0.0.1:8080",
        "containment { landlock off; seccomp best_effort; seccomp_action fail; }",
        PROXY_ROUTE,
    );
    let path = dir.config("soft.conf", &conf, Containment::AsWritten);
    let path = path.to_str().unwrap();
    let output = run(&["--check", path], Setup::default());
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "ok\n");
    assert_eq!(
        text(&output.stderr),
        format!(
            "{path}: warning: containment landlock off: Landlock is not used\n\
             {path}: warning: containment seccomp best_effort: serves on a kernel without \
             seccomp\n\
             {path}: warning: containment seccomp_action fail: an unexpected system call is \
             answered, not fatal\n\
             {path}: memory backstop: {} bytes\n",
            memory_backstop_bytes(path)
        )
    );
}

/// FR-PROC-08: the backstop `--check` prints is the one the config's
/// own limits give, and it is only printed, not set: `--check` runs
/// nothing of the startup, so a port held here does not fail it (it
/// binds nothing) and stderr holds that line alone.
#[test]
fn check_prints_the_backstop_and_sets_no_limit() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = taken.local_addr().unwrap().to_string();
    let dir = TempDir::new();
    let conf = config(
        &listen,
        "limits { max_connections 100; memory_limit 64MiB; }",
        PROXY_ROUTE,
    );
    let path = dir.config("backstop.conf", &conf, Containment::Switch);
    let path = path.to_str().unwrap();
    let output = run(&["--check", path], Setup::default());
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "ok\n");
    assert_eq!(
        text(&output.stderr),
        format!(
            "{path}: memory backstop: {} bytes\n",
            memory_backstop_bytes(path)
        )
    );
}

#[test]
fn check_on_an_invalid_config_prints_the_error_with_its_position() {
    let dir = TempDir::new();
    let cases = [
        (
            "listen 127.0.0.1:8080;\nbogus 1;\n",
            "2:1: unknown key `bogus`",
        ),
        ("listen 127.0.0.1:8080\n", "1:8: syntax error"),
        (
            "listen 127.0.0.1:0;\nhost a { path / { static /srv; } }\n",
            "1:1: invalid value `127.0.0.1:0` for `listen`",
        ),
        (
            "listen 127.0.0.1:8080;\nlimits { max_connections 65536; memory_limit 1MiB; }\nhost a { path / { static /srv; } }\n",
            "2:1: memory budget exceeded",
        ),
    ];
    for (i, (conf, message)) in cases.into_iter().enumerate() {
        let path = dir.config(&format!("bad{i}.conf"), conf, Containment::Invalid);
        let path = path.to_str().unwrap();
        let output = run(&["--check", path], Setup::default());
        assert_exit(&output, 1);
        assert_eq!(text(&output.stdout), "", "{conf}");
        let stderr = text(&output.stderr);
        assert!(
            stderr.starts_with(&format!("{path}:{message}")),
            "{conf}: {stderr}"
        );
        assert!(
            stderr.ends_with('\n') && stderr.lines().count() == 1,
            "{stderr}"
        );
    }
}

#[test]
fn check_on_a_missing_config_exits_1_naming_it() {
    let dir = TempDir::new();
    let path = dir.path().join("missing.conf");
    let path = path.to_str().unwrap();
    let output = run(&["--check", path], Setup::default());
    assert_exit(&output, 1);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!("durchlauferhitzer: {path}: ")),
        "{stderr}"
    );
    assert_eq!(text(&output.stdout), "");
}

/// `--check` touches neither the network nor the roots: a missing static
/// root and a port in use are not its business.
#[test]
fn check_opens_no_root_and_binds_no_port() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = taken.local_addr().unwrap().to_string();
    let dir = TempDir::new();
    let missing = dir.path().join("no-such-root");
    let conf = config(
        &listen,
        "",
        &format!("static {};", missing.to_str().unwrap()),
    );
    let path = dir.config("check.conf", &conf, Containment::Switch);
    let output = run(&["--check", path.to_str().unwrap()], Setup::default());
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "ok\n");
}

/// A `--ready-fd` descriptor that cannot be used is exit 2 before the
/// config is read (the config named does not exist, which would be exit 1),
/// so before anything is bound (FR-PROC-07 step 3, Review Focus 2): a
/// standard descriptor, a closed one, a listening socket, or one of
/// systemd's sockets.
#[test]
fn an_unusable_ready_fd_is_exit_2_before_the_config_is_read() {
    let dir = TempDir::new();
    let missing = dir.path().join("missing.conf");
    let missing = missing.to_str().unwrap();
    let listener = || OwnedFd::from(TcpListener::bind("127.0.0.1:0").unwrap());
    let cases: Vec<(&str, Setup, &str)> = vec![
        (
            "0",
            Setup::default(),
            "a standard descriptor (0-2) cannot be used",
        ),
        (
            "1",
            Setup::default(),
            "a standard descriptor (0-2) cannot be used",
        ),
        (
            "2",
            Setup::default(),
            "a standard descriptor (0-2) cannot be used",
        ),
        ("99", Setup::default(), "not open"),
        (
            "3",
            Setup {
                stray_fd: Some(listener()),
                ..Setup::default()
            },
            "a listening socket cannot be used",
        ),
        (
            "3",
            Setup {
                listen_fd: Some(listener()),
                ..Setup::default()
            },
            "one of systemd's sockets (LISTEN_FDS) cannot be used",
        ),
    ];
    for (n, setup, reason) in cases {
        let output = run(&["--ready-fd", n, missing], setup);
        assert_exit(&output, 2);
        assert_eq!(
            text(&output.stderr),
            format!("durchlauferhitzer: --ready-fd {n}: {reason}\n")
        );
        assert_eq!(text(&output.stdout), "", "{n}");
    }
}
