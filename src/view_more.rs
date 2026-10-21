//! Views added on top of the project map: search hits, single messages,
//! relation chains, remote-only repositories, task/file context with a byte
//! budget, and the historical-vs-current note printed before `ask`.

use super::*;
use crate::relations::{self, Rel};
use crate::search::{self, Hit};
use crate::trust;
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;

// ------------------------------------------------------------------ helpers

/// Shortest native-id prefix (>= 8 chars) that selects exactly this session.
pub fn session_prefix(all: &[Node], s: &Node) -> String {
    let native = meta(s, "native_id").to_string();
    for n in [8usize, 12, 16, 24, 36] {
        let pre: String = native.chars().take(n).collect();
        if all.iter().filter(|x| meta(x, "native_id").starts_with(&pre)).count() == 1 {
            return pre;
        }
    }
    native
}

fn project_by_repo(c: &Connection, repo_id: &str, checkout: Option<&str>) -> Option<Project> {
    let repo = get_node(c, repo_id).ok()??;
    let focus = checkout.and_then(|id| get_node(c, id).ok().flatten()).filter(|n| n.kind == "checkout");
    build_project(c, repo, focus).ok()
}

/// Reference printed for `rest` inside `target` project: short (`:rest`) when it
/// is the scope project, otherwise `handle:rest` marked as another project.
fn scoped_ref(scope: Option<&Project>, target: &Project, co: Option<&Node>, rest: &str) -> String {
    match scope {
        Some(sp) if sp.repo.id == target.repo.id => match co {
            Some(co) => fref(sp, co, rest),
            None => q(&format!(":{rest}")),
        },
        _ => {
            let raw = match co {
                Some(co) => qualified_raw(target, co, rest),
                None => format!("{}:{rest}", target.handle),
            };
            format!("{}{}", q(&raw), if scope.is_some() { " (other project)" } else { "" })
        }
    }
}

struct RefCache<'a> {
    c: &'a Connection,
    projects: HashMap<(String, Option<String>), Option<Project>>,
    sessions: HashMap<String, Vec<Node>>,
}

impl<'a> RefCache<'a> {
    fn new(c: &'a Connection) -> Self {
        RefCache { c, projects: HashMap::new(), sessions: HashMap::new() }
    }
    fn project(&mut self, repo: &str, co: Option<&str>) -> Option<&Project> {
        let c = self.c;
        self.projects.entry((repo.to_string(), co.map(str::to_string))).or_insert_with(|| project_by_repo(c, repo, co)).as_ref()
    }
    fn sessions(&mut self, p: &Project) -> Vec<Node> {
        let c = self.c;
        self.sessions.entry(p.repo.id.clone()).or_insert_with(|| sessions_of(c, p).unwrap_or_default()).clone()
    }
}

/// `(reference, description)` for a search hit.
fn hit_ref(cache: &mut RefCache, scope: Option<&Project>, h: &Hit) -> (String, String) {
    if h.kind == "remote" {
        return (q(&format!("remote:{}", h.title)), "remote repository (not cloned)".into());
    }
    if let Some(at) = h.source_path.strip_prefix("remote:") {
        // Remote snapshot passage: virtual ref tied to the indexed commit.
        let (identity, rest) = at.split_once('@').unwrap_or((at, ""));
        let sha = rest.split('/').next().unwrap_or("");
        return match h.kind.as_str() {
            "doc" => (
                q(&format!("remote:{identity}:{}{}", h.title, if h.loc.is_empty() { String::new() } else { format!("#{}", h.loc) })),
                format!("doc {}{} l.{} in REMOTE SNAPSHOT at {sha} (not on local disk)", h.title, if h.heading.is_empty() { String::new() } else { format!(" § {}", h.heading) }, h.line),
            ),
            _ => (q(&format!("remote:{identity}")), format!("head commit of REMOTE SNAPSHOT {sha} by {}", h.speaker)),
        };
    }
    let Some(repo) = h.repo_id.clone() else {
        return (h.node_id.clone(), format!("{} without project", h.kind));
    };
    let Some(p) = cache.project(&repo, h.checkout_id.as_deref()).map(|p| Project { repo: p.repo.clone(), handle: p.handle.clone(), checkouts: p.checkouts.clone(), focus: p.focus.clone() }) else {
        return (h.node_id.clone(), h.kind.clone());
    };
    let co = h.checkout_id.as_deref().and_then(|id| p.checkouts.iter().find(|c| c.id == id)).cloned();
    match h.kind.as_str() {
        "project" => (
            match scope {
                Some(sp) if sp.repo.id == p.repo.id => "(this project)".into(),
                _ => q(&p.handle),
            },
            "project".into(),
        ),
        "commit" => (scoped_ref(scope, &p, None, &commit_ref(&h.loc)[1..]), format!("commit by {}", h.speaker)),
        "doc" => {
            let top = co.as_ref().and_then(|c| c.path.clone()).unwrap_or_default();
            let relp = if h.title.starts_with("claude-memory/") { h.title.clone() } else { rel(&h.source_path, &top).to_string() };
            let rest = if h.loc.is_empty() { relp.clone() } else { format!("{relp}#{}", h.loc) };
            let mut d = format!("doc {relp}{} l.{}", if h.heading.is_empty() { String::new() } else { format!(" § {}", h.heading) }, h.line);
            if h.dups > 0 {
                let _ = write!(d, " (same text in {} other worktree(s))", h.dups);
            }
            (scoped_ref(scope, &p, co.as_ref(), &rest), d)
        }
        "message" => {
            let all = cache.sessions(&p);
            let Some(s) = all.iter().find(|s| s.id == h.node_id) else { return (h.node_id.clone(), "message".into()) };
            let missing = if s.meta.get("source_missing").is_some() { " · source transcript missing" } else { "" };
            let rest = format!("message:{}:{}", session_prefix(&all, s), h.loc);
            (scoped_ref(scope, &p, None, &rest), format!("{} {} message · {} · conversation \"{}\"{missing}", meta(s, "provider"), h.speaker, fmt_ts(h.ts), clip(&s.label, 50)))
        }
        k => (h.node_id.clone(), k.to_string()),
    }
}

// ------------------------------------------------------------------ search

pub struct SearchReq {
    pub query: search::Query,
    /// Restrict to the scope's selected worktree instead of all its worktrees.
    pub this_worktree: bool,
}

