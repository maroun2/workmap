//! Opt-in remote snapshots: for repositories in the gh/glab catalog that are
//! not cloned locally, take a temporary shallow clone of one branch (default
//! branch unless configured), index its docs, manifests and file blob ids
//! like a local main checkout, and delete the clone.
//!
//! Safety: the clone has no working tree (`--no-checkout`), no hooks
//! (`core.hooksPath=/dev/null`, empty template), no submodules and no tags.
//! Files are read as Git blobs, so nothing in the repository is executed and
//! no symlink is followed. Credentials come from the user's Git credential
//! helpers plus `gh auth git-credential` for gh hosts; no token ever appears
//! in a URL, an argument or a log.
//!
//! Caching: one row per repository identity with the indexed commit and
//! indexer version. Unchanged branch HEAD → reuse; moved HEAD or a new
//! indexer version → re-snapshot. A failed refresh records the error and
//! keeps the previously indexed data.

use crate::config::Env;
use crate::db;
use crate::scan::Stats;
use crate::util::{clip, now_unix, redact, run_bounded, stable_id};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Bump when what a snapshot extracts changes (combined with the parser version).
/// 2: passages are kind `remote` (docs, manifests, head commit) and manifests are cached.
pub const VERSION: i64 = 2;

pub fn version() -> i64 {
    VERSION * 1000 + db::PARSER_VERSION
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    /// `namespace/name` or `host/namespace/name` globs (`*` wildcard).
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// New or moved snapshots taken per scan (unchanged ones are free).
    pub max_repos: usize,
    /// Size cap per clone, KiB (the clone is killed when it grows past it).
    pub max_kb: u64,
    pub timeout_secs: u64,
    /// Branch to snapshot instead of each repository's default branch.
    pub branch: Option<String>,
    pub archived: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: false, include: vec!["*".into()], exclude: vec![], max_repos: 10, max_kb: 200 * 1024, timeout_secs: 120, branch: None, archived: false }
    }
}

pub const KEYS: &[(&str, &str)] = &[
    ("snapshots", "on|off — take temporary shallow clones of remote-only repositories (default off)"),
    ("snapshot_include", "comma-separated globs over namespace/name or host/namespace/name (default *)"),
    ("snapshot_exclude", "comma-separated globs to skip (default none)"),
    ("snapshot_max_repos", "new or changed snapshots per scan (default 10)"),
    ("snapshot_max_kb", "size cap per clone in KiB (default 204800)"),
    ("snapshot_timeout_secs", "time limit per clone (default 120)"),
    ("snapshot_branch", "branch to snapshot instead of the default branch (default: empty = default branch)"),
    ("snapshot_archived", "on|off — include archived repositories (default off)"),
];

fn config_path(env: &Env) -> PathBuf {
    env.state_dir.join("config")
}

fn read_config(env: &Env) -> Vec<(String, String)> {
    std::fs::read_to_string(config_path(env))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect()
}

fn on(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "on" | "1" | "true" | "yes")
}

