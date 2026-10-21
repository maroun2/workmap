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
            let wt = co.filter(|co| !target.is_default(co)).map(|co| format!(" --worktree {}", q(&worktree_name(target, co)))).unwrap_or_default();
            format!("{}{wt}{}", q(&format!("{}:{rest}", target.handle)), if scope.is_some() { " (other project)" } else { "" })
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
    let (hits, total) = db(search::run(v.c, &sr.query, scope.as_ref().map(|p| p.repo.id.as_str()), if sr.this_worktree { scope.as_ref().map(|p| p.focus.id.as_str()) } else { None }))?;
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

pub fn remote_view(v: &View, identity: &str) -> R<Option<String>> {
    let Some(n) = db(get_node(v.c, &crate::util::stable_id("remote", identity)))? else { return Ok(None) };
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
    let ev = db(relations::neighbors(v.c, &n.id))?;
    if !ev.is_empty() {
        let _ = writeln!(o, "\n## Local projects pointing at it ({})", ev.len());
        for r in ev.iter().take(10) {
            let _ = writeln!(o, "- {} {}", relations::node_name(v.c, &r.to), relations::evidence_line(v.c, r));
        }
    }
    let _ = writeln!(o, "\nOnly metadata is indexed. To work on it: git clone {} && {} scan", s("url"), v.cmd);
    Ok(Some(o))
}

// ------------------------------------------------------------------ task / file context

struct Item {
    score: i64,
    ts: i64,
    r: String,
    why: Vec<String>,
    detail: Option<String>,
}

fn add_item(items: &mut BTreeMap<String, Item>, score: i64, ts: i64, r: String, why: String, detail: Option<String>) {
    let e = items.entry(r.clone()).or_insert(Item { score: 0, ts, r, why: vec![], detail: None });
    e.score += score;
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

pub struct TaskReq<'a> {
    pub task: Option<&'a str>,
    pub files: &'a [String],
    pub budget: usize,
}

/// Ranked, budget-bounded context for a task and/or specific files. Every
/// item says why it is included. Ranking: observed file access > doc lines
/// naming the file > commits > text matches > related projects; ties by
/// newest, then reference.
pub fn task_context(v: &View, p: &Project, t: &TaskReq) -> R<String> {
    let c = v.c;
    let co = &p.focus;
    let top = p.top(co);
    let all = sessions_of(c, p)?;
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    let mut uncertain: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut cache = RefCache::new(c);
    for f in t.files {
        let relp = rel(f, &top).trim_start_matches("./").to_string();
        let Some(n) = find_file(c, p, co, &relp)? else {
            notes.push(format!(
                "{relp}: no indexed evidence (not linked from docs, not in recent commits, not touched by an indexed conversation){}",
                if Path::new(&format!("{top}/{relp}")).exists() { "" } else { "; not on disk in this worktree" }
            ));
            continue;
        };
        let mut st =
            db(c.prepare("SELECT e.src, e.kind, e.strength, e.source_line, e.note FROM edges e JOIN nodes s ON s.id=e.src WHERE e.dst=?1 AND s.kind='session' ORDER BY s.ts DESC, e.source_line"))?;
        let rows: Vec<(String, String, String, i64, String)> = db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))))?.collect::<Result<_, _>>())?;
        for (sid, kind, strength, line, note) in rows {
            let Some(s) = db(get_node(c, &sid))? else { continue };
            let (r, title) = match all.iter().find(|x| x.id == s.id) {
                Some(_) => (conv_ref(&all, &s), s.label.clone()),
                None => {
                    let Some(op) = s.repo_id.as_deref().and_then(|rid| cache.project(rid, None).map(|p| (p.handle.clone(), p.repo.id.clone()))) else { continue };
                    let ap = project_by_repo(c, &op.1, None);
                    let osess = ap.as_ref().map(|ap| sessions_of(c, ap).unwrap_or_default()).unwrap_or_default();
                    (format!("{} (other project)", q(&format!("{}{}", op.0, conv_ref_raw(&osess, &s)))), s.label.clone())
                }
            };
            let what = match kind.as_str() {
                "mentioned" => format!("only named {relp} in a shell command"),
                _ => format!("{} {relp} ({strength}, {})", if kind == "write" { "wrote" } else { "read" }, note.split(" @").next().unwrap_or("")),
            };
            add_item(&mut items, strength_score(&kind, &strength), s.ts, r, format!("{what} at transcript l.{line}, {}", day(s.ts)), Some(format!("conversation \"{}\"", clip(&title, 70))));
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
        let query = search::Query { text: task.to_string(), any: true, limit: 15, ..Default::default() };
        let (hits, _) = db(search::run(c, &query, Some(&p.repo.id), None))?;
        for (i, h) in hits.iter().enumerate() {
            let low = h.snippet.to_lowercase();
            let matched: Vec<&String> = words.iter().filter(|w| low.contains(&format!("[{w}"))).collect();
            let (r, d) = hit_ref(&mut cache, Some(p), h);
            if r == "(this project)" {
                continue;
            }
            add_item(
                &mut items,
                10 + 5 * matched.len() as i64 - i as i64,
                h.ts,
                r,
                format!("text matches {} in {d}", matched.iter().map(|w| format!("'{w}'")).collect::<Vec<_>>().join(",")),
                Some(format!("\"{}\"", clip(&h.snippet, 150))),
            );
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
    let _ = writeln!(o, "\n## Ranked (score; why it matters) — budget {} bytes", t.budget);
    let mut shown = 0;
    for it in &ranked {
        let mut line = format!("- {} [{}] — {}\n", it.r, it.score, it.why.join("; "));
        if let Some(d) = &it.detail {
            let _ = writeln!(line, "    {d}");
        }
        if o.len() + line.len() > t.budget {
            break;
        }
        o.push_str(&line);
        shown += 1;
    }
    if ranked.is_empty() {
        let _ = writeln!(o, "- nothing recorded for this task/file in {}", p.handle);
    }
    if !uncertain.is_empty() {
        let block = format!("\n## Uncertain (not ranked)\n{}\n", uncertain.iter().map(|u| format!("- {u}")).collect::<Vec<_>>().join("\n"));
        if o.len() + block.len() <= t.budget {
            o.push_str(&block);
        }
    }
    if shown < ranked.len() {
        let _ = writeln!(o, "… {} more item(s) omitted to stay within --budget {} (raise it, or expand a ref)", ranked.len() - shown, t.budget);
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
