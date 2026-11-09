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
//! during startup are not listed. Never listed: `ioctl` (it covers
//! Landlock's gap before ABI 5), `truncate` and `ftruncate` (they narrow,
//! but cannot close, the gap before ABI 3: `openat2` with `O_TRUNC` is out
//! of the filter's reach, FR-PROC-06), `execve`, and stats by path
//! (`newfstatat`, `statx`).

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
    /// `clone`: exactly the flags a libc's `pthread_create` passes (two sets,
    /// accepted for every libc).
    Thread,
    /// `prctl`: only the options in [`PRCTL_OPTIONS`].
    Prctl,
    /// `fcntl`: only the commands in [`FCNTL_COMMANDS`].
    Fcntl,
    /// `setsockopt`: only `IPPROTO_TCP`/`TCP_NODELAY` (the options set
    /// on listeners, `SO_REUSEADDR` and `IPV6_V6ONLY`, come before the
    /// filter).
    NoDelay,
    /// `getsockopt`: only `SOL_SOCKET`/`SO_ERROR` (a connect's outcome).
    SoError,
    /// `recvfrom`: only with flags `MSG_PEEK | MSG_DONTWAIT` (argument 3),
    /// the silence check of a pooled connection before it is reused
    /// (FR-PROXY-02); reads use `read`.
    Peek,
    /// `futex`: only the operations in [`FUTEX_OPERATIONS`].
    Futex,
    /// `tgkill`: only `SIGABRT` (glibc's `abort`) to the server's own
    /// process.
    Abort,
    /// `tkill`: only `SIGABRT` (musl's `abort`, `raise`) to the server's
    /// main thread, whose id is the pid: `tkill` names no process, so a
    /// worker's id cannot be told from another process's thread.
    #[cfg(target_env = "musl")]
    AbortMain,
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
/// Entries under `cfg(target_env = "musl")` replace or add to glibc's
/// where musl makes other calls (Alpine 3.24, musl 1.2.6, and Rust's
/// static musl 1.2.5; NFR-PORT-02),
/// each naming the musl function.
const X86_64: &[Entry] = &[
    // The loop: epoll, and reads and writes of the wake eventfd, the
    // signalfd, the sockets and the log on stderr; closing descriptors.
    // glibc and musl 1.2.5 issue `epoll_wait`; musl 1.2.6 issues
    // `epoll_pwait` with no signal mask. Both are allowed for either libc:
    // a libc update under `seccomp_action kill` must not end the server.
    e("epoll_wait", 232, Rule::Allow),
    e("epoll_pwait", 281, Rule::Allow),
    e("epoll_ctl", 233, Rule::Allow),
    e("read", 0, Rule::Allow),
    e("write", 1, Rule::Allow),
    e("close", 3, Rule::Allow),
    // Clients and upstreams: accept, connect (`SO_ERROR`), `TCP_NODELAY`,
    // `send` (glibc: `sendto`), `shutdown`, `sendfile` of static files.
    e("accept4", 288, Rule::Allow),
    e("socket", 41, Rule::Socket),
    e("connect", 42, Rule::Allow),
    e("getsockopt", 55, Rule::SoError),
    e("setsockopt", 54, Rule::NoDelay),
    e("sendto", 44, Rule::Allow),
    // The silence check of a pooled upstream (`recv`: glibc and musl
    // call `recvfrom`).
    e("recvfrom", 45, Rule::Peek),
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
    e("futex", 202, Rule::Futex),
    e("sched_yield", 24, Rule::Allow),
    e("clock_nanosleep", 230, Rule::Allow),
    e("restart_syscall", 219, Rule::Allow),
    // The clock (`Instant`, `SystemTime`): the vDSO fallback, on hosts
    // whose clocksource has no vDSO mode (hpet, acpi_pm, Xen).
    e("clock_gettime", 228, Rule::Allow),
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
    // musl's `abort` is `tkill(gettid(), SIGABRT)`: allowed from the main
    // thread; from a worker it fails, and `abort` ends the process by its
    // `a_crash` (`SIGSEGV`) under `fail`, by `SIGSYS` under `kill`.
    #[cfg(target_env = "musl")]
    e("tkill", 200, Rule::AbortMain),
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
    e("getsockopt", 209, Rule::SoError),
    e("setsockopt", 208, Rule::NoDelay),
    e("sendto", 206, Rule::Allow),
    e("recvfrom", 207, Rule::Peek),
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
    e("futex", 98, Rule::Futex),
    e("sched_yield", 124, Rule::Allow),
    e("clock_nanosleep", 115, Rule::Allow),
    e("restart_syscall", 128, Rule::Allow),
    // The clock: the vDSO fallback (also arm64 timer errata).
    e("clock_gettime", 113, Rule::Allow),
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
    #[cfg(target_env = "musl")]
    e("tkill", 130, Rule::AbortMain),
    // The end of the process.
    e("exit_group", 94, Rule::Allow),
];

/// The `prctl` options the server uses after startup: a disk worker
/// naming itself (std's `Builder::name`, glibc's `pthread_setname_np`).
const PRCTL_OPTIONS: &[u32] = &[libc::PR_SET_NAME as u32];

