//! seccomp-BPF network lockdown for the network-denied path.
//!
//! Landlock (ABI v4+) can deny TCP bind/connect, but it does NOT cover UDP, raw,
//! or other IP sockets — so a `allow_net = false` policy enforced by Landlock
//! alone still lets a child send UDP (e.g. DNS exfiltration) or open raw sockets.
//!
//! This installs a tiny seccomp filter that fails `socket(2)` for the `AF_INET`,
//! `AF_INET6`, and `AF_PACKET` families with `EACCES`. No IP socket can be
//! created at all, which blocks TCP, UDP, and raw IP uniformly. `AF_UNIX` and
//! `AF_NETLINK` stay allowed (libc/nss need them); filesystem unix sockets remain
//! governed by Landlock (the socket path must be in a granted subtree).
//!
//! Runs inside the forked child (from `pre_exec`), after Landlock. It is
//! alloc-free: the program is a fixed array on the stack, so it is safe in the
//! post-fork pre-exec context.

use crate::policy::SandboxError;

// classic-BPF opcodes (linux/filter.h)
const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

// seccomp return actions (linux/seccomp.h)
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;

// offsets into struct seccomp_data { int nr; u32 arch; u64 ip; u64 args[6]; }
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;

// AUDIT_ARCH for the compiled target (linux/audit.h). The arch gate fails closed
// on any foreign personality (e.g. a 32-bit `int 0x80` syscall whose `socket`
// number differs), so the IP-socket block cannot be sidestepped that way.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xC000_00B7;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jeq(k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: JEQ_K,
        jt,
        jf,
        k,
    }
}

/// Install the IP-socket denial filter on the current thread. Only meaningful
/// from the sandboxed child. Supported on x86_64 and aarch64; a no-op error-free
/// fallback elsewhere (Landlock still applies).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn deny_ip_sockets() -> Result<(), SandboxError> {
    let errno_eacces: u32 = RET_ERRNO | (libc::EACCES as u32 & 0x0000_ffff);
    let sys_socket = libc::SYS_socket as u32;
    let af_inet = libc::AF_INET as u32;
    let af_inet6 = libc::AF_INET6 as u32;
    let af_packet = libc::AF_PACKET as u32;

    // 0: A = arch
    // 1: if arch == native -> idx3, else fall to KILL
    // 2: RET KILL_PROCESS
    // 3: A = nr
    // 4: if nr == socket -> fall, else -> ALLOW(idx10)
    // 5: A = domain (args[0])
    // 6: if domain == AF_INET   -> ERRNO(idx9)
    // 7: if domain == AF_INET6  -> ERRNO(idx9)
    // 8: if domain == AF_PACKET -> ERRNO(idx9), else ALLOW(idx10)
    // 9: RET ERRNO(EACCES)
    // 10: RET ALLOW
    let prog: [libc::sock_filter; 11] = [
        stmt(LD_W_ABS, OFF_ARCH),
        jeq(AUDIT_ARCH_NATIVE, 1, 0),
        stmt(RET_K, RET_KILL_PROCESS),
        stmt(LD_W_ABS, OFF_NR),
        jeq(sys_socket, 0, 5),
        stmt(LD_W_ABS, OFF_ARG0),
        jeq(af_inet, 2, 0),
        jeq(af_inet6, 1, 0),
        jeq(af_packet, 0, 1),
        stmt(RET_K, errno_eacces),
        stmt(RET_K, RET_ALLOW),
    ];

    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr() as *mut libc::sock_filter,
    };

    // SAFETY: NO_NEW_PRIVS is required for an unprivileged seccomp filter; then
    // load the fixed program. `&fprog` outlives the call. No allocation.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(SandboxError::Apply("prctl(NO_NEW_PRIVS) failed".into()));
        }
        if libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER as libc::c_ulong,
            &fprog as *const libc::sock_fprog as libc::c_ulong,
            0,
            0,
        ) != 0
        {
            return Err(SandboxError::Apply("prctl(SET_SECCOMP) failed".into()));
        }
    }
    Ok(())
}

/// Fallback for architectures without an AUDIT_ARCH constant here: Landlock's
/// TCP deny still applies; UDP/raw remain unblocked. Linux release targets are
/// x86_64, so this only affects uncommon dev architectures.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn deny_ip_sockets() -> Result<(), SandboxError> {
    Ok(())
}
