//! Scan pipeline: repos -> checkouts -> docs/manifests/history -> native sessions.

use crate::config::Env;
use crate::db::{self, EdgeRow, NodeRow};
use crate::docs::{self, FileIndex};
use crate::sessions::{self, Session};
use crate::util::{clip, mtime_unix, now_unix, redact, stable_id};
use crate::{discover, git, relations};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct ScanOpts {
    pub roots: Vec<PathBuf>,
    pub include_home: bool,
    pub max_dirs: usize,
    pub max_depth: usize,
    pub commits: usize,
    pub max_parts: usize,
    pub verbose: bool,
    /// Re-index checkouts even when HEAD, index and doc mtimes are unchanged.
    pub full: bool,
    /// Parse agent conversation stores (off: refresh repos/docs/history only).
    pub sessions: bool,
    /// Remote catalog refresh through authenticated gh/glab.
    pub remote: crate::remote::Mode,
    /// Remote snapshots: Some(on/off) for this run, None = persisted setting.
    pub snapshots: Option<bool>,
}

impl Default for ScanOpts {
    fn default() -> Self {
        ScanOpts {
            roots: vec![],
            include_home: true,
            max_dirs: 300_000,
            max_depth: 12,
            commits: 30,
            max_parts: 20_000,
            verbose: false,
            full: false,
            sessions: true,
            remote: crate::remote::Mode::Off,
            snapshots: Some(false),
        }
    }
}

#[derive(Default, Debug)]
pub struct Stats {
    pub checkouts: usize,
    pub checkouts_reused: usize,
    pub repos: usize,
    pub docs: usize,
    pub sessions_parsed: usize,
    pub sessions_reused: usize,
    pub sources_unrelated: usize,
    pub sources_missing: usize,
    pub warnings: Vec<String>,
    pub parse_warnings: usize,
    pub dirs_visited: usize,
    pub ms: u128,
    pub checkouts_pruned: usize,
    /// Checkouts gone from disk but kept (marked missing) because conversations were recorded there.
    pub checkouts_missing: usize,
    pub messages: usize,
    pub sessions_truncated: usize,
    pub relations: usize,
    pub remote: Vec<String>,
}

#[derive(Clone)]
pub struct Checkout {
    pub id: String,
    pub repo_id: String,
    pub top: PathBuf,
    /// Other spellings of the same directory (e.g. git-reported vs canonical).
    pub aliases: Vec<PathBuf>,
    pub dev_ino: Option<(u64, u64)>,
    /// Indexed earlier, directory gone now: kept so recorded evidence stays attached.
    pub missing: bool,
}

/// Checkouts known to the index but not in `skip` (not walked by this run):
/// live ones when their directory exists, otherwise flagged missing.
pub fn db_checkouts(c: &Connection, skip: &HashSet<String>) -> rusqlite::Result<Vec<Checkout>> {
    let mut out = Vec::new();
    for n in crate::overview::nodes(c, "kind='checkout'", &[])? {
        if skip.contains(&n.id) {
            continue;
        }
        let Some(top) = n.path.as_deref().filter(|p| !p.is_empty()).map(PathBuf::from) else { continue };
        let exists = top.is_dir();
        let aliases: Vec<PathBuf> = n.meta.get("reported_path").and_then(|x| x.as_str()).map(PathBuf::from).filter(|r| r != &top).into_iter().collect();
        let missing = !exists || n.meta.get("missing").and_then(|x| x.as_bool()) == Some(true);
        out.push(Checkout { id: n.id, repo_id: n.repo_id.unwrap_or_default(), dev_ino: if missing { None } else { discover::dev_ino(&top) }, top, aliases, missing });
    }
    Ok(out)
}

/// Maps arbitrary paths (session cwd, tool file paths) onto checkouts, handling
/// subdirectories, symlinked spellings and bind-mount aliases via (dev, ino).
pub struct Matcher {
    pub checkouts: Vec<Checkout>,
    by_inode: HashMap<(u64, u64), usize>,
    cache: HashMap<PathBuf, Option<(usize, PathBuf)>>,
}

impl Matcher {
    pub fn new(checkouts: Vec<Checkout>) -> Matcher {
        let by_inode = checkouts.iter().enumerate().filter_map(|(i, c)| c.dev_ino.map(|d| (d, i))).collect();
        Matcher { checkouts, by_inode, cache: HashMap::new() }
    }

    /// Return (checkout index, path relative to checkout top) among live checkouts.
    pub fn locate(&mut self, p: &Path) -> Option<(usize, PathBuf)> {
        self.locate_in(p, false)
    }

    /// Like [`Matcher::locate`], but a path under a checkout that is gone from
    /// disk (kept as missing) maps onto that historical checkout.
    pub fn locate_any(&mut self, p: &Path) -> Option<(usize, PathBuf)> {
        self.locate_in(p, true)
    }

    fn locate_in(&mut self, p: &Path, any: bool) -> Option<(usize, PathBuf)> {
        // Longest textual prefix first (cheap, also works for deleted files).
        let mut best: Option<(usize, usize, PathBuf)> = None;
        for (i, c) in self.checkouts.iter().enumerate() {
            if c.missing && !any {
                continue;
            }
            for root in std::iter::once(&c.top).chain(c.aliases.iter()) {
                if let Ok(rel) = p.strip_prefix(root) {
                    let n = root.as_os_str().len();
                    if best.as_ref().map(|b| n > b.1).unwrap_or(true) {
                        best = Some((i, n, rel.to_path_buf()));
                    }
                }
            }
        }
        if let Some((i, _, rel)) = best {
            return Some((i, rel));
        }
        // Alias resolution: walk ancestors and compare inode identity.
        let mut visited: Vec<PathBuf> = Vec::new();
        let mut found: Option<(PathBuf, Option<(usize, PathBuf)>)> = None;
        let mut cur = Some(p);
        while let Some(dir) = cur {
            if let Some(hit) = self.cache.get(dir) {
                found = Some((dir.to_path_buf(), hit.clone()));
                break;
            }
            if let Some(&i) = discover::dev_ino(dir).and_then(|di| self.by_inode.get(&di)) {
                found = Some((dir.to_path_buf(), Some((i, PathBuf::new()))));
                break;
            }
            visited.push(dir.to_path_buf());
            cur = dir.parent();
        }
        let (base, hit) = found.unwrap_or((PathBuf::from("/"), None));
        // Never `join("")`: that appends a trailing slash to file paths.
        let under = |q: &Path, rel: &Path| match q.strip_prefix(&base) {
            Ok(t) if !t.as_os_str().is_empty() => rel.join(t),
            _ => rel.to_path_buf(),
        };
        self.cache.insert(base.clone(), hit.clone());
        for v in visited {
            let val = hit.as_ref().map(|(i, rel)| (*i, under(&v, rel)));
            self.cache.insert(v, val);
        }
        hit.map(|(i, rel)| (i, under(p, &rel)))
    }
}

