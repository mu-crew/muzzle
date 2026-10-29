//! D27a: which repo a run belongs to, so run data can be split per repo.
//!
//! `repo` is the canonical path of the repo's main checkout: every git
//! worktree and jj workspace of one repo maps to the same path. A git repo
//! with no shared checkout (bare, separate-git-dir) is its common dir. Found by
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
    let pointer = fs::read_to_string(&repo).ok()?;
    let target = jj.join(pointer.trim()).canonicalize().ok()?;
    // Only a real `<root>/.jj/repo` directory is followable. An empty
    // pointer joins to `.jj` itself, whose grandparent is the directory
    // *holding* the workspace: a wrong repo, so it must fall back.
    let jj_dir = target.parent()?;
    if !target.is_dir() || target.file_name()? != "repo" || jj_dir.file_name()? != ".jj" {
        return None;
    }
    // `.jj/repo` → `.jj` → workspace root.
    Some(jj_dir.parent()?.to_path_buf())
}

/// `.git` is a directory in the main checkout. Otherwise it is a file
/// `gitdir: X`. The common dir is `X/commondir` (relative to X) when that
/// exists (a worktree), else X itself (a separate-git-dir checkout or a
/// submodule). A common dir named `.git` stands for its parent, the main
/// checkout. Any other (`bare.git`, `sep.git`, `.git/modules/sm`) has no
/// checkout every worktree shares, so it is the repo itself: one value per
/// repo, and two bare repos in one directory stay apart (D27a step 3).
fn git_main(root: &Path) -> Option<PathBuf> {
    let git = root.join(".git");
    if git.is_dir() {
        return Some(root.to_path_buf());
    }
    let text = fs::read_to_string(&git).ok()?;
    let gitdir = root.join(text.trim_end().strip_prefix("gitdir:")?.trim());
    let common = match fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) => gitdir.join(c.trim_end()),
        Err(_) => gitdir,
    };
    let common = common.canonicalize().ok()?;
    if !common.is_dir() {
        return None;
    }
    if common.file_name()? == ".git" {
        Some(common.parent()?.to_path_buf())
    } else {
        Some(common)
    }
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

    /// A submodule's gitdir has no `commondir`; it is its own common dir,
    /// shared with the submodule's worktrees (next test).
    #[test]
    fn git_submodule_style_gitdir_is_the_modules_dir() {
        let (_t, p) = tmp();
        let main = sub(&p, "main");
        git(&main, &["init", "-q"]);
        let modules = sub(&main, ".git/modules/m");
        let m = sub(&main, "m");
        fs::write(m.join(".git"), "gitdir: ../.git/modules/m\n").unwrap();
        assert_eq!(repo_of(&m), Some(modules));
    }

    #[test]
    fn git_submodule_worktree_maps_with_its_submodule() {
        let (_t, p) = tmp();
        let sm = sub(&p, "sm");
        git(&sm, &["init", "-q"]);
        git(&sm, &["commit", "-q", "--allow-empty", "-m", "x"]);
        let g = sub(&p, "g");
        git(&g, &["init", "-q"]);
        git(
            &g,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../sm",
                "sm",
            ],
        );
        git(&g.join("sm"), &["worktree", "add", "-q", "../../smwt"]);
        let modules = g.join(".git/modules/sm");
        assert_eq!(repo_of(&g.join("sm")), Some(modules.clone()));
        assert_eq!(repo_of(&sub(&p, "smwt/x")), Some(modules));
        assert_eq!(repo_of(&g), Some(g));
    }

    #[test]
    fn bare_repo_worktree_is_the_bare_dir_and_bare_repos_differ() {
        let (_t, p) = tmp();
        let src = sub(&p, "src");
        git(&src, &["init", "-q"]);
        git(&src, &["commit", "-q", "--allow-empty", "-m", "x"]);
        for name in ["a.git", "b.git"] {
            git(&p, &["clone", "-q", "--bare", "src", name]);
        }
        git(&p.join("a.git"), &["worktree", "add", "-q", "../awt"]);
        git(&p.join("b.git"), &["worktree", "add", "-q", "../bwt"]);
        assert_eq!(repo_of(&sub(&p, "awt/s")), Some(p.join("a.git")));
        assert_eq!(repo_of(&p.join("bwt")), Some(p.join("b.git")));
    }

    #[test]
    fn separate_git_dir_main_and_worktree_agree() {
        let (_t, p) = tmp();
        let sep = p.join("sep.git");
        git(
            &p,
            &[
                "init",
                "-q",
                &format!("--separate-git-dir={}", sep.display()),
                "main",
            ],
        );
        let main = p.join("main");
        git(&main, &["commit", "-q", "--allow-empty", "-m", "x"]);
        git(&main, &["worktree", "add", "-q", "../wt"]);
        assert_eq!(repo_of(&main), Some(sep.clone()));
        assert_eq!(repo_of(&sub(&p, "wt/s")), Some(sep));
    }

    /// The `.bare` layout: `.git` is a file `gitdir: ./.bare`.
    #[test]
    fn dot_bare_layout_is_the_bare_dir() {
        let (_t, p) = tmp();
        let src = sub(&p, "src");
        git(&src, &["init", "-q"]);
        git(&src, &["commit", "-q", "--allow-empty", "-m", "x"]);
        let proj = sub(&p, "proj");
        git(&proj, &["clone", "-q", "--bare", "../src", ".bare"]);
        fs::write(proj.join(".git"), "gitdir: ./.bare\n").unwrap();
        git(&proj, &["worktree", "add", "-q", "wt"]);
        assert_eq!(repo_of(&proj), Some(proj.join(".bare")));
        assert_eq!(repo_of(&proj.join("wt")), Some(proj.join(".bare")));
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

    #[test]
    fn an_empty_or_odd_jj_pointer_falls_back_to_the_workspace() {
        let (_t, p) = tmp();
        let ws = sub(&p, "outer/ws");
        let jj = sub(&ws, ".jj");
        for pointer in ["", "  \n", "."] {
            fs::write(jj.join("repo"), pointer).unwrap();
            assert_eq!(repo_of(&ws), Some(ws.clone()), "pointer {pointer:?}");
        }
        // A real directory that isn't a `.jj/repo`.
        sub(&p, "other/notjj/repo");
        fs::write(jj.join("repo"), "../../../other/notjj/repo").unwrap();
        assert_eq!(repo_of(&ws), Some(ws.clone()));
        // A `.jj/repo` that is a file, not a directory.
        let f = sub(&p, "f/.jj");
        fs::write(f.join("repo"), "x").unwrap();
        fs::write(jj.join("repo"), "../../../f/.jj/repo").unwrap();
        assert_eq!(repo_of(&ws), Some(ws));
    }
}
