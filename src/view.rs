//! Human-facing views addressed by references, not ids. A reference is
//! `<project>:<rest>` or, with a scope (`--project` / `WORKMAP_PROJECT`), just `:<rest>`:
//!
//!   :<path>                     one file/doc and everything related to it
//!   :<path>#<heading-slug>      one doc section, read from the file itself
//!   :commit:<sha-prefix>        one commit
//!   :conversation:<title>       one recorded agent conversation (exact title or prefix)
//!   :session:<native-id-prefix> the same, by the provider's own session id
//!
//! Views print the scope once and then only short references. Names that could
//! match more than one thing fail with the named choices instead of guessing.

use crate::overview::{get_node, nodes, rank_files, rel, Node};
use crate::util::{clip, fmt_date, fmt_ts, sh_quote, slug};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::Path;

#[path = "view_more.rs"]
pub mod more;

type R<T> = Result<T, String>;

fn db<T>(r: rusqlite::Result<T>) -> R<T> {
    r.map_err(|e| format!("index query failed: {e}"))
}

/// How printed commands start (`workmap`, a full path, or `env … <bin>`), and
/// non-default environment (`WORKMAP_STATE_DIR=…`) the reader must export once.
pub struct View<'a> {
    pub c: &'a Connection,
    pub cmd: String,
    pub state: String,
}

pub struct Project {
    pub repo: Node,
    pub handle: String,
    pub checkouts: Vec<Node>,
    /// Checkout chosen by path or `--worktree`; otherwise the main one.
    pub focus: Node,
}

impl Project {
    fn is_default(&self, co: &Node) -> bool {
        self.checkouts.first().map(|m| m.id == co.id).unwrap_or(true)
    }
    pub fn top(&self, co: &Node) -> String {
        co.path.clone().unwrap_or_default()
    }
}

pub fn branch(co: &Node) -> String {
    co.meta.get("branch").and_then(|x| x.as_str()).map(str::to_string).unwrap_or_else(|| "(detached)".into())
}

fn dir_name(co: &Node) -> String {
    co.path.as_deref().and_then(|p| p.rsplit('/').next()).unwrap_or("").to_string()
}

// ------------------------------------------------------------------ resolution

fn all_projects(c: &Connection) -> R<Vec<Node>> {
    db(nodes(c, "kind='repo' ORDER BY label, id", &[]))
}

fn checkouts_of(c: &Connection, repo_id: &str) -> R<Vec<Node>> {
    db(nodes(c, "kind='checkout' AND repo_id=?1 ORDER BY json_extract(meta,'$.main') DESC, ts DESC, path", &[&repo_id]))
}

/// The name printed for a project: its label when unique, else its main checkout path.
pub fn project_handle(c: &Connection, repo: &Node) -> R<String> {
    let same: i64 = db(c.query_row("SELECT count(*) FROM nodes WHERE kind='repo' AND label=?1", [&repo.label], |r| r.get(0)))?;
    if same <= 1 && !repo.label.contains(':') && !repo.label.starts_with('/') {
        return Ok(repo.label.clone());
    }
    let cos = checkouts_of(c, &repo.id)?;
    Ok(cos.first().and_then(|n| n.path.clone()).unwrap_or_else(|| repo.id.clone()))
}

fn build_project(c: &Connection, repo: Node, focus: Option<Node>) -> R<Project> {
    let checkouts = checkouts_of(c, &repo.id)?;
    let focus = focus.or_else(|| checkouts.first().cloned()).ok_or_else(|| format!("project {} has no checkouts indexed", repo.label))?;
    Ok(Project { handle: project_handle(c, &repo)?, repo, checkouts, focus })
}

/// Project by exact label (case-sensitive, then case-insensitive), checkout path, or id.
pub fn resolve_project(c: &Connection, name: &str) -> R<Project> {
    if let Some(n) = db(get_node(c, name))? {
        let repo_id = n.repo_id.clone().unwrap_or(n.id.clone());
        if let Some(repo) = db(get_node(c, &repo_id))? {
            let focus = if n.kind == "checkout" { Some(n) } else { None };
            return build_project(c, repo, focus);
        }
    }
    if name.starts_with('/') || name.starts_with('.') || name.starts_with('~') {
        let abs = std::fs::canonicalize(name).ok().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| name.to_string());
        let mut best: Option<Node> = None;
        for n in db(nodes(c, "kind='checkout'", &[]))? {
            let top = n.path.clone().unwrap_or_default();
            if (abs == top || abs.starts_with(&format!("{top}/"))) && best.as_ref().map(|b| b.path.as_ref().map(|p| p.len()).unwrap_or(0) < top.len()).unwrap_or(true) {
                best = Some(n);
            }
        }
        let co = best.ok_or_else(|| format!("no indexed checkout contains {name}; run `workmap roots add <dir>` and `workmap scan`"))?;
        let repo = db(get_node(c, co.repo_id.as_deref().unwrap_or("")))?.ok_or("checkout without repository")?;
        return build_project(c, repo, Some(co));
    }
    let all = all_projects(c)?;
    let mut hits: Vec<&Node> = all.iter().filter(|r| r.label == name).collect();
    if hits.is_empty() {
        hits = all.iter().filter(|r| r.label.eq_ignore_ascii_case(name)).collect();
    }
    match hits.len() {
        1 => build_project(c, hits[0].clone(), None),
        0 => {
            let lower = name.to_lowercase();
            let near: Vec<&Node> = all.iter().filter(|r| r.label.to_lowercase().contains(&lower) || lower.contains(&r.label.to_lowercase())).take(10).collect();
            let mut m = format!("no project named {}", sh_quote(name));
            if near.is_empty() {
                m.push_str(" (see `workmap overview` for all names)");
            } else {
                m.push_str("; similar names:");
                for r in near {
                    let _ = write!(m, "\n  {}", project_handle(c, r)?);
                }
            }
            Err(m)
        }
        _ => {
            let mut m = format!("project name {} is ambiguous ({} repositories); choose one by checkout path:", sh_quote(name), hits.len());
            for r in hits {
                let cos = checkouts_of(c, &r.id)?;
                let p = cos.first().and_then(|n| n.path.clone()).unwrap_or_default();
                let _ = write!(m, "\n  {}  ({} checkout(s), last commit {})", sh_quote(&p), cos.len(), fmt_date(r.ts));
            }
            Err(m)
        }
    }
}

/// `--worktree <branch|dir-name|path>` within a project.
pub fn select_worktree(p: &mut Project, name: Option<&str>) -> R<()> {
    let Some(name) = name else { return Ok(()) };
    let hits: Vec<Node> = p.checkouts.iter().filter(|co| branch(co) == name || dir_name(co) == name || co.path.as_deref() == Some(name)).cloned().collect();
    match hits.len() {
        1 => {
            p.focus = hits[0].clone();
            Ok(())
        }
        n => {
            let mut m = if n == 0 { format!("no worktree {} in {}; worktrees:", sh_quote(name), p.handle) } else { format!("worktree name {} is ambiguous in {}:", sh_quote(name), p.handle) };
            for co in if n == 0 { &p.checkouts } else { &hits } {
                let _ = write!(m, "\n  --worktree {}  ({})", sh_quote(&worktree_name(p, co)), co.path.clone().unwrap_or_default());
            }
            Err(m)
        }
    }
}

/// Shortest unambiguous `--worktree` value for a checkout.
pub fn worktree_name(p: &Project, co: &Node) -> String {
    let b = branch(co);
    if p.checkouts.iter().filter(|x| branch(x) == b || dir_name(x) == b).count() == 1 && b != "(detached)" {
        return b;
    }
    let d = dir_name(co);
    if p.checkouts.iter().filter(|x| dir_name(x) == d || branch(x) == d).count() == 1 {
        return d;
    }
    co.path.clone().unwrap_or_default()
}

