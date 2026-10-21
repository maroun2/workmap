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
}

impl Default for ScanOpts {
    fn default() -> Self {
        ScanOpts { roots: vec![], include_home: true, max_dirs: 300_000, max_depth: 12, commits: 30, max_parts: 20_000, verbose: false, full: false, sessions: true, remote: crate::remote::Mode::Off }
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

    /// Return (checkout index, path relative to checkout top).
    pub fn locate(&mut self, p: &Path) -> Option<(usize, PathBuf)> {
        // Longest textual prefix first (cheap, also works for deleted files).
        let mut best: Option<(usize, usize, PathBuf)> = None;
        for (i, c) in self.checkouts.iter().enumerate() {
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

    // Group checkouts by common git dir = repository identity.
    let mut repos: BTreeMap<PathBuf, Vec<(PathBuf, Option<PathBuf>)>> = BTreeMap::new(); // common -> [(canonical top, reported path)]
    let mut seen_tops: HashSet<(u64, u64)> = HashSet::new();
    let mut add_top = |repos: &mut BTreeMap<PathBuf, Vec<(PathBuf, Option<PathBuf>)>>, common: &Path, top: PathBuf, reported: Option<PathBuf>| {
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
    for cand in &cands {
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

    let mut checkouts: Vec<Checkout> = Vec::new();
    let mut indexes: HashMap<String, FileIndex> = HashMap::new();
    for (common, tops) in &repos {
        // One transaction per repository: an interrupted scan keeps finished repos.
        let tx = c.transaction()?;
        let main_top = tops.iter().map(|t| &t.0).find(|t| t.join(".git").is_dir()).unwrap_or(&tops[0].0).clone();
        let remotes = git::remotes(&main_top);
        let repo_id = stable_id("repo", &common.to_string_lossy());
        let label = repo_label(common, &remotes, &main_top);
        let branches = git::branches(&main_top, 20);
        let repo_meta = json!({"common_dir": common, "remotes": remotes.iter().map(|r| json!({"name": r.0, "url": r.1})).collect::<Vec<_>>(),
            "branches": branches.iter().map(|b| json!({"name": b.0, "ts": b.1, "sha": &b.2[..b.2.len().min(10)]})).collect::<Vec<_>>(), "checkouts": tops.len()});
        let repo_ts = branches.iter().map(|b| b.1).max();
        db::upsert_node(&tx, &NodeRow { id: &repo_id, kind: "repo", label: &label, path: Some(&common.to_string_lossy()), repo_id: Some(&repo_id), checkout_id: None, ts: repo_ts, meta: &repo_meta })?;
        // Project-level searchable text: name, remotes, main checkout path.
        let porigin = format!("repo:{repo_id}");
        db::clear_origin(&tx, &porigin)?;
        let ids: Vec<String> = remotes.iter().filter_map(|r| crate::util::remote_identity(&r.1)).collect();
        let ptext = format!("{label} {} {} {}", ids.join(" "), remotes.iter().map(|r| r.1.as_str()).collect::<Vec<_>>().join(" "), main_top.display());
        db::add_passage(
            &tx,
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
            let co_id = stable_id("checkout", &top.to_string_lossy());
            let wt = wt_meta.get(top);
            let mut aliases: Vec<PathBuf> = reported.iter().filter(|r| *r != top).cloned().collect();
            if let Some(w) = wt {
                if &w.path != top && !aliases.contains(&w.path) {
                    aliases.push(w.path.clone());
                }
            }
            checkouts.push(Checkout { id: co_id.clone(), repo_id: repo_id.clone(), top: top.clone(), aliases, dev_ino: discover::dev_ino(top) });
            st.checkouts += 1;
            // Unchanged HEAD, index and doc files: keep the previous derived edges.
            let sig = checkout_signature(&tx, &co_id, top, wt.map(|w| w.head.as_str()).unwrap_or(""))?;
            let old_sig: Option<String> = tx.query_row("SELECT json_extract(meta,'$.sig') FROM nodes WHERE id=?1", [&co_id], |r| r.get(0)).optional()?.flatten();
            if !opts.full && old_sig.as_deref() == Some(sig.as_str()) {
                st.checkouts_reused += 1;
                continue;
            }
            db::clear_origin(&tx, &co_id)?;
            tx.execute("DELETE FROM excerpts WHERE node_id IN (SELECT id FROM nodes WHERE checkout_id=?1 AND kind IN ('doc','memory'))", [&co_id])?;
            let branch = wt.and_then(|w| w.branch.clone());
            let head = wt.map(|w| w.head.clone()).unwrap_or_default();
            let is_main = top == &main_top;
            let name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let co_label = format!("{label}:{}", branch.clone().unwrap_or(name));
            let log = git::log(top, opts.commits, 40);
            let co_ts = log.first().map(|c| c.ts);
            let meta = json!({"branch": branch, "head": &head[..head.len().min(10)], "main": is_main, "reported_path": reported});
            db::upsert_node(
                &tx,
                &NodeRow { id: &co_id, kind: "checkout", label: &co_label, path: Some(&top.to_string_lossy()), repo_id: Some(&repo_id), checkout_id: Some(&co_id), ts: co_ts, meta: &meta },
            )?;
            db::add_edge(&tx, &EdgeRow { src: &co_id, dst: &repo_id, kind: "worktree_of", strength: "structural", source_path: "", source_line: 0, ts: None, note: "", origin: &co_id })?;

            let staged = git::ls_files_stage(top, 50_000);
            let idx = FileIndex::new(staged.iter().map(|x| x.2.clone()).collect());
            // A failed/timed-out git call must not be cached as "no history": warn and retry next scan.
            let git_ok = head.is_empty() || (!log.is_empty() && !idx.files.is_empty());
            if !git_ok {
                st.warnings.push(format!("git log/ls-files returned nothing for {} (timeout or error); will retry next scan", top.display()));
            }
            index_docs(&tx, env, top, reported.as_deref(), &repo_id, &co_id, &idx, is_main, &mut st)?;
            if is_main {
                relations::extract_checkout(&tx, top, &repo_id, &co_id, &staged)?;
            }
            for ep in docs::entry_points(top, &idx) {
                let abs = top.join(&ep.rel).to_string_lossy().into_owned();
                let fid = db::ensure_file_node(&tx, &abs, Some(&repo_id), Some(&co_id), &ep.rel)?;
                let src = top.join(&ep.source).to_string_lossy().into_owned();
                db::add_edge(
                    &tx,
                    &EdgeRow { src: &co_id, dst: &fid, kind: "entrypoint", strength: "structural", source_path: &src, source_line: ep.line as i64, ts: None, note: &ep.note, origin: &co_id },
                )?;
            }
            for cm in &log {
                let cid = stable_id("commit", &cm.sha);
                let label = format!("{} {}", &cm.sha[..cm.sha.len().min(8)], redact(&clip(&cm.subject, 100)));
                let meta = json!({"sha": cm.sha, "author": cm.author, "files": cm.files.len()});
                db::upsert_node(&tx, &NodeRow { id: &cid, kind: "commit", label: &label, path: None, repo_id: Some(&repo_id), checkout_id: None, ts: Some(cm.ts), meta: &meta })?;
                db::add_edge(&tx, &EdgeRow { src: &co_id, dst: &cid, kind: "history", strength: "structural", source_path: "", source_line: 0, ts: Some(cm.ts), note: "", origin: &co_id })?;
                let files: Vec<&str> = cm.files.iter().take(20).map(|f| f.1.as_str()).collect();
                let ctext = format!("{} {} {}", redact(&clip(&cm.subject, 300)), cm.author, files.join(" "));
                db::add_passage(
                    &tx,
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
                    let fid = db::ensure_file_node(&tx, &abs, Some(&repo_id), Some(&co_id), rel)?;
                    db::add_edge(&tx, &EdgeRow { src: &cid, dst: &fid, kind: "touched", strength: "structural", source_path: "", source_line: 0, ts: Some(cm.ts), note: status, origin: &co_id })?;
                }
            }
            // Signature after docs are indexed, so the next scan compares like with like.
            let sig = if git_ok { checkout_signature(&tx, &co_id, top, &head)? } else { String::new() };
            tx.execute("UPDATE nodes SET meta=json_set(meta,'$.sig',?2) WHERE id=?1", params![co_id, sig])?;
            indexes.insert(co_id, idx);
        }
        tx.commit()?;
    }
    // Sessions: batches of explicit transactions so a long first scan keeps progress.
    let tx: &Connection = c;
    tx.execute_batch("BEGIN")?;
    // Checkouts listed by `git worktree list` whose directory is gone.
    for (p, wt) in &wt_meta {
        if !p.exists() {
            st.warnings.push(format!("worktree registered but missing on disk: {} ({})", p.display(), wt.branch.clone().unwrap_or_default()));
        }
    }

    let mut matcher = Matcher::new(checkouts);
    if opts.sessions {
        index_sessions(tx, env, &mut matcher, &mut indexes, opts, &mut st)?;
    }

    // Checkouts indexed earlier whose directory is gone: drop them and what they produced.
    let live: HashSet<String> = matcher.checkouts.iter().map(|c| c.id.clone()).collect();
    for co in crate::overview::nodes(tx, "kind='checkout'", &[])? {
        if live.contains(&co.id) || co.path.as_deref().map(|p| Path::new(p).exists()).unwrap_or(false) {
            continue;
        }
        db::clear_origin(tx, &co.id)?;
        tx.execute("DELETE FROM nodes WHERE id=?1 OR (kind IN ('doc','memory') AND checkout_id=?1)", [&co.id])?;
        tx.execute("UPDATE nodes SET checkout_id=NULL WHERE kind='session' AND checkout_id=?1", [&co.id])?;
        st.checkouts_pruned += 1;
        st.warnings.push(format!("pruned checkout no longer on disk: {}", co.path.unwrap_or_default()));
    }
    tx.execute("DELETE FROM nodes WHERE kind='repo' AND id NOT IN (SELECT repo_id FROM nodes WHERE kind='checkout' AND repo_id IS NOT NULL)", [])?;

    // Remote catalog (metadata only) before relations, so doc/manifest URLs can point at it.
    crate::remote::sync(env, tx, opts.remote, &mut st)?;
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
        "checkouts_pruned": st.checkouts_pruned, "messages": st.messages, "sessions_truncated": st.sessions_truncated, "relations": st.relations,
        "remote": st.remote, "sessions_indexed": opts.sessions});
    tx.execute("UPDATE scans SET finished=?1, stats=?2 WHERE id=?3", params![now_unix(), stats.to_string(), scan_id])?;
    tx.execute_batch("COMMIT")?;
    Ok(st)
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
    doc_passages(tx, &text, &id, label, &abs_s, repo_id, co_id, mtime)?;
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
fn doc_passages(tx: &Connection, text: &str, id: &str, label: &str, abs: &str, repo_id: &str, co_id: &str, mtime: Option<i64>) -> rusqlite::Result<()> {
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
                    checkout_id: Some(co_id),
                    source_path: abs,
                    line: start as i64,
                    loc: &slug,
                    part: k as i64,
                    speaker: "",
                    ts: mtime,
                    title: label,
                    heading: &heading,
                    text: &redact(&chunk),
                    origin: co_id,
                },
            )?;
        }
    }
    Ok(())
}