fn repo_label(common: &Path, remotes: &[(String, String)], main_top: &Path) -> String {
    if let Some((_, url)) = remotes.iter().find(|r| r.0 == "origin").or(remotes.first()) {
        let tail = url.trim_end_matches('/').trim_end_matches(".git");
        if let Some(name) = tail.rsplit(['/', ':']).next() {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    let base = if common.file_name().map(|n| n == ".git").unwrap_or(false) { main_top } else { common };
    base.file_name().map(|n| n.to_string_lossy().trim_end_matches(".git").to_string()).unwrap_or_else(|| "repo".into())
}

pub fn run(env: &Env, c: &mut Connection, opts: &ScanOpts) -> rusqlite::Result<Stats> {
    let t0 = Instant::now();
    let mut st = Stats::default();
    c.execute("INSERT INTO scans(started) VALUES(?1)", [now_unix()])?;
    let scan_id = c.last_insert_rowid();

    let mut roots: Vec<PathBuf> = Vec::new();
    if opts.include_home {
        roots.push(env.home.clone());
    }
    roots.extend(env.load_roots());
    roots.extend(opts.roots.iter().cloned());
    roots.dedup();
    let (cands, ws) = discover::find_checkouts(&roots, opts.max_dirs, opts.max_depth);
    st.dirs_visited = ws.dirs_visited;
    if ws.capped {
        st.warnings.push(format!("directory walk stopped at {} dirs (raise --max-dirs)", opts.max_dirs));
    }
    let (repos, wt_meta) = group_checkouts(cands.iter().map(|p| p.as_path()));

    let mut checkouts: Vec<Checkout> = Vec::new();
    let mut indexes: HashMap<String, FileIndex> = HashMap::new();
    for (common, tops) in &repos {
        // One transaction per repository: an interrupted scan keeps finished repos.
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        index_repo(&tx, env, opts, common, tops, None, &wt_meta, &mut st, &mut checkouts, &mut indexes)?;
        tx.commit()?;
    }
    // Sessions: batches of explicit transactions so a long first scan keeps progress.
    // IMMEDIATE: take the write lock up front, so a concurrent `ingest` waits
    // (busy timeout) instead of failing a read-then-write upgrade.
    let tx: &Connection = c;
    tx.execute_batch("BEGIN IMMEDIATE")?;
    // Checkouts listed by `git worktree list` whose directory is gone.
    for (p, wt) in &wt_meta {
        if !p.exists() {
            st.warnings.push(format!("worktree registered but missing on disk: {} ({})", p.display(), wt.branch.clone().unwrap_or_default()));
        }
    }

    // Checkouts this run did not walk (outside the given roots, or gone) still
    // locate transcripts and file paths, so a partial scan never re-attributes
    // or drops evidence that belongs to them.
    let walked: HashSet<String> = checkouts.iter().map(|c| c.id.clone()).collect();
    checkouts.extend(db_checkouts(tx, &walked)?);
    let mut matcher = Matcher::new(checkouts);
    if opts.sessions {
        index_sessions(tx, env, &mut matcher, &mut indexes, opts, &mut st)?;
    }

    // Checkouts indexed earlier whose directory is gone. Only when the parent
    // directory still exists, so an unmounted drive or a missing root never
    // touches its projects. Conversations recorded there keep the checkout
    // (marked missing, with its recorded path and branch); others are dropped.
    for co in crate::overview::nodes(tx, "kind='checkout'", &[])? {
        let path = PathBuf::from(co.path.clone().unwrap_or_default());
        let parent_ok = path.parent().map(|p| p.is_dir()).unwrap_or(false);
        if walked.contains(&co.id) || path.as_os_str().is_empty() || path.exists() || !parent_ok {
            continue;
        }
        let recorded: i64 = tx.query_row("SELECT count(*) FROM nodes WHERE kind='session' AND checkout_id=?1", [&co.id], |r| r.get(0))?;
        if recorded == 0 {
            db::clear_origin(tx, &co.id)?;
            tx.execute("DELETE FROM nodes WHERE id=?1 OR (kind IN ('doc','memory') AND checkout_id=?1)", [&co.id])?;
            st.checkouts_pruned += 1;
            st.warnings.push(format!("pruned checkout no longer on disk: {}", co.path.unwrap_or_default()));
            continue;
        }
        mark_missing(tx, &co, &walked, scan_id, recorded, &mut st)?;
    }
    tx.execute("DELETE FROM nodes WHERE kind='repo' AND id NOT IN (SELECT repo_id FROM nodes WHERE kind='checkout' AND repo_id IS NOT NULL)", [])?;

    // Remote catalog (metadata only) before relations, so doc/manifest URLs can point at it.
    crate::remote::sync(env, tx, opts.remote, &mut st)?;
    crate::snapshot::run(env, tx, if opts.remote == crate::remote::Mode::Off && opts.snapshots.is_none() { Some(false) } else { opts.snapshots }, &mut st)?;
    st.relations = relations::resolve(tx, &mut matcher)?;

    // Drop file/commit nodes no edge points at any more, then text whose node is gone.
    tx.execute("DELETE FROM nodes WHERE kind IN ('file','commit') AND id NOT IN (SELECT dst FROM edges) AND id NOT IN (SELECT src FROM edges)", [])?;
    tx.execute("DELETE FROM passages WHERE node_id NOT IN (SELECT id FROM nodes)", [])?;
    tx.execute("DELETE FROM raw_links WHERE checkout_id NOT IN (SELECT id FROM nodes)", [])?;
    tx.execute("DELETE FROM file_hashes WHERE checkout_id NOT IN (SELECT id FROM nodes)", [])?;
    st.messages = tx.query_row("SELECT count(DISTINCT node_id || ' ' || loc) FROM passages WHERE kind='message'", [], |r| r.get::<_, i64>(0))? as usize;
    st.sessions_truncated =
        tx.query_row("SELECT count(*) FROM nodes WHERE kind='session' AND (json_extract(meta,'$.truncated') OR json_extract(meta,'$.messages_capped'))", [], |r| r.get::<_, i64>(0))? as usize;
    st.ms = t0.elapsed().as_millis();
    let stats = json!({"ms": st.ms, "checkouts": st.checkouts, "repos": st.repos, "docs": st.docs, "sessions_parsed": st.sessions_parsed,
        "sessions_reused": st.sessions_reused, "sources_unrelated": st.sources_unrelated, "sources_missing": st.sources_missing,
        "parse_warnings": st.parse_warnings, "dirs_visited": st.dirs_visited, "warnings": st.warnings, "commits_per_worktree": opts.commits,
        "checkouts_pruned": st.checkouts_pruned, "checkouts_missing": st.checkouts_missing, "messages": st.messages, "sessions_truncated": st.sessions_truncated, "relations": st.relations,
        "remote": st.remote, "sessions_indexed": opts.sessions});
    tx.execute("UPDATE scans SET finished=?1, stats=?2 WHERE id=?3", params![now_unix(), stats.to_string(), scan_id])?;
    tx.execute_batch("COMMIT")?;
    Ok(st)
}

type RepoTops = BTreeMap<PathBuf, Vec<(PathBuf, Option<PathBuf>)>>;

/// Group checkout candidates by common git dir (= repository identity) and add
/// every existing worktree git knows of. Returns (common -> [(canonical top,
/// reported path)], `git worktree list` entries by canonical path).
fn group_checkouts<'a>(cands: impl Iterator<Item = &'a Path>) -> (RepoTops, HashMap<PathBuf, git::Worktree>) {
    let mut repos: RepoTops = BTreeMap::new();
    let mut seen_tops: HashSet<(u64, u64)> = HashSet::new();
    let mut add_top = |repos: &mut RepoTops, common: &Path, top: PathBuf, reported: Option<PathBuf>| {
        if let Some(di) = discover::dev_ino(&top) {
            if !seen_tops.insert(di) {
                if let (Some(rep), Some(list)) = (reported, repos.get_mut(common)) {
                    if let Some(e) = list.iter_mut().find(|e| discover::dev_ino(&e.0) == Some(di)) {
                        e.1.get_or_insert(rep);
                    }
                }
                return;
            }
        }
        repos.entry(common.to_path_buf()).or_default().push((top, reported));
    };
    for cand in cands {
        if let Some((top, common)) = git::probe_fast(cand).or_else(|| git::probe(cand)) {
            add_top(&mut repos, &common, top, None);
        }
    }
    let commons: Vec<PathBuf> = repos.keys().cloned().collect();
    let mut wt_meta: HashMap<PathBuf, git::Worktree> = HashMap::new();
    for common in &commons {
        let first = repos[common][0].0.clone();
        for wt in git::worktrees(&first) {
            if wt.bare {
                continue;
            }
            let canon = std::fs::canonicalize(&wt.path).unwrap_or(wt.path.clone());
            if canon.exists() {
                add_top(&mut repos, common, canon.clone(), Some(wt.path.clone()));
            }
            wt_meta.insert(canon, wt);
        }
    }
    (repos, wt_meta)
}

/// Index one repository and its checkouts. With `only`, the other checkouts are
/// registered for path matching but not re-indexed (targeted ingest).
#[allow(clippy::too_many_arguments)]
fn index_repo(
    tx: &Connection,
    env: &Env,
    opts: &ScanOpts,
    common: &Path,
    tops: &[(PathBuf, Option<PathBuf>)],
    only: Option<&HashSet<PathBuf>>,
    wt_meta: &HashMap<PathBuf, git::Worktree>,
    st: &mut Stats,
    checkouts: &mut Vec<Checkout>,
    indexes: &mut HashMap<String, FileIndex>,
) -> rusqlite::Result<()> {
    let main_top = tops.iter().map(|t| &t.0).find(|t| t.join(".git").is_dir()).unwrap_or(&tops[0].0).clone();
    // Identity comes from the raw configured URL; the insteadOf-rewritten fetch
    // URL is kept only as a secondary matching candidate when it differs.
    let remotes = git::remotes(&main_top);
    let rewritten = git::rewritten_remotes(&main_top);
    let remote_json: Vec<serde_json::Value> = remotes
        .iter()
        .map(|r| match rewritten.iter().find(|w| w.0 == r.0 && w.1 != r.1) {
            Some(w) => json!({"name": r.0, "url": r.1, "fetch_url": w.1}),
            None => json!({"name": r.0, "url": r.1}),
        })
        .collect();
    let repo_id = stable_id("repo", &common.to_string_lossy());
    let label = repo_label(common, &remotes, &main_top);
    let branches = git::branches(&main_top, 20);
    let repo_meta = json!({"common_dir": common, "remotes": remote_json,
        "branches": branches.iter().map(|b| json!({"name": b.0, "ts": b.1, "sha": &b.2[..b.2.len().min(10)]})).collect::<Vec<_>>(), "checkouts": tops.len()});
    let repo_ts = branches.iter().map(|b| b.1).max();
    db::upsert_node(tx, &NodeRow { id: &repo_id, kind: "repo", label: &label, path: Some(&common.to_string_lossy()), repo_id: Some(&repo_id), checkout_id: None, ts: repo_ts, meta: &repo_meta })?;
    // Project-level searchable text: name, remotes, main checkout path.
    let porigin = format!("repo:{repo_id}");
    db::clear_origin(tx, &porigin)?;
    let ids: Vec<String> = remotes.iter().filter_map(|r| crate::util::remote_identity(&r.1)).collect();
    let ptext = format!("{label} {} {} {}", ids.join(" "), remotes.iter().map(|r| r.1.as_str()).collect::<Vec<_>>().join(" "), main_top.display());
    db::add_passage(
        tx,
        &db::Passage {
            kind: "project",
            node_id: &repo_id,
            repo_id: Some(&repo_id),
            checkout_id: None,
            source_path: &main_top.to_string_lossy(),
            line: 0,
            loc: "",
            part: 0,
            speaker: "",
            ts: repo_ts,
            title: &label,
            heading: "",
            text: &ptext,
            origin: &porigin,
        },
    )?;
    st.repos += 1;
    for (top, reported) in tops {
        let wt = wt_meta.get(top);
        let (head, branch) = match wt {
            Some(w) => (w.head.clone(), w.branch.clone()),
            None => git::head_info(top),
        };
        let git_dir = git::git_dir(top).map(|g| std::fs::canonicalize(&g).unwrap_or(g).to_string_lossy().into_owned());
        let proposed_id = stable_id("checkout", &top.to_string_lossy());
        let co_id = reconnect_checkout_id(tx, &repo_id, git_dir.as_deref(), &head, branch.as_deref(), &proposed_id)?;
        let mut aliases: Vec<PathBuf> = reported.iter().filter(|r| *r != top).cloned().collect();
        if let Some(w) = wt {
            if &w.path != top && !aliases.contains(&w.path) {
                aliases.push(w.path.clone());
            }
        }
        checkouts.push(Checkout { id: co_id.clone(), repo_id: repo_id.clone(), top: top.clone(), aliases, dev_ino: discover::dev_ino(top), missing: false });
        if only.map(|o| !o.contains(top)).unwrap_or(false) {
            continue;
        }
        st.checkouts += 1;
        // Back on disk (or never gone): drop any earlier "missing" marker.
        tx.execute(
            "UPDATE nodes SET meta=json_remove(meta,'$.missing','$.missing_since','$.missing_scan','$.moved_to','$.moved_candidates') WHERE id=?1 AND json_extract(meta,'$.missing') IS NOT NULL",
            [&co_id],
        )?;
        let prior: Option<(Option<String>, String)> = tx.query_row("SELECT path,meta FROM nodes WHERE id=?1", [&co_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let mut moved_paths: Vec<String> = prior
            .as_ref()
            .and_then(|(_, m)| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|m| m.get("moved_paths").and_then(|x| serde_json::from_value(x.clone()).ok()))
            .unwrap_or_default();
        if let Some(Some(old_path)) = prior.as_ref().map(|(p, _)| p.clone()) {
            if old_path != top.to_string_lossy() && !moved_paths.contains(&old_path) {
                moved_paths.push(old_path);
            }
        }
        if !moved_paths.is_empty() {
            tx.execute(
                "UPDATE nodes SET path=?2,meta=json_set(meta,'$.moved_paths',json(?3)) WHERE id=?1",
                params![co_id, top.to_string_lossy(), serde_json::Value::from(moved_paths.clone()).to_string()],
            )?;
            clone_moved_file_edges(tx, &co_id, &repo_id, top, &moved_paths)?;
        }
        // A checkout `git worktree list` reports under another path (moved without
        // `git worktree repair`): read HEAD and branch from the checkout itself.
        // Unchanged HEAD, index and doc files: keep the previous derived edges.
        let sig = checkout_signature(tx, &co_id, top, &head)?;
        let old: Option<(Option<String>, Option<i64>)> =
            tx.query_row("SELECT json_extract(meta,'$.sig'), json_extract(meta,'$.first_observed') FROM nodes WHERE id=?1", [&co_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (old_sig, first_observed) = old.unwrap_or((None, None));
        if !opts.full && old_sig.as_deref() == Some(sig.as_str()) {
            st.checkouts_reused += 1;
            continue;
        }
        db::clear_origin(tx, &co_id)?;
        tx.execute("DELETE FROM excerpts WHERE node_id IN (SELECT id FROM nodes WHERE checkout_id=?1 AND kind IN ('doc','memory'))", [&co_id])?;
        let is_main = top == &main_top;
        let name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let co_label = format!("{label}:{}", branch.clone().unwrap_or(name));
        let log = git::log(top, opts.commits, 40);
        let co_ts = log.first().map(|c| c.ts);
        // Identity evidence for recognising this checkout after a move: the git
        // admin dir its `.git` points at, full HEAD and branch. `first_observed` is
        // when workmap first indexed it (never a creation time).
        let meta = json!({"branch": branch, "head": &head[..head.len().min(10)], "main": is_main, "reported_path": reported, "moved_paths": moved_paths,
            "git_dir": git_dir, "head_full": head, "first_observed": first_observed.unwrap_or_else(now_unix)});
        db::upsert_node(
            tx,
            &NodeRow { id: &co_id, kind: "checkout", label: &co_label, path: Some(&top.to_string_lossy()), repo_id: Some(&repo_id), checkout_id: Some(&co_id), ts: co_ts, meta: &meta },
        )?;
        db::add_edge(tx, &EdgeRow { src: &co_id, dst: &repo_id, kind: "worktree_of", strength: "structural", source_path: "", source_line: 0, ts: None, note: "", origin: &co_id })?;

        let staged = git::ls_files_stage(top, 50_000);
        let idx = FileIndex::new(staged.iter().map(|x| x.2.clone()).collect());
        // A failed/timed-out git call must not be cached as "no history": warn and retry next scan.
        let git_ok = head.is_empty() || (!log.is_empty() && !idx.files.is_empty());
        if !git_ok {
            st.warnings.push(format!("git log/ls-files returned nothing for {} (timeout or error); will retry next scan", top.display()));
        }
        index_docs(tx, env, top, reported.as_deref(), &repo_id, &co_id, &idx, is_main, st)?;
        if is_main {
            relations::extract_checkout(tx, top, &repo_id, &co_id, &staged)?;
        }
        for ep in docs::entry_points(top, &idx) {
            let abs = top.join(&ep.rel).to_string_lossy().into_owned();
            let fid = db::ensure_file_node(tx, &abs, Some(&repo_id), Some(&co_id), &ep.rel)?;
            let src = top.join(&ep.source).to_string_lossy().into_owned();
            db::add_edge(
                tx,
                &EdgeRow { src: &co_id, dst: &fid, kind: "entrypoint", strength: "structural", source_path: &src, source_line: ep.line as i64, ts: None, note: &ep.note, origin: &co_id },
            )?;
        }
        for cm in &log {
            let cid = stable_id("commit", &cm.sha);
            let label = format!("{} {}", &cm.sha[..cm.sha.len().min(8)], redact(&clip(&cm.subject, 100)));
            let meta = json!({"sha": cm.sha, "author": cm.author, "files": cm.files.len()});
            db::upsert_node(tx, &NodeRow { id: &cid, kind: "commit", label: &label, path: None, repo_id: Some(&repo_id), checkout_id: None, ts: Some(cm.ts), meta: &meta })?;
            db::add_edge(tx, &EdgeRow { src: &co_id, dst: &cid, kind: "history", strength: "structural", source_path: "", source_line: 0, ts: Some(cm.ts), note: "", origin: &co_id })?;
            let files: Vec<&str> = cm.files.iter().take(20).map(|f| f.1.as_str()).collect();
            let ctext = format!("{} {} {}", redact(&clip(&cm.subject, 300)), cm.author, files.join(" "));
            db::add_passage(
                tx,
                &db::Passage {
                    kind: "commit",
                    node_id: &cid,
                    repo_id: Some(&repo_id),
                    checkout_id: Some(&co_id),
                    source_path: "",
                    line: 0,
                    loc: &cm.sha,
                    part: 0,
                    speaker: &cm.author,
                    ts: Some(cm.ts),
                    title: &label,
                    heading: "",
                    text: &ctext,
                    origin: &co_id,
                },
            )?;
            for (status, rel) in &cm.files {
                let abs = top.join(rel).to_string_lossy().into_owned();
                let fid = db::ensure_file_node(tx, &abs, Some(&repo_id), Some(&co_id), rel)?;
                db::add_edge(tx, &EdgeRow { src: &cid, dst: &fid, kind: "touched", strength: "structural", source_path: "", source_line: 0, ts: Some(cm.ts), note: status, origin: &co_id })?;
            }
        }
        // Signature after docs are indexed, so the next scan compares like with like.
        let sig = if git_ok { checkout_signature(tx, &co_id, top, &head)? } else { String::new() };
        tx.execute("UPDATE nodes SET meta=json_set(meta,'$.sig',?2) WHERE id=?1", params![co_id, sig])?;
        indexes.insert(co_id, idx);
    }
    Ok(())
}

/// Keep old transcript file evidence attached to live paths after `mv` by
/// copying edges from historical absolute-path file nodes to same-relative-path
/// nodes under the moved checkout. Original refs resolve through moved_paths.
fn clone_moved_file_edges(tx: &Connection, co_id: &str, repo_id: &str, top: &Path, old_roots: &[String]) -> rusqlite::Result<()> {
    let old_files = crate::overview::nodes(tx, "kind='file' AND checkout_id=?1", &[&co_id])?;
    for old in old_files {
        let Some(path) = old.path.as_deref() else { continue };
        let Some((root, rel)) = old_roots.iter().find_map(|root| path.strip_prefix(root).filter(|suffix| suffix.starts_with('/')).map(|suffix| (root, suffix.trim_start_matches('/')))) else {
            continue;
        };
        let _ = root;
        if rel.is_empty() {
            continue;
        }
        let new_path = top.join(rel).to_string_lossy().into_owned();
        let new_id = db::ensure_file_node(tx, &new_path, Some(repo_id), Some(co_id), rel)?;
        tx.execute(
            "INSERT OR IGNORE INTO edges(src,dst,kind,strength,source_path,source_line,ts,note,origin) SELECT src,?2,kind,strength,source_path,source_line,ts,note,origin FROM edges WHERE dst=?1",
            params![old.id, new_id],
        )?;
        tx.execute("DELETE FROM edges WHERE dst=?1", [&old.id])?;
    }
    Ok(())
}

/// Reuse the recorded checkout identity after a move only when Git's admin dir
/// matches and either branch or full HEAD also matches. This keeps historical
/// refs and conversation attribution attached to the live checkout.
fn reconnect_checkout_id(tx: &Connection, repo_id: &str, git_dir: Option<&str>, head: &str, branch: Option<&str>, proposed: &str) -> rusqlite::Result<String> {
    let Some(git_dir) = git_dir.filter(|x| !x.is_empty()) else { return Ok(proposed.to_string()) };
    // `mark_missing` runs after checkout indexing, so a moved checkout may not
    // carry `missing=true` yet. An absent recorded path is sufficient to
    // consider it, while identity still requires the admin dir plus branch/HEAD.
    let candidates = crate::overview::nodes(tx, "kind='checkout' AND repo_id=?1 AND id<>?2", &[&repo_id, &proposed])?;
    let matching: Vec<_> = candidates
        .into_iter()
        .filter(|co| {
            let recorded_path_absent = co.path.as_deref().map(|p| !Path::new(p).is_dir()).unwrap_or(false);
            let same_dir = co.meta.get("git_dir").and_then(|v| v.as_str()) == Some(git_dir);
            let same_branch = branch.filter(|b| !b.is_empty()).is_some_and(|b| co.meta.get("branch").and_then(|v| v.as_str()) == Some(b));
            let same_head = !head.is_empty() && co.meta.get("head_full").and_then(|v| v.as_str()) == Some(head);
            recorded_path_absent && same_dir && (same_branch || same_head)
        })
        .collect();
    Ok(if matching.len() == 1 { matching[0].id.clone() } else { proposed.to_string() })
}

/// Keep a checkout whose directory disappeared because conversations were
/// recorded in it: its node stays (flagged missing, with the recorded path and
/// branch); what was derived from its files (docs, commit list) is dropped.
/// A live checkout is named as its new location only with identity evidence:
/// the same git worktree admin dir plus the same branch or HEAD, or else the
/// same HEAD commit and the same branch. A branch name alone never counts, and
/// several candidates are listed, never chosen.
fn mark_missing(tx: &Connection, co: &crate::overview::Node, walked: &HashSet<String>, scan_id: i64, recorded: i64, st: &mut Stats) -> rusqlite::Result<()> {
    let already = co.meta.get("missing").and_then(|x| x.as_bool()) == Some(true);
    if !already {
        db::clear_origin(tx, &co.id)?;
        tx.execute("DELETE FROM excerpts WHERE node_id IN (SELECT id FROM nodes WHERE checkout_id=?1 AND kind IN ('doc','memory'))", [&co.id])?;
        tx.execute("DELETE FROM nodes WHERE kind IN ('doc','memory') AND checkout_id=?1", [&co.id])?;
    }
    let ms = |n: &crate::overview::Node, k: &str| n.meta.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let (gd, head, branch) = (ms(co, "git_dir"), ms(co, "head_full"), ms(co, "branch"));
    let mut strong: Vec<(String, String, String)> = Vec::new();
    let mut weak: Vec<(String, String, String)> = Vec::new();
    for live in crate::overview::nodes(tx, "kind='checkout'", &[])? {
        // Any live checkout counts, walked by this run or not: a partial scan must
        // not forget a reconnection a full scan established.
        let on_disk = live.path.as_deref().map(|p| Path::new(p).is_dir()).unwrap_or(false);
        if live.id == co.id || (!walked.contains(&live.id) && (!on_disk || live.meta.get("missing").and_then(|x| x.as_bool()) == Some(true))) {
            continue;
        }
        let path = live.path.clone().unwrap_or_default();
        let (lgd, lhead, lbranch) = (ms(&live, "git_dir"), ms(&live, "head_full"), ms(&live, "branch"));
        let same_branch = !branch.is_empty() && lbranch == branch;
        let same_head = !head.is_empty() && lhead == head;
        if !gd.is_empty() && lgd == gd && (same_branch || same_head) {
            strong.push((
                live.id.clone(),
                path,
                format!("same git worktree admin dir {gd}{}", if same_branch { format!(", branch {branch}") } else { format!(", HEAD {}", &head[..head.len().min(10)]) }),
            ));
        } else if same_head && same_branch {
            weak.push((live.id.clone(), path, format!("same HEAD {} and branch {branch}", &head[..head.len().min(10)])));
        }
    }
    let cands = if strong.is_empty() { weak } else { strong };
    let as_json = |v: &[(String, String, String)]| v.iter().map(|(id, p, e)| json!({"checkout_id": id, "path": p, "evidence": e})).collect::<Vec<_>>();
    let (moved_to, ambiguous) = match cands.len() {
        1 => (as_json(&cands).pop(), vec![]),
        0 => (None, vec![]),
        _ => (None, as_json(&cands)),
    };
    tx.execute(
        "UPDATE nodes SET meta=json_set(meta,'$.missing',json('true'),'$.missing_since',coalesce(json_extract(meta,'$.missing_since'),?2),
           '$.missing_scan',coalesce(json_extract(meta,'$.missing_scan'),?3),'$.moved_to',json(?4),'$.moved_candidates',json(?5)) WHERE id=?1",
        params![co.id, now_unix(), scan_id, moved_to.map(|m| m.to_string()).unwrap_or_else(|| "null".into()), serde_json::Value::from(ambiguous).to_string()],
    )?;
    st.checkouts_missing += 1;
    if !already {
        st.warnings.push(format!("checkout no longer on disk: {} — kept as missing ({recorded} conversation(s) recorded there)", co.path.clone().unwrap_or_default()));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Cheap change detector: HEAD sha, index mtime, and mtimes of previously indexed docs.
fn checkout_signature(tx: &Connection, co_id: &str, top: &Path, head: &str) -> rusqlite::Result<String> {
    let mut st = tx.prepare("SELECT path FROM nodes WHERE checkout_id=?1 AND kind IN ('doc','memory')")?;
    let paths: Vec<String> = st.query_map([co_id], |r| r.get(0))?.flatten().collect();
    let docs: i64 = paths.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| mtime_unix(&m)).sum();
    let root_docs = docs::DOC_NAMES.iter().filter(|n| top.join(n).is_file()).count();
    Ok(format!("v2|{head}|{}|{}|{docs}|{root_docs}", git::index_mtime(top), paths.len()))
}

fn index_doc_file(
    tx: &Connection,
    env: &Env,
    kind: &str,
    abs: &Path,
    label: &str,
    doc_dir_rel: &str,
    top: &Path,
    repo_id: &str,
    co_id: &str,
    idx: &FileIndex,
    is_main: bool,
) -> rusqlite::Result<bool> {
    let Ok(bytes) = crate::util::read_head(abs, 256 << 10) else { return Ok(false) };
    let text = String::from_utf8_lossy(&bytes);
    let info = docs::analyze_markdown(&text, 40, 300);
    let abs_s = abs.to_string_lossy().into_owned();
    let id = stable_id("file", &abs_s);
    let mtime = std::fs::metadata(abs).map(|m| mtime_unix(&m)).ok();
    let meta = json!({"title": info.title, "headings": info.headings, "lines": text.lines().count(), "bytes": bytes.len()});
    db::upsert_node(tx, &NodeRow { id: &id, kind, label, path: Some(&abs_s), repo_id: Some(repo_id), checkout_id: Some(co_id), ts: mtime, meta: &meta })?;
    tx.execute("DELETE FROM excerpts WHERE node_id=?1", [&id])?;
    for ex in &info.excerpts {
        tx.execute("INSERT OR REPLACE INTO excerpts(node_id,line,heading,text) VALUES(?1,?2,?3,?4)", params![id, ex.line as i64, ex.heading, ex.text])?;
    }
    db::add_edge(
        tx,
        &EdgeRow { src: co_id, dst: &id, kind: if kind == "memory" { "memory" } else { "doc" }, strength: "structural", source_path: "", source_line: 0, ts: mtime, note: "", origin: co_id },
    )?;
    doc_passages(tx, &text, &id, label, &abs_s, repo_id, Some(co_id), co_id, mtime)?;
    if is_main && kind == "doc" {
        relations::extract_doc_links(tx, &env.home, abs, &text, top, repo_id, co_id)?;
    }
    for r in &info.refs {
        let Some((rel, how)) = idx.resolve(doc_dir_rel, &r.raw) else { continue };
        let tabs = top.join(&rel).to_string_lossy().into_owned();
        if tabs == abs_s {
            continue;
        }
        let fid = db::ensure_file_node(tx, &tabs, Some(repo_id), Some(co_id), &rel)?;
        // Exact paths (relative to the doc or repo root) are references. A bare
        // filename matched only by being unique in the repo is a weak candidate:
        // never ranked, never shown as "named in".
        let (kind, strength, note) = if how == "unique-name" {
            ("reference_candidate", "candidate", format!("{} | {} unique-name token `{}`", r.heading, r.how, r.raw))
        } else {
            ("references", "structural", format!("{} | {}{}", r.heading, r.how, if how == "path" { String::new() } else { format!(" {how}") }))
        };
        db::add_edge(tx, &EdgeRow { src: &id, dst: &fid, kind, strength, source_path: &abs_s, source_line: r.line as i64, ts: None, note: note.trim(), origin: co_id })?;
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn index_docs(tx: &Connection, env: &Env, top: &Path, reported: Option<&Path>, repo_id: &str, co_id: &str, idx: &FileIndex, is_main: bool, st: &mut Stats) -> rusqlite::Result<()> {
    let mut docs: Vec<String> = idx
        .files
        .iter()
        .filter(|f| {
            let p = Path::new(f.as_str());
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let depth = p.components().count();
            (depth == 1 && (docs::DOC_NAMES.contains(&name.as_str()) || name.ends_with(".md")))
                || (depth <= 3 && docs::DOC_NAMES.contains(&name.as_str()) && name != "CHANGELOG.md")
                || (depth == 2 && f.starts_with("docs/") && name.ends_with(".md"))
        })
        .cloned()
        .collect();
    // Untracked top-level docs (e.g. a README not committed yet) are cheap to check by name.
    for name in docs::DOC_NAMES {
        if !docs.iter().any(|d| d == name) && top.join(name).is_file() {
            docs.push(name.to_string());
        }
    }
    docs.sort_by_key(|d| (Path::new(d).components().count(), !d.to_ascii_uppercase().contains("README"), d.clone()));
    docs.truncate(25);
    for rel in &docs {
        let dir = Path::new(rel).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if index_doc_file(tx, env, "doc", &top.join(rel), rel, &dir, top, repo_id, co_id, idx, is_main)? {
            st.docs += 1;
        }
    }
    // Claude Code per-project memory lives outside the repo, keyed by encoded cwd.
    let mut names: Vec<String> = vec![crate::util::claude_project_dir_name(&top.to_string_lossy())];
    if let Some(r) = reported {
        names.push(crate::util::claude_project_dir_name(&r.to_string_lossy()));
    }
    names.dedup();
    for n in names {
        let mem = env.claude_projects().join(n).join("memory");
        let Ok(rd) = std::fs::read_dir(&mem) else { continue };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "md").unwrap_or(false)).collect();
        files.sort();
        for f in files.into_iter().take(30) {
            let label = format!("claude-memory/{}", f.file_name().unwrap().to_string_lossy());
            if index_doc_file(tx, env, "memory", &f, &label, "", top, repo_id, co_id, idx, false)? {
                st.docs += 1;
            }
        }
    }
    Ok(())
}

/// Searchable doc sections: heading + body, chunked and capped per document.
pub(crate) fn doc_passages(tx: &Connection, text: &str, id: &str, label: &str, abs: &str, repo_id: &str, checkout: Option<&str>, origin: &str, mtime: Option<i64>) -> rusqlite::Result<()> {
    let secs = docs::sections(text);
    let lines: Vec<&str> = text.lines().collect();
    // (start line, heading, slug, end line exclusive); text before the first heading is its own part.
    let mut parts: Vec<(usize, String, String, usize)> = Vec::new();
    let first = secs.first().map(|s| s.line).unwrap_or(lines.len() + 1);
    if first > 1 {
        parts.push((1, String::new(), String::new(), first));
    }
    for (i, s) in secs.iter().enumerate() {
        parts.push((s.line, s.heading.clone(), s.slug.clone(), secs.get(i + 1).map(|n| n.line).unwrap_or(lines.len() + 1)));
    }
    let mut n = 0;
    for (start, heading, slug, end) in parts {
        let body = lines[start.saturating_sub(1)..(end - 1).min(lines.len())].join("\n");
        for (k, chunk) in crate::util::chunks(&body, 1500, 3).into_iter().enumerate() {
            if n >= 80 {
                return Ok(());
            }
            n += 1;
            db::add_passage(
                tx,
                &db::Passage {
                    kind: "doc",
                    node_id: id,
                    repo_id: Some(repo_id),
                    checkout_id: checkout,
                    source_path: abs,
                    line: start as i64,
                    loc: &slug,
                    part: k as i64,
                    speaker: "",
                    ts: mtime,
                    title: label,
                    heading: &heading,
                    text: &redact(&crate::git::scrub_urls(&chunk)),
                    origin,
                },
            )?;
        }
    }
    Ok(())
}

/// Decide whether a source must be (re)parsed.
enum Decision {
    Reuse,
    Unrelated(Option<String>),
    /// Indexed before, but its cwd is in no checkout this run knows: leave the
    /// recorded evidence untouched (never re-attribute or drop it).
    Keep,
    Parse,
}

fn decide(tx: &Connection, key: &str, size: i64, mtime: i64, matcher: &mut Matcher, head_cwd: &mut dyn FnMut() -> Option<String>) -> rusqlite::Result<Decision> {
    let prev = db::source_state(tx, key)?;
    if let Some((s0, m0, status, cwd)) = &prev {
        if *s0 == size && *m0 == mtime {
            if status == "parsed" {
                return Ok(Decision::Reuse);
            }
            if status == "unrelated" {
                // Roots may have changed since: re-check the cached cwd without opening the file.
                let related = cwd.as_deref().map(|c| matcher.locate_any(Path::new(c)).is_some()).unwrap_or(false);
                return Ok(if related { Decision::Parse } else { Decision::Unrelated(cwd.clone()) });
            }
        }
    }
    let cwd = head_cwd();
    // Indexed earlier and attributed to a checkout (that this run does not know,
    // e.g. outside a partial scan's roots): keep it rather than demote it.
    let attributed = || -> rusqlite::Result<bool> {
        if prev.as_ref().map(|p| p.2 != "parsed").unwrap_or(true) {
            return Ok(false);
        }
        tx.query_row("SELECT count(*) FROM source_files f JOIN nodes n ON n.id=f.session_id WHERE f.path=?1 AND n.checkout_id IS NOT NULL", [key], |r| r.get::<_, i64>(0)).map(|n| n > 0)
    };
    match cwd.as_deref() {
        Some(c) if matcher.locate_any(Path::new(c)).is_some() => Ok(Decision::Parse),
        _ if attributed()? => Ok(Decision::Keep),
        _ => Ok(Decision::Unrelated(cwd)),
    }
}

/// What happened to one transcript source.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Reused,
    Kept,
    Unrelated(Option<String>),
    Parsed(String),
    Error,
}

/// Decide, parse and store one source exactly the same way for `scan` and `ingest`.
#[allow(clippy::too_many_arguments)]
fn process_source(
    tx: &Connection,
    key: &str,
    provider: &str,
    size: i64,
    mtime: i64,
    matcher: &mut Matcher,
    indexes: &mut HashMap<String, FileIndex>,
    st: &mut Stats,
    head: &mut dyn FnMut() -> Option<String>,
    parse: &mut dyn FnMut() -> Option<Session>,
) -> rusqlite::Result<Outcome> {
    Ok(match decide(tx, key, size, mtime, matcher, head)? {
        Decision::Reuse => {
            st.sessions_reused += 1;
            Outcome::Reused
        }
        Decision::Keep => {
            st.sessions_reused += 1;
            Outcome::Kept
        }
        Decision::Unrelated(cwd) => {
            st.sources_unrelated += 1;
            db::set_source(tx, key, provider, size, mtime, cwd.as_deref(), "unrelated", None, 0)?;
            Outcome::Unrelated(cwd)
        }
        Decision::Parse => match parse() {
            Some(sess) => {
                st.parse_warnings += sess.warnings;
                let sid = store_session(tx, key, &sess, matcher, indexes)?;
                db::set_source(tx, key, provider, size, mtime, sess.cwd.as_deref(), "parsed", Some(&sid), sess.warnings as i64)?;
                st.sessions_parsed += 1;
                Outcome::Parsed(sid)
            }
            None => {
                st.parse_warnings += 1;
                db::set_source(tx, key, provider, size, mtime, None, "error", None, 1)?;
                Outcome::Error
            }
        },
    })
}

/// Claude Code subagent transcripts: `<encoded-cwd>/<session-id>/subagents/agent-*.jsonl`.
fn claude_subagent_files(cdir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(cdir) else { return out };
    for p in projects.flatten().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)) {
        let Ok(sessions) = std::fs::read_dir(p.path()) else { continue };
        for sdir in sessions.flatten().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)) {
            let Ok(rd) = std::fs::read_dir(sdir.path().join("subagents")) else { continue };
            for f in rd.flatten() {
                let path = f.path();
                let name = f.file_name().to_string_lossy().into_owned();
                if name.starts_with("agent-") && name.ends_with(".jsonl") && f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    out.push(path);
                }
            }
        }
    }
    out
}

