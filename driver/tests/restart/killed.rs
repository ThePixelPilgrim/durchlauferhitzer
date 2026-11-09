//! The helper killed, or failing, between starting the new process and
//! listing it (FR-PROC-07 M2; stage 8 final review, wave C), a module of
//! `restart.rs` (one test binary, one `serial` lock). Stubs first on the
//! helper's `PATH` hold it, or the new process, in a chosen window:
//!
//! - a listed pid that is still the helper's fork on its way to the
//!   binary (a gated `setsid`) is ended by OpenRC's `stop_post`;
//! - a helper killed before listing it (a gated `mv`) leaves nothing
//!   running;
//! - a list that cannot be written (a failing `mv`) is exit 1, and the
//!   binary never starts (a recording `setsid`).

use std::process::Child;

use super::*;

/// A stub that creates `<gate>.entered`, then blocks opening the FIFO
/// `gate` for reading (the tests never open it for writing), then would
/// run the real `name`.
fn gated(name: &str, gate: &Path) -> String {
    format!(
        ": >'{gate}.entered'\n: <'{gate}'\nPATH=${{PATH#*:}}\nexec {name} \"$@\"\n",
        gate = gate.display()
    )
}

/// Whether a [`gated`] stub reached its `gate`.
fn gate_entered(gate: &Path) -> bool {
    gate.with_extension("entered").exists()
}