/// Conversation filters as given on the command line.
#[derive(Default, Clone, Debug)]
pub struct ConvSel {
    pub title: Option<String>,
    pub provider: Option<String>,
    pub date: Option<String>,
    pub session: Option<String>,
}

impl ConvSel {
    fn args(&self) -> String {
        let mut o = String::new();
        if let Some(t) = &self.title {
            let _ = write!(o, " --conversation {}", sh_quote(t));
        }
        if let Some(t) = &self.provider {
            let _ = write!(o, " --provider {}", sh_quote(t));
        }
        if let Some(t) = &self.date {
            let _ = write!(o, " --date {}", sh_quote(t));
        }
        if let Some(t) = &self.session {
            let _ = write!(o, " --session {}", sh_quote(t));
        }
        o
    }
}

fn meta<'a>(n: &'a Node, k: &str) -> &'a str {
    n.meta.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn sessions_of(c: &Connection, p: &Project) -> R<Vec<Node>> {
    db(nodes(c, "kind='session' AND repo_id=?1 ORDER BY ts DESC, id", &[&p.repo.id]))
}

/// Apply a selector: filters first, then exact title, then title prefix (case-insensitive).
fn matching<'a>(all: &'a [Node], sel: &ConvSel) -> Vec<&'a Node> {
    let base: Vec<&Node> = all
        .iter()
        .filter(|s| sel.provider.as_deref().map(|p| meta(s, "provider") == p || (p == "opencode" && meta(s, "provider").starts_with("opencode"))).unwrap_or(true))
        .filter(|s| sel.date.as_deref().map(|d| fmt_date(s.ts) == d).unwrap_or(true))
        .filter(|s| sel.session.as_deref().map(|x| meta(s, "native_id").starts_with(x)).unwrap_or(true))
        .collect();
    let Some(t) = sel.title.as_deref() else { return base };
    let tl = t.to_lowercase();
    let exact: Vec<&Node> = base.iter().copied().filter(|s| s.label.to_lowercase() == tl).collect();
    if !exact.is_empty() {
        return exact;
    }
    base.into_iter().filter(|s| s.label.to_lowercase().starts_with(&tl)).collect()
}

pub fn resolve_conversation(v: &View, p: &Project, sel: &ConvSel) -> R<Node> {
    if sel.title.is_none() && sel.session.is_none() {
        return Err("give --conversation <title> or --session <native-id-prefix>".into());
    }
    let all = sessions_of(v.c, p)?;
    let m = matching(&all, sel);
    match m.len() {
        1 => Ok(m[0].clone()),
        0 => {
            let mut msg = format!("no conversation in {} matches{}", p.handle, sel.args());
            if let Some(t) = &sel.title {
                let tl = t.to_lowercase();
                let near: Vec<&Node> = all.iter().filter(|s| s.label.to_lowercase().contains(&tl)).take(8).collect();
                if !near.is_empty() {
                    msg.push_str("; titles containing it:");
                    for s in near {
                        let _ = write!(msg, "\n  {}  {} {} {:?}", q(&format!("{}{}", p.handle, conv_ref_raw(&all, s))), fmt_date(s.ts), meta(s, "provider"), clip(&s.label, 60));
                    }
                }
            }
            Err(msg)
        }
        n => {
            let mut msg = format!("{n} conversations in {} match{}; pick one:", p.handle, sel.args());
            for s in m.iter().take(15) {
                let _ = write!(msg, "\n  {}  {} {} {:?}", q(&format!("{}{}", p.handle, conv_ref_raw(&all, s))), fmt_ts(s.ts), meta(s, "provider"), clip(&s.label, 60));
            }
            if n > 15 {
                let _ = write!(msg, "\n  … {} more; add --provider, --date YYYY-MM-DD or --session <id-prefix>", n - 15);
            }
            Err(msg)
        }
    }
}

// ------------------------------------------------------------------ references

fn q(s: &str) -> String {
    sh_quote(s)
}

/// Short, shell-quoted reference relative to the scope project; adds `--worktree`
/// when the item lives in a checkout other than the scope's.
fn fref(p: &Project, co: &Node, rest: &str) -> String {
    let r = q(&format!(":{rest}"));
    if co.id == p.focus.id {
        r
    } else {
        format!("{r} --worktree {}", q(&worktree_name(p, co)))
    }
}

fn commit_ref(sha: &str) -> String {
    format!(":commit:{}", &sha[..sha.len().min(8)])
}

/// Smallest reference that resolves to exactly this conversation: a title prefix
/// (`:conversation:`), else the provider's session id prefix (`:session:`).
pub fn conv_ref(all: &[Node], s: &Node) -> String {
    q(&conv_ref_raw(all, s))
}

/// Unquoted form of [`conv_ref`].
pub fn conv_ref_raw(all: &[Node], s: &Node) -> String {
    let unique = |sel: &ConvSel| {
        let m = matching(all, sel);
        m.len() == 1 && m[0].id == s.id
    };
    let title = s.label.trim_end_matches('…').trim().to_string();
    let mut acc = String::new();
    if title != "(untitled)" {
        for w in title.split(' ') {
            if !acc.is_empty() {
                acc.push(' ');
            }
            acc.push_str(w);
            if acc.chars().count() >= 12 && unique(&ConvSel { title: Some(acc.clone()), ..Default::default() }) {
                return format!(":conversation:{acc}");
            }
            if acc.chars().count() > 60 {
                break;
            }
        }
        if title.chars().count() < 12 && unique(&ConvSel { title: Some(title.clone()), ..Default::default() }) {
            return format!(":conversation:{title}");
        }
    }
    let native = meta(s, "native_id").to_string();
    for n in [8usize, 12, 16, 24, 36] {
        let pre: String = native.chars().take(n).collect();
        if unique(&ConvSel { session: Some(pre.clone()), ..Default::default() }) {
            return format!(":session:{pre}");
        }
    }
    format!(":session:{native}")
}

/// Scope line printed once per output, plus the status legend when `codes`.
fn header(v: &View, p: &Project, codes: bool) -> String {
    let mut vars = vec![format!("WORKMAP_PROJECT={}", q(&p.handle))];
    let mut flags = format!("--project {}", q(&p.handle));
    if !p.is_default(&p.focus) {
        vars.push(format!("WORKMAP_WORKTREE={}", q(&worktree_name(p, &p.focus))));
        let _ = write!(flags, " --worktree {}", q(&worktree_name(p, &p.focus)));
    }
    if !v.state.is_empty() {
        vars.push(v.state.clone());
    }
    let mut o = format!("Scope: export {} (or add {flags}); then: {} expand REF · {} ask REF 'question' --dry-run\n", vars.join(" "), v.cmd, v.cmd);
    if codes {
        o.push_str("Codes: r/w = read/written, tool result recorded · req = requested, no result · fail = failed · sh = only named in a shell command (not access)\n");
    }
    o
}

/// Repo-relative handle for an indexed file/doc/memory node in a checkout.
fn node_rel(n: &Node, top: &str) -> String {
    if n.kind == "memory" {
        return n.label.clone();
    }
    rel(n.path.as_deref().unwrap_or(&n.label), top).to_string()
}

fn on_disk(n: &Node) -> bool {
    n.path.as_deref().map(|p| Path::new(p).exists()).unwrap_or(false)
}

/// Heading list `(line, level, text, slug)` read from the file (bounded), with GitHub duplicate suffixes.
pub(crate) fn headings(path: &str) -> Vec<(usize, usize, String, String)> {
    let Ok(bytes) = crate::util::read_head(Path::new(path), 256 << 10) else { return vec![] };
    crate::docs::sections(&String::from_utf8_lossy(&bytes)).into_iter().map(|s| (s.line, s.level, s.heading, s.slug)).collect()
}