fn list_jsonl(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() && depth > 0 {
            list_jsonl(&p, depth - 1, out);
        } else if ft.is_file() && p.extension().map(|x| x == "jsonl").unwrap_or(false) {
            out.push(p);
        }
    }
}

fn index_sessions(tx: &Connection, env: &Env, matcher: &mut Matcher, indexes: &mut HashMap<String, FileIndex>, opts: &ScanOpts, st: &mut Stats) -> rusqlite::Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut handle = |tx: &Connection,
                      key: &str,
                      provider: &str,
                      size: i64,
                      mtime: i64,
                      matcher: &mut Matcher,
                      st: &mut Stats,
                      head: &mut dyn FnMut() -> Option<String>,
                      parse: &mut dyn FnMut() -> Option<Session>|
     -> rusqlite::Result<()> {
        seen.insert(key.to_string());
        if let Outcome::Parsed(_) = process_source(tx, key, provider, size, mtime, matcher, indexes, st, head, parse)? {
            if st.sessions_parsed % 25 == 0 {
                tx.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
            }
            if opts.verbose {
                eprintln!("parsed {provider} {key}");
            }
        }
        Ok(())
    };

    // Claude Code: ~/.claude/projects/<encoded-cwd>/<session>.jsonl
    let cdir = env.claude_projects();
    if cdir.is_dir() {
        let mut files = Vec::new();
        list_jsonl(&cdir, 1, &mut files);
        files.extend(claude_subagent_files(&cdir));
        files.sort();
        for f in files {
            let Ok(m) = std::fs::metadata(&f) else { continue };
            let key = f.to_string_lossy().into_owned();
            let ff = f.clone();
            handle(tx, &key, "claude", m.len() as i64, mtime_unix(&m), matcher, st, &mut || sessions::claude_head_cwd(&ff), &mut || sessions::parse_claude(&f).ok())?;
        }
    } else {
        st.warnings.push(format!("claude: source not found: {}", cdir.display()));
    }

    // Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl (+ thread index accelerator).
    let chome = env.codex_home();
    let sdir = chome.join("sessions");
    if sdir.is_dir() {
        let threads = sessions::codex_thread_index(&chome);
        let mut files = Vec::new();
        list_jsonl(&sdir, 3, &mut files);
        files.sort();
        for f in files {
            let Ok(m) = std::fs::metadata(&f) else { continue };
            let key = f.to_string_lossy().into_owned();
            let known = threads.get(&key).cloned();
            let ff = f.clone();
            let k2 = known.clone();
            handle(tx, &key, "codex", m.len() as i64, mtime_unix(&m), matcher, st, &mut || k2.as_ref().map(|t| t.0.clone()).or_else(|| sessions::codex_head_cwd(&ff)), &mut || {
                let mut s = sessions::parse_codex(&f).ok()?;
                if let Some((_, title, _)) = &known {
                    if !title.trim().is_empty() {
                        s.title = Some(clip(&redact(title), 120));
                    }
                }
                Some(s)
            })?;
        }
    } else {
        st.warnings.push(format!("codex: source not found: {}", sdir.display()));
    }

    // OpenCode current SQLite store.
    let odir = env.opencode_dir();
    let odb = odir.join("opencode.db");
    if odb.is_file() {
        match sessions::opencode_sessions(&odb) {
            Ok(rows) => {
                for row in rows {
                    let key = format!("{}#session={}", odb.display(), row.id);
                    let dir = row.directory.clone();
                    handle(tx, &key, "opencode", 0, row.updated, matcher, st, &mut || Some(dir.clone()), &mut || sessions::parse_opencode_db(&odb, &row, opts.max_parts).ok())?;
                }
            }
            Err(e) => st.warnings.push(format!("opencode: cannot read {} read-only: {e}", odb.display())),
        }
    } else {
        st.warnings.push(format!("opencode: source not found: {}", odb.display()));
    }
    // OpenCode legacy JSON storage.
    let storage = odir.join("storage");
    for info in sessions::legacy_opencode_session_files(&storage) {
        let Ok(m) = std::fs::metadata(&info) else { continue };
        let key = info.to_string_lossy().into_owned();
        let i2 = info.clone();
        handle(tx, &key, "opencode-legacy", m.len() as i64, mtime_unix(&m), matcher, st, &mut || sessions::legacy_opencode_info(&i2).map(|x| x.1), &mut || {
            sessions::parse_opencode_legacy(&storage, &info, opts.max_parts)
        })?;
    }

    // Sources indexed before but gone now: keep evidence, flag it.
    let gone: Vec<(String, Option<String>)> = {
        let mut q = tx.prepare("SELECT path, session_id FROM source_files WHERE status IN ('parsed','missing')")?;
        let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
        rows.flatten().filter(|(p, _)| !seen.contains(p)).collect()
    };
    for (p, sid) in gone {
        let file_path = p.split("#session=").next().unwrap_or(&p);
        let missing = !Path::new(file_path).exists() || p.contains("#session=");
        if !missing {
            continue;
        }
        st.sources_missing += 1;
        tx.execute("UPDATE source_files SET status='missing' WHERE path=?1", [&p])?;
        if let Some(sid) = sid {
            tx.execute("UPDATE nodes SET meta=json_set(meta,'$.source_missing',json('true')) WHERE id=?1", [&sid])?;
        }
    }
    Ok(())
}