pub fn search_view(v: &View, req: &Req, sr: &SearchReq) -> R<String> {
    let scope = match req.project.as_deref() {
        Some(_) => Some(scope_project(v, req)?),
        None => None,
    };
    let (hits, total) = db(search::run_pref(
        v.c,
        &sr.query,
        scope.as_ref().map(|p| p.repo.id.as_str()),
        if sr.this_worktree { scope.as_ref().map(|p| p.focus.id.as_str()) } else { None },
        scope.as_ref().map(|p| p.focus.id.as_str()),
    ))?;
    let mut o = String::new();
    let kinds = if sr.query.kinds.is_empty() { "all kinds".to_string() } else { sr.query.kinds.join(",") };
    let where_ = match &scope {
        Some(p) if sr.this_worktree => format!("{} @{}", p.handle, worktree_name(p, &p.focus)),
        Some(p) => format!("{} (all worktrees)", p.handle),
        None => "all projects".into(),
    };
    let _ = writeln!(o, "# search {:?} — {} shown of {total} matching passage(s) · {where_} · {kinds}", sr.query.text, hits.len());
    o.push_str(&trust::index_line(v.c));
    if let Some(p) = &scope {
        o.push_str(&header(v, p, false));
    }
    if hits.is_empty() {
        let _ = writeln!(o, "no matches{}", if sr.query.any { "" } else { " (all words required; try --any)" });
    }
    let mut cache = RefCache::new(v.c);
    for (i, h) in hits.iter().enumerate() {
        let (r, d) = hit_ref(&mut cache, scope.as_ref(), h);
        let _ = writeln!(o, "{}. {r} — {d}", i + 1);
        let _ = writeln!(o, "   \"{}\"", clip(&h.snippet, 220));
    }
    if (hits.len() as i64) < total && hits.len() >= sr.query.limit {
        let _ = writeln!(o, "More: add --limit {} (ranked by bm25, then newest)", (sr.query.limit * 3).min(total as usize));
    }
    let _ = writeln!(o, "Messages are historical transcript text, not decisions. Expand a ref for the full message and its neighbours.");
    Ok(o)
}

// ------------------------------------------------------------------ one message

fn loc_num(loc: &str) -> i64 {
    loc.get(1..).and_then(|x| x.parse().ok()).unwrap_or(0)
}

fn message_text(c: &Connection, sid: &str, loc: &str) -> R<Option<(String, String, i64, i64, String)>> {
    let mut st = db(c.prepare("SELECT speaker, text, coalesce(ts,0), line, source_path FROM passages WHERE kind='message' AND node_id=?1 AND loc=?2 ORDER BY part"))?;
    let rows: Vec<(String, String, i64, i64, String)> = db(db(st.query_map(params![sid, loc], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))))?.collect::<Result<_, _>>())?;
    let Some(first) = rows.first().cloned() else { return Ok(None) };
    let text = rows.iter().map(|r| r.1.as_str()).collect::<Vec<_>>().join(" ");
    Ok(Some((first.0, text, first.2, first.3, first.4)))
}

pub fn message_view(v: &View, p: &Project, sid_prefix: &str, loc: &str) -> R<String> {
    let all = sessions_of(v.c, p)?;
    let hits: Vec<&Node> = all.iter().filter(|s| !sid_prefix.is_empty() && meta(s, "native_id").starts_with(sid_prefix)).collect();
    let s = match hits.len() {
        1 => hits[0],
        0 => return Err(format!("no conversation in {} has a session id starting with {}", p.handle, q(sid_prefix))),
        n => {
            let mut m = format!("session prefix {} matches {n} conversations in {}:", q(sid_prefix), p.handle);
            for s in hits.iter().take(10) {
                let _ = write!(m, "\n  {}:message:{}:{loc}", p.handle, session_prefix(&all, s));
            }
            return Err(m);
        }
    };
    let Some((speaker, text, ts, line, src)) = message_text(v.c, &s.id, loc)? else {
        let mut st = db(v.c.prepare("SELECT DISTINCT loc FROM passages WHERE kind='message' AND node_id=?1 ORDER BY line, loc LIMIT 12"))?;
        let locs: Vec<String> = db(db(st.query_map([&s.id], |r| r.get(0)))?.collect::<Result<_, _>>())?;
        return Err(format!("no indexed message {} in this conversation; first locators: {}", q(loc), locs.join(" ")));
    };
    let this = q(&format!(":message:{}:{loc}", session_prefix(&all, s)));
    let mut o = String::new();
    let _ = writeln!(o, "# {this} — {speaker} message in {} conversation {}", meta(s, "provider"), conv_ref(&all, s));
    let _ = writeln!(
        o,
        "{} · transcript {src}{} · historical text, not a decision record{}",
        fmt_ts(ts),
        if line > 0 { format!(" line {line}") } else { format!(" part {}", loc_num(loc)) },
        if s.meta.get("source_missing").is_some() { " · SOURCE MISSING (kept from an earlier scan)" } else { "" }
    );
    o.push_str(&header(v, p, false));
    if s.meta.get("messages_capped").and_then(|x| x.as_bool()) == Some(true) || s.meta.get("truncated").and_then(|x| x.as_bool()) == Some(true) {
        let _ = writeln!(o, "Note: this conversation was truncated or capped at index time; later messages may be missing.");
    }
    let _ = writeln!(o, "\n{text}\n");
    if text.chars().count() >= crate::sessions::MSG_CHARS - 10 {
        let _ = writeln!(o, "(message cut at {} characters when indexed; open the transcript for the rest)", crate::sessions::MSG_CHARS);
    }
    let n = loc_num(loc);
    for (label, sql) in [
        ("Previous", "SELECT loc FROM passages WHERE kind='message' AND node_id=?1 AND CAST(substr(loc,2) AS INTEGER) < ?2 ORDER BY CAST(substr(loc,2) AS INTEGER) DESC LIMIT 1"),
        ("Next", "SELECT loc FROM passages WHERE kind='message' AND node_id=?1 AND CAST(substr(loc,2) AS INTEGER) > ?2 ORDER BY CAST(substr(loc,2) AS INTEGER) LIMIT 1"),
    ] {
        let l: Option<String> = db(v.c.query_row(sql, params![s.id, n], |r| r.get(0)).optional())?;
        if let Some(l) = l {
            if let Some((sp, t, ..)) = message_text(v.c, &s.id, &l)? {
                let _ = writeln!(o, "{label}: {} {sp}: \"{}\"", q(&format!(":message:{}:{l}", session_prefix(&all, s))), clip(&t, 160));
            }
        }
    }
    let _ = writeln!(o, "Conversation: {}", conv_ref(&all, s));
    Ok(o)
}

// ------------------------------------------------------------------ relations

fn other_ref(scope: &Project, c: &Connection, id: &str) -> String {
    let _ = scope;
    q(&format!(":related:{}", relations::node_name(c, id)))
}

fn relation_target(c: &Connection, other: &str) -> R<(String, String)> {
    if let Some(identity) = other.strip_prefix("remote:") {
        let id = crate::util::stable_id("remote", identity);
        return match db(get_node(c, &id))? {
            Some(_) => Ok((id, other.to_string())),
            None => Err(format!("no remote-only repository {identity} in the catalog (see `workmap remote list`)")),
        };
    }
    let p = resolve_project(c, other)?;
    Ok((p.repo.id.clone(), p.handle))
}