fn section_of_line(hs: &[(usize, usize, String, String)], line: usize) -> Option<&(usize, usize, String, String)> {
    hs.iter().filter(|h| h.0 <= line).last()
}

pub(crate) fn line_text(path: &str, line: usize, max: usize) -> Option<String> {
    let bytes = crate::util::read_head(Path::new(path), 256 << 10).ok()?;
    String::from_utf8_lossy(&bytes).lines().nth(line.checked_sub(1)?).map(|l| crate::util::redact(&clip(l.trim(), max)))
}

#[derive(Default)]
struct Access {
    r: (i64, i64, i64),
    w: (i64, i64, i64),
    sh: i64,
}

fn access_counts(c: &Connection, sid: &str) -> R<Access> {
    let mut st = db(c.prepare("SELECT kind, strength, count(*) FROM edges WHERE src=?1 AND kind IN ('read','write','mentioned') GROUP BY 1,2"))?;
    let rows: Vec<(String, String, i64)> = db(db(st.query_map([sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect::<Result<_, _>>())?;
    let mut a = Access::default();
    for (k, s, n) in rows {
        let slot = match k.as_str() {
            "read" => &mut a.r,
            "write" => &mut a.w,
            _ => {
                a.sh += n;
                continue;
            }
        };
        match s.as_str() {
            "observed" => slot.0 += n,
            "requested" => slot.1 += n,
            _ => slot.2 += n,
        }
    }
    Ok(a)
}

/// `r3 w20 req1 fail2 sh50` (distinct files per status; zero parts omitted).
fn codes(a: &Access) -> String {
    let mut v = Vec::new();
    if a.r.0 > 0 {
        v.push(format!("r{}", a.r.0));
    }
    if a.w.0 > 0 {
        v.push(format!("w{}", a.w.0));
    }
    if a.r.1 + a.w.1 > 0 {
        v.push(format!("req{}", a.r.1 + a.w.1));
    }
    if a.r.2 + a.w.2 > 0 {
        v.push(format!("fail{}", a.r.2 + a.w.2));
    }
    if a.sh > 0 {
        v.push(format!("sh{}", a.sh));
    }
    if v.is_empty() {
        "no file access recorded".into()
    } else {
        v.join(" ")
    }
}

fn edge_code(kind: &str, strength: &str) -> &'static str {
    match (kind, strength) {
        ("mentioned", _) => "sh",
        ("read", "observed") => "r",
        ("write", "observed") => "w",
        ("read", "requested") => "r-req",
        ("write", "requested") => "w-req",
        ("read", _) => "r-fail",
        _ => "w-fail",
    }
}

fn co_of<'a>(p: &'a Project, id: Option<&str>) -> Option<&'a Node> {
    p.checkouts.iter().find(|c| Some(c.id.as_str()) == id)
}

/// Bounds for the project map.
pub struct MapLimits {
    pub docs: usize,
    pub files: usize,
    pub commits: usize,
    pub sessions: usize,
    pub related: usize,
}

impl Default for MapLimits {
    fn default() -> Self {
        MapLimits { docs: 6, files: 8, commits: 4, sessions: 6, related: 5 }
    }
}

fn day(ts: i64) -> String {
    fmt_date(ts)
}

// ------------------------------------------------------------------ project map

