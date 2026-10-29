//! D27a: which repo a run belongs to, so run data can be split per repo.
//!
//! `repo` is the canonical path of the repo's main checkout: every git
//! worktree and jj workspace of one repo maps to the same path. Found by
//! reading files only, never by spawning git or jj: this runs on every
//! `muzzle run`, before queueing.

use std::fs;
use std::path::{Path, PathBuf};

/// The main checkout of the repo holding `dir`, or `None` outside any repo.
/// The nearest `.jj` or `.git` wins, and in one directory `.jj` wins over
/// `.git` (a colocated jj repo, D27a).
pub fn repo_of(dir: &Path) -> Option<PathBuf> {
    let dir = dir.canonicalize().ok()?;
    let found = dir
        .ancestors()
        .find(|d| d.join(".jj").is_dir() || d.join(".git").exists())?;
    let main = if found.join(".jj").is_dir() {
        jj_main(found)
    } else {
        git_main(found)
    };
    // A pointer we can't follow (a moved main checkout, a half-written
    // workspace) still names a repo: the checkout we're standing in.
    Some(
        main.and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| found.to_path_buf()),
    )
}

/// `.jj/repo` is a directory in the main workspace. A secondary workspace
/// (`jj workspace add`) has a file there instead, holding the main
/// workspace's `.jj/repo`, relative to `.jj` (jj 0.46 writes
/// `../../main/.jj/repo`).
fn jj_main(root: &Path) -> Option<PathBuf> {
    let jj = root.join(".jj");
    let repo = jj.join("repo");
    if repo.is_dir() {
        return Some(root.to_path_buf());
    }
    let target = jj.join(fs::read_to_string(&repo).ok()?.trim_end());
    // `.jj/repo` → `.jj` → workspace root.
    Some(
        target
            .canonicalize()
            .ok()?
            .parent()?
            .parent()?
            .to_path_buf(),
    )
}

