//! The generator, run on synthetic calls by a small classic-BPF
//! interpreter (the subset the program uses), on both tables.

use super::{Arch, Entry, FCNTL_COMMANDS, PRCTL_OPTIONS, Rule, program};
use durchlauferhitzer_core::config::SeccompAction;

const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];
const ACTIONS: [SeccompAction; 2] = [SeccompAction::Fail, SeccompAction::Kill];

const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const EPERM: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
const ENOSYS: u32 = libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32;
const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;

/// The thread flags glibc's `pthread_create` passes to `clone`.
const THREAD: u64 = (libc::CLONE_VM
    | libc::CLONE_FS
    | libc::CLONE_FILES
    | libc::CLONE_SIGHAND
    | libc::CLONE_THREAD
    | libc::CLONE_SYSVSEM
    | libc::CLONE_SETTLS
    | libc::CLONE_PARENT_SETTID
    | libc::CLONE_CHILD_CLEARTID) as u64;

fn expected(action: SeccompAction) -> u32 {
    match action {
        SeccompAction::Fail => EPERM,
        SeccompAction::Kill => KILL,
    }
}

/// A synthetic `seccomp_data`.
struct Call {
    nr: u32,
    arch: u32,
    args: [u64; 6],
}

/// Runs `prog` on `call` as the kernel would: loads of 32-bit words of
/// `seccomp_data` (little-endian), `and`, conditional jumps, returns.
fn run(prog: &[libc::sock_filter], call: &Call) -> u32 {
    let mut data = [0_u8; 64];
    data[0..4].copy_from_slice(&call.nr.to_le_bytes());
    data[4..8].copy_from_slice(&call.arch.to_le_bytes());
    for (i, arg) in call.args.iter().enumerate() {
        data[16 + 8 * i..24 + 8 * i].copy_from_slice(&arg.to_le_bytes());
    }
    let mut acc = 0_u32;
    let mut pc = 0_usize;
    loop {
        let ins = prog.get(pc).expect("ran off the end of the program");
        let code = u32::from(ins.code);
        let (jt, jf) = (usize::from(ins.jt), usize::from(ins.jf));
        pc += 1;
        if code == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS {
            let at = ins.k as usize;
            assert!(at % 4 == 0 && at + 4 <= data.len(), "load at {at}");
            acc = u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
        } else if code == libc::BPF_ALU | libc::BPF_AND | libc::BPF_K {
            acc &= ins.k;
        } else if code == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K {
            pc += if acc == ins.k { jt } else { jf };
        } else if code == libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K {
            pc += if acc & ins.k != 0 { jt } else { jf };
        } else if code == libc::BPF_RET | libc::BPF_K {
            return ins.k;
        } else {
            panic!("instruction {code:#x} at {}", pc - 1);
        }
    }
}

fn table(arch: Arch) -> &'static [Entry] {
    arch.table()
}

fn nr(arch: Arch, name: &str) -> u32 {
    table(arch)
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("{name} not listed for {arch:?}"))
        .nr
}

fn call(arch: Arch, nr: u32, args: [u64; 6]) -> Call {
    Call {
        nr,
        arch: arch.audit(),
        args,
    }
}

/// Every arch, every action: `f(arch, action, program)`.
fn each(mut f: impl FnMut(Arch, SeccompAction, &[libc::sock_filter])) {
    for arch in ARCHES {
        for action in ACTIONS {
            f(arch, action, &program(arch, action));
        }
    }
}

/// Numbers that are in neither table: `ftruncate`, `truncate`, `ioctl`,
/// `execve`, `getppid`, `openat`, `statx`, `ptrace`, `seccomp`.
fn unlisted(arch: Arch) -> [u32; 9] {
    match arch {
        Arch::X86_64 => [77, 76, 16, 59, 110, 257, 332, 101, 317],
        Arch::Aarch64 => [46, 45, 29, 221, 173, 56, 291, 117, 277],
    }
}

