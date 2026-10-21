//! Rendering: overview of everything, context for one project, expand one node.
//! Output lists evidence with timestamps and source locations; it never
//! summarises meaning or asserts decisions.

use crate::util::{clip, fmt_date, fmt_ts};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write;

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub path: Option<String>,
    pub repo_id: Option<String>,
    pub checkout_id: Option<String>,
    pub ts: i64,
    pub meta: Value,
}

fn row_node(r: &rusqlite::Row) -> rusqlite::Result<Node> {
    Ok(Node {
        id: r.get(0)?,
        kind: r.get(1)?,
        label: r.get(2)?,
        path: r.get(3)?,
        repo_id: r.get(4)?,
        checkout_id: r.get(5)?,
        ts: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
        meta: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or(Value::Null),
    })
}

const COLS: &str = "id,kind,label,path,repo_id,checkout_id,ts,meta";

pub fn get_node(c: &Connection, id: &str) -> rusqlite::Result<Option<Node>> {
    c.query_row(&format!("SELECT {COLS} FROM nodes WHERE id=?1"), [id], row_node).optional()
}

pub(crate) fn nodes(c: &Connection, sql_where: &str, p: &[&dyn rusqlite::ToSql]) -> rusqlite::Result<Vec<Node>> {
    let mut st = c.prepare(&format!("SELECT {COLS} FROM nodes WHERE {sql_where}"))?;
    let rows = st.query_map(p, row_node)?;
    rows.collect()
}

pub(crate) fn rel<'a>(path: &'a str, top: &str) -> &'a str {
    path.strip_prefix(top).map(|r| r.trim_start_matches('/')).filter(|r| !r.is_empty()).unwrap_or(path)
}

/// Resolve a user target: node id, repo/checkout label, or a filesystem path.
pub fn resolve_target(c: &Connection, target: &str) -> rusqlite::Result<Option<Node>> {
    if let Some(n) = get_node(c, target)? {
        return Ok(Some(n));
    }
    // Exact indexed path first (files, docs, checkouts), including deleted files.
    let raw_abs = std::env::current_dir().map(|d| crate::util::resolve_against(&d, target).to_string_lossy().into_owned()).unwrap_or_default();
    let abs = std::fs::canonicalize(target).ok().map(|p| p.to_string_lossy().into_owned());
    for cand in abs.iter().chain(std::iter::once(&raw_abs)) {
        let exact = nodes(c, "path=?1 AND kind IN ('checkout','doc','memory','file') ORDER BY CASE kind WHEN 'checkout' THEN 0 WHEN 'doc' THEN 1 WHEN 'memory' THEN 2 ELSE 3 END", &[cand])?;
        if let Some(n) = exact.into_iter().next() {
            return Ok(Some(n));
        }
    }
    if let Some(abs) = abs {
        // Deepest checkout containing the path.
        let mut best: Option<Node> = None;
        for n in nodes(c, "kind='checkout'", &[])? {
            let top = n.path.clone().unwrap_or_default();
            if (abs == top || abs.starts_with(&format!("{top}/"))) && best.as_ref().map(|b| b.path.as_ref().map(|p| p.len()).unwrap_or(0) < top.len()).unwrap_or(true) {
                best = Some(n);
            }
        }
        if best.is_some() {
            return Ok(best);
        }
    }
    let by_label = nodes(c, "kind IN ('repo','checkout') AND (label=?1 OR label LIKE ?2) ORDER BY kind DESC, ts DESC", &[&target, &format!("%{target}%")])?;
    Ok(by_label.into_iter().next())
}

/// Build output and lockfiles churn with every commit; their commit counts don't score.
fn generated(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rel.starts_with("dist/")
        || rel.starts_with("build/")
        || rel.contains("/dist/")
        || name.ends_with(".map")
        || name.ends_with(".min.js")
        || name.ends_with(".lock")
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
        || name == "go.sum"
}