fn list(v: &str) -> Vec<String> {
    v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

pub fn settings(env: &Env) -> Settings {
    let mut s = Settings::default();
    for (k, v) in read_config(env) {
        match k.as_str() {
            "snapshots" => s.enabled = on(&v),
            "snapshot_include" => s.include = list(&v),
            "snapshot_exclude" => s.exclude = list(&v),
            "snapshot_max_repos" => s.max_repos = v.parse().unwrap_or(s.max_repos),
            "snapshot_max_kb" => s.max_kb = v.parse().unwrap_or(s.max_kb),
            "snapshot_timeout_secs" => s.timeout_secs = v.parse().unwrap_or(s.timeout_secs),
            "snapshot_branch" => s.branch = Some(v).filter(|b| !b.is_empty()),
            "snapshot_archived" => s.archived = on(&v),
            _ => {}
        }
    }
    s
}

/// Persist one setting in `<state>/config` (validated; other lines kept).
pub fn set(env: &Env, key: &str, value: &str) -> Result<(), String> {
    if !KEYS.iter().any(|k| k.0 == key) {
        return Err(format!("unknown setting {key}; known: {}", KEYS.iter().map(|k| k.0).collect::<Vec<_>>().join(", ")));
    }
    let numeric = matches!(key, "snapshot_max_repos" | "snapshot_max_kb" | "snapshot_timeout_secs");
    if numeric && value.parse::<u64>().is_err() {
        return Err(format!("{key} needs a whole number"));
    }
    if matches!(key, "snapshots" | "snapshot_archived") && !matches!(value.to_ascii_lowercase().as_str(), "on" | "off" | "true" | "false" | "1" | "0" | "yes" | "no") {
        return Err(format!("{key} is on or off"));
    }
    if value.contains('\n') {
        return Err("value must be one line".into());
    }
    let mut lines: Vec<String> =
        std::fs::read_to_string(config_path(env)).unwrap_or_default().lines().filter(|l| l.split_once('=').map(|(k, _)| k.trim() != key).unwrap_or(true)).map(str::to_string).collect();
    lines.push(format!("{key} = {value}"));
    std::fs::create_dir_all(&env.state_dir).map_err(|e| e.to_string())?;
    std::fs::write(config_path(env), lines.join("\n") + "\n").map_err(|e| e.to_string())
}

fn glob(p: &str, s: &str) -> bool {
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p.eq_ignore_ascii_case(s);
    }
    let s = s.to_lowercase();
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        let part = part.to_lowercase();
        if i == 0 {
            if !s.starts_with(&part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            return s.len() >= pos + part.len() && s.ends_with(&part);
        } else {
            match s[pos..].find(&part) {
                Some(j) => pos += j + part.len(),
                None => return false,
            }
        }
    }
    true
}

pub fn in_scope(s: &Settings, identity: &str) -> bool {
    let short = identity.split_once('/').map(|x| x.1).unwrap_or(identity);
    let hit = |pats: &[String]| pats.iter().any(|p| glob(p, identity) || glob(p, short));
    hit(&s.include) && !hit(&s.exclude)
}

pub fn tmp_root(env: &Env) -> PathBuf {
    env.state_dir.join("snapshots-tmp")
}

