//! `muzzle init [--hooks LIST] [--json]` (mu's every-harness spec D13,
//! D14): muzzle's hook entry in each harness's own settings file, and the
//! check `muzzle doctor` runs against the same plan.
//!
//! Those files are shared. mu, murmur and museum write their own hooks into
//! `~/.claude/settings.json`, `~/.codex/hooks.json`, `~/.cursor/hooks.json`
//! and the `hooks` key of `~/.config/muse/settings.json`, and so do the
//! harness and the user. muzzle owns exactly the handlers whose command's
//! first two words are `muzzle hook`; every other handler (a user's
//! `muzzle run` gate among them), group, matcher and key stays as found,
//! in its place. A file muzzle can't read as the shape it
//! expects is reported and never rewritten, and a file already right is
//! not rewritten at all, so a second `init` changes nothing.

pub mod json;
pub mod plan;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use crate::cli::{HookTarget, InitArgs};
use crate::exit;
use json::{Json, Object};
use plan::{Plan, TARGETS, Target};

/// The directories the harnesses' settings live in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub home: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl Dirs {
    /// Each harness's default under `home`.
    pub fn under(home: &Path) -> Dirs {
        Dirs {
            home: home.to_path_buf(),
            claude: home.join(".claude"),
            codex: home.join(".codex"),
        }
    }

    /// `CLAUDE_CONFIG_DIR` and `CODEX_HOME` move theirs, as they do for the
    /// harness and for doctor. Muse Code runs hooks with a cleared
    /// environment, so its path stays fixed under the home directory.
    pub fn from_env(home: &Path) -> Dirs {
        let env = |var: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let d = Dirs::under(home);
        Dirs {
            claude: env("CLAUDE_CONFIG_DIR").unwrap_or(d.claude),
            codex: env("CODEX_HOME").unwrap_or(d.codex),
            home: d.home,
        }
    }

    /// One harness's settings file, and the directory whose presence says
    /// the harness is set up here. Without `--hooks`, `init` writes only
    /// where that directory (or the file) exists, so a machine with no
    /// cursor-agent grows no `~/.cursor`.
    pub fn location(&self, t: &Target) -> (PathBuf, PathBuf) {
        let (dir, file) = match t.harness {
            "claude" => (self.claude.clone(), "settings.json"),
            "codex" => (self.codex.clone(), "hooks.json"),
            "cursor-agent" => (self.home.join(".cursor"), "hooks.json"),
            _ => (self.home.join(".config/muse"), "settings.json"),
        };
        (dir.join(file), dir)
    }
}

/// One read: the parsed root, and the text it came from (`None`: no file).
struct Read {
    root: Object,
    text: Option<String>,
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read it: {e}")),
    }
}

fn read(path: &Path) -> Result<Read, String> {
    let text = read_text(path)?;
    let Some(t) = text.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Ok(Read {
            root: Object::new(),
            text,
        });
    };
    match serde_json::from_str::<Json>(t) {
        Ok(Json::Object(root)) => Ok(Read { root, text }),
        Ok(_) => Err("it is not a JSON object".into()),
        Err(e) => Err(format!("it is not valid JSON: {e}")),
    }
}

/// The file a write to `path` replaces: the end of its symlink chain, so a
/// dotfiles checkout keeps its link, even one whose target doesn't exist
/// yet.
fn write_target(path: &Path) -> Result<PathBuf, String> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(e) => return Err(format!("cannot read it: {e}")),
            Ok(m) if !m.file_type().is_symlink() => return Ok(current),
            Ok(_) => {}
        }
        let link = std::fs::read_link(&current).map_err(|e| format!("cannot read it: {e}"))?;
        current = current.parent().unwrap_or(Path::new("/")).join(link);
    }
    Err("its symlinks loop".into())
}

/// The file changed between muzzle's read and its write.
struct Raced;

