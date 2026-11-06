//! The restart without systemd (FR-PROC-07): `contrib/durchlauferhitzer-reload`
//! run with `sh` (dash on Ubuntu) against real servers.
//!
//! - Under load (keep-alive and fresh clients, plus a batch of connections
//!   queued in the old process's backlog while it is stopped across the
//!   SIGHUP) with the binary replaced and a new route, every request gets
//!   a complete 200, the old process exits 0 and the pid file names the
//!   new one; two reloads back to back lose nothing either (Review Focus
//!   3), the first old process still draining a half-received request.
//! - A handover that cannot be lossless does not start (Review Focus 4):
//!   a bad config, a new process that fails at startup, a pid file that
//!   names another program; the old process serves on, untouched.
//!   A new process that dies during the grace gets the pid file pointed
//!   back at the old one (exit 1, no SIGHUP); one not ready in time is
//!   stopped and reaped; one whose helper is killed is listed in
//!   `<pidfile>.draining`, which OpenRC's `stop_post` ends.
//! - A `listen` address the new config drops is closed (documented); one
//!   it adds is served, with a new `user`, and nothing is lost.
//! - The lock: a second helper is refused while the holder runs; a stale
//!   one (holder gone, or its pid reused) is taken over, and of two
//!   helpers racing for it one at most proceeds. A new process that
//!   ignores SIGHUP (the helper under `nohup`) is left running, listed
//!   for `stop_post`.
//! - Steering: a ready new process gets every new connection before any
//!   SIGHUP; fresh clients across back-to-back reloads lose nothing (a
//!   kernel race resets a handshake completing on a listener as it
//!   closes); the grace holds the SIGHUP back; the helper keeps its lock
//!   until the old listeners closed.
//! - Control: with `tcp_migrate_req` 0 the connections queued in the old
//!   process at its SIGHUP are reset. The old process accepts at most 64
//!   connections per wakeup (`ACCEPT_BATCH`) before it reads the signal,
//!   so a batch of 400 leaves some queued when it closes the listener.
//!
//! These run as root (the helper writes `net.ipv4.tcp_migrate_req`, and
//! the server binds as root, then drops to `nobody`); otherwise they skip
//! with a printed reason, or fail with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`.
//! A `tcp_migrate_req` that cannot be set to 1 skips them too, or fails
//! with `DURCHLAUFERHITZER_REQUIRE_MIGRATE=1`. The sysctl is global, so
//! the tests run one at a time.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

#[path = "support/load.rs"]
mod load;

#[path = "support/site.rs"]
mod site;

use std::fs;
use std::io::{self, Write};
use std::net::TcpStream;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use child::{Containment, Running, exe, free_port, signal_pid, text};
use durchlauferhitzer::sys::user::lookup_user;
use load::{Load, Pattern, Tally, collect, connect, fresh_request, queue, read_response, warm_up};
use site::{
    HELPER, Migration, OPENRC, REQUIRE_ROOT, Site, alive, await_new_pid, bounded, can_restart,
    finish, hup_pending, loopback, reload_with, required, serial, set_migrate, start_time,
    wait_gone,
};

/// How long the old process may take to drain and exit.
const DRAIN_PATIENCE: Duration = Duration::from_secs(10);

/// How many connections are queued in the stopped old process.
const QUEUED: usize = 400;

/// The handover under load, with the old process stopped across the
/// helper's SIGHUP: connections queue in its backlog (and some of the
/// clients' too), then it continues. The helper waits for its listeners
/// to close, so it runs in the background until then.
fn stopped_handover(site: &Site, old: &Running, new_conf: &Path, migration: Migration) -> Tally {
    let addr = site.addr;
    signal_pid(old.pid(), "STOP");
    let queued = queue(addr, QUEUED);
    site.replace_binary();
    thread::scope(|scope| {
        let reload = scope.spawn(|| site.reload(new_conf));
        await_hup(old.pid(), &reload);
        // The helper set it to 1.
        assert!(set_migrate(migration));
        signal_pid(old.pid(), "CONT");
        site.assert_reload_ok(&reload.join().unwrap());
    });
    let tally = collect(queued, &["old", "new"]);
    assert!(set_migrate(Migration::On));
    tally
}