/// Remove clones left behind by an interrupted run (owner process gone).
pub fn cleanup_leftovers(env: &Env) -> usize {
    let Ok(rd) = std::fs::read_dir(tmp_root(env)) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("snap-").and_then(|r| r.split('-').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
        let alive = pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists();
        if !alive && e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) && std::fs::remove_dir_all(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// Temporary clone directory, removed on drop (success, error or panic).
struct TempClone(PathBuf);

impl TempClone {
    fn new(env: &Env) -> std::io::Result<TempClone> {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let p = tmp_root(env).join(format!("snap-{}-{}-{}", std::process::id(), now_unix(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        std::fs::create_dir_all(&p)?;
        Ok(TempClone(p))
    }
}

impl Drop for TempClone {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Hardened git: no prompts, no hooks, no lazy blob fetches, credentials only
/// through helpers (the user's, plus `gh auth git-credential` for gh hosts).
fn git(env: &Env, provider: &str, host: &str) -> Command {
    let mut c = crate::util::git_cmd();
    c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_NO_LAZY_FETCH", "1").env("GIT_OPTIONAL_LOCKS", "0").env("GCM_INTERACTIVE", "never").env("GH_CONFIG_DIR", &env.gh_config).env("GH_PROMPT_DISABLED", "1");
    for k in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY"] {
        c.env_remove(k);
    }
    if env.overridden {
        for k in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN"] {
            c.env_remove(k);
        }
    }
    c.args(["-c", "core.hooksPath=/dev/null", "-c", "credential.interactive=false", "-c", "core.fsmonitor=false", "-c", "protocol.ext.allow=never"]);
    if provider == "gh" {
        c.arg("-c").arg(format!("credential.https://{host}.helper=!'{}' auth git-credential", env.gh_bin.display().to_string().replace('\'', "")));
    }
    for (k, v) in &env.snapshot_git_config {
        c.arg("-c").arg(format!("{k}={v}"));
    }
    c
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn urlenc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Branch HEAD through the provider API, else `git ls-remote` (same helpers).
fn resolve_head(env: &Env, r: &Remote, branch: &str) -> Result<String, String> {
    let (bin, path, ptr) = if r.provider == "gh" {
        (&env.gh_bin, format!("repos/{}/{}/branches/{}", r.namespace, r.name, urlenc(branch)), "/commit/sha")
    } else {
        (&env.glab_bin, format!("projects/{}/repository/branches/{}", urlenc(&format!("{}/{}", r.namespace, r.name)), urlenc(branch)), "/commit/id")
    };
    let mut cmd = Command::new(bin);
    cmd.env("GH_CONFIG_DIR", &env.gh_config).env("GLAB_CONFIG_DIR", &env.glab_config).env("GH_PROMPT_DISABLED", "1").env("NO_PROMPT", "1").args(["api", "--hostname", &r.host, &path]);
    if let Ok(out) = crate::util::run_status(&mut cmd, Duration::from_secs(30)) {
        if let Some(sha) = serde_json::from_str::<Value>(&out).ok().and_then(|v| v.pointer(ptr).and_then(|x| x.as_str()).map(str::to_string)).filter(|s| is_sha(s)) {
            return Ok(sha);
        }
    }
    let out = run_bounded(git(env, &r.provider, &r.host).args(["ls-remote", "--heads", "--", &r.clone_url, &format!("refs/heads/{branch}")]), Duration::from_secs(60), None)
        .map_err(|e| format!("cannot resolve {branch} HEAD: git ls-remote {e}"))?;
    out.split_whitespace().next().filter(|s| is_sha(s)).map(str::to_string).ok_or_else(|| format!("branch {branch} not found on the remote"))
}

struct Remote {
    id: String,
    identity: String,
    provider: String,
    host: String,
    namespace: String,
    name: String,
    clone_url: String,
    default_branch: String,
    archived: bool,
}

fn remotes(c: &Connection) -> rusqlite::Result<Vec<Remote>> {
    let mut st = c.prepare("SELECT id, label, meta FROM nodes WHERE kind='remote' ORDER BY label")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    let mut v = Vec::new();
    for row in rows {
        let (id, identity, meta) = row?;
        let m: Value = serde_json::from_str(&meta).unwrap_or(Value::Null);
        let g = |k: &str| m.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let clone_url = format!("https://{identity}.git");
        v.push(Remote {
            id,
            identity,
            provider: g("provider"),
            host: g("host"),
            namespace: g("namespace"),
            name: g("name"),
            clone_url,
            default_branch: g("default_branch"),
            archived: m.get("archived").and_then(|x| x.as_bool()).unwrap_or(false),
        });
    }
    Ok(v)
}

/// `speaker` of cached snapshot file passages (all snapshot passages are kind `remote`).
pub const SPEAKER_DOC: &str = "snapshot-doc";
pub const SPEAKER_MANIFEST: &str = "snapshot-manifest";

/// Whole lines (newlines kept) packed into at most `n` chunks of about `max` bytes.
fn line_chunks(text: &str, max: usize, n: usize) -> Vec<String> {
    let mut out: Vec<String> = vec![String::new()];
    for l in text.lines() {
        let l = clip(l, 300);
        if out.last().map(|c| !c.is_empty() && c.len() + l.len() > max).unwrap_or(false) {
            if out.len() >= n {
                break;
            }
            out.push(String::new());
        }
        let cur = out.last_mut().unwrap();
        cur.push_str(&l);
        cur.push('\n');
    }
    out.retain(|c| !c.trim().is_empty());
    out
}

/// Retag the doc passages just written for a snapshot file.
fn tag_passages(tx: &Connection, origin: &str, speaker: &str) -> rusqlite::Result<()> {
    tx.execute("UPDATE passages SET kind='remote', speaker=?2 WHERE origin=?1 AND kind='doc'", params![origin, speaker])?;
    Ok(())
}

pub fn origin(identity: &str) -> String {
    format!("snap:{identity}")
}

/// Browser link to a file at the indexed commit.
pub fn blob_link(provider: &str, url: &str, sha: &str, path: &str, line: i64) -> String {
    let base = url.trim_end_matches('/');
    let sep = if provider == "glab" { "/-/blob/" } else { "/blob/" };
    format!("{base}{sep}{sha}/{path}{}", if line > 0 { format!("#L{line}") } else { String::new() })
}

fn clear(tx: &Connection, r: &Remote) -> rusqlite::Result<()> {
    let o = origin(&r.identity);
    tx.execute("DELETE FROM passages WHERE origin=?1", [&o])?;
    tx.execute("DELETE FROM raw_links WHERE origin=?1", [&o])?;
    tx.execute("DELETE FROM file_hashes WHERE checkout_id=?1", [&r.id])?;
    Ok(())
}

/// Take and index one snapshot. On error nothing in the index is changed.
fn take(env: &Env, tx: &Connection, s: &Settings, r: &Remote, branch: &str) -> Result<(String, usize, usize), String> {
    let tmp = TempClone::new(env).map_err(|e| format!("temp dir: {e}"))?;
    let dir = tmp.0.join("repo");
    run_bounded(
        git(env, &r.provider, &r.host)
            .args([
                "clone",
                "--quiet",
                "--depth",
                "1",
                "--single-branch",
                "--no-tags",
                "--no-checkout",
                "--no-recurse-submodules",
                "--template=",
                "--filter=blob:limit=512k",
                "--branch",
                branch,
                "--",
                &r.clone_url,
            ])
            .arg(&dir),
        Duration::from_secs(s.timeout_secs),
        Some((&tmp.0, s.max_kb * 1024)),
    )
    .map_err(|e| format!("git clone {e}"))?;
    if crate::util::dir_bytes(&tmp.0) > s.max_kb * 1024 {
        return Err(format!("git clone exceeded the size cap of {} KiB", s.max_kb));
    }
    let rd = |args: &[&str]| run_bounded(git(env, &r.provider, &r.host).arg("-C").arg(&dir).args(args), Duration::from_secs(60), None);
    let sha = rd(&["rev-parse", "HEAD"]).map_err(|e| format!("rev-parse {e}"))?.trim().to_string();
    if !is_sha(&sha) {
        return Err("clone has no HEAD commit".into());
    }
    let tree = rd(&["ls-tree", "-r", "-l", "-z", "HEAD"]).map_err(|e| format!("ls-tree {e}"))?;
    // (mode, blob, size, path)
    let entries: Vec<(String, String, Option<u64>, String)> = tree
        .split('\0')
        .filter_map(|rec| {
            let (meta, path) = rec.split_once('\t')?;
            let f: Vec<&str> = meta.split_whitespace().collect();
            (f.len() == 4).then(|| (f[0].to_string(), f[2].to_string(), f[3].parse().ok(), path.to_string()))
        })
        .take(50_000)
        .collect();
    let blob = |b: &str| rd(&["cat-file", "blob", b]).ok();
    let head = rd(&["log", "-1", "--format=%at%x1f%an%x1f%s"]).unwrap_or_default();
    let short = &sha[..12];
    let o = origin(&r.identity);
    let vpath = |p: &str| format!("remote:{}@{short}/{p}", r.identity);
    // Everything below writes; it only starts once the clone is complete.
    clear(tx, r).map_err(|e| e.to_string())?;
    let db_err = |e: rusqlite::Error| format!("index write failed: {e}");
    let hp: Vec<&str> = head.trim().splitn(3, '\x1f').collect();
    if hp.len() == 3 {
        let text = format!("{} {}", redact(&clip(hp[2], 300)), hp[1]);
        db::add_passage(
            tx,
            &db::Passage {
                kind: "remote",
                node_id: &r.id,
                repo_id: Some(&r.id),
                checkout_id: None,
                source_path: &format!("remote:{}@{short}", r.identity),
                line: 0,
                loc: &sha,
                part: 0,
                speaker: hp[1],
                ts: hp[0].parse().ok(),
                title: &r.identity,
                heading: "",
                text: &text,
                origin: &o,
            },
        )
        .map_err(db_err)?;
    }
    // Docs: same selection as a local checkout, read as blobs.
    let mut docs: Vec<&(String, String, Option<u64>, String)> = entries
        .iter()
        .filter(|e| e.0.starts_with("100") && e.2.map(|n| n <= 256 << 10).unwrap_or(false))
        .filter(|e| {
            let p = Path::new(&e.3);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let depth = p.components().count();
            (depth == 1 && (crate::docs::DOC_NAMES.contains(&name.as_str()) || name.ends_with(".md")))
                || (depth <= 3 && crate::docs::DOC_NAMES.contains(&name.as_str()) && name != "CHANGELOG.md")
                || (depth == 2 && e.3.starts_with("docs/") && name.ends_with(".md"))
        })
        .collect();
    docs.sort_by_key(|e| (Path::new(&e.3).components().count(), !e.3.to_ascii_uppercase().contains("README"), e.3.clone()));
    docs.truncate(25);
    let mut ndocs = 0;
    for d in docs {
        let Some(text) = blob(&d.1) else { continue };
        crate::scan::doc_passages(tx, &text, &r.id, &d.3, &vpath(&d.3), &r.id, None, &o, None).map_err(db_err)?;
        tag_passages(tx, &o, SPEAKER_DOC).map_err(db_err)?;
        for (line, raw, kind) in crate::docs::outside_links(&text, 100) {
            if kind == "doc_url" {
                let lt = text.lines().nth(line - 1).unwrap_or("");
                db::add_raw_link(
                    tx,
                    &db::RawLink {
                        origin: &o,
                        repo_id: &r.id,
                        checkout_id: &r.id,
                        kind,
                        raw: &crate::relations::safe_raw(kind, &raw),
                        resolved: "",
                        source_path: &vpath(&d.3),
                        line: line as i64,
                        note: &crate::git::scrub_urls(&redact(&clip(lt.trim(), 160))),
                    },
                )
                .map_err(db_err)?;
            }
        }
        ndocs += 1;
    }
    // Manifests: only URL-based dependencies and names; local paths and symlinks
    // of a remote checkout say nothing about this disk.
    let mut manifests = 0;
    for e in &entries {
        let name = e.3.rsplit('/').next().unwrap_or(&e.3);
        if !e.0.starts_with("100") || e.3.matches('/').count() > 3 || crate::relations::hash_excluded(&e.3) || manifests >= 60 || e.2.map(|n| n > 512 << 10).unwrap_or(true) {
            continue;
        }
        let manifest = matches!(name, "Cargo.toml" | "package.json" | "pyproject.toml") || (name == ".gitmodules" && !e.3.contains('/'));
        if !manifest {
            continue;
        }
        let Some(text) = blob(&e.1) else { continue };
        manifests += 1;
        // Cached (bounded, redacted) so `remote:<id>:<path>` expands after the clone is gone.
        // One passage per chunk, no section parsing (`#` starts a comment in TOML).
        for (k, chunk) in line_chunks(&crate::git::scrub_urls(&text), 1500, 3).into_iter().enumerate() {
            db::add_passage(
                tx,
                &db::Passage {
                    kind: "remote",
                    node_id: &r.id,
                    repo_id: Some(&r.id),
                    checkout_id: None,
                    source_path: &vpath(&e.3),
                    line: 1,
                    // Distinct per file: search collapses passages by (node, loc).
                    loc: &e.3,
                    part: k as i64,
                    speaker: SPEAKER_MANIFEST,
                    ts: None,
                    title: &e.3,
                    heading: "",
                    text: &redact(&chunk),
                    origin: &o,
                },
            )
            .map_err(db_err)?;
        }
        for (kind, raw, line) in crate::relations::parse_manifest(name, &text).unwrap_or_default() {
            if kind.ends_with("_path") || (kind == "submodule" && raw.starts_with('.')) {
                continue;
            }
            let lt = text.lines().nth(line.saturating_sub(1)).unwrap_or("");
            db::add_raw_link(
                tx,
                &db::RawLink {
                    origin: &o,
                    repo_id: &r.id,
                    checkout_id: &r.id,
                    kind,
                    raw: &crate::relations::safe_raw(kind, &raw),
                    resolved: "",
                    source_path: &vpath(&e.3),
                    line: line as i64,
                    note: &crate::git::scrub_urls(&redact(&clip(lt.trim(), 160))),
                },
            )
            .map_err(db_err)?;
        }
    }
    // Blob ids for identical-content detection (sizes from the tree, no stat).
    let mut hashed = 0;
    for e in &entries {
        if hashed >= crate::relations::MAX_HASHED {
            break;
        }
        let Some(size) = e.2 else { continue };
        if !(e.0 == "100644" || e.0 == "100755") || crate::relations::hash_excluded(&e.3) || !(crate::relations::HASH_MIN..=crate::relations::HASH_MAX).contains(&size) {
            continue;
        }
        hashed += 1;
        tx.execute("INSERT OR REPLACE INTO file_hashes(checkout_id,repo_id,rel,blob,size) VALUES(?1,?1,?2,?3,?4)", params![r.id, e.3, e.1, size as i64]).map_err(db_err)?;
    }
    Ok((sha, entries.len(), ndocs))
}

/// Refresh snapshots for in-scope remote-only repositories. `force_on`
/// overrides the persisted setting for one run (`scan --snapshots` / `--no-snapshots`).
pub fn run(env: &Env, tx: &Connection, force_on: Option<bool>, stats: &mut Stats) -> rusqlite::Result<()> {
    let s = settings(env);
    let left = cleanup_leftovers(env);
    if left > 0 {
        stats.remote.push(format!("snapshots: removed {left} leftover temp clone(s) from an interrupted run"));
    }
    if !force_on.unwrap_or(s.enabled) || crate::remote::disabled(env) {
        return Ok(());
    }
    let mut taken = 0;
    for r in remotes(tx)? {
        if !in_scope(&s, &r.identity) || (r.archived && !s.archived) {
            continue;
        }
        let branch = s.branch.clone().unwrap_or_else(|| r.default_branch.clone());
        if branch.is_empty() {
            stats.remote.push(format!("snapshot {}: skipped, no default branch known", r.identity));
            continue;
        }
        let prev: Option<(String, i64, String)> =
            tx.query_row("SELECT sha, version, branch FROM remote_snapshots WHERE identity=?1", [&r.identity], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).optional()?;
        let now = now_unix();
        let head = match resolve_head(env, &r, &branch) {
            Ok(h) => h,
            Err(e) => {
                record_failure(tx, &r.identity, &branch, &e)?;
                stats.remote.push(format!("snapshot {}: {e}; previous snapshot kept", r.identity));
                continue;
            }
        };
        let has_data: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM passages WHERE origin=?1) OR EXISTS(SELECT 1 FROM file_hashes WHERE checkout_id=?2)", params![origin(&r.identity), r.id], |x| x.get(0))?;
        if let Some((sha, ver, b)) = &prev {
            if *sha == head && *ver == version() && *b == branch && has_data {
                tx.execute("UPDATE remote_snapshots SET checked_at=?2, status='ok', error='' WHERE identity=?1", params![r.identity, now])?;
                stats.remote.push(format!("snapshot {}: unchanged at {} ({branch})", r.identity, &head[..12]));
                continue;
            }
        }
        if taken >= s.max_repos {
            stats.remote.push(format!("snapshot {}: deferred (snapshot_max_repos = {})", r.identity, s.max_repos));
            continue;
        }
        taken += 1;
        tx.execute_batch("SAVEPOINT snap")?;
        match take(env, tx, &s, &r, &branch) {
            Ok((sha, files, docs)) => {
                tx.execute_batch("RELEASE snap")?;
                tx.execute(
                    "INSERT OR REPLACE INTO remote_snapshots(identity,branch,sha,version,indexed_at,checked_at,status,error,files,docs) VALUES(?1,?2,?3,?4,?5,?5,'ok','',?6,?7)",
                    params![r.identity, branch, sha, version(), now, files as i64, docs as i64],
                )?;
                stats.remote.push(format!("snapshot {}: indexed {} ({branch}, {files} files, {docs} docs)", r.identity, &sha[..12]));
            }
            Err(e) => {
                tx.execute_batch("ROLLBACK TO snap; RELEASE snap")?;
                record_failure(tx, &r.identity, &branch, &e)?;
                stats.remote.push(format!("snapshot {}: failed ({e}); previous snapshot kept", r.identity));
            }
        }
    }
    Ok(())
}

fn record_failure(tx: &Connection, identity: &str, branch: &str, e: &str) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO remote_snapshots(identity,branch,status,error,checked_at) VALUES(?1,?2,'failed',?3,?4)
         ON CONFLICT(identity) DO UPDATE SET status='failed', error=excluded.error, checked_at=excluded.checked_at",
        params![identity, branch, clip(e, 300), now_unix()],
    )?;
    Ok(())
}

/// Stored snapshot row: (branch, sha, indexed_at, status, error, files, docs).
pub fn row(c: &Connection, identity: &str) -> Option<(String, String, i64, String, String, i64, i64)> {
    c.query_row("SELECT branch, sha, indexed_at, status, error, files, docs FROM remote_snapshots WHERE identity=?1", [identity], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
    })
    .optional()
    .ok()
    .flatten()
}

pub fn status(env: &Env, c: &Connection) -> String {
    let s = settings(env);
    let mut o = format!(
        "remote snapshots: {} (persisted in {}; change with `workmap remote snapshots enable|disable|set KEY VALUE`)\n",
        if s.enabled { "ENABLED" } else { "disabled (default)" },
        config_path(env).display()
    );
    o.push_str(&format!(
        "scope: include {} exclude {} · archived {} · branch {} · max {} new/changed per scan · cap {} KiB · timeout {}s\n",
        s.include.join(","),
        if s.exclude.is_empty() { "-".into() } else { s.exclude.join(",") },
        if s.archived { "on" } else { "off" },
        s.branch.as_deref().unwrap_or("default branch"),
        s.max_repos,
        s.max_kb,
        s.timeout_secs
    ));
    if let Ok(mut st) = c.prepare("SELECT identity, branch, sha, indexed_at, status, error, files, docs FROM remote_snapshots ORDER BY identity") {
        let rows: Vec<(String, String, String, i64, String, String, i64, i64)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))).map(|r| r.flatten().collect()).unwrap_or_default();
        for (id, b, sha, at, stt, err, files, docs) in rows {
            let have = if sha.is_empty() { "nothing indexed".to_string() } else { format!("{} indexed {} ({files} files, {docs} docs)", &sha[..12.min(sha.len())], crate::util::fmt_ts(at)) };
            o.push_str(&format!("- {id} @{b}: {stt}; {have}{}\n", if err.is_empty() { String::new() } else { format!(" — last error: {err}") }));
        }
    }
    let _ = stable_id;
    o
}

