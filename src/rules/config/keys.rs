//! Path table keys: `~` expansion and canonical identity (P1, B1).

use std::path::{Component, Path, PathBuf};

use super::home;

pub(super) fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// `.` dropped and `..` applied without touching the filesystem.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// P1: a table key's identity. `None` for a relative key: there is no
/// directory it could be relative to.
pub(crate) fn canonical_key(key: &str) -> Option<PathBuf> {
    let p = expand_tilde(key);
    if !p.is_absolute() {
        return None;
    }
    Some(canonical_prefix(&normalise(&p)))
}

/// Canonicalises the longest existing ancestor of an already normalised
/// path and appends the rest, so a key's identity follows symlinks
/// (`/home` -> `/var/home`, macOS `/var` -> `/private/var`) whether or not
/// the directory exists yet, and doesn't change when it is created.
fn canonical_prefix(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = cur.canonicalize() {
            return rest.iter().rev().fold(c, |acc, part| acc.join(part));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_follows_symlinks_whether_or_not_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canon_real = real.canonicalize().unwrap();
        // A missing tail under a symlinked prefix resolves through the link.
        let missing = canonical_key(&format!("{}/later", link.display())).unwrap();
        assert_eq!(missing, canon_real.join("later"));
        // Creating the directory doesn't change the key (P7 hash stability).
        std::fs::create_dir(real.join("later")).unwrap();
        let existing = canonical_key(&format!("{}/later", link.display())).unwrap();
        assert_eq!(existing, missing);
        // `x/..` through a missing x is normalised before resolving.
        let dotted = canonical_key(&format!("{}/./nope/..", link.display())).unwrap();
        assert_eq!(dotted, canon_real);
    }
}