/// `expand :related:<other>`: every piece of evidence between scope and other.
pub fn relation_view(v: &View, p: &Project, other: &str, limit: usize) -> R<String> {
    let (oid, oname) = relation_target(v.c, other)?;
    let ev = db(relations::between(v.c, &p.repo.id, &oid))?;
    let mut o = String::new();
    let _ = writeln!(o, "# {} — evidence between {} and {oname}", q(&format!(":related:{oname}")), p.handle);
    o.push_str(&header(v, p, false));
    if ev.is_empty() {
        let _ = writeln!(o, "no direct relation recorded; try: {} why {} {}", v.cmd, q(&p.handle), q(&oname));
    }
    let (firm, cand): (Vec<&Rel>, Vec<&Rel>) = ev.iter().partition(|r| !r.is_candidate());
    for (title, list) in [("Evidence", firm), ("Uncertain candidates (not evidence of a relation)", cand)] {
        if list.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n## {title} ({})", list.len());
        for r in list.iter().take(limit) {
            let _ = writeln!(o, "- {}", relations::evidence_line(v.c, r));
        }
        if list.len() > limit {
            let _ = writeln!(o, "  … {} more (--limit {})", list.len() - limit, list.len());
        }
    }
    let _ = writeln!(o, "\n→ means {} declares/links/uses {oname}; ← the reverse. Classes: declared (manifest/submodule), structural (symlink), documented (doc line), observed (conversation access), hash (identical content), identity (same remote).", p.handle);
    Ok(o)
}

/// Related projects up to `depth` hops, with the evidence for each hop.
pub fn related_view(v: &View, p: &Project, depth: usize, limit: usize) -> R<String> {
    let mut reach = db(relations::bfs(v.c, &p.repo.id, depth.max(1), 500))?;
    reach.sort_by_cached_key(|(n, path)| (path.len(), path.iter().map(|r| class_rank(&r.class)).max().unwrap_or(9), relations::node_name(v.c, n)));
    let mut o = String::new();
    let _ = writeln!(o, "# {} — related projects (depth {depth}; {} found)", p.handle, reach.len());
    o.push_str(&trust::index_line(v.c));
    o.push_str(&header(v, p, false));
    if reach.is_empty() {
        let _ = writeln!(o, "- none with recorded evidence (manifests, submodules, symlinks, doc links, conversation access, identical files, shared remotes)");
    }
    for (node, path) in reach.iter().take(limit) {
        let hops = path.len();
        let via: Vec<String> = path[..hops - 1].iter().map(|r| relations::node_name(v.c, &r.to)).collect();
        let _ = writeln!(o, "- {} — {}{}", other_ref(p, v.c, node), relations::label(&path[hops - 1].kind), if via.is_empty() { String::new() } else { format!(" (via {})", via.join(" → ")) });
        if hops == 1 {
            let ev = db(relations::between(v.c, &p.repo.id, node))?;
            let firm: Vec<&Rel> = ev.iter().filter(|r| !r.is_candidate()).collect();
            for r in firm.iter().take(2) {
                let _ = writeln!(o, "    {}", relations::evidence_line(v.c, r));
            }
            if firm.len() > 2 {
                let _ = writeln!(o, "    … {} more", firm.len() - 2);
            }
        } else {
            let _ = writeln!(o, "    chain: {} why {} {}", v.cmd, q(&p.handle), q(&relations::node_name(v.c, node)));
        }
    }
    if reach.len() > limit {
        let _ = writeln!(o, "  … {} more (--limit {})", reach.len() - limit, reach.len());
    }
    let cands: Vec<Rel> = db(relations::neighbors(v.c, &p.repo.id))?.into_iter().filter(|r| r.is_candidate()).collect();
    if !cands.is_empty() {
        let _ = writeln!(o, "\n## Uncertain candidates ({}; name matches only, not evidence)", cands.len());
        for r in cands.iter().take(limit) {
            let _ = writeln!(o, "- {} {}", relations::node_name(v.c, &r.to), relations::evidence_line(v.c, r));
        }
    }
    Ok(o)
}

/// Shortest evidence chain between two projects.
pub fn why_view(v: &View, a: &Project, b_name: &str, depth: usize) -> R<String> {
    let (bid, bname) = relation_target(v.c, b_name)?;
    let mut o = String::new();
    let _ = writeln!(o, "# why {} → {bname} (max {depth} steps)", a.handle);
    o.push_str(&trust::index_line(v.c));
    match db(relations::chain(v.c, &a.repo.id, &bid, depth))? {
        Some(path) => {
            let mut from = a.repo.id.clone();
            for (i, r) in path.iter().enumerate() {
                let ev = db(relations::between(v.c, &from, &r.to))?;
                let firm: Vec<&Rel> = ev.iter().filter(|r| !r.is_candidate()).collect();
                let _ = writeln!(o, "{}. {} → {} ({} piece(s) of evidence)", i + 1, relations::node_name(v.c, &from), relations::node_name(v.c, &r.to), firm.len());
                for e in firm.iter().take(3) {
                    let _ = writeln!(o, "   {}", relations::evidence_line(v.c, e));
                }
                from = r.to.clone();
            }
            let _ = writeln!(o, "Each step is a recorded relation; a chain does not imply the first project depends on the last.");
        }
        None => {
            let _ = writeln!(o, "no evidence chain within {depth} steps");
        }
    }
    let cands: Vec<Rel> = db(relations::between(v.c, &a.repo.id, &bid))?.into_iter().filter(|r| r.is_candidate()).collect();
    if !cands.is_empty() {
        let _ = writeln!(o, "\nUncertain candidates between them (not used in the chain):");
        for r in cands {
            let _ = writeln!(o, "- {}", relations::evidence_line(v.c, &r));
        }
    }
    Ok(o)
}

/// Bounded "Related projects" block for the project map.
pub fn related_section(v: &View, p: &Project, n: usize) -> R<String> {
    let ns = db(relations::neighbors(v.c, &p.repo.id))?;
    let mut by: BTreeMap<String, Vec<&Rel>> = BTreeMap::new();
    for r in ns.iter().filter(|r| !r.is_candidate()) {
        by.entry(r.to.clone()).or_default().push(r);
    }
    let cands = ns.iter().filter(|r| r.is_candidate()).count();
    if by.is_empty() && cands == 0 {
        return Ok(String::new());
    }
    let mut o = format!("\n## Related projects ({}; evidence, not interpretation) — expand REF lists all of it\n", by.len());
    let mut rows: Vec<(&String, &Vec<&Rel>)> = by.iter().collect();
    rows.sort_by_key(|(id, rs)| (rs.iter().map(|r| class_rank(&r.class)).min().unwrap_or(9), relations::node_name(v.c, id)));
    for (id, rs) in rows.iter().take(n) {
        let _ = writeln!(o, "- {} — {}", other_ref(p, v.c, id), relations::evidence_line(v.c, rs[0]));
    }
    if cands > 0 {
        let _ = writeln!(o, "- plus {cands} uncertain name-match candidate(s): {} related {}", v.cmd, q(&p.handle));
    }
    Ok(o)
}