/// Write one session and its evidence edges (replacing earlier edges from the same source).
pub fn store_session(tx: &Connection, key: &str, s: &Session, matcher: &mut Matcher, indexes: &mut HashMap<String, FileIndex>) -> rusqlite::Result<String> {
    let origin = format!("src:{key}");
    db::clear_origin(tx, &origin)?;
    let sid = stable_id("session", &format!("{}:{}", s.provider, s.native_id));
    let cwd = s.cwd.clone().unwrap_or_default();
    let loc = matcher.locate_any(Path::new(&cwd));
    let (co_id, repo_id) = loc.as_ref().map(|(i, _)| (matcher.checkouts[*i].id.clone(), matcher.checkouts[*i].repo_id.clone())).unzip();
    let label = s.title.clone().filter(|t| !t.is_empty()).or_else(|| s.first_prompt.as_ref().map(|p| clip(&p.1, 80))).unwrap_or_else(|| "(untitled)".into());
    // Where the conversation ran, as recorded at parse time: survives the checkout
    // later disappearing from disk.
    let checkout_snapshot = match &co_id {
        Some(id) => {
            let row: Option<(Option<String>, Option<String>)> = tx.query_row("SELECT path, json_extract(meta,'$.branch') FROM nodes WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let (path, branch) = row.unwrap_or((None, None));
            json!({"id": id, "path": path, "branch": branch})
        }
        None => serde_json::Value::Null,
    };

    // Aggregate: first occurrence per (file, op, strength), with a count.
    let mut agg: BTreeMap<(String, &'static str, &'static str), (usize, &sessions::FileEvent, PathBuf)> = BTreeMap::new();
    let mut external = 0usize;
    for ev in &s.events {
        let abs = crate::util::resolve_against(Path::new(&cwd), &ev.path);
        let Some((ci, rel)) = matcher.locate_any(&abs) else {
            external += 1;
            continue;
        };
        let co = &matcher.checkouts[ci];
        // Reused checkouts load their tracked-file list only when a session needs it.
        let ix = indexes.entry(co.id.clone()).or_insert_with(|| FileIndex::new(git::ls_files(&co.top, 50_000)));
        if matches!(ev.op, "mention" | "search" | "list") && !ix.files.contains(&*rel.to_string_lossy()) {
            continue; // mentioned/searched/listed paths only count when they name a tracked file
        }
        let canon = co.top.join(&rel);
        let entry = agg.entry((canon.to_string_lossy().into_owned(), ev.op, ev.strength)).or_insert((0, ev, rel));
        entry.0 += 1;
    }
    let counts = json!({
        "read": agg.keys().filter(|k| k.1 == "read").count(),
        "write": agg.keys().filter(|k| k.1 == "write").count(),
        "mentioned": agg.keys().filter(|k| k.1 == "mention").count(),
        "read_via_shell": agg.keys().filter(|k| k.1 == "read" && k.2 == "parsed").count(),
        "external_paths_skipped": external,
    });
    let meta = json!({
        "provider": s.provider, "native_id": s.native_id, "cwd": s.cwd, "branch": s.branch,
        "first_prompt": s.first_prompt.as_ref().map(|p| json!({"ts": p.0, "text": p.1, "line": p.2})),
        "last_prompt": s.last_prompt.as_ref().map(|p| json!({"ts": p.0, "text": p.1, "line": p.2})),
        "started": s.started, "updated": s.updated, "warnings": s.warnings, "truncated": s.truncated, "files": counts,
        "messages": s.messages.len(), "messages_capped": s.messages_capped,
        "checkout": checkout_snapshot,
        "parent": s.parent.as_ref().map(|l| json!({"provider": s.provider, "native_id": l.native_id, "evidence": l.evidence, "tool_use_id": l.tool_use_id,
            "agent_type": l.agent_type, "description": l.description})),
    });
    db::upsert_node(
        tx,
        &NodeRow {
            id: &sid,
            kind: "session",
            label: &clip(&redact(&label), 120),
            path: Some(&s.source_path),
            repo_id: repo_id.as_deref(),
            checkout_id: co_id.as_deref(),
            ts: Some(s.updated.max(s.started)),
            meta: &meta,
        },
    )?;
    // The conversation title is searchable once, on its first message.
    let title = clip(&redact(&label), 120);
    let src_file = s.source_path.split("#session=").next().unwrap_or(&s.source_path);
    for m in &s.messages {
        let loc = if m.line > 0 { format!("l{}", m.line) } else { format!("p{}", m.seq) };
        for (k, chunk) in crate::util::chunks(&m.text, 1200, 4).into_iter().enumerate() {
            db::add_passage(
                tx,
                &db::Passage {
                    kind: "message",
                    node_id: &sid,
                    repo_id: repo_id.as_deref(),
                    checkout_id: co_id.as_deref(),
                    source_path: src_file,
                    line: m.line as i64,
                    loc: &loc,
                    part: k as i64,
                    speaker: m.speaker,
                    ts: m.ts,
                    title: if m.seq == 1 && k == 0 { &title } else { "" },
                    heading: s.provider,
                    text: &chunk,
                    origin: &origin,
                },
            )?;
        }
    }
    if let Some(co) = &co_id {
        db::add_edge(tx, &EdgeRow { src: &sid, dst: co, kind: "ran_in", strength: "inferred", source_path: &s.source_path, source_line: 0, ts: Some(s.started), note: "cwd", origin: &origin })?;
    }
    // Subagent -> parent conversation, with the evidence that proved it. The parent
    // id is derived the same way as every session id, so the edge resolves once
    // (or whenever) the parent transcript is indexed.
    if let Some(l) = &s.parent {
        let parent = stable_id("session", &format!("{}:{}", s.provider, l.native_id));
        let first = s.messages.first().map(|m| m.line as i64).unwrap_or(0);
        db::add_edge(
            tx,
            &EdgeRow {
                src: &sid,
                dst: &parent,
                kind: "subagent_of",
                strength: "structural",
                source_path: src_file,
                source_line: first,
                ts: Some(s.started),
                note: &l.evidence.join("; "),
                origin: &origin,
            },
        )?;
    }
    for ((abs, op, strength), (n, ev, rel)) in agg.iter().take(400) {
        let (ci, _) = matcher.locate_any(Path::new(abs)).unwrap_or((0, PathBuf::new()));
        let co = matcher.checkouts[ci].clone();
        let fid = db::ensure_file_node(tx, abs, Some(&co.repo_id), Some(&co.id), &rel.to_string_lossy())?;
        let kind = match *op {
            "mention" => "mentioned",
            "search" => "searched",
            "list" => "listed",
            other => other,
        };
        let note = format!("{}{}{}", ev.tool, if *n > 1 { format!(" x{n}") } else { String::new() }, if ev.locator.is_empty() { String::new() } else { format!(" @{}", ev.locator) });
        let src_path = s.source_path.split("#session=").next().unwrap_or(&s.source_path);
        db::add_edge(tx, &EdgeRow { src: &sid, dst: &fid, kind, strength, source_path: src_path, source_line: ev.line as i64, ts: ev.ts, note: &note, origin: &origin })?;
    }
    Ok(sid)
}

// ------------------------------------------------------------------ targeted ingest

/// `workmap ingest`: refresh only the named transcripts and/or checkouts.
#[derive(Default, Debug, Clone)]
pub struct IngestOpts {
    /// Transcript sources: a Claude Code `.jsonl` (incl. `subagents/agent-*.jsonl`),
    /// a Codex `rollout-*.jsonl`, an OpenCode legacy `storage/session/**.json`, or
    /// `<opencode.db>#session=<id>`.
    pub transcripts: Vec<PathBuf>,
    /// Checkout directories to re-index (docs, entry points, recent commits).
    pub checkouts: Vec<PathBuf>,
}

#[derive(Default, Debug)]
pub struct IngestStats {
    pub ms: u128,
    pub checkouts: usize,
    /// One line per requested source: `<outcome> <provider> <key>`.
    pub lines: Vec<String>,
    pub stats: Stats,
}

/// Same spelling of a source path as the scan walk produces (`<provider dir>/<rest>`),
/// so ingest and scan replace the same rows instead of creating twins.
fn source_key(p: &Path, dirs: &[PathBuf]) -> PathBuf {
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf()) };
    let canon = std::fs::canonicalize(&abs).unwrap_or(abs.clone());
    for d in dirs {
        if let Ok(rest) = abs.strip_prefix(d) {
            return d.join(rest);
        }
        if let Some(rest) = std::fs::canonicalize(d).ok().and_then(|cd| canon.strip_prefix(&cd).ok().map(Path::to_path_buf)) {
            return d.join(rest);
        }
    }
    canon
}