/// `.git` is a directory in the main checkout. A worktree has a file
/// `gitdir: X` instead, and `X/commondir` (relative to X) names the main
/// `.git`. A submodule's `X` has no `commondir`: it is its own main
/// checkout.
fn git_main(root: &Path) -> Option<PathBuf> {
    let git = root.join(".git");
    if git.is_dir() {
        return Some(root.to_path_buf());
    }
    let text = fs::read_to_string(&git).ok()?;
    let gitdir = root.join(text.trim_end().strip_prefix("gitdir:")?.trim());
    let Ok(common) = fs::read_to_string(gitdir.join("commondir")) else {
        return Some(root.to_path_buf());
    };
    let common = gitdir.join(common.trim_end()).canonicalize().ok()?;
    Some(common.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn sh(dir: &Path, program: &str, args: &[&str]) -> bool {
        let mut c = Command::new(program);
        c.args(args)
            .current_dir(dir)
            // Hermetic: no user config, no identity prompts.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("JJ_CONFIG", "/dev/null")
            .env("JJ_USER", "t")
            .env("JJ_EMAIL", "t@t");
        match c.output() {
            Ok(o) if o.status.success() => true,
            Ok(o) => panic!("{program} {args:?}: {}", String::from_utf8_lossy(&o.stderr)),
            Err(_) => false,
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        assert!(sh(dir, "git", args), "git is needed for these tests");
    }

    /// False, with a note in the output, when jj is not installed.
    fn jj(dir: &Path, args: &[&str]) -> bool {
        let ok = sh(dir, "jj", args);
        if !ok {
            eprintln!("jj not found: skipping a jj case");
        }
        ok
    }

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().canonicalize().unwrap();
        (t, p)
    }

    fn sub(p: &Path, name: &str) -> PathBuf {
        let d = p.join(name);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn plain_git_is_its_root_from_any_depth() {
        let (_t, p) = tmp();
        let main = sub(&p, "main");
        git(&main, &["init", "-q"]);
        let deep = sub(&main, "a/b");
        assert_eq!(repo_of(&main), Some(main.clone()));
        assert_eq!(repo_of(&deep), Some(main));
    }

    #[test]
    fn git_worktree_maps_to_the_main_checkout() {
        let (_t, p) = tmp();
        let main = sub(&p, "main");
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "x"]);
        git(&main, &["worktree", "add", "-q", "../wt"]);
        let wt = p.join("wt");
        assert!(wt.join(".git").is_file());
        assert_eq!(repo_of(&sub(&wt, "src")), Some(main));
    }

    #[test]
    fn git_submodule_style_gitdir_without_commondir_is_its_own_repo() {
        let (_t, p) = tmp();
        let main = sub(&p, "main");
        git(&main, &["init", "-q"]);
        let modules = sub(&main, ".git/modules/m");
        let m = sub(&main, "m");
        fs::write(m.join(".git"), "gitdir: ../.git/modules/m\n").unwrap();
        assert!(modules.is_dir());
        assert_eq!(repo_of(&m), Some(m));
    }

    #[test]
    fn jj_colocated_is_its_root() {
        let (_t, p) = tmp();
        if !jj(&p, &["git", "init", "--colocate", "main"]) {
            return;
        }
        let main = p.join("main");
        assert!(main.join(".git").is_dir() && main.join(".jj").is_dir());
        assert_eq!(repo_of(&sub(&main, "x")), Some(main));
    }

    #[test]
    fn jj_workspace_maps_to_the_main_workspace() {
        let (_t, p) = tmp();
        if !jj(&p, &["git", "init", "--colocate", "main"]) {
            return;
        }
        let main = p.join("main");
        jj(&main, &["workspace", "add", "../ws"]);
        let ws = p.join("ws");
        assert!(ws.join(".jj/repo").is_file(), "jj writes a pointer file");
        assert_eq!(repo_of(&sub(&ws, "x")), Some(main));
    }

    /// Without git, only `.jj/repo` says where the main workspace is.
    #[test]
    fn jj_non_colocated_workspace_maps_to_the_main_workspace() {
        let (_t, p) = tmp();
        if !jj(&p, &["git", "init", "--no-colocate", "main"]) {
            return;
        }
        let main = p.join("main");
        jj(&main, &["workspace", "add", "../ws"]);
        let ws = p.join("ws");
        assert!(!ws.join(".git").exists());
        assert_eq!(repo_of(&sub(&ws, "x")), Some(main));
    }

    #[test]
    fn jj_non_colocated_is_its_root() {
        let (_t, p) = tmp();
        if !jj(&p, &["git", "init", "--no-colocate", "main"]) {
            return;
        }
        let main = p.join("main");
        assert!(!main.join(".git").exists());
        assert_eq!(repo_of(&sub(&main, "x")), Some(main));
    }

    #[test]
    fn nested_git_inside_jj_the_nearest_wins() {
        let (_t, p) = tmp();
        if !jj(&p, &["git", "init", "--colocate", "outer"]) {
            return;
        }
        let outer = p.join("outer");
        let inner = sub(&outer, "vendor/inner");
        git(&inner, &["init", "-q"]);
        assert_eq!(repo_of(&sub(&inner, "src")), Some(inner));
        assert_eq!(repo_of(&sub(&outer, "vendor")), Some(outer));
    }

    #[test]
    fn nested_without_jj_still_picks_the_nearest() {
        let (_t, p) = tmp();
        let outer = sub(&p, "outer");
        git(&outer, &["init", "-q"]);
        let inner = sub(&outer, "inner");
        git(&inner, &["init", "-q"]);
        assert_eq!(repo_of(&inner), Some(inner));
    }

    #[test]
    fn no_repo_is_none() {
        let (_t, p) = tmp();
        // The tempdir lives under /tmp, which is in no repo.
        assert_eq!(repo_of(&sub(&p, "a/b")), None);
        assert_eq!(repo_of(&p.join("missing")), None);
    }

    #[test]
    fn a_dangling_pointer_falls_back_to_the_checkout() {
        let (_t, p) = tmp();
        let w = sub(&p, "w");
        fs::write(w.join(".git"), "gitdir: /nonexistent/.git/worktrees/w\n").unwrap();
        assert_eq!(repo_of(&w), Some(w.clone()));
        let j = sub(&p, "j/.jj");
        fs::write(j.join("repo"), "../../gone/.jj/repo").unwrap();
        assert_eq!(repo_of(&p.join("j")), Some(p.join("j")));
    }
}