pub fn project_map(v: &View, p: &Project, lim: &MapLimits) -> R<String> {
    let c = v.c;
    let mut o = String::new();
    let co = &p.focus;
    let top = p.top(co);
    let dirty = co.path.as_deref().and_then(|x| crate::git::dirty_count(Path::new(x)));
    let _ = writeln!(o, "# {} — project map", p.handle);
    let mut facts = vec![format!("{} @ {top}", branch(co)), format!("HEAD {}", &meta(co, "head")[..meta(co, "head").len().min(8)])];
    facts.push(format!("{} modified now", dirty.map(|d| d.to_string()).unwrap_or_else(|| "?".into())));
    facts.push(format!("last commit {}", day(co.ts)));
    if let Some(Value::Array(rs)) = p.repo.meta.get("remotes") {
        if let Some(r) = rs.first() {
            facts.push(format!("{} {}", r.get("name").and_then(|x| x.as_str()).unwrap_or(""), r.get("url").and_then(|x| x.as_str()).unwrap_or("")));
        }
    }
    if let Some(cat) = p.repo.meta.get("catalog") {
        let g = |k: &str| cat.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        facts.push(format!("{} {}{}", g("identity"), g("visibility"), if cat.get("archived").and_then(|x| x.as_bool()) == Some(true) { " archived" } else { "" }));
    }
    let _ = writeln!(o, "{}", facts.join(" · "));
    if let Some(d) = p.repo.meta.pointer("/catalog/description").and_then(|x| x.as_str()).filter(|d| !d.is_empty()) {
        let _ = writeln!(o, "description ({}): {}", p.repo.meta.pointer("/catalog/provider").and_then(|x| x.as_str()).unwrap_or("remote"), clip(d, 160));
    }
    o.push_str(&crate::trust::index_line(c));
    o.push_str(&crate::trust::coverage_line(&crate::trust::coverage(c, Some(&p.repo.id))));
    o.push_str(&header(v, p, true));
    let mut more: Vec<String> = Vec::new();

    if p.checkouts.len() > 1 {
        let _ = writeln!(o, "\n## Worktrees ({}) — select with WORKMAP_WORKTREE=<name> or --worktree <name>", p.checkouts.len());
        for x in &p.checkouts {
            let ses: i64 = db(c.query_row("SELECT count(*) FROM nodes WHERE kind='session' AND checkout_id=?1", [&x.id], |r| r.get(0)))?;
            let _ = writeln!(o, "{} {} @ {} — last commit {}, {ses} conversation(s)", if x.id == co.id { "*" } else { "-" }, worktree_name(p, x), x.path.clone().unwrap_or_default(), day(x.ts));
        }
    }

    let docs = db(nodes(
        c,
        "kind IN ('doc','memory') AND checkout_id=?1 ORDER BY kind, (label NOT LIKE '%README%'), (label NOT LIKE '%CLAUDE%' AND label NOT LIKE '%AGENTS%'), length(label), label",
        &[&co.id],
    ))?;
    if !docs.is_empty() {
        let _ = writeln!(o, "\n## Docs ({}) — the project's own words, with line numbers", docs.len());
        for d in docs.iter().take(lim.docs) {
            let relp = node_rel(d, &top);
            let intro: Option<(i64, String)> = db(c
                .query_row(
                    "SELECT line, text FROM excerpts WHERE node_id=?1 AND text NOT LIKE '![%' AND text NOT LIKE '[![%' AND text NOT LIKE '<%' AND text NOT LIKE '---%' ORDER BY line LIMIT 1",
                    [&d.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map(Some)
                .or_else(|e| if matches!(e, rusqlite::Error::QueryReturnedNoRows) { Ok(None) } else { Err(e) }))?;
            let nrefs: i64 = db(c.query_row("SELECT count(DISTINCT dst) FROM edges WHERE src=?1 AND kind='references'", [&d.id], |r| r.get(0)))?;
            let mut st = db(c.prepare(
                "SELECT note, count(DISTINCT dst) n FROM edges WHERE src=?1 AND kind='references' GROUP BY substr(note,1,instr(note||' |',' |')-1) ORDER BY n DESC, min(source_line) LIMIT 1",
            ))?;
            let top_sec: Option<(String, i64)> = db(st.query_map([&d.id], |r| Ok((r.get(0)?, r.get(1)?))))?.flatten().next();
            let mut line = format!("- {}", fref(p, co, &relp));
            if let Some((l, t)) = &intro {
                let _ = write!(line, " l.{l} \"{}\"", clip(t, 95));
            }
            if nrefs > 0 {
                let _ = write!(line, " — links {nrefs}");
            }
            if let Some((note, n)) = top_sec.filter(|x| x.1 > 1) {
                let heading = note.split(" | ").next().unwrap_or("").to_string();
                let s = headings(d.path.as_deref().unwrap_or("")).into_iter().find(|h| h.2 == heading).map(|h| h.3).unwrap_or_else(|| slug(&heading));
                let _ = write!(line, ", {n} in {}", fref(p, co, &format!("{relp}#{s}")));
            }
            let _ = writeln!(o, "{line}");
        }
        if docs.len() > lim.docs {
            more.push(format!("--docs {}", docs.len()));
        }
    }

    let top_of: BTreeMap<String, String> = p.checkouts.iter().map(|n| (n.id.clone(), n.path.clone().unwrap_or_default())).collect();
    let ranked = db(rank_files(c, std::slice::from_ref(&co.id), &top_of))?;
    let _ = writeln!(o, "\n## Key files ({} with evidence; why: doc links, entry points, commits, conversation writes/reads)", ranked.len());
    for r in ranked.iter().take(lim.files) {
        let relp = node_rel(&r.node, &top);
        let mut why = Vec::new();
        if !on_disk(&r.node) {
            why.push("NOT ON DISK (history only)".to_string());
        }
        if let Some(e) = &r.entry {
            why.push(format!("entry point per {}", e.split(' ').next().unwrap_or(e)));
        }
        if !r.refs.is_empty() {
            why.push(format!("named in {}{}", r.refs[0], if r.refs.len() > 1 { format!(" +{}", r.refs.len() - 1) } else { String::new() }));
        }
        if r.commits > 0 {
            why.push(format!("{} commit(s), last {}", r.commits, day(r.last_commit)));
        }
        if !r.sessions.is_empty() {
            let w = r.sessions.iter().filter(|s| s.starts_with("write")).count();
            let rd = r.sessions.len() - w;
            why.push(format!("conversations {}", [(w, "w"), (rd, "r")].iter().filter(|x| x.0 > 0).map(|x| format!("{}{}", x.1, x.0)).collect::<Vec<_>>().join(" ")));
        }
        let _ = writeln!(o, "- {} — {}", fref(p, co, &relp), why.join("; "));
    }
    if ranked.len() > lim.files {
        more.push(format!("--files {}", (lim.files * 3).min(ranked.len())));
    }

    let commits = db(nodes(c, "kind='commit' AND id IN (SELECT dst FROM edges WHERE src=?1 AND kind='history') ORDER BY ts DESC, id", &[&co.id]))?;
    let _ = writeln!(o, "\n## Recent commits ({} indexed on {})", commits.len(), branch(co));
    for n in commits.iter().take(lim.commits) {
        let subject = n.label.split_once(' ').map(|x| x.1).unwrap_or(&n.label);
        let nf = n.meta.get("files").and_then(|x| x.as_u64()).unwrap_or(0);
        let _ = writeln!(o, "- {} {} \"{}\" ({nf} file{})", commit_ref(meta(n, "sha")), day(n.ts), clip(subject, 70), if nf == 1 { "" } else { "s" });
    }
    if commits.len() > lim.commits {
        more.push(format!("--commits {}", (lim.commits * 3).min(commits.len())));
    }

    let all = sessions_of(c, p)?;
    let mut active: Vec<(bool, &Node, Access)> = Vec::new();
    for s in &all {
        let a = access_counts(c, &s.id)?;
        active.push((a.r.0 + a.r.1 + a.w.0 + a.w.1 > 0, s, a));
    }
    active.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.ts.cmp(&a.1.ts)).then(a.1.id.cmp(&b.1.id)));
    let n_active = active.iter().filter(|x| x.0).count();
    let _ = writeln!(o, "\n## Conversations ({}; {} with file reads/writes first) — expand REF reads it, ask REF 'question' continues it read-only", all.len(), n_active);
    if all.is_empty() {
        let _ = writeln!(o, "- none found in the indexed Claude Code / Codex / OpenCode stores");
    }
    for (_, s, a) in active.iter().take(lim.sessions) {
        let wt = co_of(p, s.checkout_id.as_deref()).map(|x| worktree_name(p, x)).unwrap_or_else(|| "?".into());
        let _ = writeln!(o, "- {} {} {}@{wt} {} — \"{}\"", conv_ref(&all, s), day(s.ts), meta(s, "provider"), codes(a), clip(&s.label, 60));
    }
    if all.len() > lim.sessions {
        more.push(format!("--sessions {}", (lim.sessions * 3).min(all.len())));
    }
    o.push_str(&more::related_section(v, p, lim.related)?);
    if !more.is_empty() {
        let _ = writeln!(o, "\nMore: {} context {}", v.cmd, more.join(" "));
    }
    Ok(o)
}

// ------------------------------------------------------------------ file / doc / section

fn find_file(c: &Connection, p: &Project, co: &Node, relp: &str) -> R<Option<Node>> {
    let abs = format!("{}/{}", p.top(co), relp.trim_start_matches("./"));
    let n = db(nodes(c, "path=?1 AND kind IN ('doc','file','memory') ORDER BY CASE kind WHEN 'doc' THEN 0 WHEN 'memory' THEN 1 ELSE 2 END", &[&abs]))?;
    if let Some(x) = n.into_iter().next() {
        return Ok(Some(x));
    }
    Ok(db(nodes(c, "kind='memory' AND checkout_id=?1 AND label=?2", &[&co.id, &relp]))?.into_iter().next())
}

pub struct FileLimits {
    pub per_category: usize,
}

/// Raw doc token stored on a `reference_candidate` edge note.
fn candidate_token(note: &str) -> Option<&str> {
    note.split("token `").nth(1)?.strip_suffix('`')
}

/// Reference for a file node that may live in another worktree or project.
fn file_ref(v: &View, p: &Project, coid: Option<&str>, path: Option<&str>, memory_label: Option<&str>) -> Option<String> {
    let path = path.unwrap_or("");
    if let Some(co) = co_of(p, coid) {
        let r = memory_label.map(str::to_string).unwrap_or_else(|| rel(path, co.path.as_deref().unwrap_or("")).to_string());
        return Some(fref(p, co, &r));
    }
    let co = get_node(v.c, coid?).ok()??;
    let repo = get_node(v.c, co.repo_id.as_deref()?).ok()??;
    let other = build_project(v.c, repo, Some(co.clone())).ok()?;
    let r = memory_label.map(str::to_string).unwrap_or_else(|| rel(path, co.path.as_deref().unwrap_or("")).to_string());
    Some(format!("{}{} (other project)", q(&format!("{}:{r}", other.handle)), if other.is_default(&co) { String::new() } else { format!(" --worktree {}", q(&worktree_name(&other, &co))) }))
}

fn more_line(o: &mut String, v: &View, this: &str, shown: usize, total: usize) {
    if total > shown {
        let _ = writeln!(o, "  … {} more (expand {this} --limit {total})", total - shown);
    }
    let _ = v;
}

