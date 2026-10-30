//! macOS half of `proc`: libproc (`proc_listpids` with `PROC_PGRP_ONLY`,
//! which is what `proc_listpgrppids` wraps, and `proc_pidinfo`),
//! `hw.logicalcpu`, `hw.memsize`. No systemd, so no scope figures (D15:
//! runs are `advisory`).

use std::ffi::CStr;
use std::io;
use std::mem;
use std::path::{Path, PathBuf};

use super::{CgroupFigures, Pid, Stat};

/// `ru_maxrss` is in bytes on macOS.
pub(super) const MAXRSS_UNIT: u64 = 1;

/// `<sys/proc_info.h>`; libc doesn't export it.
const PROC_PGRP_ONLY: u32 = 2;

/// xnu `bsd/sys/proc_info_private.h` (flavor 17, `struct
/// proc_uniqidentifierinfo`, 56 bytes); private, so libc doesn't export it.
const PROC_PIDUNIQIDENTIFIERINFO: libc::c_int = 17;

#[repr(C)]
struct ProcUniqIdentifierInfo {
    p_uuid: [u8; 16],
    p_uniqueid: u64,
    p_puniqueid: u64,
    p_idversion: i32,
    p_reserve2: u32,
    p_reserve3: u64,
    p_reserve4: u64,
}

const _: () = assert!(mem::size_of::<ProcUniqIdentifierInfo>() == 56);

/// One `proc_pidinfo` call into a plain C struct; `Err(errno)` on failure.
/// libproc returns 0 (not -1) with errno set; a short read means the flavor
/// isn't there, which reads as gone (errno 0).
fn pidinfo<T>(pid: Pid, flavor: libc::c_int) -> Result<T, i32> {
    // SAFETY: only called with plain C structs, for which zeroed is valid.
    let mut info: T = unsafe { mem::zeroed() };
    let size = mem::size_of::<T>() as libc::c_int;
    // SAFETY: the buffer is exactly `size` bytes.
    let n = unsafe { libc::proc_pidinfo(pid, flavor, 0, (&mut info as *mut T).cast(), size) };
    if n <= 0 {
        return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    if n != size {
        return Err(0);
    }
    Ok(info)
}

/// ppid, pgid and status come from `PROC_PIDT_SHORTBSDINFO` and the start
/// stamp from `PROC_PIDUNIQIDENTIFIERINFO` `p_uniqueid` (64-bit, never reused
/// within a boot). xnu `bsd/kern/proc_info.c` `proc_pidinfo_internal` exempts
/// exactly these flavors (with PATH, COALITION and PLATFORM info) from the
/// same-user check; `PROC_PIDTBSDINFO` returns EPERM for another euid unless
/// the caller is root, which hid a setuid `sudo` in the job's group.
///
/// `Err(errno)` when unreadable. EPERM still means the process exists (as
/// `kill(pid, 0) == EPERM` does): `group_members` keeps such a listed pid
/// (see `super::listed_member`), while `stat` reads it as `None`, because
/// without a start stamp it can't be told apart from a recycled pid.
fn read_stat(pid: Pid) -> Result<Stat, i32> {
    if pid <= 0 {
        return Err(libc::ESRCH);
    }
    let short: libc::proc_bsdshortinfo = pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO)?;
    if short.pbsi_status == libc::SZOMB {
        return Err(libc::ESRCH);
    }
    let uniq: ProcUniqIdentifierInfo = pidinfo(pid, PROC_PIDUNIQIDENTIFIERINFO)?;
    Ok(Stat {
        ppid: short.pbsi_ppid as Pid,
        pgid: short.pbsi_pgid as Pid,
        start: uniq.p_uniqueid,
    })
}

pub(super) fn stat(pid: Pid) -> Option<Stat> {
    read_stat(pid).ok()
}

pub(super) fn group_members(pgid: Pid) -> io::Result<Vec<Pid>> {
    if pgid <= 0 {
        return Ok(Vec::new());
    }
    // proc_listpids returns bytes. Size from a null-buffer call plus slack,
    // and retry when the group grew past it.
    let mut cap = 64usize;
    loop {
        let mut buf: Vec<libc::c_int> = vec![0; cap];
        let bytes = (cap * mem::size_of::<libc::c_int>()) as libc::c_int;
        // SAFETY: the buffer is `bytes` long.
        let n = unsafe {
            libc::proc_listpids(PROC_PGRP_ONLY, pgid as u32, buf.as_mut_ptr().cast(), bytes)
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n >= bytes {
            cap *= 4;
            continue;
        }
        buf.truncate(n as usize / mem::size_of::<libc::c_int>());
        // The kernel lists zombies too, and the group can change mid-call.
        buf.retain(|&pid| pid > 0 && super::listed_member(read_stat(pid), pgid));
        return Ok(buf);
    }
}

/// No subreapers on macOS: an orphan goes to launchd.
pub(super) fn adopt_orphans() {}

/// Nothing is adopted, so muzzle's only child is the one it waits on.
pub(super) fn reap_adopted(_keep: Pid) {}

pub(super) fn all_pids() -> io::Result<Vec<Pid>> {
    // Sized as `group_members` is: grow and retry while the table fills
    // the buffer.
    let mut cap = 1024usize;
    loop {
        let mut buf: Vec<libc::c_int> = vec![0; cap];
        let bytes = (cap * mem::size_of::<libc::c_int>()) as libc::c_int;
        // SAFETY: the buffer is `bytes` long.
        let n = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // proc_listallpids returns a count of pids, not bytes.
        if n as usize >= cap {
            cap *= 4;
            continue;
        }
        buf.truncate(n as usize);
        buf.retain(|&p| p > 0);
        return Ok(buf);
    }
}

fn sysctl<T: Copy + Default>(name: &CStr) -> io::Result<T> {
    let mut value = T::default();
    let mut len = mem::size_of::<T>();
    // SAFETY: `value` is `len` bytes; no new value is written.
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut T).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    if len != mem::size_of::<T>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sysctl {name:?}: unexpected size {len}"),
        ));
    }
    Ok(value)
}

pub(super) fn logical_cores() -> io::Result<usize> {
    let n: libc::c_int = sysctl(c"hw.logicalcpu")?;
    usize::try_from(n)
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hw.logicalcpu <= 0"))
}

pub(super) fn total_ram() -> io::Result<u64> {
    sysctl::<u64>(c"hw.memsize")
}

pub(super) fn probe_systemd_user() -> bool {
    false
}

pub(super) fn cgroup_of(_pid: Pid) -> Option<PathBuf> {
    None
}

pub(super) fn cgroup_figures(_dir: &Path) -> Option<CgroupFigures> {
    None
}

pub(super) fn namespace(_kind: &str) -> Option<u64> {
    None
}

pub(super) fn cgroup_populated(_rel: &str) -> Option<bool> {
    None
}

pub(super) fn cgroup_has_process(_leaf: &str) -> Option<bool> {
    None
}