/// Decide whether a source must be (re)parsed. Returns Some(cwd) when related.
enum Decision {
    Reuse,
    Unrelated(Option<String>),
    Parse,
}

fn decide(tx: &Connection, key: &str, size: i64, mtime: i64, matcher: &mut Matcher, head_cwd: impl FnOnce() -> Option<String>) -> rusqlite::Result<Decision> {
    if let Some((s0, m0, status, cwd)) = db::source_state(tx, key)? {
        if s0 == size && m0 == mtime {
            if status == "parsed" {
                return Ok(Decision::Reuse);
            }
            if status == "unrelated" {
                // Roots may have changed since: re-check the cached cwd without opening the file.
                let related = cwd.as_deref().map(|c| matcher.locate(Path::new(c)).is_some()).unwrap_or(false);
                return Ok(if related { Decision::Parse } else { Decision::Unrelated(cwd) });
            }
        }
    }
    let cwd = head_cwd();
    match cwd.as_deref() {
        Some(c) if matcher.locate(Path::new(c)).is_some() => Ok(Decision::Parse),
        _ => Ok(Decision::Unrelated(cwd)),
    }
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
        match decide(tx, key, size, mtime, matcher, head)? {
            Decision::Reuse => st.sessions_reused += 1,
            Decision::Unrelated(cwd) => {
                st.sources_unrelated += 1;
                db::set_source(tx, key, provider, size, mtime, cwd.as_deref(), "unrelated", None, 0)?;
            }
            Decision::Parse => match parse() {
                Some(sess) => {
                    st.parse_warnings += sess.warnings;
                    let sid = store_session(tx, key, &sess, matcher, indexes)?;
                    db::set_source(tx, key, provider, size, mtime, sess.cwd.as_deref(), "parsed", Some(&sid), sess.warnings as i64)?;
                    st.sessions_parsed += 1;
                    if st.sessions_parsed % 25 == 0 {
                        tx.execute_batch("COMMIT; BEGIN")?;
                    }
                    if opts.verbose {
                        eprintln!("parsed {provider} {key}");
                    }
                }
                None => {
                    st.parse_warnings += 1;
                    db::set_source(tx, key, provider, size, mtime, None, "error", None, 1)?;
                }
            },
        }
        Ok(())
    };

    // Claude Code: ~/.claude/projects/<encoded-cwd>/<session>.jsonl
    let cdir = env.claude_projects();
    if cdir.is_dir() {
        let mut files = Vec::new();
        list_jsonl(&cdir, 1, &mut files);
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
    let loc = matcher.locate(Path::new(&cwd));
    let (co_id, repo_id) = loc.as_ref().map(|(i, _)| (matcher.checkouts[*i].id.clone(), matcher.checkouts[*i].repo_id.clone())).unzip();
    let label = s.title.clone().filter(|t| !t.is_empty()).or_else(|| s.first_prompt.as_ref().map(|p| clip(&p.1, 80))).unwrap_or_else(|| "(untitled)".into());

    // Aggregate: first occurrence per (file, op, strength), with a count.
    let mut agg: BTreeMap<(String, &'static str, &'static str), (usize, &sessions::FileEvent, PathBuf)> = BTreeMap::new();
    let mut external = 0usize;
    for ev in &s.events {
        let abs = crate::util::resolve_against(Path::new(&cwd), &ev.path);
        let Some((ci, rel)) = matcher.locate(&abs) else {
            external += 1;
            continue;
        };
        let co = &matcher.checkouts[ci];
        // Reused checkouts load their tracked-file list only when a session needs it.
        let ix = indexes.entry(co.id.clone()).or_insert_with(|| FileIndex::new(git::ls_files(&co.top, 50_000)));
        if ev.op == "mention" && !ix.files.contains(&*rel.to_string_lossy()) {
            continue; // mentioned paths only count when they name a tracked file
        }
        let canon = co.top.join(&rel);
        let entry = agg.entry((canon.to_string_lossy().into_owned(), ev.op, ev.strength)).or_insert((0, ev, rel));
        entry.0 += 1;
    }
    let counts = json!({
        "read": agg.keys().filter(|k| k.1 == "read").count(),
        "write": agg.keys().filter(|k| k.1 == "write").count(),
        "mentioned": agg.keys().filter(|k| k.1 == "mention").count(),
        "external_paths_skipped": external,
    });
    let meta = json!({
        "provider": s.provider, "native_id": s.native_id, "cwd": s.cwd, "branch": s.branch,
        "first_prompt": s.first_prompt.as_ref().map(|p| json!({"ts": p.0, "text": p.1, "line": p.2})),
        "last_prompt": s.last_prompt.as_ref().map(|p| json!({"ts": p.0, "text": p.1, "line": p.2})),
        "started": s.started, "updated": s.updated, "warnings": s.warnings, "truncated": s.truncated, "files": counts,
        "messages": s.messages.len(), "messages_capped": s.messages_capped,
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
    for ((abs, op, strength), (n, ev, rel)) in agg.iter().take(400) {
        let (ci, _) = matcher.locate(Path::new(abs)).unwrap_or((0, PathBuf::new()));
        let co = matcher.checkouts[ci].clone();
        let fid = db::ensure_file_node(tx, abs, Some(&co.repo_id), Some(&co.id), &rel.to_string_lossy())?;
        let kind = if *op == "mention" { "mentioned" } else { op };
        let note = format!("{}{}{}", ev.tool, if *n > 1 { format!(" x{n}") } else { String::new() }, if ev.locator.is_empty() { String::new() } else { format!(" @{}", ev.locator) });
        let src_path = s.source_path.split("#session=").next().unwrap_or(&s.source_path);
        db::add_edge(tx, &EdgeRow { src: &sid, dst: &fid, kind, strength, source_path: src_path, source_line: ev.line as i64, ts: ev.ts, note: &note, origin: &origin })?;
    }
    Ok(sid)
}