/// Waits until the stopped `pid` has the helper's SIGHUP pending; a
/// helper that ends without having sent it fails the test.
fn await_hup(pid: u32, reload: &thread::ScopedJoinHandle<'_, Output>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !hup_pending(pid) {
        assert!(
            !reload.is_finished() || hup_pending(pid),
            "the helper ended, and pid {pid} has no SIGHUP pending"
        );
        assert!(Instant::now() < deadline, "no SIGHUP for pid {pid}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// FR-PROC-07: no connection is lost, accepted or queued, and the binary
/// may change.
#[test]
fn reload_under_load_loses_nothing() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    let old_pid = old.pid();
    let load = Load::start(addr, &["old", "new"]);
    warm_up();
    let queued = stopped_handover(&site, &old, &new_conf, Migration::On);
    let status = old.wait_exit(DRAIN_PATIENCE);
    thread::sleep(Duration::from_millis(200));
    let load = load.finish();
    eprintln!("queued: {:?}; load: {:?}", queued.answers, load.answers);
    assert_eq!(status.code(), Some(0), "old: {status}\n{}", site.log_text());
    assert!(queued.failures.is_empty(), "queued: {queued:?}");
    assert_eq!(queued.answers.values().sum::<usize>(), QUEUED);
    // More than one accept batch was queued: some moved to the new process.
    assert!(
        queued.answers.get("new").is_some_and(|&n| n > 0),
        "{queued:?}"
    );
    assert!(load.failures.is_empty(), "load: {:?}", load.failures);
    assert!(load.answers.get("old").is_some_and(|&n| n > 0), "{load:?}");
    assert!(load.answers.get("new").is_some_and(|&n| n > 0), "{load:?}");
    let new_pid = site.pid();
    assert_ne!(new_pid, old_pid);
    assert_eq!(site.processes(), vec![new_pid]);
    for _ in 0..10 {
        assert_eq!(fresh_request(addr, &["new"]), Ok("new".to_owned()));
    }
}

/// FR-PROC-07 "Steering": once the new process is ready, before any
/// SIGHUP, every new connection reaches it, none the old process.
#[test]
fn ready_steers_new_connections_to_the_new_process() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let _old = site.start(&old_conf);
    let _new = site.start_ready(&new_conf);
    for _ in 0..50 {
        assert_eq!(fresh_request(addr, &["old", "new"]), Ok("new".to_owned()));
    }
}

/// How many back-to-back reloads the storm sees.
const STORM_RELOADS: usize = 6;

/// FR-PROC-07 "Steering": a probabilistic regression smoke test, not a
/// proof. Many fresh connections across several reloads lose nothing; a
/// connect reset at the old listener's close is the kernel race steering
/// avoids. Before steering it failed about 1 run in 10 here. The
/// deterministic guard is `ready_steers_new_connections_to_the_new_process`;
/// the grace's timing is pinned by
/// `the_old_process_gets_no_sighup_before_the_grace_ends`.
#[test]
fn reload_under_fresh_storm_loses_nothing() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let confs = [
        site.config("old", &[addr], "old"),
        site.config("new", &[addr], "new"),
    ];
    let _first = site.start(&confs[0]);
    let load = Load::with(addr, &["old", "new"], &[Pattern::Fresh; 12]);
    warm_up();
    for i in 0..STORM_RELOADS {
        site.assert_reload_ok(&site.reload(&confs[(i + 1) % 2]));
    }
    let deadline = Instant::now() + DRAIN_PATIENCE;
    while site.processes() != vec![site.pid()] {
        assert!(Instant::now() < deadline, "{:?}", site.processes());
        thread::sleep(Duration::from_millis(10));
    }
    let load = load.finish();
    eprintln!("load: {:?}", load.answers);
    assert!(load.failures.is_empty(), "load: {:?}", load.failures);
}

