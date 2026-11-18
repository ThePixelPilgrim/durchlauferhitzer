// THROWAWAY spike: seccomp allow-list for the helper after start-up.
// Anything not listed kills the process (SIGSYS).

use std::io;

const ALLOWED: &[libc::c_long] = &[
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_recvmsg,
    libc::SYS_sendmsg,
    libc::SYS_sendto,
    libc::SYS_close,
    libc::SYS_getsockopt,
    libc::SYS_setsockopt,
    libc::SYS_getrandom,
    libc::SYS_clock_gettime,
    libc::SYS_getrusage,
    libc::SYS_exit_group,
    libc::SYS_exit,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_brk,
    libc::SYS_mremap,
    libc::SYS_sigaltstack,
    libc::SYS_rt_sigprocmask,
    libc::SYS_madvise,
    libc::SYS_futex,
    libc::SYS_getpid,
];

const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

pub fn install() -> io::Result<()> {
    let bpf_ld_w_abs = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    let bpf_jeq = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    let bpf_ret = (libc::BPF_RET | libc::BPF_K) as u16;
    let mut f = vec![
        stmt(bpf_ld_w_abs, 4), // arch
        jump(bpf_jeq, AUDIT_ARCH_X86_64, 1, 0),
        stmt(bpf_ret, SECCOMP_RET_KILL_PROCESS),
        stmt(bpf_ld_w_abs, 0), // nr
    ];
    for &nr in ALLOWED {
        f.push(jump(bpf_jeq, nr as u32, 0, 1));
        f.push(stmt(bpf_ret, SECCOMP_RET_ALLOW));
    }
    f.push(stmt(bpf_ret, SECCOMP_RET_KILL_PROCESS));
    let prog = libc::sock_fprog { len: f.len() as u16, filter: f.as_mut_ptr() };
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog as *const _ as libc::c_ulong, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