/// Refresh only the given sources/checkouts in ONE immediate transaction: no
/// home walk, no pruning of anything else, no remote sync, no relation pass.
/// Parsing and storing go through the same code as `scan` (`process_source` →
/// `store_session`), so the rows are those a full scan would write. Readers
/// (WAL) see either the previous or the new state of a conversation, never a mix.
pub fn ingest(env: &Env, c: &mut Connection, o: &IngestOpts) -> Result<IngestStats, String> {
    let t0 = Instant::now();
    let mut out = IngestStats::default();
    // A running scan holds the write lock for a while; a hook-spawned ingest waits.
    c.busy_timeout(std::time::Duration::from_secs(120)).map_err(|e| e.to_string())?;
    let opts = ScanOpts { full: true, ..Default::default() };
    let e = |e: rusqlite::Error| e.to_string();
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(e)?;
    let mut st = Stats::default();
    let mut checkouts: Vec<Checkout> = Vec::new();
    let mut indexes: HashMap<String, FileIndex> = HashMap::new();
    if !o.checkouts.is_empty() {
        let mut only: HashSet<PathBuf> = HashSet::new();
        let mut cands: Vec<PathBuf> = Vec::new();
        for d in &o.checkouts {
            let (top, _) = git::probe_fast(d).or_else(|| git::probe(d)).ok_or_else(|| format!("{} is not a git checkout", d.display()))?;
            only.insert(top.clone());
            cands.push(top);
        }
        let (repos, wt_meta) = group_checkouts(cands.iter().map(|p| p.as_path()));
        for (common, tops) in &repos {
            index_repo(&tx, env, &opts, common, tops, Some(&only), &wt_meta, &mut st, &mut checkouts, &mut indexes).map_err(e)?;
        }
        out.checkouts = st.checkouts;
        for top in &only {
            out.lines.push(format!("indexed checkout {}", top.display()));
        }
    }
    let known: HashSet<String> = checkouts.iter().map(|c| c.id.clone()).collect();
    checkouts.extend(db_checkouts(&tx, &known).map_err(e)?);
    let mut matcher = Matcher::new(checkouts);

    let cdir = env.claude_projects();
    let sdir = env.codex_home().join("sessions");
    let odir = env.opencode_dir();
    let mut codex_threads: Option<HashMap<String, (String, String, String)>> = None;
    for raw in &o.transcripts {
        let raw_s = raw.to_string_lossy().into_owned();
        // OpenCode SQLite: one session of the database, `<db>#session=<id>`.
        if let Some((db_part, ses)) = raw_s.split_once("#session=") {
            let dbp = source_key(Path::new(db_part), std::slice::from_ref(&odir));
            let rows = sessions::opencode_sessions(&dbp).map_err(|er| format!("{}: cannot read OpenCode database read-only: {er}", dbp.display()))?;
            let row = rows.into_iter().find(|r| r.id == ses).ok_or_else(|| format!("{}: no OpenCode session {ses}", dbp.display()))?;
            let key = format!("{}#session={}", dbp.display(), row.id);
            let dir = row.directory.clone();
            let updated = row.updated;
            let r =
                process_source(&tx, &key, "opencode", 0, updated, &mut matcher, &mut indexes, &mut st, &mut || Some(dir.clone()), &mut || sessions::parse_opencode_db(&dbp, &row, opts.max_parts).ok())
                    .map_err(e)?;
            out.lines.push(outcome_line(&r, "opencode", &key));
            continue;
        }
        if raw.extension().map(|x| x == "db").unwrap_or(false) {
            return Err(format!("{}: an OpenCode database holds many sessions; pass one as --transcript '{}#session=<id>' (or run `workmap scan`)", raw.display(), raw.display()));
        }
        let path = source_key(raw, &[cdir.clone(), sdir.clone(), odir.clone()]);
        let Ok(m) = std::fs::metadata(&path) else {
            // Gone since indexed: evidence stays, flagged missing (as scan does).
            let key = path.to_string_lossy().into_owned();
            let sid: Option<Option<String>> = tx.query_row("SELECT session_id FROM source_files WHERE path=?1 AND status IN ('parsed','missing')", [&key], |r| r.get(0)).optional().map_err(e)?;
            if let Some(sid) = sid {
                tx.execute("UPDATE source_files SET status='missing' WHERE path=?1", [&key]).map_err(e)?;
                if let Some(sid) = sid {
                    tx.execute("UPDATE nodes SET meta=json_set(meta,'$.source_missing',json('true')) WHERE id=?1", [&sid]).map_err(e)?;
                }
                out.lines.push(format!("missing {} (indexed earlier; evidence kept, flagged missing)", path.display()));
            } else {
                out.lines.push(format!("missing {} (not a file; nothing indexed)", path.display()));
            }
            continue;
        };
        let key = path.to_string_lossy().into_owned();
        let provider = if path.starts_with(&cdir) {
            "claude"
        } else if path.starts_with(&sdir) {
            "codex"
        } else if path.starts_with(odir.join("storage")) {
            "opencode-legacy"
        } else {
            sniff_provider(&path).ok_or_else(|| format!("{}: not a Claude Code, Codex or OpenCode transcript", path.display()))?
        };
        let (size, mtime) = (m.len() as i64, mtime_unix(&m));
        let r = match provider {
            "claude" => process_source(&tx, &key, provider, size, mtime, &mut matcher, &mut indexes, &mut st, &mut || sessions::claude_head_cwd(&path), &mut || sessions::parse_claude(&path).ok()),
            "codex" => {
                let threads = codex_threads.get_or_insert_with(|| sessions::codex_thread_index(&env.codex_home()));
                let known = threads.get(&key).cloned();
                let k2 = known.clone();
                process_source(&tx, &key, provider, size, mtime, &mut matcher, &mut indexes, &mut st, &mut || k2.as_ref().map(|t| t.0.clone()).or_else(|| sessions::codex_head_cwd(&path)), &mut || {
                    let mut s = sessions::parse_codex(&path).ok()?;
                    if let Some((_, title, _)) = &known {
                        if !title.trim().is_empty() {
                            s.title = Some(clip(&redact(title), 120));
                        }
                    }
                    Some(s)
                })
            }
            _ => {
                let storage = odir.join("storage");
                process_source(&tx, &key, provider, size, mtime, &mut matcher, &mut indexes, &mut st, &mut || sessions::legacy_opencode_info(&path).map(|x| x.1), &mut || {
                    sessions::parse_opencode_legacy(&storage, &path, opts.max_parts)
                })
            }
        }
        .map_err(e)?;
        out.lines.push(outcome_line(&r, provider, &key));
    }
    // Nodes only this ingest's replaced evidence pointed at.
    tx.execute("DELETE FROM nodes WHERE kind IN ('file','commit') AND id NOT IN (SELECT dst FROM edges) AND id NOT IN (SELECT src FROM edges)", []).map_err(e)?;
    tx.commit().map_err(e)?;
    out.ms = t0.elapsed().as_millis();
    out.stats = st;
    Ok(out)
}