/// The helper releases its lock only once the old process has closed its
/// listeners: here the old process is stopped across its SIGHUP, and the
/// helper runs on until it continues.
#[test]
fn the_helper_waits_for_the_old_listeners_to_close() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    // The helper reads it to see the listeners: root without
    // CAP_SYS_PTRACE cannot (the server runs as `nobody`), and the helper
    // then warns and does not wait (README). A skip, or a failure where
    // root is required.
    if let Err(err) = fs::read_dir(format!("/proc/{}/fd", old.pid())) {
        assert!(
            !required(REQUIRE_ROOT),
            "/proc/<server>/fd cannot be read here ({err}), but {REQUIRE_ROOT}=1 requires it"
        );
        eprintln!("skipped: /proc/<server>/fd cannot be read here ({err})");
        return;
    }
    let lock = site.dir.path().join("durchlauferhitzer.pid.lock");
    signal_pid(old.pid(), "STOP");
    thread::scope(|scope| {
        let reload = scope.spawn(|| site.reload(&new_conf));
        await_hup(old.pid(), &reload);
        thread::sleep(Duration::from_millis(500));
        assert!(!reload.is_finished(), "the helper did not wait");
        assert!(lock.is_dir(), "the lock went early");
        signal_pid(old.pid(), "CONT");
        let output = reload.join().unwrap();
        site.assert_reload_ok(&output);
        assert_eq!(text(&output.stderr), "");
    });
    assert!(!lock.exists(), "the lock was left behind");
    assert_eq!(old.wait_exit(DRAIN_PATIENCE).code(), Some(0));
    assert_eq!(fresh_request(addr, &["new"]), Ok("new".to_owned()));
}

/// Control: without `tcp_migrate_req` the connections still queued in
/// the old process when it closes its listener are reset.
#[test]
fn without_migration_queued_connections_are_reset() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    let queued = stopped_handover(&site, &old, &new_conf, Migration::Off);
    let status = old.wait_exit(DRAIN_PATIENCE);
    assert_eq!(status.code(), Some(0));
    let reset = queued
        .failures
        .iter()
        .filter(|failure| failure.contains("reset"))
        .count();
    eprintln!("queued: {:?}; {reset} reset", queued.answers);
    assert!(reset > 0, "{queued:?}");
    assert_eq!(reset, queued.failures.len(), "{queued:?}");
}

/// Review Focus 3: the second handover runs while the first old process
/// still drains (a half-received request holds it); nothing is lost and
/// the pid file names the newest process.
#[test]
fn two_reloads_back_to_back_lose_nothing() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let first_conf = site.config("first", &[addr], "old");
    let second_conf = site.config("second", &[addr], "second");
    let third_conf = site.config("third", &[addr], "third");
    let mut first = site.start(&first_conf);
    let mut half = connect(addr).unwrap();
    half.write_all(b"GET /a.txt HTTP/1.1\r\nHost: exam")
        .unwrap();
    let load = Load::start(addr, &["old", "second", "third"]);
    warm_up();
    site.assert_reload_ok(&site.reload(&second_conf));
    let second = site.pid();
    site.assert_reload_ok(&site.reload(&third_conf));
    let third = site.pid();
    // Both earlier processes still ran at the second handover.
    assert_eq!(site.draining(), vec![first.pid(), second]);
    assert!(
        first.child.try_wait().unwrap().is_none(),
        "the first old process ended early"
    );
    wait_gone(second, DRAIN_PATIENCE);
    half.write_all(b"ple.test\r\n\r\n").unwrap();
    let response = read_response(&mut half).unwrap();
    assert_eq!(response.status, "HTTP/1.1 200 OK");
    assert_eq!(response.body, "old\n");
    assert!(response.close, "{response:?}");
    let status = first.wait_exit(DRAIN_PATIENCE);
    assert_eq!(status.code(), Some(0));
    let load = load.finish();
    assert!(load.failures.is_empty(), "load: {:?}", load.failures);
    assert_ne!(second, third);
    assert_eq!(site.processes(), vec![third]);
    assert_eq!(fresh_request(addr, &["third"]), Ok("third".to_owned()));
}

/// The helper's refusals: exit 1, a one-line reason, the old process
/// untouched and serving, the pid file unchanged, no new process.
fn assert_refused(site: &Site, old: &mut Running, output: &Output, reason: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "{output:?}\nlog: {}",
        site.log_text()
    );
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains(&format!("durchlauferhitzer-reload: {reason}")),
        "{stderr}"
    );
    assert!(
        old.child.try_wait().unwrap().is_none(),
        "the old process ended"
    );
    assert_eq!(site.pid(), old.pid());
    assert_eq!(site.processes(), vec![old.pid()]);
    let addr = site.addr;
    assert_eq!(fresh_request(addr, &["old"]), Ok("old".to_owned()));
}

