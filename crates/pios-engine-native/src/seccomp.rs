//! Default seccomp filter, modelled on Docker's profile but written as a
//! deny list: the syscalls Docker's allow list leaves out that matter on
//! this kernel. Groups gated by a capability are only blocked when the
//! container does not hold that capability (as Docker's profile does).
//! `clone3` fails with ENOSYS so libc falls back to `clone`, whose flags a
//! classic BPF filter can inspect (no new user namespaces).

use std::io;

use libc::c_long;

const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
#[cfg(target_arch = "arm")]
const AUDIT_ARCH: u32 = 0x4000_0028; // AUDIT_ARCH_ARM (EABI, little endian)
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;
const CLONE_NEWUSER: u32 = 0x1000_0000;

// struct seccomp_data offsets.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0_LO: u32 = 16;

// Classic BPF opcodes.
const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JSET_K: u16 = 0x45;
const RET_K: u16 = 0x06;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

fn stmt(code: u16, k: u32) -> Insn {
    Insn { code, jt: 0, jf: 0, k }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code, jt, jf, k }
}

/// Syscalls blocked unless the container holds `cap` (None: always).
fn groups() -> Vec<(Option<u32>, Vec<c_long>)> {
    use libc::*;
    let mut v: Vec<(Option<u32>, Vec<c_long>)> = vec![
        // Always: kernel attack surface with no place in a container.
        (
            None,
            vec![
                SYS_keyctl, SYS_add_key, SYS_request_key, SYS_userfaultfd, SYS_kexec_load, SYS_io_uring_setup,
                SYS_io_uring_enter, SYS_io_uring_register, SYS_uselib, SYS_swapon, SYS_swapoff,
            ],
        ),
        // CAP_SYS_ADMIN (21)
        (
            Some(21),
            vec![
                SYS_bpf, SYS_mount, SYS_umount2, SYS_unshare, SYS_setns, SYS_fanotify_init, SYS_name_to_handle_at,
                SYS_lookup_dcookie, SYS_quotactl, SYS_open_tree, SYS_move_mount, SYS_fsopen, SYS_fsconfig,
                SYS_fsmount, SYS_fspick, SYS_pivot_root,
            ],
        ),
        (Some(16), vec![SYS_init_module, SYS_finit_module, SYS_delete_module]), // SYS_MODULE
        (Some(22), vec![SYS_reboot]),                                           // SYS_BOOT
        (Some(20), vec![SYS_acct]),                                             // SYS_PACCT
        (Some(19), vec![SYS_ptrace, SYS_process_vm_readv, SYS_process_vm_writev, SYS_kcmp]), // SYS_PTRACE
        (Some(25), vec![SYS_settimeofday, SYS_clock_settime, SYS_clock_adjtime]), // SYS_TIME
        (Some(26), vec![SYS_vhangup]),                                          // SYS_TTY_CONFIG
        (Some(2), vec![SYS_open_by_handle_at]),                                 // DAC_READ_SEARCH
        (Some(34), vec![SYS_syslog]),                                           // SYSLOG
        (Some(38), vec![SYS_perf_event_open]),                                  // PERFMON
    ];
    v.retain(|(_, s)| !s.is_empty());
    v
}

/// The filter program for a container holding `caps`.
pub fn program(caps: &[u32]) -> Vec<Insn> {
    let mut blocked: Vec<u32> = groups()
        .into_iter()
        .filter(|(cap, _)| cap.map_or(true, |c| !caps.contains(&c)))
        .flat_map(|(_, s)| s.into_iter().map(|n| n as u32))
        .collect();
    blocked.sort_unstable();
    blocked.dedup();
    let eperm = RET_ERRNO | libc::EPERM as u32;
    let mut p = vec![
        stmt(LD_W_ABS, OFF_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_KILL_PROCESS),
        stmt(LD_W_ABS, OFF_NR),
        jump(JEQ_K, libc::SYS_clone3 as u32, 0, 1),
        stmt(RET_K, RET_ERRNO | libc::ENOSYS as u32),
    ];
    if !caps.contains(&21) {
        // clone(flags, ...): refuse CLONE_NEWUSER, allow everything else.
        p.extend([
            jump(JEQ_K, libc::SYS_clone as u32, 0, 4),
            stmt(LD_W_ABS, OFF_ARG0_LO),
            jump(JSET_K, CLONE_NEWUSER, 0, 1),
            stmt(RET_K, eperm),
            stmt(RET_K, RET_ALLOW),
        ]);
    }
    for nr in blocked {
        p.push(jump(JEQ_K, nr, 0, 1));
        p.push(stmt(RET_K, eperm));
    }
    p.push(stmt(RET_K, RET_ALLOW));
    p
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const Insn,
}

