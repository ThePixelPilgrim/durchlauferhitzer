//! `sys::seccomp::install` on the real kernel: this process filters
//! itself, so the test has its own `main` (no libtest harness, one test).
//! A thread started before the install is filtered too (TSYNC).
//!
//! The program allows everything but `getppid`, which fails with `EPERM`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test binary; clippy.toml allows these in tests"
)]

use std::fs;
use std::io;
use std::os::unix::process::parent_id;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;

#[cfg(target_arch = "aarch64")]
use durchlauferhitzer::sys::seccomp::AUDIT_ARCH_AARCH64 as AUDIT_ARCH;
#[cfg(target_arch = "x86_64")]
use durchlauferhitzer::sys::seccomp::AUDIT_ARCH_X86_64 as AUDIT_ARCH;
use durchlauferhitzer::sys::seccomp::{Support, install, supported};

const NAME: &str = "install_filters_every_thread";

fn stmt(code: u32, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump_eq(k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: u16::try_from(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K).unwrap(),
        jt,
        jf,
        k,
    }
}

/// Everything allowed but `getppid` (`EPERM`); another arch is killed.
fn program() -> Vec<libc::sock_filter> {
    let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
    let ret = libc::BPF_RET | libc::BPF_K;
    let eperm = u32::try_from(libc::EPERM).unwrap();
    vec![
        stmt(load, 4),
        jump_eq(AUDIT_ARCH, 1, 0),
        stmt(ret, libc::SECCOMP_RET_KILL_PROCESS),
        stmt(load, 0),
        jump_eq(u32::try_from(libc::SYS_getppid).unwrap(), 0, 1),
        stmt(ret, libc::SECCOMP_RET_ERRNO | eperm),
        stmt(ret, libc::SECCOMP_RET_ALLOW),
    ]
}

/// `getppid` fails with `EPERM`: glibc's wrapper cannot fail, so it
/// returns the kernel's `-EPERM` (-1, no errno), which std casts to
/// `u32::MAX`.
fn getppid_is_refused() -> Result<(), String> {
    let ppid = parent_id();
    if ppid == 0_u32.wrapping_sub(libc::EPERM.unsigned_abs()) {
        Ok(())
    } else {
        Err(format!("getppid returned {ppid}"))
    }
}

/// The `Seccomp:` mode of every thread (`/proc/self/task/*/status`).
fn modes() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for task in fs::read_dir("/proc/self/task").unwrap() {
        let task = task.unwrap();
        let status = fs::read_to_string(task.path().join("status")).unwrap();
        let mode = status
            .lines()
            .find_map(|line| line.strip_prefix("Seccomp:"))
            .unwrap()
            .trim()
            .to_owned();
        out.push((task.file_name().to_string_lossy().into_owned(), mode));
    }
    out
}

fn check() -> Result<(), String> {
    if supported() != Support::Supported {
        return Err("supported() is not Supported on this kernel".to_owned());
    }
    if parent_id() == u32::MAX {
        return Err("getppid fails before the install".to_owned());
    }
    // A thread that exists before the install and checks afterwards.
    let (go, wait) = mpsc::channel::<()>();
    let other = thread::spawn(move || {
        wait.recv().unwrap();
        getppid_is_refused()
    });
    install(&program()).map_err(|err| format!("install: {err}"))?;
    getppid_is_refused().map_err(|err| format!("main thread: {err}"))?;
    let modes = modes();
    if modes.len() != 2 || modes.iter().any(|(_, mode)| mode != "2") {
        return Err(format!("Seccomp modes per thread: {modes:?}"));
    }
    go.send(()).unwrap();
    other
        .join()
        .unwrap()
        .map_err(|err| format!("second thread: {err}"))?;
    // An empty program, or one longer than the kernel takes, is refused.
    for bad in [
        Vec::new(),
        vec![stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW); 4097],
    ] {
        match install(&bad) {
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => {}
            other => return Err(format!("{} instructions: {other:?}", bad.len())),
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match check() {
        Ok(()) => {
            println!("test {NAME} ... ok");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("test {NAME} ... FAILED: {err}");
            ExitCode::FAILURE
        }
    }
}