#[test]
fn a_bad_config_reload_changes_nothing() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let bad = site.dir.config(
        "bad.conf",
        "listen 127.0.0.1:1;\nnonsense;\n",
        Containment::Invalid,
    );
    let mut old = site.start(&old_conf);
    let output = site.reload(&bad);
    assert_refused(&site, &mut old, &output, "the config check failed");
    assert!(text(&output.stderr).contains("bad.conf:2:1"), "{output:?}");
}

/// A new config that passes `--check` but fails at startup (a missing
/// root, before binding; an unknown `user`, after binding).
#[test]
fn a_new_process_that_fails_leaves_the_old_untouched() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let missing = site.config("missing", &[addr], "/nonexistent-dlh-root");
    let mut old = site.start(&old_conf);
    let output = site.reload(&missing);
    assert_refused(
        &site,
        &mut old,
        &output,
        "the new process exited before it was ready",
    );
    assert!(
        site.log_text().contains("/nonexistent-dlh-root"),
        "{}",
        site.log_text()
    );
    let unknown = site.dir.config(
        "unknown.conf",
        &fs::read_to_string(&old_conf)
            .unwrap()
            .replace("user nobody;", "user no_such_user_dlh;"),
        Containment::AsWritten,
    );
    let load = Load::start(addr, &["old"]);
    warm_up();
    let output = site.reload(&unknown);
    thread::sleep(Duration::from_millis(100));
    let load = load.finish();
    assert_refused(
        &site,
        &mut old,
        &output,
        "the new process exited before it was ready",
    );
    assert!(load.failures.is_empty(), "load: {:?}", load.failures);
}

#[test]
fn a_stale_pidfile_is_refused() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let conf = site.config("old", &[addr], "old");
    let mut old = site.start(&conf);
    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    // Another program, no process (above any pid_max), no number.
    for (stale, reason) in [
        (sleeper.id().to_string(), "pid "),
        ("4194304".to_owned(), "pid 4194304 "),
        ("x".to_owned(), "no pid in "),
    ] {
        fs::write(&site.pidfile, format!("{stale}\n")).unwrap();
        let output = site.reload(&conf);
        fs::write(&site.pidfile, format!("{}\n", old.pid())).unwrap();
        assert_refused(&site, &mut old, &output, reason);
        let stderr = text(&output.stderr);
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
    }
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

/// FR-PROC-07: a `listen` address the new config drops is closed with
/// the old process (connections queued on it would be reset).
#[test]
fn a_dropped_listen_address_is_closed() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let dropped = loopback(free_port());
    let old_conf = site.config("old", &[addr, dropped], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    assert_eq!(fresh_request(dropped, &["old"]), Ok("old".to_owned()));
    site.assert_reload_ok(&site.reload(&new_conf));
    let status = old.wait_exit(DRAIN_PATIENCE);
    assert_eq!(status.code(), Some(0));
    let err = TcpStream::connect(dropped).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    assert_eq!(fresh_request(addr, &["new"]), Ok("new".to_owned()));
}

/// The helper's argument checks (no root needed).
#[test]
fn the_helper_refuses_bad_arguments() {
    let _serial = serial();
    for args in [
        &[][..],
        &["a", "b", "c"][..],
        &["a", "b", "c", "d", "0"][..],
        &["a", "b", "c", "d", "x"][..],
        &["a", "b", "c", "d", "1", "e"][..],
        &["a", "b", "c", "d", "1", "01"][..],
        &["a", "b", "c", "d", "1", "1", "f"][..],
    ] {
        let output = reload_with(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        let stderr = text(&output.stderr);
        assert!(
            stderr.starts_with("durchlauferhitzer-reload: usage: "),
            "{stderr}"
        );
    }
}

/// The helper and the OpenRC files are shell that `sh` parses.
#[test]
fn the_scripts_parse() {
    let _serial = serial();
    for path in std::iter::once(HELPER).chain(OPENRC) {
        let output = Command::new("sh").arg("-n").arg(path).output().unwrap();
        assert!(output.status.success(), "{path}: {output:?}");
    }
    let mode = fs::metadata(HELPER).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "the helper is executable");
}