pub fn file_view(v: &View, p: &Project, relp: &str, section: Option<&str>, lim: &FileLimits) -> R<String> {
    let c = v.c;
    let co = &p.focus;
    let top = p.top(co);
    let n = match find_file(c, p, co, relp)? {
        Some(n) => n,
        None => {
            let exists = Path::new(&format!("{top}/{relp}")).exists();
            return if exists {
                Ok(format!(
                    "# {} — in {} @{}\nexists on disk but has no recorded relationships (not linked from docs, not in recent commits, not touched by an indexed conversation)\n",
                    fref(p, co, relp),
                    p.handle,
                    branch(co)
                ))
            } else {
                Err(format!("{relp} is neither indexed nor on disk in worktree {} ({top}){}", branch(co), if p.checkouts.len() > 1 { "; try another --worktree" } else { "" }))
            };
        }
    };
    let path = n.path.clone().unwrap_or_default();
    let this = fref(p, co, relp);
    let mut o = String::new();
    let state = if on_disk(&n) { "" } else { " — NOT ON DISK now; evidence below is historical" };
    let hs = if n.kind != "file" || relp.ends_with(".md") { headings(&path) } else { vec![] };

    if let Some(sl) = section {
        let i = hs.iter().position(|h| h.3 == sl).ok_or_else(|| {
            let mut m = format!("no section #{sl} in {relp}; sections:");
            for h in hs.iter().take(40) {
                let _ = write!(m, "\n  {}  ({})", fref(p, co, &format!("{relp}#{}", h.3)), h.2);
            }
            m
        })?;
        let (start, level, title, _) = hs[i].clone();
        let end = hs[i + 1..].iter().find(|h| h.1 <= level).map(|h| h.0 - 1).unwrap_or(usize::MAX);
        let bytes = crate::util::read_head(Path::new(&path), 256 << 10).map_err(|e| format!("{path}: {e}"))?;
        let text = String::from_utf8_lossy(&bytes);
        let _ = writeln!(o, "# {} — \"{title}\", lines {start}-{} of {path}{state}", fref(p, co, &format!("{relp}#{sl}")), if end == usize::MAX { "end".into() } else { end.to_string() });
        o.push_str(&header(v, p, false));
        let mut bytes_out = 0usize;
        for (shown, (k, l)) in text.lines().enumerate().skip(start - 1).enumerate() {
            let ln = k + 1;
            if ln > end {
                break;
            }
            if shown >= 60 || bytes_out > 4096 {
                let _ = writeln!(o, "… section continues to line {}; open {path}", if end == usize::MAX { "end".into() } else { end.to_string() });
                break;
            }
            let l = crate::util::redact(l);
            bytes_out += l.len();
            let _ = writeln!(o, "{ln:>4} | {l}");
        }
        let mut st = db(c.prepare("SELECT e.source_line, n.path, e.kind, e.note FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 AND e.kind IN ('references','reference_candidate') AND e.source_line BETWEEN ?2 AND ?3 ORDER BY e.kind DESC, e.source_line, n.path"))?;
        let refs: Vec<(i64, Option<String>, String, String)> =
            db(db(st.query_map(params![n.id, start as i64, end.min(i64::MAX as usize) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))))?.collect::<Result<_, _>>())?;
        let strong: Vec<_> = refs.iter().filter(|r| r.2 == "references").collect();
        let weak: Vec<_> = refs.iter().filter(|r| r.2 != "references").collect();
        let mut seen = std::collections::HashSet::new();
        let links: Vec<String> = strong
            .iter()
            .filter_map(|(line, tp, _, _)| {
                let tr = rel(tp.as_deref().unwrap_or(""), &top).to_string();
                seen.insert(tr.clone()).then(|| format!("{} l.{line}", fref(p, co, &tr)))
            })
            .collect();
        let _ = writeln!(o, "\nLinks ({}): {}", links.len(), if links.is_empty() { "none".into() } else { links.join(", ") });
        if !weak.is_empty() {
            let w: Vec<String> =
                weak.iter().map(|(line, tp, _, note)| format!("l.{line} `{}` → maybe {}", candidate_token(note).unwrap_or("?"), fref(p, co, rel(tp.as_deref().unwrap_or(""), &top)))).collect();
            let _ = writeln!(o, "Unverified name matches (bare filename, unique in repo; may be a different file): {}", w.join(", "));
        }
        let _ = writeln!(o, "Whole document: {}", fref(p, co, relp));
        return Ok(o);
    }

    let _ = writeln!(o, "# {} — {} in {} @{}{state}\n{path}", this, if n.kind == "file" { "file" } else { "document" }, p.handle, branch(co));
    o.push_str(&header(v, p, true));

    // Conversations first: they are what is easily lost under commit noise.
    let all = sessions_of(c, p)?;
    let mut st = db(c.prepare(
        "SELECT e.src, e.kind, e.strength, e.source_line, e.note FROM edges e JOIN nodes s ON s.id=e.src
         WHERE e.dst=?1 AND s.kind='session' AND e.kind IN ('read','write','mentioned') ORDER BY s.ts DESC, e.kind DESC, e.source_line",
    ))?;
    let rows: Vec<(String, String, String, i64, String)> = db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))))?.collect::<Result<_, _>>())?;
    let mut by_session: Vec<(String, Vec<String>, bool)> = Vec::new();
    for (sid, kind, strength, line, note) in rows {
        let tool = note.split(" @").next().unwrap_or(&note).to_string();
        let ev = format!("{} {tool}{}", edge_code(&kind, &strength), if line > 0 { format!(" l.{line}") } else { String::new() });
        let structured = kind != "mentioned";
        match by_session.iter_mut().find(|x| x.0 == sid) {
            Some(x) => {
                x.1.push(ev);
                x.2 |= structured;
            }
            None => by_session.push((sid, vec![ev], structured)),
        }
    }
    by_session.sort_by_key(|x| !x.2);
    let n_struct = by_session.iter().filter(|x| x.2).count();
    let _ = writeln!(o, "\n## Conversations ({} read/wrote it, {} only named it in shell) — evidence: code tool transcript-line", n_struct, by_session.len() - n_struct);
    if by_session.is_empty() {
        let _ = writeln!(o, "- none recorded");
    }
    for (sid, evs, _) in by_session.iter().take(lim.per_category) {
        let Some(s) = all.iter().find(|x| &x.id == sid) else { continue };
        let wt = co_of(p, s.checkout_id.as_deref()).map(|x| worktree_name(p, x)).unwrap_or_else(|| "?".into());
        let _ = writeln!(o, "- {} {} {}@{wt}: {} — \"{}\"", conv_ref(&all, s), day(s.ts), meta(s, "provider"), evs.iter().take(3).cloned().collect::<Vec<_>>().join("; "), clip(&s.label, 50));
    }
    more_line(&mut o, v, &this, lim.per_category.min(by_session.len()), by_session.len());

    let mut hcache: HashMap<String, Vec<(usize, usize, String, String)>> = HashMap::new();
    for (kind, title) in [("references", "Docs naming it (exact path)"), ("reference_candidate", "Unverified mentions (bare filename matched only because it is unique; may be a different file)")] {
        let mut st = db(c.prepare("SELECT d.path, d.label, d.kind, e.source_line, e.note FROM edges e JOIN nodes d ON d.id=e.src WHERE e.dst=?1 AND e.kind=?2 ORDER BY d.label, e.source_line"))?;
        let refs: Vec<(Option<String>, String, String, i64, String)> =
            db(db(st.query_map(params![n.id, kind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))))?.collect::<Result<_, _>>())?;
        if refs.is_empty() && kind == "reference_candidate" {
            continue;
        }
        let _ = writeln!(o, "\n## {title} ({})", refs.len());
        for (dpath, dlabel, dkind, line, note) in refs.iter().take(lim.per_category) {
            let dp = dpath.clone().unwrap_or_default();
            let drel = if dkind == "memory" { dlabel.clone() } else { rel(&dp, &top).to_string() };
            let hs = hcache.entry(dp.clone()).or_insert_with(|| headings(&dp));
            let target = match section_of_line(hs, *line as usize) {
                Some(h) => format!("{drel}#{}", h.3),
                None => drel.clone(),
            };
            let token = candidate_token(note).map(|t| format!(" `{t}`")).unwrap_or_default();
            let _ = writeln!(o, "- {} l.{line}{token}: \"{}\"", fref(p, co, &target), line_text(&dp, *line as usize, 100).unwrap_or_default());
        }
        more_line(&mut o, v, &this, lim.per_category.min(refs.len()), refs.len());
    }

    if !hs.is_empty() {
        let secs: Vec<String> = hs.iter().take(lim.per_category * 2).map(|h| format!("#{}", h.3)).collect();
        let _ = writeln!(o, "\n## Sections ({}; append the #slug to this file's ref): {}{}", hs.len(), secs.join(" "), if hs.len() > secs.len() { " …" } else { "" });
    }
    let mut st = db(c.prepare("SELECT DISTINCT n.path FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 AND e.kind='references' ORDER BY n.path"))?;
    let outs: Vec<Option<String>> = db(db(st.query_map([&n.id], |r| r.get(0)))?.collect::<Result<_, _>>())?;
    if !outs.is_empty() {
        let l: Vec<String> = outs.iter().take(lim.per_category * 2).map(|tp| fref(p, co, rel(tp.as_deref().unwrap_or(""), &top))).collect();
        let _ = writeln!(o, "\n## Links to ({}): {}{}", outs.len(), l.join(", "), if outs.len() > l.len() { ", …" } else { "" });
    }
    let mut st = db(c.prepare("SELECT n.path, e.source_line, e.note FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 AND e.kind='reference_candidate' ORDER BY e.source_line"))?;
    let couts: Vec<(Option<String>, i64, String)> = db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect::<Result<_, _>>())?;
    if !couts.is_empty() {
        let l: Vec<String> = couts
            .iter()
            .take(lim.per_category)
            .map(|(tp, line, note)| format!("l.{line} `{}` → maybe {}", candidate_token(note).unwrap_or("?"), fref(p, co, rel(tp.as_deref().unwrap_or(""), &top))))
            .collect();
        let _ = writeln!(o, "\n## Unverified name matches ({}; not counted as links): {}{}", couts.len(), l.join(", "), if couts.len() > l.len() { ", …" } else { "" });
    }
    let mut st = db(c.prepare("SELECT source_path, source_line, note FROM edges WHERE dst=?1 AND kind='entrypoint'"))?;
    let eps: Vec<(String, i64, String)> = db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect::<Result<_, _>>())?;
    for (sp, line, note) in eps {
        let _ = writeln!(o, "\nEntry point: declared by {}:{line} ({note})", rel(&sp, &top));
    }

    let mut st = db(c.prepare("SELECT n.id, e.note FROM edges e JOIN nodes n ON n.id=e.src WHERE e.dst=?1 AND e.kind='touched' ORDER BY n.ts DESC"))?;
    let cms: Vec<(String, String)> = db(db(st.query_map([&n.id], |r| Ok((r.get(0)?, r.get(1)?))))?.collect::<Result<_, _>>())?;
    let _ = writeln!(o, "\n## Commits changing it ({} in indexed history)", cms.len());
    for (cid, status) in cms.iter().take(lim.per_category) {
        let Some(cm) = db(get_node(c, cid))? else { continue };
        let subject = cm.label.split_once(' ').map(|x| x.1).unwrap_or(&cm.label);
        let _ = writeln!(o, "- {} {} {status} \"{}\"", commit_ref(meta(&cm, "sha")), day(cm.ts), clip(subject, 70));
    }
    more_line(&mut o, v, &this, lim.per_category.min(cms.len()), cms.len());
    Ok(o)
}

