//! Unit `rules` (spec §Architecture 5, §Interface Config): config loading,
//! `match`, `wrap`, `hook` formats, and the config hash (D28).

pub mod config;
pub mod hook;

use std::process::ExitCode;

use crate::cli::{HookArgs, MatchArgs, WrapArgs};

pub fn match_main(_args: &MatchArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("match")
}

pub fn wrap_main(_args: &WrapArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("wrap")
}

pub fn hook_main(_args: &HookArgs) -> anyhow::Result<ExitCode> {
    crate::exit::not_implemented("hook")
}