/// Install the filter on the calling thread (inherited across fork/exec).
/// Needs CAP_SYS_ADMIN or no_new_privs; call before dropping to non-root.
pub fn install(caps: &[u32]) -> io::Result<()> {
    let p = program(caps);
    let prog = SockFprog { len: p.len() as u16, filter: p.as_ptr() };
    // SAFETY: prog points to a valid filter for the duration of the call.
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog as *const SockFprog) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny classic BPF interpreter over (nr, arch, arg0).
    fn run(p: &[Insn], nr: u32, arch: u32, arg0: u32) -> u32 {
        let (mut acc, mut pc) = (0u32, 0usize);
        loop {
            let i = p[pc];
            pc += 1;
            match i.code {
                LD_W_ABS => {
                    acc = match i.k {
                        OFF_NR => nr,
                        OFF_ARCH => arch,
                        OFF_ARG0_LO => arg0,
                        _ => 0,
                    }
                }
                JEQ_K => pc += if acc == i.k { i.jt } else { i.jf } as usize,
                JSET_K => pc += if acc & i.k != 0 { i.jt } else { i.jf } as usize,
                RET_K => return i.k,
                _ => panic!("bad opcode"),
            }
        }
    }

    #[test]
    fn blocks_by_default_and_honours_capabilities() {
        let default_caps = [0, 1, 3, 4, 5, 6, 7, 8, 10, 13, 18, 27, 29, 31];
        let p = program(&default_caps);
        let eperm = RET_ERRNO | libc::EPERM as u32;
        assert_eq!(run(&p, libc::SYS_keyctl as u32, AUDIT_ARCH, 0), eperm);
        assert_eq!(run(&p, libc::SYS_mount as u32, AUDIT_ARCH, 0), eperm);
        assert_eq!(run(&p, libc::SYS_read as u32, AUDIT_ARCH, 0), RET_ALLOW);
        assert_eq!(run(&p, libc::SYS_clone3 as u32, AUDIT_ARCH, 0), RET_ERRNO | libc::ENOSYS as u32);
        assert_eq!(run(&p, libc::SYS_clone as u32, AUDIT_ARCH, 0x11), RET_ALLOW, "fork-style clone");
        assert_eq!(run(&p, libc::SYS_clone as u32, AUDIT_ARCH, CLONE_NEWUSER), eperm);
        assert_eq!(run(&p, libc::SYS_read as u32, 0x1234, 0), RET_KILL_PROCESS, "foreign arch");
        // --cap-add SYS_ADMIN / SYS_PTRACE lift their groups.
        let mut more = default_caps.to_vec();
        more.extend([21, 19]);
        let p = program(&more);
        assert_eq!(run(&p, libc::SYS_mount as u32, AUDIT_ARCH, 0), RET_ALLOW);
        assert_eq!(run(&p, libc::SYS_ptrace as u32, AUDIT_ARCH, 0), RET_ALLOW);
        assert_eq!(run(&p, libc::SYS_clone as u32, AUDIT_ARCH, CLONE_NEWUSER), RET_ALLOW);
        assert_eq!(run(&p, libc::SYS_keyctl as u32, AUDIT_ARCH, 0), eperm);
        // Jump offsets are u8: the program must stay well within range.
        assert!(p.len() < 200);
    }
}
