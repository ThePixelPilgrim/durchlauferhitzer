//! The seccomp allow-list (FR-PROC-06) as a classic BPF program: pure, so
//! it is tested by running it on synthetic calls, never by installing it.
//!
//! The program checks `seccomp_data.arch` first (a foreign architecture,
//! e.g. a 32-bit `int 0x80` call, gets the action), then compares the
//! call number with each entry of the architecture's table. An entry
//! either allows the call or checks its key arguments; anything else gets
//! the action: `EPERM` under `fail`, the end of the process under `kill`.
//!
//! The tables list exactly the calls the server makes once the filter is
//! installed (right before the loop runs): derived with `strace -f` over
//! the driver tests, plus the rare paths named per group. Calls made only
//! during startup are not listed. Never listed: `truncate`, `ftruncate`
//! and `ioctl` (they cover Landlock's gaps before ABI 3 and 5), `execve`,
//! and stats by path (`newfstatat`, `statx`).

use durchlauferhitzer_core::config::SeccompAction;

use crate::sys::seccomp::{AUDIT_ARCH_AARCH64, AUDIT_ARCH_X86_64};

/// The architectures the filter has a table for (NFR-PORT-01).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// The architecture this binary runs on; `None` on any other (and on
    /// big-endian aarch64: the argument checks read the low word of each
    /// argument at its little-endian offset).
    pub const fn current() -> Option<Self> {
        if cfg!(target_endian = "big") {
            None
        } else if cfg!(target_arch = "x86_64") {
            Some(Self::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Self::Aarch64)
        } else {
            None
        }
    }

    /// The `AUDIT_ARCH_*` value the kernel puts in `seccomp_data.arch`.
    const fn audit(self) -> u32 {
        match self {
            Self::X86_64 => AUDIT_ARCH_X86_64,
            Self::Aarch64 => AUDIT_ARCH_AARCH64,
        }
    }

    fn table(self) -> &'static [Entry] {
        match self {
            Self::X86_64 => X86_64,
            Self::Aarch64 => AARCH64,
        }
    }
}

/// What the filter does with a listed call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    /// Allowed with any arguments.
    Allow,
    /// `socket`: domain `AF_INET` or `AF_INET6`, type `SOCK_STREAM`
    /// (`SOCK_NONBLOCK` and `SOCK_CLOEXEC` masked off), protocol 0.
    Socket,
    /// `mmap`, `mprotect`: `prot` (argument 2) without `PROT_EXEC`.
    NoExec,
    /// `clone`: exactly the flags glibc's `pthread_create` passes.
    Thread,
    /// `prctl`: only the options in [`PRCTL_OPTIONS`].
    Prctl,
    /// `fcntl`: only the commands in [`FCNTL_COMMANDS`].
    Fcntl,
    /// `tgkill`: only `SIGABRT` (glibc's `abort`).
    Abort,
    /// `clone3`: `ENOSYS` whatever the action, so glibc falls back to
    /// `clone`, whose flags can be checked (`clone3` passes them in
    /// memory, out of the filter's reach).
    Enosys,
}

/// One call of a table: its name (for tests and readers), number, rule.
#[derive(Clone, Copy, Debug)]
struct Entry {
    #[cfg_attr(not(test), expect(dead_code, reason = "for readers and the tests"))]
    name: &'static str,
    nr: u32,
    rule: Rule,
}

const fn e(name: &'static str, nr: u32, rule: Rule) -> Entry {
    Entry { name, nr, rule }
}