pub(crate) struct Ranked {
    pub node: Node,
    pub score: i64,
    pub refs: Vec<String>,
    pub commits: usize,
    pub last_commit: i64,
    pub sessions: Vec<String>,
    pub entry: Option<String>,
}

/// Deterministic importance: explicit doc references, entry points, recent commits,
/// and structured session file access. Mentions do not score.
pub(crate) fn rank_files(c: &Connection, checkout_ids: &[String], top_of: &BTreeMap<String, String>) -> rusqlite::Result<Vec<Ranked>> {
    let mut out: BTreeMap<String, Ranked> = BTreeMap::new();
    for co in checkout_ids {
        let top = top_of.get(co).cloned().unwrap_or_default();
        let mut st = c.prepare(
            "SELECT e.dst, e.kind, e.strength, e.source_path, e.source_line, e.note, e.ts, e.src FROM edges e
             JOIN nodes n ON n.id=e.dst WHERE n.kind IN ('file','doc') AND n.checkout_id=?1 AND e.kind IN ('references','entrypoint','touched','read','write')",
        )?;
        let rows: Vec<(String, String, String, String, i64, String, Option<i64>, String)> =
            st.query_map([co], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<Result<_, _>>()?;
        for (dst, kind, strength, sp, line, note, ts, src) in rows {
            if !out.contains_key(&dst) {
                let Some(n) = get_node(c, &dst)? else { continue };
                out.insert(dst.clone(), Ranked { node: n, score: 0, refs: vec![], commits: 0, last_commit: 0, sessions: vec![], entry: None });
            }
            let r = out.get_mut(&dst).unwrap();
            match kind.as_str() {
                "references" => {
                    r.score += 3;
                    let heading = note.split(" | ").next().unwrap_or("");
                    r.refs.push(format!("{}:{}{}", rel(&sp, &top), line, if heading.is_empty() { String::new() } else { format!(" ({heading})") }));
                }
                "entrypoint" => {
                    r.score += 2;
                    r.entry = Some(format!("{}:{} {}", rel(&sp, &top), line, note));
                }
                "touched" => {
                    r.commits += 1;
                    r.score += if generated(&r.node.label) { 0 } else { 1 };
                    r.last_commit = r.last_commit.max(ts.unwrap_or(0));
                }
                "read" | "write" if strength == "observed" || strength == "requested" => {
                    r.score += if kind == "write" { 2 } else { 1 };
                    r.sessions.push(format!("{kind}:{src}"));
                }
                _ => {}
            }
        }
    }
    let mut v: Vec<Ranked> = out.into_values().collect();
    for r in &mut v {
        r.refs.sort();
        r.refs.dedup();
        r.sessions.sort();
        r.sessions.dedup();
        r.commits = r.commits.min(99);
    }
    v.sort_by(|a, b| b.score.cmp(&a.score).then(b.last_commit.cmp(&a.last_commit)).then(a.node.path.cmp(&b.node.path)));
    Ok(v)
}

pub struct Limits {
    pub files: usize,
    pub commits: usize,
    pub sessions: usize,
    pub excerpts: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { files: 15, commits: 8, sessions: 8, excerpts: 8 }
    }
}