fn class_rank(c: &str) -> u8 {
    match c {
        "declared" => 0,
        "structural" => 1,
        "documented" => 2,
        "identity" => 3,
        "observed" => 4,
        "hash" => 5,
        _ => 9,
    }
}

// ------------------------------------------------------------------ remote

pub fn remote_view(v: &View, target: &str) -> R<Option<String>> {
    let (identity, path) = match target.split_once(':') {
        Some((i, p)) => (i, Some(p)),
        None => (target, None),
    };
    let Some(n) = db(get_node(v.c, &crate::util::stable_id("remote", identity)))? else { return Ok(None) };
    if let Some(p) = path {
        return remote_doc_view(v, &n, p).map(Some);
    }
    let m = &n.meta;
    let s = |k: &str| m.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut o = String::new();
    let _ = writeln!(o, "# {} — remote repository, not cloned locally", q(&format!("remote:{}", n.label)));
    let _ = writeln!(
        o,
        "{} · {}{} · default branch {} · updated {} · via {} on {}",
        s("url"),
        s("visibility"),
        if m.get("archived").and_then(|x| x.as_bool()) == Some(true) { ", archived" } else { "" },
        s("default_branch"),
        s("updated_at"),
        s("provider"),
        s("host")
    );
    let fetched = m.get("fetched_at").and_then(|x| x.as_i64()).unwrap_or(0);
    let _ = writeln!(o, "catalog fetched {}{}", fmt_ts(fetched), if m.get("stale").and_then(|x| x.as_bool()) == Some(true) { " (STALE: refresh failed or older than the TTL)" } else { "" });
    if !s("description").is_empty() {
        let _ = writeln!(o, "description: {}", s("description"));
    }
    match crate::snapshot::row(v.c, &n.label) {
        Some((branch, sha, at, status, err, files, docs)) if !sha.is_empty() => {
            let _ = writeln!(o, "\n## Remote snapshot (opt-in; derived text from a temporary shallow clone, already deleted)");
            let _ = writeln!(
                o,
                "{branch} @ {sha} indexed {} · {files} files, {docs} docs · status {status}{}",
                fmt_ts(at),
                if err.is_empty() { String::new() } else { format!(" (last refresh failed: {err}; data above is from the indexed commit)") }
            );
            let _ = writeln!(o, "Only {branch} is indexed; other branches are not. This is the remote at that commit, not anything on local disk.");
            let mut st = db(v.c.prepare("SELECT title, min(line), count(*) FROM passages WHERE node_id=?1 AND kind='doc' AND origin LIKE 'snap:%' GROUP BY title ORDER BY title"))?;
            let ds: Vec<(String, i64, i64)> = db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect::<Result<_, _>>())?;
            for (t, _, k) in ds.iter().take(15) {
                let _ = writeln!(o, "- {} ({k} passage(s)) {}", q(&format!("remote:{}:{t}", n.label)), crate::snapshot::blob_link(&s("provider"), &s("url"), &sha, t, 0));
            }
        }
        Some((branch, _, _, _, err, _, _)) => {
            let _ = writeln!(o, "\nRemote snapshot of {branch}: not available yet ({err})");
        }
        None => {}
    }
    let ev = db(relations::neighbors(v.c, &n.id))?;
    if !ev.is_empty() {
        let _ = writeln!(o, "\n## Projects related to it ({})", ev.len());
        for r in ev.iter().take(10) {
            let _ = writeln!(o, "- {} {}", relations::node_name(v.c, &r.to), relations::evidence_line(v.c, r));
        }
    }
    let _ = writeln!(o, "\nOnly metadata is indexed. To work on it: git clone {} && {} scan", s("url"), v.cmd);
    Ok(Some(o))
}

/// `remote:<identity>:<path>[#slug]`: cached snapshot text of one doc (section).
fn remote_doc_view(v: &View, n: &Node, target: &str) -> R<String> {
    let (path, slug) = match target.rsplit_once('#') {
        Some((p, s)) if !s.is_empty() => (p, Some(s)),
        _ => (target.trim_end_matches('#'), None),
    };
    let row = crate::snapshot::row(v.c, &n.label).filter(|r| !r.1.is_empty()).ok_or_else(|| format!("remote:{} has no snapshot; enable with `workmap remote snapshots enable` and scan", n.label))?;
    let mut st = db(v.c.prepare("SELECT line, heading, loc, text FROM passages WHERE node_id=?1 AND kind='doc' AND origin LIKE 'snap:%' AND title=?2 AND (?3 IS NULL OR loc=?3) ORDER BY line, part"))?;
    let rows: Vec<(i64, String, String, String)> = db(db(st.query_map(params![n.id, path, slug], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))))?.collect::<Result<_, _>>())?;
    if rows.is_empty() {
        let mut st = db(v.c.prepare("SELECT DISTINCT title FROM passages WHERE node_id=?1 AND kind='doc' AND origin LIKE 'snap:%' ORDER BY title LIMIT 20"))?;
        let titles: Vec<String> = db(db(st.query_map([&n.id], |r| r.get(0)))?.collect::<Result<_, _>>())?;
        return Err(format!("no indexed doc {target} in the snapshot of {}; indexed docs: {}", n.label, titles.join(", ")));
    }
    let m = &n.meta;
    let g = |k: &str| m.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut o = String::new();
    let _ = writeln!(o, "# {} — REMOTE SNAPSHOT of {} at {} ({}), not on local disk", q(&format!("remote:{}:{target}", n.label)), n.label, &row.1[..12], row.0);
    let _ = writeln!(
        o,
        "indexed {} from a temporary shallow clone (deleted); the remote may have moved since. Browser: {}",
        fmt_ts(row.2),
        crate::snapshot::blob_link(&g("provider"), &g("url"), &row.1, path, rows[0].0)
    );
    let mut bytes = 0;
    let mut last = String::new();
    for (line, heading, loc, text) in &rows {
        if *loc != last {
            let _ =
                writeln!(o, "\n## l.{line}{} {}", if heading.is_empty() { String::new() } else { format!(" {heading}") }, crate::snapshot::blob_link(&g("provider"), &g("url"), &row.1, path, *line));
            last = loc.clone();
        }
        if bytes > 6000 {
            let _ = writeln!(o, "… more in the browser link above");
            break;
        }
        bytes += text.len();
        let _ = writeln!(o, "{text}");
    }
    Ok(o)
}

// ------------------------------------------------------------------ task / file context