/// x86_64 (`strace -f` over the driver tests on Ubuntu 24.04, glibc 2.39).
const X86_64: &[Entry] = &[
    // The loop: epoll, and reads and writes of the wake eventfd, the
    // signalfd, the sockets and the log on stderr; closing descriptors.
    e("epoll_wait", 232, Rule::Allow),
    e("epoll_ctl", 233, Rule::Allow),
    e("read", 0, Rule::Allow),
    e("write", 1, Rule::Allow),
    e("close", 3, Rule::Allow),
    // Clients and upstreams: accept, connect (`SO_ERROR`), `TCP_NODELAY`,
    // `send` (glibc: `sendto`), `shutdown`, `sendfile` of static files.
    e("accept4", 288, Rule::Allow),
    e("socket", 41, Rule::Socket),
    e("connect", 42, Rule::Allow),
    e("getsockopt", 55, Rule::Allow),
    e("setsockopt", 54, Rule::Allow),
    e("sendto", 44, Rule::Allow),
    e("shutdown", 48, Rule::Allow),
    e("sendfile", 40, Rule::Allow),
    // The log sink: `poll` in the bounded final flush; the status flags
    // put back on drop (`F_GETFL`, `F_SETFL`); std's check before each
    // close in debug builds (`F_GETFD`).
    e("poll", 7, Rule::Allow),
    e("fcntl", 72, Rule::Fcntl),
    // Disk workers: open beneath a root, `fstat` (called directly, so
    // never glibc's `newfstatat`, which takes a path), `readahead`.
    e("openat2", 437, Rule::Allow),
    e("fstat", 5, Rule::Allow),
    e("readahead", 187, Rule::Allow),
    // Memory: the allocator (`brk`, `mmap`, `munmap`, `mremap` for a
    // large realloc, `madvise`), thread stacks and their guard pages.
    e("brk", 12, Rule::Allow),
    e("mmap", 9, Rule::NoExec),
    e("mprotect", 10, Rule::NoExec),
    e("munmap", 11, Rule::Allow),
    e("mremap", 25, Rule::Allow),
    e("madvise", 28, Rule::Allow),
    // Locks and channels (`futex`; a channel's back-off yields), the
    // pool join's 1 ms sleeps; a sleep restarted after SIGSTOP/SIGCONT.
    e("futex", 202, Rule::Allow),
    e("sched_yield", 24, Rule::Allow),
    e("clock_nanosleep", 230, Rule::Allow),
    e("restart_syscall", 219, Rule::Allow),
    // A disk worker's start and end: it may still be starting when the
    // filter lands (TSYNC covers it); thread name (`PR_SET_NAME`).
    e("set_robust_list", 273, Rule::Allow),
    e("rseq", 334, Rule::Allow),
    e("gettid", 186, Rule::Allow),
    e("sched_getaffinity", 204, Rule::Allow),
    e("sigaltstack", 131, Rule::Allow),
    e("rt_sigprocmask", 14, Rule::Allow),
    e("prctl", 157, Rule::Prctl),
    e("exit", 60, Rule::Allow),
    // New threads: none are started after startup; if one were, only as
    // a thread.
    e("clone", 56, Rule::Thread),
    e("clone3", 435, Rule::Enosys),
    // Fatal paths, so they end the process as meant, not in stray code:
    // std's fault handlers (stack overflow) reset themselves and return;
    // `abort` (also on OOM) is glibc's `tgkill(getpid(), gettid(),
    // SIGABRT)`.
    e("rt_sigreturn", 15, Rule::Allow),
    e("rt_sigaction", 13, Rule::Allow),
    e("getpid", 39, Rule::Allow),
    e("tgkill", 234, Rule::Abort),
    // The end of the process.
    e("exit_group", 231, Rule::Allow),
];