#[test]
fn every_plain_entry_is_allowed() {
    each(|arch, _, prog| {
        for entry in table(arch).iter().filter(|e| e.rule == Rule::Allow) {
            let got = run(prog, &call(arch, entry.nr, [u64::MAX; 6]));
            assert_eq!(got, ALLOW, "{arch:?} {}", entry.name);
        }
    });
}

#[test]
fn unlisted_calls_get_the_action() {
    each(|arch, action, prog| {
        for nr in unlisted(arch) {
            assert_eq!(
                table(arch).iter().find(|e| e.nr == nr).map(|e| e.name),
                None
            );
            assert_eq!(
                run(prog, &call(arch, nr, [0; 6])),
                expected(action),
                "{arch:?} {nr}"
            );
        }
    });
}

#[test]
fn truncation_and_ioctl_are_never_allowed() {
    each(|arch, action, prog| {
        let [ftruncate, truncate, ioctl, ..] = unlisted(arch);
        for nr in [ftruncate, truncate, ioctl] {
            for args in [[0; 6], [3, 0, 0, 0, 0, 0], [u64::MAX; 6]] {
                assert_eq!(run(prog, &call(arch, nr, args)), expected(action));
            }
        }
        for name in ["truncate", "ftruncate", "ioctl", "execve", "execveat"] {
            assert!(table(arch).iter().all(|e| e.name != name), "{name}");
        }
    });
}

#[test]
fn a_foreign_architecture_gets_the_action() {
    each(|arch, action, prog| {
        // i386 (`int 0x80`), and the other table's architecture.
        let other = match arch {
            Arch::X86_64 => Arch::Aarch64,
            Arch::Aarch64 => Arch::X86_64,
        };
        for foreign in [0x4000_0003, other.audit()] {
            for entry in table(arch) {
                let got = run(
                    prog,
                    &Call {
                        nr: entry.nr,
                        arch: foreign,
                        args: [0; 6],
                    },
                );
                assert_eq!(
                    got,
                    expected(action),
                    "{arch:?} {} on {foreign:#x}",
                    entry.name
                );
            }
        }
    });
}

#[test]
fn x32_numbers_get_the_action() {
    for action in ACTIONS {
        let prog = program(Arch::X86_64, action);
        for entry in table(Arch::X86_64) {
            let got = run(&prog, &call(Arch::X86_64, entry.nr | 0x4000_0000, [0; 6]));
            assert_eq!(got, expected(action), "x32 {}", entry.name);
        }
    }
}

#[test]
fn socket_only_for_inet_streams() {
    let flags = (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) as u64;
    let stream = libc::SOCK_STREAM as u64;
    let (inet, inet6) = (libc::AF_INET as u64, libc::AF_INET6 as u64);
    each(|arch, action, prog| {
        let socket = nr(arch, "socket");
        let run =
            |domain: u64, kind: u64| run(prog, &call(arch, socket, [domain, kind, 0, 0, 0, 0]));
        for domain in [inet, inet6] {
            assert_eq!(run(domain, stream), ALLOW);
            assert_eq!(run(domain, stream | flags), ALLOW);
            assert_eq!(
                run(domain, libc::SOCK_DGRAM as u64 | flags),
                expected(action)
            );
            assert_eq!(run(domain, libc::SOCK_RAW as u64), expected(action));
            assert_eq!(run(domain, libc::SOCK_SEQPACKET as u64), expected(action));
        }
        for domain in [
            libc::AF_UNIX,
            libc::AF_NETLINK,
            libc::AF_PACKET,
            libc::AF_UNSPEC,
        ] {
            assert_eq!(
                run(domain as u64, stream | flags),
                expected(action),
                "{domain}"
            );
        }
    });
}

