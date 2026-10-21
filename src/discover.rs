//! Filesystem walk that finds Git checkouts without following symlinks.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Directory names never descended into: caches, dependency trees, build output,
/// and agent transcript stores (indexed separately by the provider parsers).
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".direnv",
    "result",
    "dist",
    "build",
    ".cache",
    ".npm",
    ".pnpm-store",
    ".rustup",
    ".cargo",
    ".gradle",
    ".m2",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    ".next",
    ".nuxt",
    ".turbo",
    ".terraform",
    "vendor",
    "site-packages",
    ".Trash",
    ".local",
    ".nix-profile",
    ".nix-defexpr",
    ".mozilla",
    ".config",
    ".claude",
    ".codex",
    ".vscode-server",
    ".cursor-server",
    "snap",
    "plugin-build",
    "py_modules",
    ".svelte-kit",
    "coverage",
    "tmp",
];

pub struct WalkStats {
    pub dirs_visited: usize,
    pub capped: bool,
}

/// Symlinked directories are followed only this close to a root (e.g. `~/src -> /mnt/data/src`).
/// Deeper symlinks are never followed; `(dev, ino)` dedupe prevents loops and duplicates.
pub const FOLLOW_DEPTH: usize = 2;

/// Return directories containing a `.git` entry (dir for main checkouts, file for linked worktrees).
pub fn find_checkouts(roots: &[PathBuf], max_dirs: usize, max_depth: usize) -> (Vec<PathBuf>, WalkStats) {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut found = Vec::new();
    let mut stats = WalkStats { dirs_visited: 0, capped: false };
    let mut stack: Vec<(PathBuf, usize, bool)> = roots.iter().map(|r| (r.clone(), 0, true)).collect();
    while let Some((dir, depth, follow)) = stack.pop() {
        // Roots and shallow symlinks are followed; everything else is lstat-only.
        let meta = if follow { std::fs::metadata(&dir) } else { std::fs::symlink_metadata(&dir) };
        let Ok(meta) = meta else { continue };
        if !meta.is_dir() || !seen.insert((meta.dev(), meta.ino())) {
            continue; // not a dir, or a bind-mount / duplicate path already walked
        }
        stats.dirs_visited += 1;
        if stats.dirs_visited > max_dirs {
            stats.capped = true;
            break;
        }
        if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
            found.push(dir.clone());
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut children: Vec<(PathBuf, bool)> = Vec::new();
        for ent in rd.flatten() {
            let Ok(ft) = ent.file_type() else { continue };
            let follow = ft.is_symlink() && depth < FOLLOW_DEPTH;
            if !ft.is_dir() && !follow {
                continue; // deeper symlinks are never followed
            }
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            children.push((ent.path(), follow));
        }
        children.sort();
        for (c, f) in children.into_iter().rev() {
            stack.push((c, depth + 1, f));
        }
    }
    found.sort();
    (found, stats)
}

/// `(dev, ino)` for a path, following symlinks (used to detect path aliases).
pub fn dev_ino(p: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_dependency_dirs_and_deep_symlinks_dedups_aliases() {
        let t = crate::testutil::TempDir::new("walk");
        let a = t.path().join("proj/a");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::create_dir_all(t.path().join("proj/node_modules/dep/.git")).unwrap();
        std::fs::create_dir_all(t.path().join("proj/a/nested/.git")).unwrap();
        // Shallow alias (like ~/src -> /mnt/data/src): followed, but deduped by inode.
        std::os::unix::fs::symlink(&a, t.path().join("alias")).unwrap();
        // Deep symlink to a repo outside the root: never followed.
        let ext = crate::testutil::TempDir::new("walk-ext");
        std::fs::create_dir_all(ext.path().join("r/.git")).unwrap();
        std::fs::create_dir_all(t.path().join("x/y/z")).unwrap();
        std::os::unix::fs::symlink(ext.path().join("r"), t.path().join("x/y/z/link")).unwrap();
        let (found, _) = find_checkouts(&[t.path().to_path_buf()], 10_000, 20);
        let mut canon: Vec<_> = found.iter().map(|p| std::fs::canonicalize(p).unwrap().strip_prefix(t.path()).unwrap().to_string_lossy().into_owned()).collect();
        canon.sort();
        assert_eq!(canon, vec!["proj/a", "proj/a/nested"]);
    }
}