/// aarch64: the same set. This architecture has no `epoll_wait` and no
/// `poll`; glibc uses `epoll_pwait` and `ppoll` for them.
const AARCH64: &[Entry] = &[
    // The loop.
    e("epoll_pwait", 22, Rule::Allow),
    e("epoll_ctl", 21, Rule::Allow),
    e("read", 63, Rule::Allow),
    e("write", 64, Rule::Allow),
    e("close", 57, Rule::Allow),
    // Clients and upstreams.
    e("accept4", 242, Rule::Allow),
    e("socket", 198, Rule::Socket),
    e("connect", 203, Rule::Allow),
    e("getsockopt", 209, Rule::Allow),
    e("setsockopt", 208, Rule::Allow),
    e("sendto", 206, Rule::Allow),
    e("shutdown", 210, Rule::Allow),
    e("sendfile", 71, Rule::Allow),
    // The log sink.
    e("ppoll", 73, Rule::Allow),
    e("fcntl", 25, Rule::Fcntl),
    // Disk workers.
    e("openat2", 437, Rule::Allow),
    e("fstat", 80, Rule::Allow),
    e("readahead", 213, Rule::Allow),
    // Memory.
    e("brk", 214, Rule::Allow),
    e("mmap", 222, Rule::NoExec),
    e("mprotect", 226, Rule::NoExec),
    e("munmap", 215, Rule::Allow),
    e("mremap", 216, Rule::Allow),
    e("madvise", 233, Rule::Allow),
    // Locks, channels, sleeps.
    e("futex", 98, Rule::Allow),
    e("sched_yield", 124, Rule::Allow),
    e("clock_nanosleep", 115, Rule::Allow),
    e("restart_syscall", 128, Rule::Allow),
    // A disk worker's start and end.
    e("set_robust_list", 99, Rule::Allow),
    e("rseq", 293, Rule::Allow),
    e("gettid", 178, Rule::Allow),
    e("sched_getaffinity", 123, Rule::Allow),
    e("sigaltstack", 132, Rule::Allow),
    e("rt_sigprocmask", 135, Rule::Allow),
    e("prctl", 167, Rule::Prctl),
    e("exit", 93, Rule::Allow),
    // New threads.
    e("clone", 220, Rule::Thread),
    e("clone3", 435, Rule::Enosys),
    // Fatal paths.
    e("rt_sigreturn", 139, Rule::Allow),
    e("rt_sigaction", 134, Rule::Allow),
    e("getpid", 172, Rule::Allow),
    e("tgkill", 131, Rule::Abort),
    // The end of the process.
    e("exit_group", 94, Rule::Allow),
];

/// The `prctl` options the server uses after startup: a disk worker
/// naming itself (std's `Builder::name`, glibc's `pthread_setname_np`).
const PRCTL_OPTIONS: &[u32] = &[libc::PR_SET_NAME as u32];

/// The `fcntl` commands the server uses after startup (see the table).
/// These values, like every constant the argument checks use, are the
/// same on x86_64 and aarch64.
const FCNTL_COMMANDS: &[u32] = &[
    libc::F_GETFD as u32,
    libc::F_GETFL as u32,
    libc::F_SETFL as u32,
];

/// The program for `arch`: every listed call allowed (with its argument
/// checks), everything else answered by `action`.
pub fn program(arch: Arch, action: SeccompAction) -> Vec<libc::sock_filter> {
    let deny = match action {
        SeccompAction::Fail => libc::SECCOMP_RET_ERRNO | errno(libc::EPERM),
        SeccompAction::Kill => libc::SECCOMP_RET_KILL_PROCESS,
    };
    let mut prog = vec![
        load(ARCH),
        jump(JEQ, arch.audit(), 1, 0),
        ret(deny),
        load(NR),
    ];
    // Each entry: `nr == entry ? body : skip the body`. A body ends in a
    // return, so the accumulator it loads never reaches the next entry,
    // and every jump is short (no body is longer than a few instructions).
    for entry in arch.table() {
        let body = body(entry.rule, deny);
        prog.push(jump(JEQ, entry.nr, 0, offset(body.len())));
        prog.extend(body);
    }
    prog.push(ret(deny));
    prog
}

/// `seccomp_data` offsets: `nr`, `arch`, and the low 32-bit word of each
/// argument (little-endian). The checked arguments are `int`s, or flags
/// whose upper word the kernel ignores (`clone`) or that carry no
/// `PROT_EXEC` there (`prot`), so the low word decides.
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg(i: u32) -> u32 {
    16 + 8 * i
}