fn outcome_line(r: &Outcome, provider: &str, key: &str) -> String {
    match r {
        Outcome::Parsed(_) => format!("parsed {provider} {key}"),
        Outcome::Reused => format!("unchanged {provider} {key}"),
        Outcome::Kept => format!("kept {provider} {key} (cwd in no known checkout; earlier evidence left as is)"),
        Outcome::Unrelated(cwd) => format!("unrelated {provider} {key} (cwd {} is in no indexed checkout)", cwd.as_deref().unwrap_or("unknown")),
        Outcome::Error => format!("error {provider} {key} (could not be parsed)"),
    }
}

/// Provider of a transcript outside the default stores, from its first record.
fn sniff_provider(p: &Path) -> Option<&'static str> {
    let head = crate::util::read_head(p, 256 << 10).ok()?;
    let first = head.split(|b| *b == b'\n').find(|l| !l.iter().all(|b| b.is_ascii_whitespace()))?;
    let v: serde_json::Value = serde_json::from_slice(first).ok()?;
    if v.get("type").and_then(|x| x.as_str()) == Some("session_meta") {
        return Some("codex");
    }
    let txt = String::from_utf8_lossy(&head);
    (txt.contains("\"sessionId\"") || v.get("type").and_then(|x| x.as_str()) == Some("summary")).then_some("claude")
}