fn meta_str(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// One-screen list of indexed repositories, each with its follow-up command.
pub fn overview(c: &Connection, max_repos: usize, cmd: &str) -> rusqlite::Result<String> {
    let mut o = String::new();
    let repos = nodes(c, "kind='repo' ORDER BY ts DESC, label", &[])?;
    let total_sessions: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='session'", [], |r| r.get(0))?;
    let last_scan: Option<(i64, String)> = c.query_row("SELECT finished, stats FROM scans WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let _ = writeln!(
        o,
        "workmap: {} projects, {} agent conversations; last scan {}",
        repos.len(),
        total_sessions,
        last_scan.as_ref().map(|s| fmt_ts(s.0)).unwrap_or_else(|| "never (run `workmap scan`)".into())
    );
    for r in repos.iter().take(max_repos) {
        let cos: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='checkout' AND repo_id=?1", [&r.id], |x| x.get(0))?;
        let ses: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='session' AND repo_id=?1", [&r.id], |x| x.get(0))?;
        let last_ses: Option<i64> = c.query_row("SELECT max(ts) FROM nodes WHERE kind='session' AND repo_id=?1", [&r.id], |x| x.get(0))?;
        let handle = crate::view::project_handle(c, r).unwrap_or_else(|_| r.label.clone());
        let _ = writeln!(
            o,
            "- {} — {} worktree(s), {} conversation(s), last commit {}{}\n    {} context {}",
            handle,
            cos,
            ses,
            fmt_date(r.ts),
            last_ses.map(|t| format!(", last conversation {}", fmt_date(t))).unwrap_or_default(),
            cmd,
            crate::util::sh_quote(&handle)
        );
    }
    if repos.len() > max_repos {
        let _ = writeln!(o, "  … {} more: {} overview --limit {}", repos.len() - max_repos, cmd, repos.len());
    }
    let remote: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='remote'", [], |r| r.get(0))?;
    if remote > 0 {
        let rows = crate::remote::remote_only(c, 15)?;
        let _ = writeln!(o, "Remote-only, not cloned ({remote}; metadata from authenticated gh/glab; expand remote:<name>):");
        for (id, vis, archived, desc, stale) in rows {
            let _ = writeln!(
                o,
                "- remote:{id} — {vis}{}{}{}",
                if archived { ", archived" } else { "" },
                if stale { ", stale" } else { "" },
                if desc.is_empty() { String::new() } else { format!(" — {}", clip(&desc, 70)) }
            );
        }
        if remote > 15 {
            let _ = writeln!(o, "  … {} more: {cmd} remote list", remote - 15);
        }
    }
    Ok(o)
}

/// Connected contextual overview for a repo / checkout / path.
pub fn context(c: &Connection, target: &Node, lim: &Limits) -> rusqlite::Result<String> {
    let mut o = String::new();
    let repo_id = target.repo_id.clone().unwrap_or_else(|| target.id.clone());
    let Some(repo) = get_node(c, &repo_id)? else {
        return Ok(format!("node {} has no repository; try `workmap expand {}`\n", target.id, target.id));
    };
    let focus = if target.kind == "checkout" { Some(target.id.clone()) } else { target.checkout_id.clone().filter(|_| target.kind != "repo") };
    let checkouts = nodes(c, "kind='checkout' AND repo_id=?1 ORDER BY json_extract(meta,'$.main') DESC, ts DESC", &[&repo_id])?;
    let top_of: BTreeMap<String, String> = checkouts.iter().map(|n| (n.id.clone(), n.path.clone().unwrap_or_default())).collect();
    let _ = writeln!(o, "# {} [{}]", repo.label, repo.id);
    let _ = writeln!(o, "git common dir: {}", meta_str(&repo.meta, "common_dir"));
    if let Some(Value::Array(rs)) = repo.meta.get("remotes") {
        for r in rs.iter().take(3) {
            let _ = writeln!(o, "remote {}: {}", meta_str(r, "name"), meta_str(r, "url"));
        }
    }
    let _ = writeln!(o, "\n## Checkouts ({}) — same repository, separate working trees", checkouts.len());
    for co in &checkouts {
        let ses: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='session' AND checkout_id=?1", [&co.id], |x| x.get(0))?;
        let mark = if Some(&co.id) == focus.as_ref() { "*" } else { "-" };
        let _ = writeln!(
            o,
            "{mark} {} [{}] branch={} head={} dirty={} last-commit={} sessions={}{}",
            co.path.clone().unwrap_or_default(),
            co.id,
            co.meta.get("branch").and_then(|x| x.as_str()).unwrap_or("(detached)"),
            meta_str(&co.meta, "head"),
            // Live, not indexed: modified tracked files right now.
            co.path.as_deref().and_then(|p| crate::git::dirty_count(std::path::Path::new(p))).map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
            fmt_date(co.ts),
            ses,
            if co.meta.get("main") == Some(&Value::Bool(true)) { " (main)" } else { "" }
        );
    }
    // Docs of the focused (or main) checkout.
    let primary = focus.clone().or_else(|| checkouts.first().map(|c| c.id.clone())).unwrap_or_default();
    let ptop = top_of.get(&primary).cloned().unwrap_or_default();
    let docs = nodes(
        c,
        "kind IN ('doc','memory') AND checkout_id=?1 ORDER BY kind, (label NOT LIKE '%README%'), (label NOT LIKE '%CLAUDE%' AND label NOT LIKE '%AGENTS%'), length(label), label",
        &[&primary],
    )?;
    if !docs.is_empty() {
        let _ = writeln!(o, "\n## Docs in {ptop} (excerpts: path:line [heading] text; intro first, then sections with most file references)");
        for d in docs.iter().take(8) {
            let refs: i64 = c.query_row("SELECT count(*) FROM edges WHERE src=?1 AND kind='references'", [&d.id], |x| x.get(0))?;
            let _ = writeln!(o, "### {} [{}] {} lines, {} local refs, modified {}", d.label, d.id, d.meta.get("lines").map(|x| x.to_string()).unwrap_or_default(), refs, fmt_date(d.ts));
            // Sections that reference the most local files come first, then document order.
            let mut st = c.prepare(
                "SELECT * FROM (SELECT x.line, x.heading, x.text FROM excerpts x WHERE x.node_id=?1
                   ORDER BY x.line = (SELECT min(line) FROM excerpts WHERE node_id=?1 AND text NOT LIKE '![%' AND text NOT LIKE '[![%' AND text NOT LIKE '<%') DESC,
                     (SELECT count(*) FROM edges e WHERE e.src=x.node_id AND e.kind='references' AND e.note LIKE x.heading || ' |%') DESC, x.line LIMIT ?2)
                 ORDER BY line",
            )?;
            let ex: Vec<(i64, String, String)> = st.query_map(params![d.id, lim.excerpts as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            for (line, h, t) in ex {
                let _ = writeln!(o, "  {}:{} [{}] {}", d.label, line, clip(&h, 40), clip(&t, 160));
            }
        }
        if docs.len() > 8 {
            let _ = writeln!(o, "  … {} more docs: workmap expand {}", docs.len() - 8, primary);
        }
    }
    // Commits and conversations span every checkout of the repository: related work
    // in sibling worktrees is exactly what one would not think to look for.
    let scope: Vec<String> = checkouts.iter().map(|c| c.id.clone()).collect();
    // Ranking stays within one checkout so sibling worktrees don't repeat the same paths.
    let ranked = rank_files(c, std::slice::from_ref(&primary), &top_of)?;
    let _ = writeln!(o, "\n## Important files in {} (score = 3/doc ref + 2/entry point + 1/recent commit + 2/session write + 1/session read)", ptop.rsplit('/').next().unwrap_or(""));
    for r in ranked.iter().take(lim.files) {
        let top = r.node.checkout_id.as_ref().and_then(|c| top_of.get(c)).cloned().unwrap_or_default();
        let mut why = Vec::new();
        if let Some(e) = &r.entry {
            why.push(format!("entry {e}"));
        }
        if !r.refs.is_empty() {
            let more = if r.refs.len() > 3 { format!(" +{}", r.refs.len() - 3) } else { String::new() };
            why.push(format!("referenced by {}{}", r.refs.iter().take(3).cloned().collect::<Vec<_>>().join(", "), more));
        }
        if r.commits > 0 {
            why.push(format!("{} recent commit(s), last {}", r.commits, fmt_date(r.last_commit)));
        }
        if !r.sessions.is_empty() {
            let w = r.sessions.iter().filter(|s| s.starts_with("write")).count();
            why.push(format!("sessions: {} write, {} read", w, r.sessions.len() - w));
        }
        let gone = r.node.path.as_deref().map(|p| !std::path::Path::new(p).exists()).unwrap_or(false);
        if gone {
            why.insert(0, "NOT ON DISK (historical: only in commits/sessions)".into());
        }
        let _ = writeln!(o, "- {} [{}] score={} — {}", rel(r.node.path.as_deref().unwrap_or(&r.node.label), &top), r.node.id, r.score, why.join("; "));
    }
    if ranked.len() > lim.files {
        let _ = writeln!(o, "  … {} more ranked files (workmap context {} --files N)", ranked.len() - lim.files, target.id);
    }
    // Recent commits across scope (dedup by sha).
    let mut commits: BTreeMap<String, (Node, Vec<String>)> = BTreeMap::new();
    for co in &scope {
        for n in nodes(c, "kind='commit' AND id IN (SELECT dst FROM edges WHERE src=?1 AND kind='history')", &[co])? {
            let br = checkouts.iter().find(|x| &x.id == co).and_then(|x| x.meta.get("branch").and_then(|b| b.as_str()).map(str::to_string)).unwrap_or_default();
            commits.entry(n.id.clone()).or_insert((n, vec![])).1.push(br);
        }
    }
    let mut cv: Vec<(Node, Vec<String>)> = commits.into_values().collect();
    cv.sort_by(|a, b| b.0.ts.cmp(&a.0.ts).then(a.0.id.cmp(&b.0.id)));
    let _ = writeln!(o, "\n## Recent commits (newest first; evidence, not decisions)");
    for (n, brs) in cv.iter().take(lim.commits) {
        let on = if scope.len() > 1 { format!(" on {}", brs.join(",")) } else { String::new() };
        let _ = writeln!(o, "- {} {} ({} files){} [{}]", fmt_ts(n.ts), n.label, n.meta.get("files").map(|x| x.to_string()).unwrap_or_default(), on, n.id);
    }
    // Sessions.
    let sql = format!("kind='session' AND checkout_id IN ({}) ORDER BY ts DESC", scope.iter().map(|s| format!("'{}'", s.replace('\'', ""))).collect::<Vec<_>>().join(","));
    let sess = nodes(c, &sql, &[])?;
    let _ = writeln!(o, "\n## Agent conversations linked by cwd ({})", sess.len());
    if sess.is_empty() {
        let _ = writeln!(o, "- none found in indexed Claude Code / Codex / OpenCode stores for these checkouts");
    }
    for s in sess.iter().take(lim.sessions) {
        let m = &s.meta;
        let f = m.get("files").cloned().unwrap_or(Value::Null);
        let wt = checkouts.iter().find(|x| Some(&x.id) == s.checkout_id.as_ref()).and_then(|x| x.path.as_deref()).map(|p| p.rsplit('/').next().unwrap_or(p)).unwrap_or("");
        let _ = writeln!(
            o,
            "- {} {} in {} \"{}\" [{}] reads={} writes={}{}",
            fmt_date(s.ts),
            meta_str(m, "provider"),
            wt,
            clip(&s.label, 70),
            s.id,
            f.get("read").unwrap_or(&Value::Null),
            f.get("write").unwrap_or(&Value::Null),
            if m.get("source_missing").is_some() { " (source missing)" } else { "" }
        );
        if let Some(p) = m.get("first_prompt").filter(|p| !p.is_null()) {
            let _ = writeln!(o, "    first user input {}: {}", fmt_ts(p.get("ts").and_then(|x| x.as_i64()).unwrap_or(0)), clip(&meta_str(p, "text"), 140));
        }
        if let Some(p) = m.get("last_prompt").filter(|p| !p.is_null() && *p != m.get("first_prompt").unwrap_or(&Value::Null)) {
            let _ = writeln!(o, "    latest user input {}: {}", fmt_ts(p.get("ts").and_then(|x| x.as_i64()).unwrap_or(0)), clip(&meta_str(p, "text"), 140));
        }
    }
    if sess.len() > lim.sessions {
        let _ = writeln!(o, "  … {} more sessions (--sessions N)", sess.len() - lim.sessions);
    }
    let _ = writeln!(o, "\nNext: workmap expand <id> for evidence; workmap ask <session-id> \"question\" --dry-run");
    Ok(o)
}

/// All evidence around one node, bounded.
pub fn expand(c: &Connection, id: &str, max_edges: usize) -> rusqlite::Result<String> {
    let mut o = String::new();
    let Some(n) = resolve_target(c, id)? else {
        return Ok(format!("unknown node or path: {id}\n"));
    };
    let _ = writeln!(o, "{} {} [{}]", n.kind, n.label, n.id);
    if let Some(p) = &n.path {
        let missing = matches!(n.kind.as_str(), "file" | "doc" | "memory" | "checkout") && !std::path::Path::new(p).exists();
        let _ = writeln!(o, "path: {p}{}", if missing { " (not on disk now; evidence below is historical)" } else { "" });
    }
    if n.ts > 0 {
        let _ = writeln!(o, "time: {}", fmt_ts(n.ts));
    }
    let meta = serde_json::to_string_pretty(&n.meta).unwrap_or_default();
    for l in meta.lines().take(30) {
        let _ = writeln!(o, "  {l}");
    }
    let mut st = c.prepare("SELECT line, heading, text FROM excerpts WHERE node_id=?1 ORDER BY line LIMIT 20")?;
    let ex: Vec<(i64, String, String)> = st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    for (line, h, t) in ex {
        let _ = writeln!(o, "  line {line} [{h}] {t}");
    }
    for (dir, sql) in [("->", "SELECT e.kind,e.strength,e.source_path,e.source_line,e.ts,e.note,n.id,n.kind,n.label FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 ORDER BY e.kind, coalesce(e.ts,0) DESC, n.label LIMIT ?2"),
                       ("<-", "SELECT e.kind,e.strength,e.source_path,e.source_line,e.ts,e.note,n.id,n.kind,n.label FROM edges e JOIN nodes n ON n.id=e.src WHERE e.dst=?1 ORDER BY e.kind, coalesce(e.ts,0) DESC, n.label LIMIT ?2")] {
        let mut st = c.prepare(sql)?;
        let rows: Vec<(String, String, String, i64, Option<i64>, String, String, String, String)> = st
            .query_map(params![n.id, max_edges as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))?
            .collect::<Result<_, _>>()?;
        let total: i64 = c.query_row(&format!("SELECT count(*) FROM edges WHERE {}=?1", if dir == "->" { "src" } else { "dst" }), [&n.id], |r| r.get(0))?;
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n{} edges {} ({} total):", if dir == "->" { "outgoing" } else { "incoming" }, dir, total);
        for (kind, strength, sp, line, ts, note, oid, okind, olabel) in rows {
            let ev = if sp.is_empty() { String::new() } else if line > 0 { format!(" @ {sp}:{line}") } else { format!(" @ {sp}") };
            let when = ts.map(|t| format!(" {}", fmt_ts(t))).unwrap_or_default();
            let _ = writeln!(o, "  {dir} {kind}/{strength} {okind} {} [{oid}]{when}{ev}{}", clip(&olabel, 80), if note.is_empty() { String::new() } else { format!(" ({})", clip(&note, 60)) });
        }
    }
    if n.kind == "session" {
        let _ = writeln!(o, "\nAsk this conversation (read-only): workmap ask {} \"<question>\" --dry-run", n.id);
    }
    Ok(o)
}