struct Item {
    /// Direct evidence (summed) plus the best graph route (max: many weak routes
    /// to one commit must not outrank direct evidence).
    score: i64,
    hop: i64,
    ts: i64,
    r: String,
    why: Vec<String>,
    detail: Option<String>,
}

/// Reasons kept per item; more routes to the same item only add to its score.
const MAX_WHY: usize = 3;

fn add_item(items: &mut BTreeMap<String, Item>, score: i64, ts: i64, r: String, why: String, detail: Option<String>) {
    let e = items.entry(r.clone()).or_insert(Item { score: 0, hop: 0, ts, r, why: vec![], detail: None });
    if why.starts_with("[hop1]") || why.starts_with("[hop2]") {
        e.score += (score - e.hop).max(0);
        e.hop = e.hop.max(score);
    } else {
        e.score += score;
    }
    e.ts = e.ts.max(ts);
    if !e.why.contains(&why) {
        e.why.push(why);
    }
    if e.detail.is_none() {
        e.detail = detail;
    }
}

fn strength_score(kind: &str, strength: &str) -> i64 {
    match (kind, strength) {
        ("write", "observed") => 100,
        ("read", "observed") => 90,
        (_, "requested") => 45,
        ("mentioned", _) => 5,
        _ => 10,
    }
}

/// Score of a file reached through a conversation (hop 1): structured access
/// dominates; shell mentions are candidates and rank far lower.
fn hop_edge_score(kind: &str, strength: &str) -> i64 {
    match (kind, strength) {
        ("write", "observed") => 40,
        ("read", "observed") => 30,
        (_, "requested") => 15,
        ("mentioned", _) => 3,
        _ => 5,
    }
}

fn hop_verb(kind: &str, strength: &str) -> &'static str {
    match (kind, strength) {
        ("mentioned", _) => "only named in a shell command (mention, not access) by",
        ("write", "observed") => "written by",
        ("read", "observed") => "read by",
        ("write", "requested") => "write requested (no result) by",
        ("read", "requested") => "read requested (no result) by",
        _ => "failed access by",
    }
}

pub struct TaskReq<'a> {
    pub task: Option<&'a str>,
    pub files: &'a [String],
    pub budget: usize,
}

/// Graph expansion bounds for `context --task`: lexical matches (hop 0) →
/// their conversations' observed file access, and files changed by matched
/// commits (hop 1) → commits touching those files (hop 2). Fixed, no recursion.
const HOP_CONVS: usize = 5;
const HOP_FILES_PER_CONV: usize = 8;
const HOP_FILES_PER_COMMIT: usize = 5;
const HOP_COMMIT_SEEDS: usize = 3;
const HOP_COMMITS_PER_FILE: usize = 4;
/// Files whose version is compared against the focus checkout (git calls).
const HOP_COMPARE: usize = 6;

/// A file reached at hop 1, carried to hop 2.
struct HopFile {
    id: String,
    r: String,
    score: i64,
    chain: String,
    co: Option<String>,
    repo: Option<String>,
}

/// Commit reference for `sha` seen from `scope`: the focus checkout's short form
/// when it is in the focus's indexed history, otherwise qualified with the
/// checkout (`handle@wt:commit:…`) whose history has it, plus that provenance.
fn commit_link(c: &Connection, pc: &mut ProjectCache, scope: &Project, repo: Option<&str>, file_co: Option<&str>, cid: &str, sha: &str) -> (String, String) {
    let rest = &commit_ref(sha)[1..];
    let in_hist = |co: &str| c.query_row("SELECT 1 FROM edges WHERE src=?1 AND dst=?2 AND kind='history'", params![co, cid], |_| Ok(())).is_ok();
    if repo == Some(scope.repo.id.as_str()) {
        if in_hist(&scope.focus.id) {
            return (fref(scope, &scope.focus, rest), String::new());
        }
        let co = scope.checkouts.iter().find(|x| Some(x.id.as_str()) == file_co && in_hist(&x.id)).or_else(|| scope.checkouts.iter().find(|x| in_hist(&x.id)));
        return match co {
            Some(co) => {
                let wt = worktree_name(scope, co);
                (q(&qualified_raw(scope, co, rest)), format!("indexed in {wt} history, not in {}'s indexed history", worktree_name(scope, &scope.focus)))
            }
            None => (q(&qualified_raw(scope, &scope.focus, rest)), String::new()),
        };
    }
    match repo.and_then(|r| pc.get(r)) {
        Some((op, _)) => {
            let co = op.checkouts.iter().find(|x| Some(x.id.as_str()) == file_co && in_hist(&x.id)).or_else(|| op.checkouts.iter().find(|x| in_hist(&x.id))).unwrap_or(&op.focus);
            (format!("{} (other project)", q(&qualified_raw(op, co, rest))), String::new())
        }
        None => (cid.to_string(), String::new()),
    }
}

