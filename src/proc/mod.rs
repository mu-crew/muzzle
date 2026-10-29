//! Unit `proc` (spec §Architecture 3, §Platforms): liveness with start time,
//! group members, ancestry, cores, RAM, rusage, systemd reachability.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