/// Writes `stubs` (name, `sh` body) into a directory of their own and
/// returns a `PATH` with it first, or `None` (with a note) when `sh`
/// runs a built-in of one of the names instead (a busybox built to
/// prefer its applets, as Ubuntu's; dash and Alpine's busybox take the
/// `PATH`). That skip fails with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`
/// (CI): there `sh` must take the `PATH`.
fn stub_path(site: &Site, stubs: &[(&str, String)]) -> Option<String> {
    let bin_dir = site.dir.dir("stub-bin");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    for (name, body) in stubs {
        let stub = bin_dir.join(name);
        fs::write(&stub, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let found = Command::new("sh")
            .arg("-c")
            .arg(r#"command -v "$1""#)
            .arg("sh")
            .arg(name)
            .env("PATH", &path)
            .output()
            .unwrap();
        if text(&found.stdout).trim_end() != stub.display().to_string() {
            assert!(
                !required(REQUIRE_ROOT),
                "sh does not run {name} from PATH, but {REQUIRE_ROOT}=1 requires the test"
            );
            eprintln!("skipped: sh does not run {name} from PATH");
            return None;
        }
    }
    Some(path)
}

/// Starts the helper as [`Site::spawn_reload`] does, with `path` as its
/// `PATH`.
fn spawn_reload_with_path(site: &Site, conf: &Path, path: &str) -> Child {
    Command::new("sh")
        .arg(HELPER)
        .args([&site.bin, conf, &site.pidfile, &site.log])
        .arg("30")
        .env("TMPDIR", site.tmp())
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// On a failure path, ends what a test left blocked: the helper and the
/// processes it [`Reaper::adds`] (each only while it still runs with
/// its start time then, not a reused pid).
struct Reaper {
    helper: Child,
    others: Vec<(u32, String)>,
}

impl Reaper {
    fn adds(&mut self, pid: u32) {
        if let Some(start) = start_time(pid) {
            self.others.push((pid, start));
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        // The helper's children too (a fork or stub already blocked),
        // read while it still runs, so its pid names it.
        if matches!(self.helper.try_wait(), Ok(None)) {
            for child in children(self.helper.id()) {
                self.adds(child);
            }
        }
        let _ = self.helper.kill();
        let _ = self.helper.wait();
        for (pid, start) in &self.others {
            if start_time(*pid).as_deref() == Some(start.as_str()) {
                let _ = Command::new("kill")
                    .args(["-s", "KILL", &pid.to_string()])
                    .status();
            }
        }
    }
}

/// The command line of `pid`, its arguments joined by spaces (empty if
/// it is gone).
fn cmdline(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
        .unwrap_or_default()
}

/// The running children of `pid`.
fn children(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap() {
        let Ok(entry) = entry else { continue };
        let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let parent = stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.split(' ').nth(1))
            .and_then(|ppid| ppid.parse::<u32>().ok());
        if parent == Some(pid) && alive(child) {
            found.push(child);
        }
    }
    found
}

/// The new pid is listed while it is still the helper's fork on its way
/// to the binary (here held in a gated `setsid` stub, a shell script). A
/// helper killed then leaves that process to `stop_post`, which must end
/// it by its pid and start time, though it does not run the binary
/// (yet): it never gets to.
#[test]
fn a_killed_helpers_fork_is_ended_by_stop_post_before_it_runs_the_binary() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let old = site.start(&old_conf);
    let gate = site.dir.path().join("gate");
    let made = Command::new("mkfifo").arg(&gate).status().unwrap();
    assert!(made.success());
    let Some(path) = stub_path(&site, &[("setsid", gated("setsid", &gate))]) else {
        return;
    };
    let stub = site
        .dir
        .path()
        .join("stub-bin/setsid")
        .display()
        .to_string();
    let mut reaper = Reaper {
        helper: spawn_reload_with_path(&site, &new_conf, &path),
        others: Vec::new(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let new = loop {
        if let Some(&new) = site.draining().first()
            && cmdline(new).contains(&stub)
            && gate_entered(&gate)
        {
            break new;
        }
        assert!(
            Instant::now() < deadline,
            "the stub never ran as the listed pid"
        );
        thread::sleep(Duration::from_millis(5));
    };
    reaper.adds(new);
    signal_pid(reaper.helper.id(), "KILL");
    let _ = reaper.helper.wait();
    let output = site.stop_post("TERM/2/KILL/2");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!alive(new), "stop_post left the new process");
    assert_eq!(site.processes(), vec![old.pid()], "the binary ran");
    assert!(!site.draining_file().exists());
    assert_eq!(site.pid(), old.pid());
}

/// A helper killed between starting the new process and listing it
/// (here held in a gated `mv` stub, the list's rename) leaves no shell
/// behind: its fork, still waiting for the helper's go, ends, and never
/// runs the binary.
#[test]
fn a_helper_killed_before_its_fork_is_listed_leaves_nothing_behind() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let old = site.start(&old_conf);
    let gate = site.dir.path().join("gate");
    let made = Command::new("mkfifo").arg(&gate).status().unwrap();
    assert!(made.success());
    let Some(path) = stub_path(&site, &[("mv", gated("mv", &gate))]) else {
        return;
    };
    let stub = site.dir.path().join("stub-bin/mv").display().to_string();
    let mut reaper = Reaper {
        helper: spawn_reload_with_path(&site, &new_conf, &path),
        others: Vec::new(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mv, forks) = loop {
        let kids = children(reaper.helper.id());
        let (stubs, others): (Vec<u32>, Vec<u32>) = kids
            .into_iter()
            .partition(|&kid| cmdline(kid).contains(&stub));
        if let [mv] = stubs[..]
            && gate_entered(&gate)
        {
            break (mv, others);
        }
        assert!(Instant::now() < deadline, "the mv stub never ran");
        thread::sleep(Duration::from_millis(5));
    };
    reaper.adds(mv);
    for &fork in &forks {
        reaper.adds(fork);
    }
    assert_eq!(forks.len(), 1, "the helper's children: {forks:?}");
    let fork = forks[0];
    signal_pid(reaper.helper.id(), "KILL");
    let _ = reaper.helper.wait();
    signal_pid(mv, "KILL");
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(fork) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!alive(fork), "the helper's fork was left behind");
    assert_eq!(site.processes(), vec![old.pid()], "the binary ran");
    assert_eq!(site.pid(), old.pid());
}

/// A new process the helper cannot list in `<pidfile>.draining` (here
/// every `mv` fails, so the list's rename does) is never started: exit 1
/// with the reason, its fork ends without running `setsid` (a stub that
/// records a call), and the old process serves on.
#[test]
fn a_new_process_that_cannot_be_listed_never_runs_the_binary() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let old = site.start(&old_conf);
    let ran = site.dir.path().join("setsid.ran");
    let recording = format!(
        ": >'{}'\nPATH=${{PATH#*:}}\nexec setsid \"$@\"\n",
        ran.display()
    );
    let stubs = [("mv", "exit 1\n".to_owned()), ("setsid", recording)];
    let Some(path) = stub_path(&site, &stubs) else {
        return;
    };
    let output = finish(spawn_reload_with_path(&site, &new_conf, &path));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = text(&output.stderr);
    let prefix = "durchlauferhitzer-reload: cannot list the new process (pid ";
    let rest = stderr
        .strip_prefix(prefix)
        .unwrap_or_else(|| panic!("{stderr}"));
    let (pid, rest) = rest.split_once(')').unwrap();
    let fork: u32 = pid.parse().unwrap();
    assert_eq!(
        rest,
        format!(" in {}\n", site.draining_file().display()),
        "{stderr}"
    );
    assert!(!ran.exists(), "the fork ran setsid");
    assert!(!alive(fork), "the helper's fork still runs");
    assert_eq!(site.processes(), vec![old.pid()], "the binary ran");
    assert_eq!(site.pid(), old.pid());
    assert_eq!(fresh_request(addr, &["old"]), Ok("old".to_owned()));
}