// ------------------------------------------------------------------ commit

pub fn commit_view(v: &View, p: &Project, prefix: &str, lim: &FileLimits) -> R<String> {
    let c = v.c;
    if prefix.len() < 4 || !prefix.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!("commit prefix {} must be at least 4 hex characters", sh_quote(prefix)));
    }
    let like = format!("{}%", prefix.to_lowercase());
    let hits = db(nodes(c, "kind='commit' AND repo_id=?1 AND json_extract(meta,'$.sha') LIKE ?2 ORDER BY ts DESC", &[&p.repo.id, &like]))?;
    let cm = match hits.len() {
        1 => hits[0].clone(),
        0 => return Err(format!("no indexed commit {prefix} in {} (only the latest commits per worktree are indexed)", p.handle)),
        _ => {
            let mut m = format!("commit prefix {prefix} is ambiguous in {}:", p.handle);
            for h in hits.iter().take(10) {
                let _ = write!(m, "\n  :commit:{}  {} \"{}\"", &meta(h, "sha")[..12], day(h.ts), clip(&h.label, 60));
            }
            return Err(m);
        }
    };
    let sha = meta(&cm, "sha").to_string();
    let subject = cm.label.split_once(' ').map(|x| x.1).unwrap_or(&cm.label).to_string();
    let this = commit_ref(&sha);
    let mut o = String::new();
    let _ = writeln!(o, "# {this} — \"{subject}\" in {}", p.handle);
    let mut st = db(c.prepare("SELECT src FROM edges WHERE dst=?1 AND kind='history'"))?;
    let cos: Vec<String> = db(db(st.query_map([&cm.id], |r| r.get(0)))?.collect::<Result<_, _>>())?;
    let names: Vec<String> = cos.iter().filter_map(|id| co_of(p, Some(id))).map(|x| worktree_name(p, x)).collect();
    let _ = writeln!(o, "{sha} · {} · {} · in worktree(s): {}", meta(&cm, "author"), fmt_ts(cm.ts), if names.is_empty() { "none".into() } else { names.join(", ") });
    o.push_str(&header(v, p, false));
    let mut st = db(c.prepare("SELECT n.path, n.checkout_id, e.note FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 AND e.kind='touched' ORDER BY n.path"))?;
    let files: Vec<(Option<String>, Option<String>, String)> = db(db(st.query_map([&cm.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect::<Result<_, _>>())?;
    let mut seen = std::collections::HashSet::new();
    let uniq: Vec<_> = files
        .into_iter()
        .filter(|f| {
            let co = co_of(p, f.1.as_deref());
            let r = co.map(|x| rel(f.0.as_deref().unwrap_or(""), x.path.as_deref().unwrap_or("")).to_string()).unwrap_or_default();
            seen.insert(r)
        })
        .collect();
    let capped = cm.meta.get("files").and_then(|x| x.as_u64()).map(|n| n as usize > uniq.len()).unwrap_or(false);
    let _ = writeln!(o, "\n## Files changed ({}{}; A added, M modified, D deleted)", uniq.len(), if capped { ", list capped at index time" } else { "" });
    let shown = (lim.per_category * 3).min(uniq.len());
    for (path, coid, status) in uniq.iter().take(shown) {
        let Some(r) = file_ref(v, p, coid.as_deref(), path.as_deref(), None) else { continue };
        let _ = writeln!(o, "- {status} {r}");
    }
    if uniq.len() > shown {
        let _ = writeln!(o, "  … {} more (expand {this} --limit {})", uniq.len() - shown, uniq.len().div_ceil(3));
    }
    Ok(o)
}

// ------------------------------------------------------------------ conversation

pub fn conversation_view(v: &View, p: &Project, s: &Node, lim: &FileLimits) -> R<String> {
    let c = v.c;
    let all = sessions_of(c, p)?;
    let this = conv_ref(&all, s);
    let wt = co_of(p, s.checkout_id.as_deref());
    let mut o = String::new();
    let _ = writeln!(o, "# {this} — {} conversation in {} @{}", meta(s, "provider"), p.handle, wt.map(|x| worktree_name(p, x)).unwrap_or_else(|| "?".into()));
    let _ = writeln!(o, "title \"{}\"", clip(&s.label, 120));
    let started = s.meta.get("started").and_then(|x| x.as_i64()).unwrap_or(0);
    let _ = writeln!(o, "session {} · {} → {} · cwd {}", meta(s, "native_id"), fmt_ts(started), fmt_ts(s.ts), meta(s, "cwd"));
    let _ = writeln!(o, "transcript {}{}", s.path.clone().unwrap_or_default(), if s.meta.get("source_missing").is_some() { " (source file no longer exists)" } else { "" });
    for (k, label) in [("first_prompt", "first user input"), ("last_prompt", "latest user input (as typed; not a decision record)")] {
        if let Some(pr) = s.meta.get(k).filter(|x| !x.is_null()) {
            if k == "last_prompt" && s.meta.get("first_prompt") == Some(pr) {
                continue;
            }
            let _ = writeln!(
                o,
                "{label} {} l.{}: \"{}\"",
                fmt_ts(pr.get("ts").and_then(|x| x.as_i64()).unwrap_or(0)),
                pr.get("line").and_then(|x| x.as_i64()).unwrap_or(0),
                clip(pr.get("text").and_then(|x| x.as_str()).unwrap_or(""), 160)
            );
        }
    }
    let _ = writeln!(o, "access: {}", codes(&access_counts(c, &s.id)?));
    let nmsg = s.meta.get("messages").and_then(|x| x.as_i64()).unwrap_or(0);
    let mut flags = Vec::new();
    for (k, t) in [("truncated", "transcript truncated at the size cap"), ("messages_capped", "message text capped"), ("source_missing", "source transcript missing")] {
        if s.meta.get(k).and_then(|x| x.as_bool()) == Some(true) {
            flags.push(t);
        }
    }
    if s.meta.get("warnings").and_then(|x| x.as_i64()).unwrap_or(0) > 0 {
        flags.push("some transcript lines unparseable");
    }
    let _ = writeln!(o, "coverage: {nmsg} searchable message(s){}", if flags.is_empty() { String::new() } else { format!("; {}", flags.join(", ")) });
    let all_s = sessions_of(c, p)?;
    let pre = more::session_prefix(&all_s, s);
    if nmsg > 0 {
        let mut st = db(c.prepare("SELECT loc, speaker FROM passages WHERE kind='message' AND node_id=?1 AND part=0 ORDER BY CAST(substr(loc,2) AS INTEGER) LIMIT 3"))?;
        let first: Vec<(String, String)> = db(db(st.query_map([&s.id], |r| Ok((r.get(0)?, r.get(1)?))))?.collect::<Result<_, _>>())?;
        let l: Vec<String> = first.iter().map(|(loc, sp)| format!("{} ({sp})", q(&format!(":message:{pre}:{loc}")))).collect();
        let _ = writeln!(o, "messages: {} … (search inside: search 'words' --kind message)", l.join(", "));
    }
    o.push_str(&header(v, p, true));
    let groups = [
        ("w — written", "write", "observed"),
        ("r — read", "read", "observed"),
        ("w-req — write requested, no result", "write", "requested"),
        ("r-req — read requested, no result", "read", "requested"),
        ("w-fail — write failed", "write", "failed"),
        ("r-fail — read failed", "read", "failed"),
        ("sh — named in shell commands (not access)", "mentioned", "mentioned"),
    ];
    for (title, kind, strength) in groups {
        let mut st = db(c.prepare("SELECT n.path, n.checkout_id, n.kind, n.label, e.source_line, e.note FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 AND e.kind=?2 AND e.strength=?3 ORDER BY e.source_line, n.path"))?;
        let rows: Vec<(Option<String>, Option<String>, String, String, i64, String)> =
            db(db(st.query_map(params![s.id, kind, strength], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))))?.collect::<Result<_, _>>())?;
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n## {title} ({})", rows.len());
        for (path, coid, nk, nl, line, note) in rows.iter().take(lim.per_category) {
            let mem = if nk == "memory" { Some(nl.as_str()) } else { None };
            let Some(r) = file_ref(v, p, coid.as_deref(), path.as_deref(), mem) else { continue };
            let tool = clip(note.split(" @").next().unwrap_or(note), 30);
            let _ = writeln!(o, "- {r}{}{}", if *line > 0 { format!(" l.{line}") } else { String::new() }, if tool.is_empty() { String::new() } else { format!(" {tool}") });
        }
        more_line(&mut o, v, &this, lim.per_category.min(rows.len()), rows.len());
    }
    if meta(s, "provider").starts_with("opencode") {
        let _ = writeln!(o, "\nAsk: not available for OpenCode (read-only cannot be enforced; ask fails closed)");
    } else {
        let _ = writeln!(o, "\nAsk (read-only resume of this session): {} ask {this} 'question' --dry-run", v.cmd);
    }
    Ok(o)
}