/// Ranked, budget-bounded context for a task and/or specific files. Every
/// item says why it is included. Ranking: observed file access > doc lines
/// naming the file > commits > text matches > related projects; ties by
/// newest, then reference. With a task, matches expand through a fixed
/// number of hops (see HOP_*), each item carrying its reason chain.
pub fn task_context(v: &View, p: &Project, t: &TaskReq) -> R<String> {
    let c = v.c;
    let co = &p.focus;
    let top = p.top(co);
    let all = sessions_of(c, p)?;
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    let mut uncertain: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut sib_blocks: Vec<String> = Vec::new();
    let mut cache = RefCache::new(c);
    let mut pc = ProjectCache::new(c);
    for f in t.files {
        let relp = rel(f, &top).trim_start_matches("./").to_string();
        let (head, blocks) = same_path_blocks(c, &mut pc, p, &all, &relp, 3)?;
        if !blocks.is_empty() {
            // The heading travels with its first block: never printed alone.
            let mut it = blocks.into_iter();
            let first = it.next().unwrap_or_default();
            sib_blocks.push(format!("{}{first}", head.trim_start_matches('\n').replace("## Same path", &format!("## {relp}: same path"))));
            sib_blocks.extend(it);
        }
        let Some(n) = find_file(c, p, co, &relp)? else {
            notes.push(format!(
                "{relp}: no indexed evidence in this checkout (not linked from docs, not in recent commits, not touched by an indexed conversation){}",
                if Path::new(&format!("{top}/{relp}")).exists() { "" } else { "; not on disk in this worktree" }
            ));
            continue;
        };
        for row in file_conversations(c, &mut pc, p, &all, &n.id, false)? {
            let s = &row.session;
            let Some(r) = row.link.r.clone() else {
                uncertain.push(format!("conversation \"{}\" ({}) touched {relp} but its project is no longer indexed", clip(&s.label, 60), row.link.prov));
                continue;
            };
            let r = if row.link.other { format!("{r} (other project)") } else { r };
            let mut st = db(c.prepare("SELECT kind, strength, source_line, note FROM edges WHERE src=?1 AND dst=?2 AND kind IN ('read','write','mentioned') ORDER BY kind DESC, source_line"))?;
            let edges: Vec<(String, String, i64, String)> = db(db(st.query_map(params![s.id, n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))))?.collect::<Result<_, _>>())?;
            for (kind, strength, line, note) in edges {
                let what = match kind.as_str() {
                    "mentioned" => format!("only named {relp} in a shell command"),
                    _ => format!("{} {relp} ({strength}, {})", if kind == "write" { "wrote" } else { "read" }, note.split(" @").next().unwrap_or("")),
                };
                let at = if row.link.other { format!(", {}", row.link.prov) } else { String::new() };
                add_item(
                    &mut items,
                    strength_score(&kind, &strength),
                    s.ts,
                    r.clone(),
                    format!("{what} at transcript l.{line}, {}{at}", day(s.ts)),
                    Some(format!("conversation \"{}\"", clip(&s.label, 70))),
                );
            }
        }
        let mut st = db(c.prepare("SELECT d.path, d.label, d.kind, e.kind, e.source_line, d.ts FROM edges e JOIN nodes d ON d.id=e.src WHERE e.dst=?1 AND e.kind IN ('references','reference_candidate') ORDER BY d.label, e.source_line"))?;
        let rows: Vec<(Option<String>, String, String, String, i64, i64)> =
            db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, Option<i64>>(5)?.unwrap_or(0)))))?.collect::<Result<_, _>>())?;
        for (dp, dl, dk, ek, line, ts) in rows {
            let dp = dp.unwrap_or_default();
            let drel = if dk == "memory" { dl } else { rel(&dp, &top).to_string() };
            let hs = headings(&dp);
            let target = section_of_line(&hs, line as usize).map(|h| format!("{drel}#{}", h.3)).unwrap_or(drel.clone());
            let text = line_text(&dp, line as usize, 100).unwrap_or_default();
            if ek == "references" {
                add_item(&mut items, 40, ts, fref(p, co, &target), format!("names {relp} at {drel}:{line}"), Some(format!("\"{text}\"")));
            } else {
                uncertain.push(format!("{} l.{line} names a file called like {relp} (bare filename, unverified): \"{text}\"", fref(p, co, &target)));
            }
        }
        let mut st =
            db(c.prepare("SELECT n.label, json_extract(n.meta,'$.sha'), e.note, n.ts FROM edges e JOIN nodes n ON n.id=e.src WHERE e.dst=?1 AND e.kind='touched' ORDER BY n.ts DESC, n.id LIMIT 5"))?;
        let rows: Vec<(String, String, String, i64)> = db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<i64>>(3)?.unwrap_or(0)))))?.collect::<Result<_, _>>())?;
        for (i, (label, sha, status, ts)) in rows.into_iter().enumerate() {
            let subject = label.split_once(' ').map(|x| x.1.to_string()).unwrap_or(label);
            add_item(&mut items, 30 - i as i64 * 2, ts, q(&commit_ref(&sha)), format!("changed {relp} ({status}) on {}", day(ts)), Some(format!("\"{}\"", clip(&subject, 70))));
        }
        let mut st = db(c.prepare("SELECT source_path, source_line, note FROM edges WHERE dst=?1 AND kind='entrypoint'"))?;
        let eps: Vec<(String, i64, String)> = db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect::<Result<_, _>>())?;
        for (sp, line, note) in eps {
            add_item(&mut items, 20, 0, fref(p, co, &relp), format!("declared entry point ({note}) at {}:{line}", rel(&sp, &top)), None);
        }
    }
    if let Some(task) = t.task.filter(|x| !x.trim().is_empty()) {
        let words: Vec<String> = task.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')).filter(|w| w.chars().count() > 2).map(|w| w.to_lowercase()).collect();
        let query = search::Query { text: words.join(" "), any: true, limit: 15, ..Default::default() };
        let (hits, _) = db(search::run_pref(c, &query, Some(&p.repo.id), None, Some(&co.id)))?;
        // Hop seeds: (session or commit node id, hit score, "which matched … in REF").
        let mut conv_seeds: Vec<(String, i64, String)> = Vec::new();
        let mut commit_seeds: Vec<(String, i64, String)> = Vec::new();
        for (i, h) in hits.iter().enumerate() {
            let low = h.snippet.to_lowercase();
            let matched: Vec<&String> = words.iter().filter(|w| low.contains(&format!("[{w}"))).collect();
            let (r, d) = hit_ref(&mut cache, Some(p), h);
            if r == "(this project)" {
                continue;
            }
            let score = 10 + 5 * matched.len() as i64 - i as i64;
            let mw = matched.iter().map(|w| format!("'{w}'")).collect::<Vec<_>>().join(" ");
            let origin = format!("which matched {} in {r}", if mw.is_empty() { "the task words".to_string() } else { mw.clone() });
            match h.kind.as_str() {
                "message" if !conv_seeds.iter().any(|x| x.0 == h.node_id) => conv_seeds.push((h.node_id.clone(), score, origin)),
                "commit" if !commit_seeds.iter().any(|x| x.0 == h.node_id) => commit_seeds.push((h.node_id.clone(), score, origin)),
                _ => {}
            }
            add_item(
                &mut items,
                score,
                h.ts,
                r,
                format!("[hop0] text matches {}in {d}", matched.iter().map(|w| format!("'{w}' ")).collect::<String>()),
                Some(format!("\"{}\"", clip(&h.snippet, 150))),
            );
        }
        // Hop 1: files the matched conversations observably read/wrote (structured
        // edges; shell mentions kept but labelled and ranked low), and files
        // changed by matched commits.
        let mut hop_files: Vec<HopFile> = Vec::new();
        let mut compared = 0usize;
        for (sid, parent, origin) in conv_seeds.iter().take(HOP_CONVS) {
            let Some(s) = db(get_node(c, sid))? else { continue };
            let link = conv_link(&mut pc, p, &all, &s, false);
            let conv = match &link.r {
                Some(r) if link.other => format!("{r} (other project)"),
                Some(r) => r.clone(),
                None => format!("conversation \"{}\"", clip(&s.label, 40)),
            };
            let mut st = db(c.prepare(
                "SELECT e.dst, e.kind, e.strength, e.source_line, e.note, n.path, n.checkout_id, n.repo_id, n.kind, n.label FROM edges e JOIN nodes n ON n.id=e.dst
                 WHERE e.src=?1 AND e.kind IN ('read','write','mentioned') AND n.kind IN ('file','doc')
                 ORDER BY CASE e.kind WHEN 'write' THEN 0 WHEN 'read' THEN 1 ELSE 2 END, CASE e.strength WHEN 'observed' THEN 0 WHEN 'requested' THEN 1 ELSE 2 END, e.source_line, n.path",
            ))?;
            type Row = (String, String, String, i64, String, Option<String>, Option<String>, Option<String>, String, String);
            let rows: Vec<Row> =
                db(db(st.query_map([&s.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))))?.collect::<Result<_, _>>())?;
            let mut seen: Vec<String> = Vec::new();
            for (dst, kind, strength, line, note, path, coid, repo, nk, nl) in rows {
                if seen.contains(&dst) {
                    continue;
                }
                if seen.len() >= HOP_FILES_PER_CONV {
                    break;
                }
                seen.push(dst.clone());
                let mem = if nk == "memory" { Some(nl.as_str()) } else { None };
                let r = file_ref_or_path(v, p, coid.as_deref(), path.as_deref(), mem);
                let tool = note.split(" @").next().unwrap_or("").to_string();
                let at = if line > 0 { format!(" l.{line}") } else { String::new() };
                let chain = format!("{} {conv} ({} {tool}{at}) {origin}", hop_verb(&kind, &strength), edge_code(&kind, &strength));
                let score = hop_edge_score(&kind, &strength) + parent / 2;
                let mut detail = None;
                // Same path, other version: compare with the focus checkout's copy.
                if repo.as_deref() == Some(p.repo.id.as_str()) && coid.as_deref() != Some(co.id.as_str()) && compared < HOP_COMPARE {
                    if let Some(fco) = co_of(p, coid.as_deref()) {
                        compared += 1;
                        let relp = rel(path.as_deref().unwrap_or(""), &p.top(fco)).to_string();
                        let (a, _) = blob_now(co, &relp);
                        let (b, dirty) = blob_now(fco, &relp);
                        let fwt = worktree_name(p, co);
                        let mut d = match (a, b) {
                            (Some(a), Some(b)) if a == b => format!("same blob as {fwt}'s {}", fref(p, co, &relp)),
                            (Some(a), Some(b)) => format!("DIFFERS from {fwt}'s {} (blob {}≠{}, git index now)", fref(p, co, &relp), short(&b), short(&a)),
                            (None, Some(_)) => format!("not tracked in {fwt}"),
                            _ => "not compared (not tracked there now)".to_string(),
                        };
                        if dirty {
                            d.push_str("; uncommitted change (dirty) there");
                        }
                        detail = Some(d);
                    }
                }
                add_item(&mut items, score, s.ts, r.clone(), format!("[hop1] {chain}"), detail);
                if kind != "mentioned" {
                    hop_files.push(HopFile { id: dst, r, score, chain, co: coid, repo });
                }
            }
        }
        for (cid, parent, origin) in commit_seeds.iter().take(HOP_COMMIT_SEEDS) {
            let Some(cm) = db(get_node(c, cid))? else { continue };
            let (cr, _) = commit_link(c, &mut pc, p, cm.repo_id.as_deref(), None, cid, meta(&cm, "sha"));
            let mut st = db(c.prepare(
                "SELECT n.id, n.path, n.checkout_id, n.repo_id, e.note FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 AND e.kind='touched'
                 ORDER BY CASE WHEN n.checkout_id=?2 THEN 0 ELSE 1 END, n.path LIMIT 40",
            ))?;
            let rows: Vec<(String, Option<String>, Option<String>, Option<String>, String)> =
                db(db(st.query_map(params![cid, co.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))))?.collect::<Result<_, _>>())?;
            let mut seen_rel: Vec<String> = Vec::new();
            for (fid, path, coid, repo, status) in rows {
                let top_f = coid.as_deref().and_then(|id| co_of(p, Some(id))).map(|x| p.top(x)).unwrap_or_default();
                let key = rel(path.as_deref().unwrap_or(""), &top_f).to_string();
                if seen_rel.contains(&key) {
                    continue;
                }
                if seen_rel.len() >= HOP_FILES_PER_COMMIT {
                    break;
                }
                seen_rel.push(key);
                let r = file_ref_or_path(v, p, coid.as_deref(), path.as_deref(), None);
                let chain = format!("changed ({status}) by {cr} {origin}");
                let score = 12 + parent / 2;
                add_item(&mut items, score, cm.ts, r.clone(), format!("[hop1] {chain}"), None);
                hop_files.push(HopFile { id: fid, r, score, chain, co: coid, repo });
            }
        }
        // Hop 2: commits touching the hop-1 files, in any checkout of their repository.
        for f in &hop_files {
            let mut st = db(c.prepare(
                "SELECT n.id, json_extract(n.meta,'$.sha'), n.label, n.ts, e.note FROM edges e JOIN nodes n ON n.id=e.src WHERE e.dst=?1 AND e.kind='touched' ORDER BY n.ts DESC, n.id LIMIT ?2",
            ))?;
            let rows: Vec<(String, String, String, Option<i64>, String)> =
                db(db(st.query_map(params![f.id, HOP_COMMITS_PER_FILE as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))))?.collect::<Result<_, _>>())?;
            for (j, (cid, sha, label, ts, status)) in rows.into_iter().enumerate() {
                let (r, prov) = commit_link(c, &mut pc, p, f.repo.as_deref(), f.co.as_deref(), &cid, &sha);
                let subject = label.split_once(' ').map(|x| x.1.to_string()).unwrap_or(label);
                // A commit the focus checkout does not have is what a sibling adds: rank it up.
                let bonus = if prov.is_empty() { 0 } else { 5 };
                let prov = if prov.is_empty() { String::new() } else { format!(" [{prov}]") };
                add_item(
                    &mut items,
                    (8 + bonus + f.score / 4 - j as i64).max(1),
                    ts.unwrap_or(0),
                    r,
                    format!("[hop2] changed {} ({status}){prov}; that file was {}", f.r, f.chain),
                    Some(format!("\"{}\"", clip(&subject, 70))),
                );
            }
        }
        // Related projects: the relation is the reason; a text match there adds to it.
        for (other, rels) in group_related(c, &p.repo.id)?.into_iter().take(5) {
            let oname = relations::node_name(c, &other);
            let ev = relations::evidence_line(c, &rels[0]);
            add_item(&mut items, 8 + if rels[0].class == "declared" { 4 } else { 0 }, 0, q(&format!(":related:{oname}")), format!("related project: {ev}"), None);
            let (oh, _) = db(search::run(c, &query, Some(&other), None))?;
            for h in oh.iter().take(2) {
                let (r, d) = hit_ref(&mut cache, Some(p), h);
                add_item(&mut items, 6, h.ts, r, format!("text match in related project {oname} ({d})"), Some(format!("\"{}\"", clip(&h.snippet, 150))));
            }
        }
    } else if !t.files.is_empty() {
        for (other, rels) in group_related(c, &p.repo.id)?.into_iter().take(3) {
            let oname = relations::node_name(c, &other);
            add_item(&mut items, 5, 0, q(&format!(":related:{oname}")), format!("related project: {}", relations::evidence_line(c, &rels[0])), None);
        }
    }
    let mut ranked: Vec<Item> = items.into_values().collect();
    ranked.sort_by(|a, b| b.score.cmp(&a.score).then(b.ts.cmp(&a.ts)).then(a.r.cmp(&b.r)));

    let mut o = String::new();
    let _ = writeln!(o, "# {} — task context{}", p.handle, t.task.map(|x| format!(" for {:?}", clip(x, 80))).unwrap_or_default());
    if !t.files.is_empty() {
        let _ = writeln!(o, "files: {}", t.files.join(", "));
    }
    o.push_str(&trust::index_line(c));
    o.push_str(&trust::coverage_line(&trust::coverage(c, Some(&p.repo.id))));
    o.push_str(&header(v, p, false));
    for n in &notes {
        let _ = writeln!(o, "note: {n}");
    }
    let _ = writeln!(o, "\n## Ranked (score; why it matters) — budget {} bytes{}", t.budget, if t.task.is_some() { "; [hopN] = N steps from a text match, each with its reason chain" } else { "" });
    // Whole items only: an item that does not fit is dropped, never cut. The
    // final line (reserved up front) says what was left out and how to get it;
    // reasons are joined with ` · ` so a trailing ref is never glued to punctuation.
    const RESERVE: usize = 200;
    let lines: Vec<String> = ranked
        .iter()
        .map(|it| {
            let extra = it.why.len().saturating_sub(MAX_WHY);
            let mut line = format!(
                "- {} [{}] — {}{}\n",
                it.r,
                it.score,
                it.why.iter().take(MAX_WHY).cloned().collect::<Vec<_>>().join(" · "),
                if extra > 0 { format!(" (+{extra} more route(s))") } else { String::new() }
            );
            if let Some(d) = &it.detail {
                let _ = writeln!(line, "    {d}");
            }
            line
        })
        .collect();
    let mut needed = o.len();
    let mut shown = 0;
    let mut first_omitted: Option<&str> = None;
    for (it, line) in ranked.iter().zip(&lines) {
        needed += line.len();
        if first_omitted.is_none() && o.len() + line.len() + RESERVE <= t.budget {
            o.push_str(line);
            shown += 1;
        } else if first_omitted.is_none() {
            first_omitted = Some(it.r.as_str());
        }
    }
    if ranked.is_empty() {
        let _ = writeln!(o, "- nothing recorded for this task/file in {}", p.handle);
    }
    let mut blocks_omitted = 0;
    for (i, b) in sib_blocks.iter().enumerate() {
        let b = if i == 0 || b.starts_with("## ") { format!("\n{b}") } else { b.clone() };
        needed += b.len();
        if blocks_omitted == 0 && o.len() + b.len() + RESERVE <= t.budget {
            o.push_str(&b);
        } else {
            blocks_omitted += 1;
        }
    }
    let mut uncertain_omitted = 0;
    if !uncertain.is_empty() {
        let block = format!("\n## Uncertain (not ranked)\n{}\n", uncertain.iter().map(|u| format!("- {u}")).collect::<Vec<_>>().join("\n"));
        needed += block.len();
        if o.len() + block.len() + RESERVE <= t.budget {
            o.push_str(&block);
        } else {
            uncertain_omitted = uncertain.len();
        }
    }
    let omitted = ranked.len() - shown;
    if omitted + blocks_omitted + uncertain_omitted > 0 {
        let mut parts = Vec::new();
        if omitted > 0 {
            parts.push(format!("{omitted} ranked item(s)"));
        }
        if blocks_omitted > 0 {
            parts.push(format!("{blocks_omitted} other-checkout block(s)"));
        }
        if uncertain_omitted > 0 {
            parts.push(format!("{uncertain_omitted} uncertain mention(s)"));
        }
        let mut footer = format!("… omitted to stay within --budget {}: {}; rerun with --budget {}", t.budget, parts.join(", "), needed + RESERVE + 20);
        if let Some(r) = first_omitted.filter(|r| footer.len() + r.len() + 14 < RESERVE) {
            let _ = write!(footer, ", or expand {r}");
        }
        let _ = writeln!(o, "{footer}");
    }
    Ok(o)
}