/// A second helper while one runs (its lock exists and names a running
/// pid, here a `sleep`) is refused; the lock is only removed by the run
/// that took it.
#[test]
fn a_concurrent_reload_is_refused() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    let lock = site.dir.path().join("durchlauferhitzer.pid.lock");
    let mut holder = Command::new("sleep").arg("30").spawn().unwrap();
    fs::create_dir(&lock).unwrap();
    fs::write(lock.join("pid"), format!("{}\n", holder.id())).unwrap();
    let output = site.reload(&new_conf);
    let _ = holder.kill();
    let _ = holder.wait();
    assert_refused(
        &site,
        &mut old,
        &output,
        &format!("another reload is running (pid {};", holder.id()),
    );
    assert!(lock.is_dir(), "the refused run removed the lock");
    fs::remove_dir_all(&lock).unwrap();
    site.assert_reload_ok(&site.reload(&new_conf));
    assert!(!lock.exists(), "the lock was left behind");
}

/// The helper lists each old process in `<pidfile>.draining`; the OpenRC
/// script's `stop_post` ends those still running the binary (SIGTERM,
/// then SIGKILL per `retry`) and removes the list; another program's pid
/// in it is left alone.
#[test]
fn stop_post_ends_the_draining_processes() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    // A half-received request keeps the old process draining.
    let mut half = connect(addr).unwrap();
    half.write_all(b"GET /a.txt HTTP/1.1\r\nHost: exam")
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    site.assert_reload_ok(&site.reload(&new_conf));
    let new = site.pid();
    assert_eq!(site.draining(), vec![old.pid()]);
    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let mut list = fs::read_to_string(site.draining_file()).unwrap();
    // Another program; the server's binary but another start time (a
    // reused pid); a line without a start time.
    let sleeper_start = start_time(sleeper.id()).unwrap();
    list.push_str(&format!("{} {sleeper_start}\n", sleeper.id()));
    list.push_str(&format!("{new} 1\n{new}\n"));
    fs::write(site.draining_file(), list).unwrap();
    assert!(
        old.child.try_wait().unwrap().is_none(),
        "the old process ended early"
    );
    let output = site.stop_post("TERM/1/KILL/2");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    old.wait_exit(Duration::from_secs(1));
    assert!(
        sleeper.try_wait().unwrap().is_none(),
        "another program was signalled"
    );
    assert!(!site.draining_file().exists());
    assert!(alive(new), "stop_post ended the current process");
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

/// The new process inherits none of the helper's descriptors but fd 3
/// (closed by the server after `ready`): neither the caller's extra ones
/// (fds 4-9) nor the FIFO's read side.
#[test]
fn the_new_process_inherits_no_helper_descriptors() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let _old = site.start(&old_conf);
    let marker = site.dir.file("inherited", "");
    site.assert_reload_ok(&site.reload_inheriting(&new_conf, &marker));
    let new = site.pid();
    let fds: Vec<String> = fs::read_dir(format!("/proc/{new}/fd"))
        .unwrap()
        .filter_map(|entry| fs::read_link(entry.unwrap().path()).ok())
        .map(|target| target.display().to_string())
        .collect();
    let marker = marker.display().to_string();
    assert!(
        !fds.iter().any(|fd| *fd == marker || fd.contains("ready")),
        "{fds:?}"
    );
}