// ------------------------------------------------------------------ dispatch

#[derive(Debug)]
pub enum Target {
    Project,
    File(String, Option<String>),
    Commit(String),
    Conversation(String),
    Session(String),
    /// `message:<session-id-prefix>:<l<line>|p<ordinal>>`
    Message(String, String),
    /// `related:<other project>`: all relation evidence between the two.
    Related(String),
}

/// What follows `<project>:` (or a leading `:` under a scope).
pub fn parse_rest(rest: &str) -> Target {
    if rest.is_empty() {
        return Target::Project;
    }
    if let Some(x) = rest.strip_prefix("commit:") {
        return Target::Commit(x.to_string());
    }
    if let Some(x) = rest.strip_prefix("conversation:") {
        return Target::Conversation(x.to_string());
    }
    if let Some(x) = rest.strip_prefix("session:") {
        return Target::Session(x.to_string());
    }
    if let Some(x) = rest.strip_prefix("message:") {
        let (sid, loc) = x.rsplit_once(':').unwrap_or((x, ""));
        return Target::Message(sid.to_string(), loc.to_string());
    }
    if let Some(x) = rest.strip_prefix("related:") {
        return Target::Related(x.to_string());
    }
    match rest.rsplit_once('#') {
        Some((f, sl)) if !sl.is_empty() && !f.is_empty() => Target::File(f.to_string(), Some(sl.to_string())),
        _ => Target::File(rest.trim_end_matches('#').to_string(), None),
    }
}

/// Split `<project>[:<rest>]`. Paths starting with `/` are handled by the caller.
pub fn parse_target(s: &str) -> (String, Target) {
    match s.split_once(':') {
        None => (s.to_string(), Target::Project),
        Some((proj, rest)) => (proj.to_string(), parse_rest(rest)),
    }
}

/// Absolute path to an indexed file: return (project with its checkout focused, relative path).
pub fn project_for_path(c: &Connection, path: &str) -> R<Option<(Project, String)>> {
    let Some(n) = db(crate::overview::resolve_target(c, path))? else { return Ok(None) };
    if !matches!(n.kind.as_str(), "file" | "doc" | "memory") {
        return Ok(None);
    }
    let Some(co) = n.checkout_id.as_deref().and_then(|id| get_node(c, id).ok().flatten()) else { return Ok(None) };
    let repo = db(get_node(c, co.repo_id.as_deref().unwrap_or("")))?.ok_or("file without repository")?;
    let top = co.path.clone().unwrap_or_default();
    let r = node_rel(&n, &top);
    Ok(Some((build_project(c, repo, Some(co))?, r)))
}

/// Options shared by `context`, `expand` and `ask`.
#[derive(Default)]
pub struct Req {
    /// Scope for relative `:` references (`--project`, else `WORKMAP_PROJECT`).
    pub project: Option<String>,
    /// `--worktree` (applies to any reference).
    pub worktree: Option<String>,
    /// `WORKMAP_WORKTREE` (applies only to the scope project).
    pub scope_worktree: Option<String>,
    pub conv: ConvSel,
    pub limit: Option<usize>,
    pub map: MapLimits,
}

