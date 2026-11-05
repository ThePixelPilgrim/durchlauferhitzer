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
//! - A `listen` address the new config drops is closed (documented).
//! - Steering: a ready new process gets every new connection before any
//!   SIGHUP; fresh clients across back-to-back reloads lose nothing (a
//!   kernel race resets a handshake completing on a listener as it
//!   closes); the helper keeps its lock until the old listeners closed.
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
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use child::{Containment, Running, exe, free_port, signal_pid, text};
use load::{Load, Pattern, Tally, collect, connect, fresh_request, queue, read_response, warm_up};
use site::{
    HELPER, Migration, OPENRC, Site, alive, can_restart, hup_pending, loopback, reload_with,
    serial, set_migrate, start_time, wait_gone,
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

/// FR-PROC-07 "Steering": many fresh connections across several reloads
/// lose nothing (a connect reset at the old listener's close was the
/// kernel race steering avoids).
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

/// A second helper while one runs (its lock exists) is refused; the lock
/// is only removed by the run that took it.
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
    fs::create_dir(&lock).unwrap();
    fs::write(lock.join("pid"), "12345\n").unwrap();
    let output = site.reload(&new_conf);
    assert_refused(
        &site,
        &mut old,
        &output,
        "another reload is running (pid 12345;",
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
}