/// A lock that cannot be created (its directory is missing) is named as
/// such, not as another reload.
#[test]
fn a_lock_that_cannot_be_created_is_named() {
    let _serial = serial();
    let output = reload_with(&[
        exe(),
        "/nonexistent-dlh/x.conf",
        "/nonexistent-dlh/x.pid",
        "/nonexistent-dlh/x.log",
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(
            "durchlauferhitzer-reload: cannot create the lock /nonexistent-dlh/x.pid.lock: "
        ),
        "{stderr}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

/// A new process that never reports ready (it blocks opening its config,
/// a FIFO that `--check` alone was fed) is stopped and reaped after the
/// timeout; the old process, the pid file and the temp dirs are as before.
#[test]
fn a_new_process_that_is_not_ready_in_time_is_stopped() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let fed = site.config("fifo", &[addr], "new");
    let text_of_conf = fs::read_to_string(&fed).unwrap();
    fs::remove_file(&fed).unwrap();
    let made = Command::new("mkfifo").arg(&fed).status().unwrap();
    assert!(made.success());
    let mut old = site.start(&old_conf);
    let feeder = {
        let fed = fed.clone();
        thread::spawn(move || fs::write(fed, text_of_conf).unwrap())
    };
    let started = Instant::now();
    let output = site.reload_within(&fed, "2");
    let took = started.elapsed();
    feeder.join().unwrap();
    assert_refused(
        &site,
        &mut old,
        &output,
        "the new process was not ready within 2 s",
    );
    assert!(took >= Duration::from_secs(2), "{took:?}");
    assert!(took < Duration::from_secs(8), "{took:?}");
    assert_eq!(fs::read_dir(site.tmp()).unwrap().count(), 0);
    assert!(!site.dir.path().join("durchlauferhitzer.pid.lock").exists());
    // Listed while it ran (review M2), dropped once it ended.
    assert_eq!(site.draining(), Vec::<u32>::new());
}

/// Review I1: the new process dies during the grace (after the pid file
/// names it, before the SIGHUP). The helper sends no SIGHUP, points the
/// pid file back at the old process, takes it off the draining list and
/// exits 1; the old process serves on.
#[test]
fn a_new_process_that_dies_during_the_grace_leaves_the_old_serving() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    let helper = site.spawn_reload(&new_conf, &["10", "3"]);
    let new = await_new_pid(&site.pidfile, old.pid());
    signal_pid(new, "KILL");
    let output = finish(helper);
    assert_refused(
        &site,
        &mut old,
        &output,
        &format!("the new process (pid {new}) ended during the grace;"),
    );
    assert!(!hup_pending(old.pid()));
    assert!(
        !site.log_text().contains("phase=restarting"),
        "{}",
        site.log_text()
    );
    assert_eq!(site.draining(), Vec::<u32>::new());
}

/// Review B-M1: the grace holds the SIGHUP back. With a grace of 2 s the
/// idle old process, which exits at once on SIGHUP, still runs about 2 s
/// after the pid file names the new one, and the reload takes that long.
#[test]
fn the_old_process_gets_no_sighup_before_the_grace_ends() {
    const GRACE: Duration = Duration::from_secs(2);
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    let started = Instant::now();
    let helper = site.spawn_reload(&new_conf, &["10", "2"]);
    await_new_pid(&site.pidfile, old.pid());
    let handed = Instant::now();
    let status = old.wait_exit(DRAIN_PATIENCE);
    let held = handed.elapsed();
    let output = finish(helper);
    site.assert_reload_ok(&output);
    assert_eq!(status.code(), Some(0));
    // The pid file is polled every 5 ms; the old process exits within
    // milliseconds of its SIGHUP.
    assert!(held >= GRACE - Duration::from_millis(100), "{held:?}");
    assert!(started.elapsed() >= GRACE, "{:?}", started.elapsed());
}

/// Review M2: a helper killed before the handover (here while it waits
/// for a new process stuck in its startup) leaves that process listed in
/// `<pidfile>.draining`, so the OpenRC script's `stop_post` ends it; the
/// old process is untouched.
#[test]
fn a_new_process_of_a_killed_helper_is_ended_by_stop_post() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let stuck = site.config("fifo", &[addr], "new");
    let text_of_conf = fs::read_to_string(&stuck).unwrap();
    fs::remove_file(&stuck).unwrap();
    let made = Command::new("mkfifo").arg(&stuck).status().unwrap();
    assert!(made.success());
    let old = site.start(&old_conf);
    // `--check` gets the config; the new process then blocks opening it.
    let feeder = {
        let stuck = stuck.clone();
        thread::spawn(move || fs::write(stuck, text_of_conf).unwrap())
    };
    let mut helper = site.spawn_reload(&stuck, &["30"]);
    feeder.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let new = loop {
        if let Some(&new) = site.draining().first() {
            break new;
        }
        assert!(Instant::now() < deadline, "the new pid was never listed");
        thread::sleep(Duration::from_millis(5));
    };
    assert_ne!(new, old.pid());
    signal_pid(helper.id(), "KILL");
    let _ = helper.wait();
    assert!(alive(new), "the new process is gone already");
    let output = site.stop_post("TERM/2/KILL/2");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!alive(new), "stop_post left the new process");
    assert!(alive(old.pid()), "stop_post ended the old process");
    assert!(!site.draining_file().exists());
    assert_eq!(site.pid(), old.pid());
}

