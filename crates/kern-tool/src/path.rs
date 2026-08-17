//! Shared path-safety helpers (canonicalization + lexical normalization).
//!
//! Both the `filesystem` builtin (in-tool defense in depth) and the
//! `kern-core` permission engine (the authoritative policy layer) must apply
//! the SAME canonicalization or they can disagree about what a path means.
//! Keeping the logic here guarantees they cannot drift.

use std::io;
use std::path::{Component, Path, PathBuf};

/// Resolve `.` and `..` textually. This must happen BEFORE canonicalization:
/// realpath fails on missing intermediate components, so `sub/../../x` would
/// otherwise stop the walk at the missing `sub` instead of cancelling it out.
/// Symlinks are still resolved by the later `canonicalize` step.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the path if it exists; otherwise canonicalize the deepest
/// existing ancestor and re-append the missing tail. Fails with
/// `io::ErrorKind::NotFound` if no ancestor exists. Walking up (rather than
/// assuming a single missing component) handles multi-level write targets
/// like `a/b/c.txt`, and the lexical normalization step guarantees `..`
/// components cancel correctly.
pub fn canonicalize_path(path: &Path) -> io::Result<PathBuf> {
    let path = normalize_lexically(path);
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return Ok(canonical);
    }
    let mut tail = Vec::new();
    let mut current = path.as_path();
    loop {
        if let Some(name) = current.file_name() {
            tail.push(name.to_os_string());
        }
        match current.parent() {
            Some(parent) => {
                if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
                    let mut result = canonical_parent;
                    for name in tail.iter().rev() {
                        result.push(name);
                    }
                    return Ok(result);
                }
                current = parent;
            }
            None => break,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no existing ancestor of {}", path.display()),
    ))
}

/// Canonicalize a rule root the same way targets are canonicalized: resolve
/// the deepest existing ancestor and re-append the missing tail. If the root
/// itself exists this is `std::fs::canonicalize`; otherwise the ancestor walk
/// keeps rules and targets in agreement even when an ancestor is a symlink
/// (macOS `/var` → `/private/var` — a target under a symlinked prefix would
/// otherwise never match its own rule). Falls back to the lexical form when
/// no ancestor exists (such a root can never contain a canonical target).
pub fn canonical_root(root: &Path) -> PathBuf {
    canonicalize_path(root).unwrap_or_else(|_| root.to_path_buf())
}

/// Component-wise containment: `target` is inside `root` iff every component
/// of `root` prefixes `target`.
pub fn is_within(target: &Path, root: &Path) -> bool {
    target.starts_with(root)
}

/// Resolve a (possibly relative) path against `base` and canonicalize it.
pub fn resolve_against(base: &Path, path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    normalize_lexically(&abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalization_cancels_parents() {
        assert_eq!(
            normalize_lexically(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(
            normalize_lexically(Path::new("a/../../b")),
            PathBuf::from("b")
        );
        // Parent at the root is a no-op, not a climb.
        assert_eq!(
            normalize_lexically(Path::new("/../../x")),
            PathBuf::from("/x")
        );
    }

    #[test]
    fn canonicalize_resolves_existing_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        std::fs::write(tmp.path().join("a/b/c.txt"), "x").unwrap();

        let existing = canonicalize_path(&tmp.path().join("a/b/c.txt")).unwrap();
        assert_eq!(
            existing,
            std::fs::canonicalize(tmp.path().join("a/b/c.txt")).unwrap()
        );

        // Multi-level missing tail resolves against the deepest existing ancestor.
        let missing = canonicalize_path(&tmp.path().join("a/b/new/deep/file.txt")).unwrap();
        assert_eq!(
            missing,
            std::fs::canonicalize(tmp.path().join("a/b"))
                .unwrap()
                .join("new/deep/file.txt")
        );

        // Nothing exists under an absolute path: the walk-up lands on the
        // filesystem root — `/` on unix, the current drive's root (\\)?\D:\
        // when the cwd is on D:) on Windows — so the result is a
        // canonical-root-relative path. Containment checks run afterward, so
        // this cannot widen access — the tool/engine compares the result
        // against canonical roots and denies escapes.
        let resolved = canonicalize_path(Path::new("/definitely/not/here/kern-x")).unwrap();
        let root = std::fs::canonicalize(Path::new("/")).unwrap();
        assert_eq!(resolved, root.join("definitely/not/here/kern-x"));
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_follows_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("target.txt"), "x").unwrap();
        symlink(
            outside.path().join("target.txt"),
            tmp.path().join("link.txt"),
        )
        .unwrap();

        let resolved = canonicalize_path(&tmp.path().join("link.txt")).unwrap();
        // Compare against the canonicalized target: the temp dir itself may
        // live behind a symlink (macOS /var -> /private/var), so the raw
        // tempdir path is not the canonical form of the file.
        let expected = std::fs::canonicalize(outside.path().join("target.txt")).unwrap();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn containment_is_component_wise() {
        assert!(is_within(Path::new("/a/b/c"), Path::new("/a/b")));
        // /a/bc is NOT a parent of /a/b/c (component-wise).
        assert!(!is_within(Path::new("/a/b/c"), Path::new("/a/bc")));
        assert!(!is_within(Path::new("/a/x"), Path::new("/a/b")));
    }
}