/// Replace the file's contents in one rename, through a symlink to its
/// target, keeping the target's permissions. Its other writers (the
/// harness, mu, murmur, museum) don't lock it either, so the file is read
/// again just before the rename: if it is no longer the text the plan was
/// made from, nothing is written and the caller plans again.
fn write(
    path: &Path,
    root: &Object,
    read_from: &Option<String>,
) -> Result<Result<(), Raced>, String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let real = write_target(path)?;
    let dir = real.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // A new file is owner-only: settings can carry secrets.
    let mode = std::fs::metadata(&real).map_or(0o600, |m| m.permissions().mode() & 0o777);
    let name = real
        .file_name()
        .map_or("hooks".into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.muzzle-{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::io::Write::write_all(&mut f, Json::Object(root.clone()).to_pretty().as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        drop(f);
        // `mode` above applies only to a new file, and the umask trims it.
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
        if read_text(&real)? != *read_from {
            return Ok(Err(Raced));
        }
        std::fs::rename(&tmp, &real).map_err(|e| format!("cannot replace it: {e}"))?;
        Ok(Ok(()))
    })();
    if !matches!(result, Ok(Ok(()))) {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// How many times a write that raced another writer is planned again.
const WRITE_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallState {
    /// The harness isn't set up here and wasn't asked for.
    Skipped,
    /// muzzle's entry was already right; the file wasn't touched.
    Unchanged,
    Written,
    /// codex only: its `config.toml` already runs `muzzle hook` from
    /// inline hooks and `hooks.json` has none, so `hooks.json` wasn't
    /// touched: a second entry would match each shell call twice.
    Inline,
    /// The file couldn't be read as muzzle expects; it wasn't touched.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstallResult {
    pub harness: &'static str,
    pub path: PathBuf,
    pub state: InstallState,
    #[serde(flatten)]
    pub plan: Plan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// codex only: its `config.toml`, when its inline hooks run
    /// `muzzle hook`. Unless the state is `inline`, `hooks.json` runs it
    /// too, so each shell call is matched twice. muzzle never edits it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub also_in: Option<PathBuf>,
}

/// codex's `config.toml` beside `hooks.json`, when its inline hooks also
/// run `muzzle hook`.
fn codex_toml_hook(hooks_json: &Path) -> Option<PathBuf> {
    let toml = hooks_json.with_file_name("config.toml");
    crate::doctor::hook_in(&toml).then_some(toml)
}

/// Plan and write one file, planning again from the new text when another
/// writer changed it meanwhile. `hooked_elsewhere`: another file already
/// runs muzzle's hook for this harness, so a file with no muzzle handler
/// of its own is left as it is.
fn install_one(
    t: &Target,
    path: &Path,
    hooked_elsewhere: bool,
) -> Result<(InstallState, Plan), String> {
    for _ in 0..WRITE_ATTEMPTS {
        let Read { mut root, text } = read(path)?;
        let plan = plan::plan(t, &mut root)?;
        if hooked_elsewhere && plan.owned == 0 {
            return Ok((InstallState::Inline, Plan::default()));
        }
        if !plan.changed() {
            return Ok((InstallState::Unchanged, plan));
        }
        if write(path, &root, &text)?.is_ok() {
            return Ok((InstallState::Written, plan));
        }
    }
    Err("another program kept changing it; run `muzzle init` again".into())
}

/// Add or update muzzle's entry in each harness's settings file under
/// `dirs`. `only` names the harnesses to write, creating their files;
/// without it each harness that is set up here is written and the rest
/// skipped. One harness's failure never stops the others.
pub fn install(dirs: &Dirs, only: Option<&[&str]>) -> Vec<InstallResult> {
    TARGETS
        .iter()
        .map(|t| {
            let (path, dir) = dirs.location(t);
            let mut r = InstallResult {
                harness: t.harness,
                path: path.clone(),
                state: InstallState::Skipped,
                plan: Plan::default(),
                error: None,
                also_in: None,
            };
            let wanted = match only {
                Some(names) => names.contains(&t.harness),
                None => dir.exists() || path.exists(),
            };
            if !wanted {
                return r;
            }
            let toml = (t.harness == "codex")
                .then(|| codex_toml_hook(&path))
                .flatten();
            match install_one(t, &path, toml.is_some()) {
                Ok((state, plan)) => {
                    r.state = state;
                    r.plan = plan;
                    r.also_in = toml;
                }
                Err(e) => {
                    r.state = InstallState::Refused;
                    r.error = Some(e);
                }
            }
            r
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckState {
    /// muzzle's entry is exactly what `init` writes.
    Ok,
    /// muzzle entries, but not the ones `init` writes.
    Drift,
    /// The harness is set up here, with no muzzle entry in its file.
    Unhooked,
    /// The file couldn't be read as muzzle expects.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HookCheck {
    pub harness: &'static str,
    pub path: PathBuf,
    pub state: CheckState,
    #[serde(flatten)]
    pub plan: Plan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// codex only, as in [`InstallResult::also_in`]: its `config.toml`,
    /// when its inline hooks run `muzzle hook`. With no muzzle handler in
    /// `hooks.json` that is the hook, and the state is `ok`; with one,
    /// each shell call is matched twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub also_in: Option<PathBuf>,
}

impl HookCheck {
    /// Both codex files run `muzzle hook`.
    pub fn doubled(&self) -> bool {
        self.also_in.is_some() && self.plan.owned > 0
    }

    /// The difference from what `init` writes, as a short clause.
    pub fn drift(&self) -> String {
        let mut parts = Vec::new();
        if !self.plan.added.is_empty() {
            parts.push(format!("missing {}", self.plan.added.join(", ")));
        }
        if !self.plan.fixed.is_empty() {
            parts.push(format!("needs {}", self.plan.fixed.join(", ")));
        }
        if !self.plan.removed.is_empty() {
            parts.push(format!("stray {}", self.plan.removed.join(", ")));
        }
        parts.join("; ")
    }
}

/// muzzle's entry in each harness's settings under `dirs`, as `init` would
/// leave it. Read-only. A harness whose directory and file are both absent
/// isn't set up here and isn't reported.
pub fn check(dirs: &Dirs) -> Vec<HookCheck> {
    TARGETS
        .iter()
        .filter_map(|t| {
            let (path, dir) = dirs.location(t);
            if !dir.exists() && !path.exists() {
                return None;
            }
            let mut c = HookCheck {
                harness: t.harness,
                path: path.clone(),
                state: CheckState::Ok,
                plan: Plan::default(),
                error: None,
                also_in: (t.harness == "codex")
                    .then(|| codex_toml_hook(&path))
                    .flatten(),
            };
            match read(&path).and_then(|mut r| plan::plan(t, &mut r.root)) {
                Ok(plan) if plan.owned == 0 && c.also_in.is_some() => {
                    c.state = CheckState::Ok;
                }
                Ok(plan) => {
                    c.state = if plan.owned == 0 {
                        CheckState::Unhooked
                    } else if plan.changed() {
                        CheckState::Drift
                    } else {
                        CheckState::Ok
                    };
                    c.plan = plan;
                }
                Err(e) => {
                    c.state = CheckState::Unreadable;
                    c.error = Some(e);
                }
            }
            Some(c)
        })
        .collect()
}

/// One line saying what `init` did to a harness's file.
pub fn describe(r: &InstallResult) -> String {
    let head = format!("hooks {}:", r.harness);
    let path = r.path.display();
    match r.state {
        InstallState::Skipped => format!(
            "{head} not set up here (no {path}); --hooks {} writes it",
            r.harness
        ),
        InstallState::Unchanged => format!("{head} up to date in {path}"),
        InstallState::Inline => format!(
            "{head} runs from the inline hooks in {}; {path} left alone",
            r.also_in.as_deref().unwrap_or(Path::new("?")).display()
        ),
        InstallState::Refused => format!(
            "{head} left {path} untouched: {}",
            r.error.as_deref().unwrap_or("?")
        ),
        InstallState::Written => {
            let p = &r.plan;
            let parts: Vec<String> = [
                ("added", &p.added),
                ("fixed", &p.fixed),
                ("removed", &p.removed),
            ]
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k} {}", v.join(", ")))
            .collect();
            // codex runs a new or changed hook only once the user trusts it.
            let trust = if r.harness == "codex" {
                " (codex asks you to trust it once: /hooks)"
            } else {
                ""
            };
            format!("{head} {} in {path}{trust}", parts.join("; "))
        }
    }
}

fn harness_name(h: HookTarget) -> &'static str {
    match h {
        HookTarget::Claude => "claude",
        HookTarget::Codex => "codex",
        HookTarget::CursorAgent => "cursor-agent",
        HookTarget::Muse => "muse",
        HookTarget::All => "all",
    }
}

pub fn main(args: &InitArgs) -> anyhow::Result<ExitCode> {
    let names: Option<Vec<&str>> = args.hooks.as_ref().map(|hs| {
        if hs.contains(&HookTarget::All) {
            TARGETS.iter().map(|t| t.harness).collect()
        } else {
            hs.iter().map(|h| harness_name(*h)).collect()
        }
    });
    let results = install(
        &Dirs::from_env(&crate::rules::config::home()),
        names.as_deref(),
    );
    let refused = results.iter().any(|r| r.state == InstallState::Refused);
    if args.json {
        println!("{}", serde_json::to_string(&results)?);
    } else {
        for r in &results {
            if r.state == InstallState::Refused {
                eprintln!("muzzle: {}", describe(r));
            } else {
                println!("{}", describe(r));
            }
            if let Some(toml) = r
                .also_in
                .as_ref()
                .filter(|_| r.state != InstallState::Inline)
            {
                eprintln!(
                    "muzzle: hooks codex: {} also runs `muzzle hook`, so each shell call \
                     is matched twice; remove its inline hook",
                    toml.display()
                );
            }
        }
    }
    Ok(ExitCode::from(if refused { exit::INTERNAL } else { 0 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn dirs(h: &tempfile::TempDir) -> Dirs {
        Dirs::under(h.path())
    }

    #[test]
    fn only_harnesses_set_up_here_are_written() {
        let h = home();
        std::fs::create_dir_all(h.path().join(".cursor")).unwrap();
        let r = install(&dirs(&h), None);
        let states: Vec<_> = r.iter().map(|r| (r.harness, r.state)).collect();
        assert!(
            states.contains(&("cursor-agent", InstallState::Written)),
            "{states:?}"
        );
        assert!(
            states.contains(&("muse", InstallState::Skipped)),
            "{states:?}"
        );
        assert!(!h.path().join(".config").exists());
        let r = install(&dirs(&h), Some(&["muse"]));
        assert_eq!(r[3].state, InstallState::Written);
        assert!(h.path().join(".config/muse/settings.json").is_file());
    }

    #[test]
    fn a_second_install_rewrites_nothing() {
        let h = home();
        let path = h.path().join(".cursor/hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"hooks\": {\"stop\": [{\"command\": \"x\"}]}}").unwrap();
        install(&dirs(&h), Some(&["cursor-agent"]));
        let first = std::fs::read(&path).unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let r = install(&dirs(&h), Some(&["cursor-agent"]));
        assert_eq!(r[2].state, InstallState::Unchanged);
        assert_eq!(std::fs::read(&path).unwrap(), first);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    }

    #[test]
    fn a_symlink_is_written_through_and_a_bad_file_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let h = home();
        let dots = h.path().join("dots/hooks.json");
        let link = h.path().join(".cursor/hooks.json");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        // Dangling: the target is created where the link points.
        std::os::unix::fs::symlink(&dots, &link).unwrap();
        install(&dirs(&h), Some(&["cursor-agent"]));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::read_to_string(&dots)
                .unwrap()
                .contains("muzzle hook cursor")
        );
        assert_eq!(
            std::fs::metadata(&dots).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::write(&dots, "{not json").unwrap();
        let r = install(&dirs(&h), Some(&["cursor-agent"]));
        assert_eq!(r[2].state, InstallState::Refused, "{r:?}");
        assert!(r[2].error.as_deref().unwrap().contains("not valid JSON"));
        assert_eq!(std::fs::read_to_string(&dots).unwrap(), "{not json");
        let c = check(&dirs(&h));
        assert_eq!(c[0].state, CheckState::Unreadable);
    }

    #[test]
    fn check_reports_unhooked_ok_and_drift() {
        let h = home();
        let path = h.path().join(".cursor/hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(check(&dirs(&h))[0].state, CheckState::Unhooked);
        install(&dirs(&h), None);
        assert_eq!(check(&dirs(&h))[0].state, CheckState::Ok);
        std::fs::write(
            &path,
            r#"{"version":1,"hooks":{"stop":[{"command":"muzzle hook cursor"}]}}"#,
        )
        .unwrap();
        let c = &check(&dirs(&h))[0];
        assert_eq!(c.state, CheckState::Drift);
        assert_eq!(
            c.drift(),
            "missing beforeShellExecution; stray stop: muzzle hook cursor"
        );
    }

    #[test]
    fn a_file_changed_since_its_read_is_not_written() {
        let h = home();
        let path = h.path().join(".cursor/hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}").unwrap();
        let root = Object::from_pairs(vec![("version", Json::Number(1.into()))]);
        let r = write(&path, &root, &Some("stale".into())).unwrap();
        assert!(r.is_err(), "raced");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["hooks.json"], "no temp file left behind");
        // Read from the current text, the same write goes through.
        let r = write(&path, &root, &Some("{}".into())).unwrap();
        assert!(r.is_ok());
        assert!(std::fs::read_to_string(&path).unwrap().contains("version"));
    }

    #[test]
    fn an_existing_files_permissions_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        let h = home();
        let path = h.path().join(".claude/settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let r = install(&dirs(&h), Some(&["claude"]));
        assert_eq!(r[0].state, InstallState::Written);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn codex_hooked_inline_in_config_toml_gets_no_second_entry() {
        let h = home();
        let codex = h.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("config.toml"),
            "[[hooks.PreToolUse]]\nmatcher = \"^Bash$\"\n\n\
             [[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"muzzle hook codex\"\n",
        )
        .unwrap();
        let r = install(&dirs(&h), None);
        assert_eq!(r[1].state, InstallState::Inline, "{r:?}");
        assert_eq!(r[1].also_in, Some(codex.join("config.toml")));
        assert!(!r[1].plan.changed());
        assert!(!codex.join("hooks.json").exists());
        assert!(describe(&r[1]).contains("runs from the inline hooks in"));
        let c = check(&dirs(&h));
        assert_eq!((c[0].harness, c[0].state), ("codex", CheckState::Ok));
        assert!(!c[0].doubled());

        // Both files: init keeps hooks.json's entry right, doctor sees two.
        std::fs::write(
            codex.join("hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"muzzle hook codex"}]}]}}"#,
        )
        .unwrap();
        let r = install(&dirs(&h), None);
        assert_eq!(r[1].state, InstallState::Written, "{r:?}");
        assert!(r[1].also_in.is_some());
        let c = check(&dirs(&h));
        assert_eq!(c[0].state, CheckState::Ok);
        assert!(c[0].doubled());
    }

    #[test]
    fn written_lines_say_what_changed() {
        let r = InstallResult {
            harness: "codex",
            path: "/h/.codex/hooks.json".into(),
            state: InstallState::Written,
            plan: Plan {
                owned: 1,
                added: vec!["PreToolUse".into()],
                fixed: vec![],
                removed: vec!["Stop: muzzle hook codex".into()],
            },
            error: None,
            also_in: None,
        };
        assert_eq!(
            describe(&r),
            "hooks codex: added PreToolUse; removed Stop: muzzle hook codex in \
             /h/.codex/hooks.json (codex asks you to trust it once: /hooks)"
        );
    }
}