#[test]
fn mmap_and_mprotect_never_with_prot_exec() {
    let (read, write, exec) = (
        libc::PROT_READ as u64,
        libc::PROT_WRITE as u64,
        libc::PROT_EXEC as u64,
    );
    each(|arch, action, prog| {
        for name in ["mmap", "mprotect"] {
            let n = nr(arch, name);
            let run = |prot: u64| run(prog, &call(arch, n, [0, 4096, prot, 0x22, u64::MAX, 0]));
            assert_eq!(run(0), ALLOW, "{name}");
            assert_eq!(run(read | write), ALLOW, "{name}");
            assert_eq!(run(exec), expected(action), "{name}");
            assert_eq!(run(read | exec), expected(action), "{name}");
            assert_eq!(run(read | write | exec), expected(action), "{name}");
        }
    });
}

#[test]
fn clone_only_as_a_thread_and_clone3_is_enosys() {
    each(|arch, action, prog| {
        let clone = nr(arch, "clone");
        let run_clone = |flags: u64| run(prog, &call(arch, clone, [flags, 1, 2, 3, 4, 0]));
        assert_eq!(run_clone(THREAD), ALLOW);
        // A fork, a vfork, a thread in new namespaces, a thread that also
        // asks for a signal on exit.
        let sigchld = libc::SIGCHLD as u64;
        let vfork = (libc::CLONE_VM | libc::CLONE_VFORK) as u64 | sigchld;
        for flags in [
            sigchld,
            vfork,
            THREAD | libc::CLONE_NEWUSER as u64,
            THREAD | sigchld,
            0,
        ] {
            assert_eq!(run_clone(flags), expected(action), "{flags:#x}");
        }
        let clone3 = nr(arch, "clone3");
        assert_eq!(run(prog, &call(arch, clone3, [0; 6])), ENOSYS);
        assert_eq!(run(prog, &call(arch, clone3, [u64::MAX; 6])), ENOSYS);
    });
}

#[test]
fn prctl_only_for_its_options() {
    each(|arch, action, prog| {
        let prctl = nr(arch, "prctl");
        let run = |option: i32| run(prog, &call(arch, prctl, [option as u64, 0, 0, 0, 0, 0]));
        assert_eq!(PRCTL_OPTIONS, &[libc::PR_SET_NAME as u32]);
        assert_eq!(run(libc::PR_SET_NAME), ALLOW);
        for option in [
            libc::PR_SET_SECCOMP,
            libc::PR_SET_NO_NEW_PRIVS,
            libc::PR_SET_DUMPABLE,
            libc::PR_CAP_AMBIENT,
            libc::PR_SET_PDEATHSIG,
            libc::PR_GET_NAME,
            0,
        ] {
            assert_eq!(run(option), expected(action), "{option}");
        }
    });
}

#[test]
fn fcntl_only_for_its_commands() {
    each(|arch, action, prog| {
        let fcntl = nr(arch, "fcntl");
        let run = |cmd: i32| run(prog, &call(arch, fcntl, [2, cmd as u64, 0, 0, 0, 0]));
        assert_eq!(FCNTL_COMMANDS.len(), 3);
        for cmd in [libc::F_GETFD, libc::F_GETFL, libc::F_SETFL] {
            assert_eq!(run(cmd), ALLOW, "{cmd}");
        }
        for cmd in [
            libc::F_DUPFD,
            libc::F_DUPFD_CLOEXEC,
            libc::F_SETFD,
            libc::F_SETOWN,
            libc::F_SETLK,
            libc::F_SETLEASE,
            libc::F_ADD_SEALS,
        ] {
            assert_eq!(run(cmd), expected(action), "{cmd}");
        }
    });
}

#[test]
fn the_program_fits_with_room_to_spare_and_every_jump_lands_inside() {
    each(|arch, _, prog| {
        let max = libc::BPF_MAXINSNS as usize;
        assert!(
            prog.len() <= max / 4,
            "{arch:?}: {} instructions",
            prog.len()
        );
        for (at, ins) in prog.iter().enumerate() {
            let class = u32::from(ins.code) & 0x07;
            if class == libc::BPF_JMP {
                let far = at + 1 + usize::from(ins.jt.max(ins.jf));
                assert!(far < prog.len(), "{arch:?}: jump at {at} to {far}");
            }
        }
        let last = prog.last().unwrap();
        assert_eq!(u32::from(last.code), libc::BPF_RET | libc::BPF_K);
    });
}

