//! Seccomp filter for hardened steps, after Docker's default profile (moby `profiles/seccomp/default.json`)
//! for a process without CAP_SYS_ADMIN: no new namespaces, mounts, keyrings, BPF, kernel modules,
//! ptrace or clock changes. Most of these already fail without the dropped capabilities; the filter
//! is what stops a step from creating a user namespace and getting them all back inside it.

use libc::{c_long, sock_filter};

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("the step seccomp filter needs the AUDIT_ARCH value of this architecture");

const BLOCKED: &[c_long] = &[
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_bpf,
    libc::SYS_userfaultfd,
    libc::SYS_perf_event_open,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_open_by_handle_at,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_mount_setattr,
    libc::SYS_setns,
    libc::SYS_unshare,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_reboot,
    libc::SYS_syslog,
    libc::SYS_acct,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_iopl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_ioperm,
];

/// CLONE_NEWNS | NEWCGROUP | NEWUTS | NEWIPC | NEWUSER | NEWPID | NEWNET, as in Docker's profile.
/// CLONE_NEWTIME (0x80) overlaps the exit signal byte of `clone` and only works with `clone3`.
const CLONE_NAMESPACES: u32 = 0x7e02_0000;
/// The x32 ABI on x86_64 reuses AUDIT_ARCH_X86_64 with this bit set in the syscall number.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

// Offsets in `struct seccomp_data`; args[0] is little-endian on both architectures.
const NR: u32 = 0;
const ARCH: u32 = 4;
const ARG0_LOW: u32 = 16;

const LD: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const JGE: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
const JSET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

const HEAD: usize = 4;
const TAIL: usize = 8;
const LEN: usize = HEAD + BLOCKED.len() + TAIL;

const fn op(code: u16, k: u32, jt: usize, jf: usize) -> sock_filter {
    sock_filter {
        code,
        jt: jt as u8,
        jf: jf as u8,
        k,
    }
}

/// Jumps are relative to the next instruction: `to(from, target)` is the offset from `from` to `target`.
const fn to(from: usize, target: usize) -> usize {
    target - from - 1
}

const fn build() -> [sock_filter; LEN] {
    let allow = LEN - 4;
    let eperm = LEN - 3;
    let enosys = LEN - 2;
    let kill = LEN - 1;
    let mut f = [op(0, 0, 0, 0); LEN];
    f[0] = op(LD, ARCH, 0, 0);
    f[1] = op(JEQ, AUDIT_ARCH, 0, to(1, kill));
    f[2] = op(LD, NR, 0, 0);
    f[3] = op(JGE, X32_SYSCALL_BIT, to(3, eperm), 0);
    let mut i = 0;
    while i < BLOCKED.len() {
        let at = HEAD + i;
        f[at] = op(JEQ, BLOCKED[i] as u32, to(at, eperm), 0);
        i += 1;
    }
    let at = HEAD + BLOCKED.len();
    f[at] = op(JEQ, libc::SYS_clone3 as u32, to(at, enosys), 0);
    f[at + 1] = op(JEQ, libc::SYS_clone as u32, 0, to(at + 1, allow));
    f[at + 2] = op(LD, ARG0_LOW, 0, 0);
    f[at + 3] = op(JSET, CLONE_NAMESPACES, to(at + 3, eperm), to(at + 3, allow));
    f[allow] = op(RET, libc::SECCOMP_RET_ALLOW, 0, 0);
    f[eperm] = op(RET, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32, 0, 0);
    // glibc and musl fall back to `clone`, whose flags a classic filter can read (clone3 passes a pointer).
    f[enosys] = op(RET, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32, 0, 0);
    f[kill] = op(RET, libc::SECCOMP_RET_KILL_PROCESS, 0, 0);
    f
}

static FILTER: [sock_filter; LEN] = build();

/// Installs the filter on the calling thread. Needs `PR_SET_NO_NEW_PRIVS` (set by `drop_caps`) and
/// must come after every mount the step setup makes: call it right before exec.
pub(crate) fn install() -> std::io::Result<()> {
    let prog = libc::sock_fprog {
        len: LEN as u16,
        filter: FILTER.as_ptr() as *mut sock_filter,
    };
    crate::check(unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER as libc::c_ulong,
            &prog as *const libc::sock_fprog,
        )
    })
}
