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

pub(super) fn stat(pid: Pid) -> Option<Stat> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: a zeroed proc_bsdinfo is a valid plain C struct.
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is exactly `size` bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n != size || info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(Stat {
        ppid: info.pbi_ppid as Pid,
        pgid: info.pbi_pgid as Pid,
        start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
    })
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
        buf.retain(|&pid| pid > 0 && stat(pid).is_some_and(|s| s.pgid == pgid));
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
