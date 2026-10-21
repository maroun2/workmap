//! Read-only Git queries via the `git` CLI with timeouts and bounded output.

use crate::util::run_cmd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const T: Duration = Duration::from_secs(20);
const LOG_T: Duration = Duration::from_secs(90);

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    // No optional locks: never rewrite the index while we only look.
    c.env("GIT_OPTIONAL_LOCKS", "0").env("GIT_TERMINAL_PROMPT", "0").arg("-C").arg(dir);
    c
}

/// `(toplevel, common_dir)` for a checkout, both canonical.
pub fn probe(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let out = run_cmd(git(dir).args(["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir"]), T)?;
    let mut it = out.lines();
    let top = PathBuf::from(it.next()?.trim());
    let common = PathBuf::from(it.next()?.trim());
    Some((std::fs::canonicalize(&top).unwrap_or(top), std::fs::canonicalize(&common).unwrap_or(common)))
}

/// `.git` dir for a checkout: the directory itself, or the target of a `gitdir:` file.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    let dotgit = dir.join(".git");
    let md = std::fs::symlink_metadata(&dotgit).ok()?;
    if md.is_dir() {
        return dotgit.join("HEAD").is_file().then_some(dotgit);
    }
    let txt = std::fs::read_to_string(&dotgit).ok()?;
    let gd = txt.trim().strip_prefix("gitdir:")?.trim();
    Some(crate::util::resolve_against(dir, gd))
}

/// Same as [`probe`] but from plain file reads (`.git`, `gitdir:`, `commondir`); no git process.
pub fn probe_fast(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let gd = git_dir(dir)?;
    let common = match std::fs::read_to_string(gd.join("commondir")) {
        Ok(c) => crate::util::resolve_against(&gd, c.trim()),
        Err(_) => gd.clone(),
    };
    let top = std::fs::canonicalize(dir).ok()?;
    Some((top, std::fs::canonicalize(&common).unwrap_or(common)))
}

/// mtime of the checkout's index file (changes on commit, checkout, add, reset).
pub fn index_mtime(dir: &Path) -> i64 {
    git_dir(dir).and_then(|g| std::fs::metadata(g.join("index")).ok()).map(|m| crate::util::mtime_unix(&m)).unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub bare: bool,
    pub prunable: bool,
}

pub fn worktrees(dir: &Path) -> Vec<Worktree> {
    let Some(out) = run_cmd(git(dir).args(["worktree", "list", "--porcelain"]), T) else { return vec![] };
    let mut v = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in out.lines().chain(std::iter::once("")) {
        if let Some(p) = line.strip_prefix("worktree ") {
            cur = Some(Worktree { path: PathBuf::from(p), head: String::new(), branch: None, bare: false, prunable: false });
        } else if let Some(w) = cur.as_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                w.head = h.to_string();
            } else if let Some(b) = line.strip_prefix("branch ") {
                w.branch = Some(b.trim_start_matches("refs/heads/").to_string());
            } else if line == "bare" {
                w.bare = true;
            } else if line.starts_with("prunable") {
                w.prunable = true;
            } else if line.is_empty() {
                v.push(cur.take().unwrap());
            }
        }
    }
    v
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub ts: i64,
    pub author: String,
    pub subject: String,
    pub files: Vec<(String, String)>, // (status, repo-relative path)
}

/// Last `n` commits reachable from HEAD of this checkout, with changed files (capped).
pub fn log(dir: &Path, n: usize, max_files: usize) -> Vec<Commit> {
    let fmt = "--format=%x1e%H%x1f%at%x1f%an%x1f%s";
    // --name-status walks trees; on networked storage this measured ~19 s for 30 commits.
    let Some(out) = run_cmd(git(dir).args(["log", "--no-color", "--no-renames", &format!("-n{n}"), fmt, "--name-status"]), LOG_T) else {
        return vec![];
    };
    let mut v = Vec::new();
    for rec in out.split('\x1e').skip(1) {
        let mut lines = rec.lines();
        let head: Vec<&str> = lines.next().unwrap_or("").split('\x1f').collect();
        if head.len() < 4 {
            continue;
        }
        let files = lines
            .filter_map(|l| {
                let mut p = l.splitn(2, '\t');
                Some((p.next()?.to_string(), p.next()?.to_string()))
            })
            .take(max_files)
            .collect();
        v.push(Commit { sha: head[0].into(), ts: head[1].parse().unwrap_or(0), author: head[2].into(), subject: head[3].into(), files });
    }
    v
}