/// Review M4: a lock left by a helper that no longer runs is taken over,
/// with a note on stderr, and the reload goes on.
#[test]
fn a_stale_lock_is_taken_over() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let _old = site.start(&old_conf);
    let lock = site.dir.path().join("durchlauferhitzer.pid.lock");
    let mut reused = Command::new("sleep").arg("30").spawn().unwrap();
    // A pid above any pid_max (not running), without a start time; a
    // running pid with another start time (reused: review minor 6).
    for (pid, holder) in [
        ("4194304".to_owned(), "4194304\n".to_owned()),
        (reused.id().to_string(), format!("{} 1\n", reused.id())),
    ] {
        fs::create_dir(&lock).unwrap();
        fs::write(lock.join("pid"), holder).unwrap();
        let output = site.reload(&new_conf);
        site.assert_reload_ok(&output);
        assert_eq!(
            text(&output.stderr),
            format!(
                "durchlauferhitzer-reload: taking over the stale lock {} (pid {pid} is not running)\n",
                lock.display()
            )
        );
        assert!(!lock.exists(), "the lock was left behind");
        assert_eq!(fresh_request(addr, &["new"]), Ok("new".to_owned()));
    }
    let _ = reused.kill();
    let _ = reused.wait();
}

/// Review B-M4, FR-PROC-07 "What a restart may change": the new config
/// adds a `listen` address and changes `user` (to `daemon` where it
/// exists); nothing on the old address is lost, and the new address is
/// served by the new process.
#[test]
fn a_reload_may_add_a_listen_address_and_change_user() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let added = loopback(free_port());
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr, added], "new");
    let user = match lookup_user("daemon") {
        Ok(Some(_)) => "daemon",
        _ => "nobody",
    };
    let new_conf = site.dir.config(
        "new-user.conf",
        &fs::read_to_string(&new_conf)
            .unwrap()
            .replace("user nobody;", &format!("user {user};")),
        Containment::AsWritten,
    );
    let mut old = site.start(&old_conf);
    let load = Load::start(addr, &["old", "new"]);
    warm_up();
    site.assert_reload_ok(&site.reload(&new_conf));
    let status = old.wait_exit(DRAIN_PATIENCE);
    thread::sleep(Duration::from_millis(200));
    let load = load.finish();
    assert_eq!(status.code(), Some(0));
    assert!(load.failures.is_empty(), "load: {:?}", load.failures);
    assert!(load.answers.get("new").is_some_and(|&n| n > 0), "{load:?}");
    assert_eq!(fresh_request(added, &["new"]), Ok("new".to_owned()));
    assert_eq!(fresh_request(addr, &["new"]), Ok("new".to_owned()));
    let new = site.pid();
    let (uid, _) = lookup_user(user).unwrap().unwrap();
    let status = fs::read_to_string(format!("/proc/{new}/status")).unwrap();
    let real_uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().next())
        .unwrap();
    assert_eq!(real_uid, uid.to_string(), "{status}");
}

/// How many times two helpers race for one stale lock.
const LOCK_RACES: usize = 30;