#[test]
fn both_tables_list_the_same_calls() {
    let aarch64_name = |name: &'static str| match name {
        "epoll_wait" => "epoll_pwait",
        "poll" => "ppoll",
        other => other,
    };
    let x86: Vec<(&str, Rule)> = table(Arch::X86_64)
        .iter()
        .map(|e| (aarch64_name(e.name), e.rule))
        .collect();
    let arm: Vec<(&str, Rule)> = table(Arch::Aarch64)
        .iter()
        .map(|e| (e.name, e.rule))
        .collect();
    assert_eq!(x86, arm);
    for arch in ARCHES {
        let mut numbers: Vec<u32> = table(arch).iter().map(|e| e.nr).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), table(arch).len(), "{arch:?}: a number twice");
    }
}

/// The current architecture's numbers against libc's (this checks the
/// aarch64 table when the tests run there).
#[test]
fn the_current_table_matches_libc() {
    let arch = Arch::current().unwrap();
    for entry in table(arch) {
        assert_eq!(
            i64::from(entry.nr),
            libc_number(entry.name),
            "{}",
            entry.name
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn current_is_x86_64_here() {
    assert_eq!(Arch::current(), Some(Arch::X86_64));
}

#[cfg(target_arch = "aarch64")]
#[test]
fn current_is_aarch64_here() {
    assert_eq!(Arch::current(), Some(Arch::Aarch64));
}

#[cfg(target_arch = "x86_64")]
fn libc_number(name: &str) -> i64 {
    match name {
        "epoll_wait" => libc::SYS_epoll_wait,
        "poll" => libc::SYS_poll,
        other => common_number(other),
    }
}

#[cfg(target_arch = "aarch64")]
fn libc_number(name: &str) -> i64 {
    match name {
        "epoll_pwait" => libc::SYS_epoll_pwait,
        "ppoll" => libc::SYS_ppoll,
        other => common_number(other),
    }
}

fn common_number(name: &str) -> i64 {
    match name {
        "epoll_ctl" => libc::SYS_epoll_ctl,
        "read" => libc::SYS_read,
        "write" => libc::SYS_write,
        "close" => libc::SYS_close,
        "accept4" => libc::SYS_accept4,
        "socket" => libc::SYS_socket,
        "connect" => libc::SYS_connect,
        "getsockopt" => libc::SYS_getsockopt,
        "setsockopt" => libc::SYS_setsockopt,
        "sendto" => libc::SYS_sendto,
        "shutdown" => libc::SYS_shutdown,
        "sendfile" => libc::SYS_sendfile,
        "fcntl" => libc::SYS_fcntl,
        "openat2" => libc::SYS_openat2,
        "fstat" => libc::SYS_fstat,
        "newfstatat" => libc::SYS_newfstatat,
        "readahead" => libc::SYS_readahead,
        "brk" => libc::SYS_brk,
        "mmap" => libc::SYS_mmap,
        "mprotect" => libc::SYS_mprotect,
        "munmap" => libc::SYS_munmap,
        "mremap" => libc::SYS_mremap,
        "madvise" => libc::SYS_madvise,
        "futex" => libc::SYS_futex,
        "sched_yield" => libc::SYS_sched_yield,
        "clock_nanosleep" => libc::SYS_clock_nanosleep,
        "restart_syscall" => libc::SYS_restart_syscall,
        "set_robust_list" => libc::SYS_set_robust_list,
        "rseq" => libc::SYS_rseq,
        "gettid" => libc::SYS_gettid,
        "sched_getaffinity" => libc::SYS_sched_getaffinity,
        "sigaltstack" => libc::SYS_sigaltstack,
        "rt_sigprocmask" => libc::SYS_rt_sigprocmask,
        "prctl" => libc::SYS_prctl,
        "exit" => libc::SYS_exit,
        "clone" => libc::SYS_clone,
        "clone3" => libc::SYS_clone3,
        "exit_group" => libc::SYS_exit_group,
        other => panic!("no libc number for {other}"),
    }
}