impl Req {
    /// Combine flags and environment: flags win; env worktree only follows the env/flag scope.
    pub fn scoped(mut self, flag_project: Option<String>, env_project: Option<String>, env_worktree: Option<String>) -> Req {
        self.project = flag_project.or(env_project).filter(|s| !s.is_empty());
        self.scope_worktree = env_worktree.filter(|s| !s.is_empty());
        self
    }
}

pub const SCOPE_HELP: &str = "relative reference needs a project scope: add --project <name> or export WORKMAP_PROJECT=<name> (names: workmap overview)";

fn scope_project(v: &View, req: &Req) -> R<Project> {
    let name = req.project.as_deref().ok_or(SCOPE_HELP)?;
    let mut p = resolve_project(v.c, name)?;
    let wt = req.worktree.as_deref().or(req.scope_worktree.as_deref());
    select_worktree(&mut p, wt).map_err(|e| if req.worktree.is_none() { format!("WORKMAP_WORKTREE: {e}") } else { e })?;
    Ok(p)
}

fn locate(v: &View, target: &str, req: &Req) -> R<(Project, Target)> {
    if target.is_empty() {
        return Ok((scope_project(v, req)?, Target::Project));
    }
    if let Some(rest) = target.strip_prefix(':') {
        return Ok((scope_project(v, req)?, parse_rest(rest)));
    }
    if target.starts_with('/') || target.starts_with('.') || target.starts_with('~') {
        if let Some((mut p, r)) = project_for_path(v.c, target)? {
            if req.worktree.is_some() {
                select_worktree(&mut p, req.worktree.as_deref())?;
            }
            return Ok((p, Target::File(r, None)));
        }
        if let Ok(mut p) = resolve_project(v.c, target) {
            let top = p.top(&p.focus);
            let abs = std::fs::canonicalize(target).map(|x| x.to_string_lossy().into_owned()).unwrap_or_else(|_| target.to_string());
            let r = rel(&abs, &top).to_string();
            select_worktree(&mut p, req.worktree.as_deref())?;
            return Ok((p, if abs == top { Target::Project } else { Target::File(r, None) }));
        }
        // `<checkout-path>:<rest>` for projects named by path.
        if let Some(i) = target.find(':') {
            let mut p = resolve_project(v.c, &target[..i])?;
            select_worktree(&mut p, req.worktree.as_deref())?;
            return Ok((p, parse_rest(&target[i + 1..])));
        }
        return Err(format!("{target} is not inside an indexed checkout"));
    }
    let (proj, t) = parse_target(target);
    let mut p = resolve_project(v.c, &proj)?;
    select_worktree(&mut p, req.worktree.as_deref())?;
    Ok((p, t))
}

fn conv_from(t: &Target, req: &Req) -> Option<ConvSel> {
    let mut sel = req.conv.clone();
    match t {
        Target::Conversation(x) => sel.title = Some(x.clone()),
        Target::Session(x) => sel.session = Some(x.clone()),
        Target::Project if sel.title.is_some() || sel.session.is_some() => {}
        _ => return None,
    }
    Some(sel)
}

pub fn context(v: &View, target: &str, req: &Req) -> R<String> {
    let (p, t) = locate(v, target, req)?;
    match t {
        Target::Project => project_map(v, &p, &req.map),
        _ => Err(format!("`context` takes a project; for {target} use: {} expand {}", v.cmd, q(target))),
    }
}

pub fn expand(v: &View, target: &str, req: &Req) -> R<String> {
    let lim = FileLimits { per_category: req.limit.unwrap_or(8) };
    // Backward compatibility: internal ids still expand to the raw evidence view.
    if !target.contains(':') && !target.starts_with('/') && !target.is_empty() && db(get_node(v.c, target))?.map(|n| n.kind != "repo" && n.kind != "checkout").unwrap_or(false) {
        return db(crate::overview::expand(v.c, target, req.limit.unwrap_or(40)));
    }
    if let Some(id) = target.strip_prefix("remote:") {
        if let Some(out) = more::remote_view(v, id)? {
            return Ok(out);
        }
    }
    let (p, t) = locate(v, target, req)?;
    if let Some(sel) = conv_from(&t, req) {
        let s = resolve_conversation(v, &p, &sel)?;
        return conversation_view(v, &p, &s, &lim);
    }
    match t {
        Target::Project => project_map(v, &p, &req.map),
        Target::File(f, sl) => file_view(v, &p, &f, sl.as_deref(), &lim),
        Target::Commit(sha) => commit_view(v, &p, &sha, &lim),
        Target::Message(sid, loc) => more::message_view(v, &p, &sid, &loc),
        Target::Related(other) => more::relation_view(v, &p, &other, req.limit.unwrap_or(20)),
        Target::Conversation(_) | Target::Session(_) => unreachable!(),
    }
}

/// Project named by a target (or the scope when empty) for project-level commands.
pub fn project_arg(v: &View, target: Option<&str>, req: &Req) -> R<Project> {
    let (p, t) = locate(v, target.unwrap_or(""), req)?;
    match t {
        Target::Project => Ok(p),
        _ => Err(format!("{} names something inside a project; give the project name", target.unwrap_or(""))),
    }
}

/// `context --task … --file …`: ranked, budget-bounded context.
pub fn task(v: &View, target: &str, req: &Req, t: &more::TaskReq) -> R<String> {
    let p = project_arg(v, Some(target), req)?;
    more::task_context(v, &p, t)
}

/// Session node id for `ask REF` / `ask <project> --conversation …`.
pub fn ask_session(v: &View, target: &str, req: &Req) -> R<String> {
    let (p, t) = locate(v, target, req)?;
    let sel = conv_from(&t, req).ok_or("ask needs a conversation: :conversation:<title>, :session:<id-prefix>, or <project> --conversation <title>")?;
    Ok(resolve_conversation(v, &p, &sel)?.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse() {
        assert!(matches!(parse_target("p"), (ref a, Target::Project) if a == "p"));
        assert!(matches!(parse_target("p:commit:abcd"), (_, Target::Commit(ref s)) if s == "abcd"));
        assert!(matches!(parse_target("p:docs/a b.md#key-files"), (_, Target::File(ref f, Some(ref s))) if f == "docs/a b.md" && s == "key-files"));
        assert!(matches!(parse_target("p:README.md"), (_, Target::File(ref f, None)) if f == "README.md"));
        assert!(matches!(parse_rest("conversation:Fix: the build #2"), Target::Conversation(ref t) if t == "Fix: the build #2"));
        assert!(matches!(parse_rest("session:01a0fe62"), Target::Session(ref t) if t == "01a0fe62"));
        assert!(matches!(parse_rest("dir with space/a:b.md"), Target::File(ref f, None) if f == "dir with space/a:b.md"));
        assert!(matches!(parse_rest(""), Target::Project));
    }

    #[test]
    fn slugs_and_quotes() {
        assert_eq!(slug("Plugin Won't Install"), "plugin-wont-install");
        assert_eq!(slug("What the script does:"), "what-the-script-does");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("p:README.md#x"), "p:README.md#x");
        assert_eq!(sh_quote(":commit:1f58a6ad"), ":commit:1f58a6ad");
        assert_eq!(sh_quote("a b"), "'a b'");
    }

    #[test]
    fn scope_flags_beat_env() {
        let r = Req::default().scoped(Some("a".into()), Some("b".into()), Some("wt".into()));
        assert_eq!(r.project.as_deref(), Some("a"));
        let r = Req::default().scoped(None, Some("b".into()), None);
        assert_eq!(r.project.as_deref(), Some("b"));
        let r = Req::default().scoped(None, Some(String::new()), None);
        assert!(r.project.is_none());
    }
}