/// Review M4 follow-up: two helpers that find the same stale lock at
/// once never both hold it. A holder blocks in `--check` on a FIFO
/// config; the race has settled once every helper still running is in
/// its `--check` (a process of the site's binary). Two such processes at
/// once would be two holders. Then the FIFO gets an invalid config for
/// each holder: each exits 1, a holder with "the config check failed",
/// a loser refused as "another reload".
#[test]
fn two_helpers_racing_for_a_stale_lock_never_both_hold_it() {
    let _serial = serial();
    let mut holders = [0usize; 3];
    for _ in 0..LOCK_RACES {
        let site = Site::new();
        let conf = site.dir.path().join("fifo.conf");
        let made = Command::new("mkfifo").arg(&conf).status().unwrap();
        assert!(made.success());
        let lock = site.dir.path().join("durchlauferhitzer.pid.lock");
        fs::create_dir(&lock).unwrap();
        // Above any pid_max: not running.
        fs::write(lock.join("pid"), "4194304\n").unwrap();
        let mut helpers = [site.spawn_reload(&conf, &[]), site.spawn_reload(&conf, &[])];
        let deadline = Instant::now() + Duration::from_secs(15);
        let checking = loop {
            let running = helpers
                .iter_mut()
                .map(|helper| helper.try_wait().unwrap().is_none())
                .filter(|&runs| runs)
                .count();
            let checking = site.processes().len();
            if checking == running {
                break checking;
            }
            assert!(Instant::now() < deadline, "the race never settled");
            thread::sleep(Duration::from_millis(5));
        };
        holders[checking] += 1;
        // Feed every reader of the FIFO until the helpers have ended.
        let deadline = Instant::now() + Duration::from_secs(15);
        while helpers
            .iter_mut()
            .any(|helper| helper.try_wait().unwrap().is_none())
        {
            if let Ok(mut fifo) = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&conf)
            {
                let _ = fifo.write_all(b"nonsense;\n");
            }
            assert!(Instant::now() < deadline, "a helper never ended");
            thread::sleep(Duration::from_millis(5));
        }
        let outputs: Vec<String> = helpers
            .into_iter()
            .map(|helper| {
                let output = finish(helper);
                assert_eq!(output.status.code(), Some(1), "{output:?}");
                text(&output.stderr)
            })
            .collect();
        for stderr in &outputs {
            assert!(
                stderr.contains("durchlauferhitzer-reload: the config check failed")
                    || stderr.contains("durchlauferhitzer-reload: another reload "),
                "{outputs:#?}"
            );
        }
        assert!(!lock.exists(), "the lock was left behind: {outputs:#?}");
    }
    eprintln!("races with 0, 1, 2 holders at once: {holders:?}");
    assert_eq!(holders[2], 0, "both helpers held the lock: {holders:?}");
}

/// Review minor 1: a helper started with SIGHUP ignored (`nohup`) passes
/// that on to the new process, whose SIGHUP then stops nothing: after
/// its 10 s wait the helper leaves it running, listed in
/// `<pidfile>.draining` with a warning, and `stop_post` ends it.
#[test]
fn a_new_process_that_ignores_sighup_is_left_to_stop_post() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let stuck = site.config("fifo", &[addr], "new");
    let text_of_conf = fs::read_to_string(&stuck).unwrap();
    fs::remove_file(&stuck).unwrap();
    let made = Command::new("mkfifo").arg(&stuck).status().unwrap();
    assert!(made.success());
    let old = site.start(&old_conf);
    let feeder = {
        let stuck = stuck.clone();
        thread::spawn(move || fs::write(stuck, text_of_conf).unwrap())
    };
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(r#"trap '' HUP; exec sh "$@""#)
        .arg("sh")
        .arg(HELPER)
        .args([&site.bin, &stuck, &site.pidfile, &site.log])
        .arg("1")
        .env("TMPDIR", site.tmp());
    let output = bounded(cmd);
    feeder.join().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("durchlauferhitzer-reload: the new process was not ready within 1 s"),
        "{stderr}"
    );
    let new = *site.draining().first().unwrap();
    assert!(
        stderr.contains(&format!(
            "durchlauferhitzer-reload: warning: the new process (pid {new}) still runs"
        )),
        "{stderr}"
    );
    assert!(alive(new));
    assert_eq!(site.pid(), old.pid());
    let output = site.stop_post("TERM/2/KILL/2");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!alive(new), "stop_post left the new process");
    assert!(alive(old.pid()), "stop_post ended the old process");
    assert_eq!(fresh_request(addr, &["old"]), Ok("old".to_owned()));
}

/// Review minor 4: the new and the old process both end during the
/// grace: exit 1, no success claimed.
#[test]
fn both_processes_ending_during_the_grace_is_exit_1() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    let helper = site.spawn_reload(&new_conf, &["10", "3"]);
    let new = await_new_pid(&site.pidfile, old.pid());
    signal_pid(new, "KILL");
    signal_pid(old.pid(), "KILL");
    let _ = old.child.wait();
    let output = finish(helper);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        text(&output.stderr),
        format!(
            "durchlauferhitzer-reload: the new process (pid {new}) and the old one (pid {}) \
             both ended during the grace: nothing serves (see {})\n",
            old.pid(),
            site.log.display()
        )
    );
}