/// The `futex` operations the server uses after startup: std's locks,
/// channels and condition variables (`FUTEX_WAIT_BITSET` with a
/// monotonic deadline, `FUTEX_WAKE`, both private), glibc's internal
/// locks under contention (malloc arenas, `pthread_once`:
/// `FUTEX_WAIT`, private) and `pthread_join` on a worker that is still
/// exiting (a shared `FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME` on its
/// thread id, which the kernel clears). Seen with `strace -f` on glibc
/// 2.39; the same code in 2.36. No priority-inheritance, requeue or
/// wake-op operation.
const FUTEX_OPERATIONS: &[u32] = &[
    (libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG) as u32,
    (libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG) as u32,
    (libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG) as u32,
    (libc::FUTEX_WAIT_BITSET | libc::FUTEX_CLOCK_REALTIME) as u32,
    // musl's thread list lock (`__tl_lock`, `__tl_unlock` in
    // `pthread_exit` and `pthread_create`; `__tl_sync` in `pthread_join`
    // waits for an exiting thread's id, which the kernel clears): a shared
    // `FUTEX_WAIT` and `FUTEX_WAKE`, musl's `__wait` and `__wake` with
    // `priv` 0.
    #[cfg(target_env = "musl")]
    FUTEX_WAIT_SHARED,
    #[cfg(target_env = "musl")]
    FUTEX_WAKE_SHARED,
];

#[cfg(target_env = "musl")]
const FUTEX_WAIT_SHARED: u32 = libc::FUTEX_WAIT as u32;
#[cfg(target_env = "musl")]
const FUTEX_WAKE_SHARED: u32 = libc::FUTEX_WAKE as u32;

/// The `fcntl` commands the server uses after startup (see the table).
/// These values, like every constant the argument checks use, are the
/// same on x86_64 and aarch64.
const FCNTL_COMMANDS: &[u32] = &[
    libc::F_GETFD as u32,
    libc::F_GETFL as u32,
    libc::F_SETFL as u32,
];

/// The program for `arch`: every listed call allowed (with its argument
/// checks), everything else answered by `action`. `pid` is the server's
/// own (`getpid`): the only process `tgkill` may signal (and on musl the
/// only thread `tkill` may).
pub fn program(arch: Arch, action: SeccompAction, pid: u32) -> Vec<libc::sock_filter> {
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
        let body = body(entry.rule, deny, pid);
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

/// The `pthread_create` flags (`clone`) accepted for every libc, each as an
/// exact match: glibc's (≥ 2.34; also musl 1.2.5's), and the same plus
/// `CLONE_DETACHED`, which musl 1.2.6 adds and the kernel ignores. A libc
/// update under `seccomp_action kill` must not end the server.
const THREAD_FLAGS: &[u32] = &[GLIBC_THREAD_FLAGS, GLIBC_THREAD_FLAGS | DETACHED_FLAG];

/// glibc's `pthread_create` flags (glibc ≥ 2.34).
const GLIBC_THREAD_FLAGS: u32 = (libc::CLONE_VM
    | libc::CLONE_FS
    | libc::CLONE_FILES
    | libc::CLONE_SIGHAND
    | libc::CLONE_THREAD
    | libc::CLONE_SYSVSEM
    | libc::CLONE_SETTLS
    | libc::CLONE_PARENT_SETTID
    | libc::CLONE_CHILD_CLEARTID) as u32;

/// `CLONE_DETACHED` (`linux/sched.h`; not in the `libc` crate).
const DETACHED_FLAG: u32 = 0x0040_0000;

/// The instructions after an entry's number matched; `pid` is the
/// server's.
fn body(rule: Rule, deny: u32, pid: u32) -> Vec<libc::sock_filter> {
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
        Rule::Thread => one_of(arg(0), THREAD_FLAGS, deny),
        Rule::Prctl => one_of(arg(0), PRCTL_OPTIONS, deny),
        Rule::Fcntl => one_of(arg(1), FCNTL_COMMANDS, deny),
        Rule::Futex => one_of(arg(1), FUTEX_OPERATIONS, deny),
        Rule::NoDelay => level_and_name(libc::IPPROTO_TCP, libc::TCP_NODELAY, deny),
        Rule::SoError => level_and_name(libc::SOL_SOCKET, libc::SO_ERROR, deny),
        Rule::Peek => one_of(arg(3), &[crate::sys::socket::PEEK_FLAGS as u32], deny),
        Rule::Abort => {
            // `tgkill(tgid, tid, sig)`: `tgid` must be the server, then
            // `sig` SIGABRT.
            let mut out = vec![load(arg(0)), jump(JEQ, pid, 1, 0), ret(deny)];
            out.extend(one_of(arg(2), &[libc::SIGABRT as u32], deny));
            out
        }
        #[cfg(target_env = "musl")]
        Rule::AbortMain => {
            // `tkill(tid, sig)`: `tid` must be the main thread (the pid),
            // then `sig` SIGABRT.
            let mut out = vec![load(arg(0)), jump(JEQ, pid, 1, 0), ret(deny)];
            out.extend(one_of(arg(1), &[libc::SIGABRT as u32], deny));
            out
        }
    }
}

/// `setsockopt`/`getsockopt(fd, level, name, ...)`: allows exactly this
/// `level` (argument 1) and `name` (argument 2).
fn level_and_name(level: i32, name: i32, deny: u32) -> Vec<libc::sock_filter> {
    vec![
        load(arg(1)),
        jump(JEQ, level as u32, 0, 3),
        load(arg(2)),
        jump(JEQ, name as u32, 0, 1),
        ret(libc::SECCOMP_RET_ALLOW),
        ret(deny),
    ]
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