const JEQ: u32 = libc::BPF_JEQ;
const JSET: u32 = libc::BPF_JSET;

/// glibc's `pthread_create` flags (`clone`, glibc ≥ 2.34).
const THREAD_FLAGS: u32 = (libc::CLONE_VM
    | libc::CLONE_FS
    | libc::CLONE_FILES
    | libc::CLONE_SIGHAND
    | libc::CLONE_THREAD
    | libc::CLONE_SYSVSEM
    | libc::CLONE_SETTLS
    | libc::CLONE_PARENT_SETTID
    | libc::CLONE_CHILD_CLEARTID) as u32;

/// The instructions after an entry's number matched.
fn body(rule: Rule, deny: u32) -> Vec<libc::sock_filter> {
    match rule {
        Rule::Allow => vec![ret(libc::SECCOMP_RET_ALLOW)],
        Rule::Enosys => vec![ret(libc::SECCOMP_RET_ERRNO | errno(libc::ENOSYS))],
        Rule::Socket => vec![
            load(arg(0)),
            jump(JEQ, libc::AF_INET as u32, 1, 0),
            jump(JEQ, libc::AF_INET6 as u32, 0, 6),
            load(arg(1)),
            alu_and(!((libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) as u32)),
            jump(JEQ, libc::SOCK_STREAM as u32, 0, 3),
            load(arg(2)),
            jump(JEQ, 0, 0, 1),
            ret(libc::SECCOMP_RET_ALLOW),
            ret(deny),
        ],
        Rule::NoExec => vec![
            load(arg(2)),
            jump(JSET, libc::PROT_EXEC as u32, 1, 0),
            ret(libc::SECCOMP_RET_ALLOW),
            ret(deny),
        ],
        Rule::Thread => vec![
            load(arg(0)),
            jump(JEQ, THREAD_FLAGS, 0, 1),
            ret(libc::SECCOMP_RET_ALLOW),
            ret(deny),
        ],
        Rule::Prctl => one_of(arg(0), PRCTL_OPTIONS, deny),
        Rule::Fcntl => one_of(arg(1), FCNTL_COMMANDS, deny),
        Rule::Abort => one_of(arg(2), &[libc::SIGABRT as u32], deny),
    }
}

/// Allows the call when the word at `at` is one of `values`.
fn one_of(at: u32, values: &[u32], deny: u32) -> Vec<libc::sock_filter> {
    let n = values.len();
    let mut out = vec![load(at)];
    // The i-th compare jumps over the rest and the deny to the allow.
    for (i, value) in values.iter().enumerate() {
        out.push(jump(JEQ, *value, offset(n - i), 0));
    }
    out.push(ret(deny));
    out.push(ret(libc::SECCOMP_RET_ALLOW));
    out
}

const fn errno(code: i32) -> u32 {
    code as u32 & libc::SECCOMP_RET_DATA
}

/// A jump offset. Every body is at most 255 instructions (the test
/// `every_body_fits_under_a_jump` fails otherwise), so the conversion
/// never saturates.
fn offset(n: usize) -> u8 {
    u8::try_from(n).unwrap_or(u8::MAX)
}

const fn stmt(code: u32, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn load(at: u32) -> libc::sock_filter {
    stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, at)
}

const fn alu_and(k: u32) -> libc::sock_filter {
    stmt(libc::BPF_ALU | libc::BPF_AND | libc::BPF_K, k)
}

const fn ret(k: u32) -> libc::sock_filter {
    stmt(libc::BPF_RET | libc::BPF_K, k)
}

const fn jump(op: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_JMP | op | libc::BPF_K) as u16,
        jt,
        jf,
        k,
    }
}

#[cfg(test)]
mod tests;