fn group_related(c: &Connection, repo: &str) -> R<Vec<(String, Vec<Rel>)>> {
    let mut by: BTreeMap<String, Vec<Rel>> = BTreeMap::new();
    for r in db(relations::neighbors(c, repo))?.into_iter().filter(|r| !r.is_candidate()) {
        by.entry(r.to.clone()).or_default().push(r);
    }
    let mut v: Vec<(String, Vec<Rel>)> = by.into_iter().collect();
    v.sort_by_key(|(id, rs)| (class_rank(&rs[0].class), id.clone()));
    Ok(v)
}

// ------------------------------------------------------------------ ask

/// What changed on disk since the stored conversation ended.
pub fn ask_note(c: &Connection, sid: &str) -> String {
    let Ok(Some(s)) = get_node(c, sid) else { return String::new() };
    let ended = s.ts;
    let mut parts = Vec::new();
    if let Some(co) = s.checkout_id.as_deref().and_then(|id| get_node(c, id).ok().flatten()) {
        let commits: i64 = c.query_row("SELECT count(*) FROM nodes n JOIN edges e ON e.dst=n.id AND e.kind='history' AND e.src=?1 WHERE n.ts > ?2", params![co.id, ended], |r| r.get(0)).unwrap_or(0);
        parts.push(format!("{commits} indexed commit(s) on {}", branch(&co)));
        if let Some(d) = co.path.as_deref().and_then(|p| crate::git::dirty_count(Path::new(p))) {
            parts.push(format!("{d} tracked file(s) modified now"));
        }
    }
    let mut st = match c.prepare("SELECT DISTINCT n.path FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 AND e.kind='write' AND e.strength='observed' ORDER BY n.path") {
        Ok(st) => st,
        Err(_) => return String::new(),
    };
    let written: Vec<String> = st.query_map([sid], |r| r.get::<_, Option<String>>(0)).map(|r| r.flatten().flatten().collect()).unwrap_or_default();
    let changed: Vec<&String> = written.iter().filter(|p| std::fs::metadata(p).map(|m| crate::util::mtime_unix(&m) > ended).unwrap_or(true)).collect();
    if !written.is_empty() {
        parts.push(format!("{} of {} file(s) it wrote changed or vanished since", changed.len(), written.len()));
    }
    format!(
        "historical session: last activity {}{}. Since then: {}. The resumed agent remembers its transcript, not today's files; its latest user input is not a decision record.\n",
        fmt_ts(ended),
        if s.meta.get("source_missing").is_some() { " (transcript missing locally; resume will likely fail)" } else { "" },
        if parts.is_empty() { "unknown".into() } else { parts.join(", ") }
    )
}