/// Delete every snapshot row and its derived data (opt-out).
pub fn clear_all(c: &Connection) -> rusqlite::Result<()> {
    c.execute("DELETE FROM passages WHERE origin LIKE 'snap:%'", [])?;
    c.execute("DELETE FROM raw_links WHERE origin LIKE 'snap:%'", [])?;
    c.execute("DELETE FROM file_hashes WHERE checkout_id LIKE 'remote-%'", [])?;
    c.execute("DELETE FROM remote_snapshots", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_globs_and_settings_roundtrip() {
        let s = Settings { include: vec!["acme/*".into()], exclude: vec!["*-archive".into()], ..Default::default() };
        assert!(in_scope(&s, "github.com/acme/tool") && !in_scope(&s, "github.com/other/tool") && !in_scope(&s, "github.com/acme/old-archive"));
        assert!(in_scope(&Settings::default(), "gitlab.example.org/g/sub/p"));
        let t = crate::testutil::TempDir::new("snapcfg");
        let env = Env::with_home(t.path());
        assert_eq!(settings(&env), Settings::default());
        set(&env, "snapshots", "on").unwrap();
        set(&env, "snapshot_max_kb", "64").unwrap();
        set(&env, "snapshot_branch", "release").unwrap();
        set(&env, "snapshots", "off").unwrap();
        assert!(set(&env, "snapshot_max_kb", "lots").is_err() && set(&env, "nope", "1").is_err());
        let s = settings(&env);
        assert!(!s.enabled && s.max_kb == 64 && s.branch.as_deref() == Some("release"));
        assert_eq!(blob_link("gh", "https://github.com/a/b", "abc", "README.md", 3), "https://github.com/a/b/blob/abc/README.md#L3");
        assert_eq!(blob_link("glab", "https://gitlab.com/g/p/", "abc", "x.md", 0), "https://gitlab.com/g/p/-/blob/abc/x.md");
    }
}