pub fn remotes(dir: &Path) -> Vec<(String, String)> {
    let Some(out) = run_cmd(git(dir).args(["remote", "-v"]), T) else { return vec![] };
    let mut v: Vec<(String, String)> = out
        .lines()
        .filter(|l| l.ends_with("(fetch)"))
        .filter_map(|l| {
            let mut p = l.split_whitespace();
            Some((p.next()?.to_string(), strip_credentials(p.next()?)))
        })
        .collect();
    v.dedup();
    v
}

/// Never persist `https://user:token@host` credentials.
pub fn strip_credentials(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some((_, host)) = rest.split_once('@') {
            return format!("{scheme}://{host}");
        }
    }
    url.to_string()
}

/// Local branches by recency: (name, unix ts, sha).
pub fn branches(dir: &Path, n: usize) -> Vec<(String, i64, String)> {
    let Some(out) = run_cmd(git(dir).args(["for-each-ref", &format!("--count={n}"), "--sort=-committerdate", "--format=%(refname:short)%09%(committerdate:unix)%09%(objectname)", "refs/heads"]), T)
    else {
        return vec![];
    };
    out.lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split('\t').collect();
            (p.len() == 3).then(|| (p[0].to_string(), p[1].parse().unwrap_or(0), p[2].to_string()))
        })
        .collect()
}

/// Tracked files (repo-relative), capped.
pub fn ls_files(dir: &Path, cap: usize) -> Vec<String> {
    run_cmd(git(dir).args(["ls-files", "-z"]), T).map(|o| o.split('\0').filter(|s| !s.is_empty()).take(cap).map(str::to_string).collect()).unwrap_or_default()
}

/// Tracked entries with mode and blob id: `(mode, blob, repo-relative path)`, capped.
pub fn ls_files_stage(dir: &Path, cap: usize) -> Vec<(String, String, String)> {
    run_cmd(git(dir).args(["ls-files", "-s", "-z"]), T)
        .map(|o| {
            o.split('\0')
                .filter_map(|rec| {
                    let (meta, path) = rec.split_once('\t')?;
                    let mut m = meta.split(' ');
                    Some((m.next()?.to_string(), m.next()?.to_string(), path.to_string()))
                })
                .take(cap)
                .collect()
        })
        .unwrap_or_default()
}

/// Count of modified tracked entries. Untracked files are not walked (slow on big trees).
pub fn dirty_count(dir: &Path) -> Option<usize> {
    run_cmd(git(dir).args(["status", "--porcelain", "--untracked-files=no"]), T).map(|o| o.lines().count())
}

/// Staged blob id of one tracked path (`git ls-files -s -- <rel>`), if tracked.
pub fn index_blob(dir: &Path, rel: &str) -> Option<String> {
    let out = run_cmd(git(dir).args(["ls-files", "-s", "-z", "--", rel]), T)?;
    let rec = out.split('\0').find(|r| r.ends_with(&format!("\t{rel}")))?;
    rec.split(' ').nth(1).map(str::to_string)
}

/// Blob id of the file as it is on disk now (`git hash-object`), without writing it.
pub fn disk_blob(dir: &Path, rel: &str) -> Option<String> {
    run_cmd(git(dir).args(["hash-object", "--", rel]), T).map(|o| o.trim().to_string()).filter(|s| !s.is_empty())
}
