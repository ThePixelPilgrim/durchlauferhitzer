//! The command line (FR-PROC-01, FR-PROC-05a): usage for bad arguments,
//! `--check` (it loads the config and touches neither the network nor the
//! roots). Each test runs the built binary in a subprocess.

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

use child::{PROXY_ROUTE, Setup, TempDir, USAGE, assert_exit, config, run, text};

fn valid(dir: &TempDir) -> String {
    let path = dir.file("ok.conf", &config("127.0.0.1:8080", "", PROXY_ROUTE));
    path.to_str().unwrap().to_owned()
}

#[test]
fn bad_arguments_print_the_usage_and_exit_1() {
    let dir = TempDir::new();
    let path = valid(&dir);
    let cases: [&[&str]; 7] = [
        &[],
        &["--frobnicate"],
        &["--frobnicate", &path],
        &["--check"],
        &["--check", &path, &path],
        &[&path, &path],
        &["-x"],
    ];
    for args in cases {
        let output = run(args, Setup::default());
        assert_exit(&output, 1);
        assert_eq!(text(&output.stderr), USAGE, "{args:?}");
        assert_eq!(text(&output.stdout), "", "{args:?}");
    }
}

#[test]
fn check_on_a_valid_config_prints_ok() {
    let dir = TempDir::new();
    let path = valid(&dir);
    let output = run(&["--check", &path], Setup::default());
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "ok\n");
    assert_eq!(text(&output.stderr), "");
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
        let path = dir.file(&format!("bad{i}.conf"), conf);
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
    let path = dir.file("check.conf", &conf);
    let output = run(&["--check", path.to_str().unwrap()], Setup::default());
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "ok\n");
}
