//! Host hook adapter: reads one documented hook event (JSON on stdin) and prints
//! bounded, fact-based context or nothing. Never calls the network or a model.
//!
//! Contract (verified 2026-10 against the primary docs and the installed CLIs):
//! - Claude Code 2.1.x, <https://code.claude.com/docs/en/hooks>: command hooks get one
//!   JSON object on stdin with `session_id`, `transcript_path`, `cwd`,
//!   `hook_event_name` (+ `source` for SessionStart, `prompt` for UserPromptSubmit,
//!   `reason` for SessionEnd). On exit 0, "JSON with `additionalContext`" under
//!   `hookSpecificOutput` (which "requires a `hookEventName` field") is added to
//!   Claude's context for SessionStart and UserPromptSubmit; any other exit code
//!   is a visible "hook error", so this adapter always exits 0. Context strings
//!   are capped at 10,000 characters (larger output is replaced by a file path
//!   and a preview), hence the byte budget below. SessionEnd hooks get a 1.5 s
//!   budget and JSON output is discarded; the docs say work that must outlive the
//!   session should "start a fully detached process".
//! - Codex CLI 0.163 (`codex features list`: `hooks stable true`),
//!   <https://learn.chatgpt.com/docs/hooks>: same event names, stdin fields and
//!   `hookSpecificOutput.additionalContext` output; `transcript_path` may be null;
//!   SessionEnd default timeout 1 s. The installed binary embeds the same
//!   `SessionStartHookSpecificOutputWire {hookEventName, additionalContext}` schema.
//!
//! We print the JSON form (not plain stdout) because both hosts document it
//! identically for both context events.

use crate::config::Env;
use crate::overview::{nodes, rel, Node};
use crate::search;
use crate::util::{clip, fmt_date, sh_quote};
use crate::view::{self, Project};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest event JSON read from stdin.
pub const MAX_INPUT: u64 = 1 << 20;
/// Default `additionalContext` budget in bytes (`WORKMAP_HOOK_BUDGET`).
pub const DEFAULT_BUDGET: usize = 1500;
/// Ownership marker carried by every hook command workmap installs.
pub const MARKER: &str = "WORKMAP_HOOK=1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    Stop,
    SessionEnd,
}

impl Event {
    pub const ALL: [Event; 4] = [Event::SessionStart, Event::UserPromptSubmit, Event::Stop, Event::SessionEnd];
    pub fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::Stop => "Stop",
            Event::SessionEnd => "SessionEnd",
        }
    }
    pub fn parse(s: &str) -> Option<Event> {
        let k: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
        Event::ALL.into_iter().find(|e| e.name().to_lowercase() == k)
    }
}

pub const HARNESSES: &[&str] = &["claude", "codex"];

/// Per-invocation settings (resolved from the environment by [`main`]).
pub struct Ctx {
    /// Binary spawned for `ingest` (`WORKMAP_HOOK_SELF`, else this executable).
    pub self_exe: PathBuf,
    /// Byte budget for `additionalContext`.
    pub budget: usize,
    /// How printed follow-up commands invoke workmap.
    pub cmd: String,
}

/// What one event produced: stdout bytes and, for Stop/SessionEnd, the argv spawned.
#[derive(Default, Debug)]
pub struct Outcome {
    pub stdout: String,
    pub spawned: Option<Vec<String>>,
}

pub fn budget_from_env() -> usize {
    std::env::var("WORKMAP_HOOK_BUDGET").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_BUDGET).clamp(300, 9000)
}

/// CLI entry: never fails the host session. Reads stdin (bounded), prints the
/// result, logs internal errors to `<state>/hook.log`, always returns.
pub fn main(env: &Env, event: Option<&str>, harness: &str, cmd: String) {
    let mut input = Vec::new();
    let _ = std::io::stdin().take(MAX_INPUT).read_to_end(&mut input);
    let self_exe = std::env::var_os("WORKMAP_HOOK_SELF").map(PathBuf::from).or_else(|| std::env::current_exe().ok()).unwrap_or_else(|| PathBuf::from("workmap"));
    // Printed commands must reach the same index as this hook.
    let vars: Vec<String> = env
        .exports()
        .into_iter()
        .filter(|(k, v)| (k == "WORKMAP_HOME" || k == "WORKMAP_STATE_DIR") && !(k == "WORKMAP_STATE_DIR" && !env.overridden && Path::new(v) == env.home.join(".local/state/workmap")))
        .map(|(k, v)| sh_quote(&format!("{k}={v}")))
        .collect();
    let cmd = if vars.is_empty() { cmd } else { format!("env {} {cmd}", vars.join(" ")) };
    let out = respond(env, event, harness, &input, &Ctx { self_exe, budget: budget_from_env(), cmd });
    if !out.is_empty() {
        use std::io::Write;
        let mut so = std::io::stdout().lock();
        let _ = so.write_all(out.as_bytes());
        let _ = so.flush();
    }
}

/// Exactly what `workmap hook` prints for these stdin bytes: the JSON line, or
/// nothing (errors go to the log, never to the host).
pub fn respond(env: &Env, event: Option<&str>, harness: &str, input: &[u8], ctx: &Ctx) -> String {
    let res = match event {
        Some(e) => handle(env, e, harness, input, ctx),
        None => Err("usage: workmap hook <SessionStart|UserPromptSubmit|Stop|SessionEnd> --harness claude|codex (event JSON on stdin)".into()),
    };
    match res {
        Ok(o) => {
            if let Some(argv) = &o.spawned {
                log(env, &format!("spawned {}", argv.join(" ")));
            }
            o.stdout
        }
        Err(e) => {
            log(env, &format!("{} {}: {e}", event.unwrap_or("?"), harness));
            String::new()
        }
    }
}

fn log(env: &Env, msg: &str) {
    use std::io::Write;
    if std::fs::create_dir_all(&env.state_dir).is_err() {
        return;
    }
    let p = env.state_dir.join("hook.log");
    if std::fs::metadata(&p).map(|m| m.len() > 64 << 10).unwrap_or(false) {
        let _ = std::fs::rename(&p, env.state_dir.join("hook.log.old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{} {}", crate::util::now_unix(), clip(msg, 400));
    }
}

/// Open the existing index without creating, migrating or waiting long on it.
fn open_index(env: &Env) -> Option<Connection> {
    let p = env.db_path();
    if !p.is_file() {
        return None;
    }
    let c = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    let _ = c.busy_timeout(std::time::Duration::from_millis(250));
    Some(c)
}

/// One event, from raw stdin bytes. `Ok` with empty stdout means "nothing to add".
pub fn handle(env: &Env, event: &str, harness: &str, input: &[u8], ctx: &Ctx) -> Result<Outcome, String> {
    if !HARNESSES.contains(&harness) {
        return Err(format!("unsupported harness {harness:?} (supported: {})", HARNESSES.join(", ")));
    }
    let ev = Event::parse(event).ok_or_else(|| format!("unsupported event {event:?}"))?;
    let v: Value = serde_json::from_slice(input).map_err(|e| format!("event JSON: {e}"))?;
    if !v.is_object() {
        return Err("event JSON is not an object".into());
    }
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    if let Some(n) = v.get("hook_event_name").and_then(|x| x.as_str()) {
        if Event::parse(n) != Some(ev) {
            return Err(format!("stdin event {n:?} does not match argument {event:?}"));
        }
    }
    match ev {
        Event::Stop | Event::SessionEnd => Ok(Outcome { stdout: String::new(), spawned: spawn_ingest(env, &s("transcript_path"), ev == Event::Stop, ctx)? }),
        Event::SessionStart | Event::UserPromptSubmit => {
            let cwd = s("cwd");
            if !cwd.starts_with('/') {
                return Ok(Outcome::default());
            }
            let Some(c) = open_index(env) else { return Ok(Outcome::default()) };
            let Ok(p) = view::resolve_project(&c, &cwd) else { return Ok(Outcome::default()) };
            let text = if ev == Event::SessionStart { session_start(&c, &p, &s("session_id"), ctx)? } else { prompt_context(env, &c, &p, &s("session_id"), &s("prompt"), ctx)? };
            if text.is_empty() {
                return Ok(Outcome::default());
            }
            let out = json!({"hookSpecificOutput": {"hookEventName": ev.name(), "additionalContext": text}});
            Ok(Outcome { stdout: format!("{out}\n"), spawned: None })
        }
    }
}

// ------------------------------------------------------------------ Stop / SessionEnd

/// Start `workmap ingest --transcript <path> --quiet` fully detached (own process
/// group, null stdio, not waited for) and return its argv.
/// Stop fires after every turn; one ingest per transcript per this many seconds.
pub const STOP_THROTTLE_SECS: u64 = 20;

fn spawn_ingest(env: &Env, transcript: &str, throttle: bool, ctx: &Ctx) -> Result<Option<Vec<String>>, String> {
    if transcript.is_empty() {
        return Ok(None); // Codex documents `transcript_path: string | null`
    }
    let path = match transcript.strip_prefix("~/") {
        Some(r) => env.home.join(r),
        None => PathBuf::from(transcript),
    };
    if !path.is_absolute() || !path.is_file() {
        return Ok(None);
    }
    let mark = env.state_dir.join("hook-ingest").join(format!("{:016x}", crate::util::fnv64(&path.display().to_string())));
    let recent = std::fs::metadata(&mark).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|e| e.as_secs() < STOP_THROTTLE_SECS).unwrap_or(false);
    if throttle && recent {
        return Ok(None);
    }
    if let Some(d) = mark.parent() {
        let _ = std::fs::create_dir_all(d).and_then(|_| std::fs::write(&mark, b""));
    }
    let argv = vec!["ingest".to_string(), "--transcript".into(), path.display().to_string(), "--quiet".into()];
    use std::os::unix::process::CommandExt;
    let child = std::process::Command::new(&ctx.self_exe)
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", ctx.self_exe.display()))?;
    drop(child);
    let mut full = vec![ctx.self_exe.display().to_string()];
    full.extend(argv);
    Ok(Some(full))
}

// ------------------------------------------------------------------ SessionStart

fn meta<'a>(n: &'a Node, k: &str) -> &'a str {
    n.meta.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn repo_sessions(c: &Connection, repo: &str) -> Vec<Node> {
    nodes(c, "kind='session' AND repo_id=?1 ORDER BY ts DESC, id", &[&repo]).unwrap_or_default()
}

/// Fully qualified reference for a conversation, pinned to the checkout it ran in.
fn conv_ref(p: &Project, all: &[Node], s: &Node) -> String {
    let co = s.checkout_id.as_deref().and_then(|id| p.checkouts.iter().find(|c| c.id == id)).unwrap_or(&p.focus);
    let rest = view::conv_ref_raw(all, s);
    sh_quote(&view::qualified_raw(p, co, &rest[1..]))
}

fn files_note(s: &Node) -> String {
    let f = s.meta.get("files");
    let n = |k: &str| f.and_then(|f| f.get(k)).and_then(|x| x.as_i64()).unwrap_or(0);
    match (n("read"), n("write")) {
        (0, 0) => "no file access recorded".into(),
        (r, w) => format!("read {r}, wrote {w} file(s)"),
    }
}

/// Push `line` if it still fits the budget; returns false once full.
fn push_line(o: &mut String, line: &str, budget: usize) -> bool {
    if o.len() + line.len() + 1 > budget {
        return false;
    }
    o.push_str(line);
    o.push('\n');
    true
}

fn session_start(c: &Connection, p: &Project, session_id: &str, ctx: &Ctx) -> Result<String, String> {
    let co = &p.focus;
    let fresh = crate::trust::freshness(c);
    let age = fresh.last.map(|t| format!("last scan {} ago", crate::trust::age(crate::util::now_unix() - t).trim_end_matches(" ago"))).unwrap_or_else(|| "never scanned".into());
    let mut o = String::new();
    let _ = writeln!(o, "workmap (local index; references to recorded evidence, not decisions) · {age}");
    let _ = writeln!(o, "This checkout: project {} · worktree {} · branch {} · {}", p.handle, view::worktree_name(p, co), view::branch(co), p.top(co));
    let foot = format!("Expand any reference (no scope needed): {} expand '<ref>'\n", ctx.cmd);
    let budget = ctx.budget.saturating_sub(foot.len());
    let all = repo_sessions(c, &p.repo.id);
    let mine: Vec<&Node> = all.iter().filter(|s| s.checkout_id.as_deref() == Some(co.id.as_str()) && meta(s, "native_id") != session_id).take(5).collect();
    if !mine.is_empty() && push_line(&mut o, "Recent conversations in this checkout:", budget) {
        for s in mine {
            let line = format!("- {} — {} {} · {} · \"{}\"", conv_ref(p, &all, s), fmt_date(s.ts), meta(s, "provider"), files_note(s), clip(&s.label, 60));
            if !push_line(&mut o, &line, budget) {
                break;
            }
        }
    }
    // Sibling worktrees ordered by their latest recorded activity.
    let mut sib: Vec<(i64, &Node, Option<&Node>)> = p
        .checkouts
        .iter()
        .filter(|x| x.id != co.id)
        .map(|x| {
            let last = all.iter().find(|s| s.checkout_id.as_deref() == Some(x.id.as_str()));
            (last.map(|s| s.ts).unwrap_or(0).max(x.ts), x, last)
        })
        .collect();
    sib.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    if !sib.is_empty() && push_line(&mut o, "Other worktrees of this project:", budget) {
        for (ts, x, last) in sib.into_iter().take(4) {
            let mut line = format!("- {} · branch {} · {} · last activity {}", view::worktree_name(p, x), view::branch(x), p.top(x), if ts > 0 { fmt_date(ts) } else { "unknown".into() });
            if let Some(s) = last {
                let _ = write!(line, " · latest conversation {} \"{}\"", conv_ref(p, &all, s), clip(&s.label, 40));
            }
            if !push_line(&mut o, &line, budget) {
                break;
            }
        }
    }
    o.push_str(&foot);
    // Hard cap: header lines are not optional, so if even they do not fit, say nothing.
    Ok(if o.len() > ctx.budget { String::new() } else { o })
}

// ------------------------------------------------------------------ UserPromptSubmit

/// Function words that carry no topic; dropped before matching.
const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "are",
    "but",
    "not",
    "you",
    "your",
    "all",
    "any",
    "can",
    "had",
    "her",
    "was",
    "one",
    "our",
    "out",
    "has",
    "have",
    "him",
    "his",
    "how",
    "its",
    "may",
    "new",
    "now",
    "old",
    "see",
    "two",
    "who",
    "did",
    "get",
    "got",
    "let",
    "say",
    "she",
    "too",
    "use",
    "via",
    "with",
    "this",
    "that",
    "from",
    "they",
    "them",
    "then",
    "than",
    "there",
    "these",
    "those",
    "what",
    "when",
    "where",
    "which",
    "while",
    "why",
    "will",
    "would",
    "could",
    "should",
    "into",
    "onto",
    "about",
    "after",
    "before",
    "again",
    "also",
    "just",
    "like",
    "make",
    "made",
    "more",
    "most",
    "much",
    "must",
    "need",
    "only",
    "other",
    "over",
    "same",
    "some",
    "such",
    "very",
    "were",
    "been",
    "being",
    "does",
    "doing",
    "done",
    "each",
    "here",
    "it's",
    "i'm",
    "please",
    "want",
    "know",
    "think",
    "look",
    "check",
    "find",
    "show",
    "tell",
    "give",
    "take",
    "help",
    "work",
    "thing",
    "things",
    "something",
    "anything",
    "everything",
    "really",
    "maybe",
    "still",
    "yet",
    "ok",
    "okay",
    "yes",
    "lets",
];

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

/// Topic terms of a prompt: distinct, no stop-words, at least 3 chars, at most 12.
pub fn prompt_terms(prompt: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for w in words(prompt) {
        if w.chars().count() < 3 || STOP.contains(&w.as_str()) || (w.chars().all(|c| c.is_ascii_digit()) && w.len() < 4) {
            continue;
        }
        if seen.insert(w.clone()) {
            out.push(w);
        }
        if out.len() >= 12 {
            break;
        }
    }
    out
}

/// Path-like tokens (`ledger/webhooks.py`, `charges.py`) named in a prompt.
fn prompt_paths(prompt: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in prompt.split_whitespace() {
        let t = t.trim_matches(|c: char| "`'\"()[]{},;:!?<>".contains(c)).trim_end_matches('.');
        let t = t.trim_start_matches("./");
        let looks = t.len() >= 4
            && t.len() <= 200
            && (t.contains('/') || t.rsplit_once('.').map(|(a, b)| !a.is_empty() && (1..=5).contains(&b.len()) && b.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or(false));
        if looks && !t.contains("://") && !t.starts_with('/') && !out.iter().any(|o: &String| o == t) {
            out.push(t.to_string());
        }
        if out.len() >= 4 {
            break;
        }
    }
    out
}

/// Minimum distinct prompt terms a passage must contain.
fn required(n: usize) -> usize {
    n.div_ceil(3).clamp(2, 4)
}

fn passage_words(c: &Connection, node: &str, loc: &str) -> BTreeSet<String> {
    let mut st = match c.prepare_cached("SELECT title, heading, text FROM passages WHERE node_id=?1 AND loc=?2 LIMIT 8") {
        Ok(s) => s,
        Err(_) => return BTreeSet::new(),
    };
    let rows = st.query_map(params![node, loc], |r| Ok(format!("{} {} {}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)));
    rows.map(|r| r.flatten().flat_map(|t| words(&t)).collect()).unwrap_or_default()
}

struct Item {
    r: String,
    lines: Vec<String>,
}

fn prompt_context(env: &Env, c: &Connection, p: &Project, session_id: &str, prompt: &str, ctx: &Ctx) -> Result<String, String> {
    let terms = prompt_terms(prompt);
    let paths = prompt_paths(prompt);
    let mut items: Vec<Item> = Vec::new();
    // Exact path hits inside this project.
    for t in &paths {
        let like = format!("%/{}", t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let cands = nodes(c, "repo_id=?1 AND kind IN ('file','doc') AND path LIKE ?2 ESCAPE '\\' LIMIT 20", &[&p.repo.id, &like]).unwrap_or_default();
        let n_conv = |id: &str| c.query_row("SELECT count(DISTINCT src) FROM edges WHERE dst=?1 AND kind IN ('read','write')", [id], |r| r.get::<_, i64>(0)).unwrap_or(0);
        let best = cands
            .iter()
            .filter_map(|n| n.checkout_id.as_deref().and_then(|id| p.checkouts.iter().find(|x| x.id == id)).map(|co| (n_conv(&n.id), co.id == p.focus.id, n, co)))
            .max_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        if let Some((k, _, n, co)) = best {
            let relp = rel(n.path.as_deref().unwrap_or(""), &p.top(co)).to_string();
            let r = sh_quote(&view::qualified_raw(p, co, &relp));
            items.push(Item { r: r.clone(), lines: vec![format!("- {r} — file named in the prompt · {k} recorded conversation(s) read/wrote it")] });
        }
    }
    if terms.len() >= 2 {
        let q = search::Query { text: terms.join(" "), kinds: vec!["doc".into(), "message".into(), "commit".into()], any: true, limit: 25, ..Default::default() };
        let (hits, _) = search::run_pref(c, &q, Some(&p.repo.id), None, Some(&p.focus.id)).map_err(|e| e.to_string())?;
        let need = required(terms.len());
        let mut sessions: Option<Vec<Node>> = None;
        for h in hits {
            if items.len() >= 6 {
                break;
            }
            let have = passage_words(c, &h.node_id, &h.loc);
            let matched: Vec<&str> = terms.iter().filter(|t| have.contains(*t)).map(String::as_str).collect();
            if matched.len() < need {
                continue;
            }
            let co = h.checkout_id.as_deref().and_then(|id| p.checkouts.iter().find(|x| x.id == id)).unwrap_or(&p.focus);
            let (r, desc) = match h.kind.as_str() {
                "message" => {
                    let all = sessions.get_or_insert_with(|| repo_sessions(c, &p.repo.id));
                    let Some(s) = all.iter().find(|s| s.id == h.node_id) else { continue };
                    if !session_id.is_empty() && meta(s, "native_id") == session_id {
                        continue; // this very conversation
                    }
                    let rest = format!("message:{}:{}", view::more::session_prefix(all, s), h.loc);
                    (sh_quote(&view::qualified_raw(p, co, &rest)), format!("{} {} message {} · conversation \"{}\"", meta(s, "provider"), h.speaker, fmt_date(h.ts), clip(&s.label, 50)))
                }
                "doc" => {
                    let relp = if h.title.starts_with("claude-memory/") { h.title.clone() } else { rel(&h.source_path, &p.top(co)).to_string() };
                    let rest = if h.loc.is_empty() { relp.clone() } else { format!("{relp}#{}", h.loc) };
                    (sh_quote(&view::qualified_raw(p, co, &rest)), format!("doc {relp}{} l.{}", if h.heading.is_empty() { String::new() } else { format!(" § {}", clip(&h.heading, 40)) }, h.line))
                }
                "commit" => {
                    let main = p.checkouts.first().unwrap_or(&p.focus);
                    let sha = &h.loc[..h.loc.len().min(8)];
                    (sh_quote(&view::qualified_raw(p, main, &format!("commit:{sha}"))), format!("commit {sha} by {} {}", h.speaker, fmt_date(h.ts)))
                }
                _ => continue,
            };
            if items.iter().any(|i| i.r == r) {
                continue;
            }
            let excerpt = clip(&h.snippet.replace('\n', " "), 200);
            items.push(Item { r: r.clone(), lines: vec![format!("- {r} — {desc} · matched: {}", matched.join(", ")), format!("  \"{excerpt}\"")] });
        }
    }
    if items.is_empty() {
        return Ok(String::new());
    }
    let seen_path = seen_file(env, session_id);
    let seen: BTreeSet<String> = seen_path.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
    items.retain(|i| !seen.contains(&i.r));
    if items.is_empty() {
        return Ok(String::new());
    }
    let mut o = format!("workmap: indexed evidence in project {} lexically matching this prompt (historical text, not decisions):\n", p.handle);
    let foot = format!("Expand a reference for the full text and neighbours: {} expand '<ref>'\n", ctx.cmd);
    let budget = ctx.budget.saturating_sub(foot.len());
    let mut emitted = Vec::new();
    for it in items {
        let block: String = it.lines.iter().map(|l| format!("{l}\n")).collect();
        if o.len() + block.len() > budget {
            continue; // a smaller later item may still fit; never cut one
        }
        o.push_str(&block);
        emitted.push(it.r);
    }
    if emitted.is_empty() {
        return Ok(String::new());
    }
    o.push_str(&foot);
    if let Some(f) = seen_path {
        remember(&f, &seen, &emitted);
    }
    Ok(o)
}

// ------------------------------------------------------------------ per-session dedup

/// `<state>/hook-seen/<sanitized session id>`; None without a session id.
fn seen_file(env: &Env, session_id: &str) -> Option<PathBuf> {
    let safe: String = session_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(80).collect();
    if safe.is_empty() {
        return None;
    }
    Some(env.state_dir.join("hook-seen").join(safe))
}

fn remember(f: &Path, seen: &BTreeSet<String>, new: &[String]) {
    let Some(dir) = f.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Prune: entries older than 7 days, and at most 200 sessions.
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut es: Vec<(std::time::SystemTime, PathBuf)> = rd.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
        let week = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 86400);
        es.sort();
        let excess = es.len().saturating_sub(200);
        for (i, (t, p)) in es.iter().enumerate() {
            if (*t < week || i < excess) && p != f {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let mut all: Vec<&String> = seen.iter().chain(new.iter()).collect();
    let skip = all.len().saturating_sub(500);
    all.drain(..skip);
    let body: String = all.iter().map(|r| format!("{r}\n")).collect();
    let tmp = f.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, f);
    }
}

// ------------------------------------------------------------------ hooks install / uninstall / status

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

/// Settings file a harness reads hooks from.
pub fn settings_path(env: &Env, harness: &str, scope: Scope, cwd: &Path) -> Result<PathBuf, String> {
    Ok(match (harness, scope) {
        ("claude", Scope::User) => env.home.join(".claude/settings.json"),
        // Personal, not committed: Claude Code gitignores settings.local.json.
        ("claude", Scope::Project) => cwd.join(".claude/settings.local.json"),
        ("codex", Scope::User) => env.codex_home().join("hooks.json"),
        ("codex", Scope::Project) => cwd.join(".codex/hooks.json"),
        (h, _) => return Err(format!("unsupported harness {h:?}: {}", unsupported_note(h))),
    })
}

fn unsupported_note(h: &str) -> &'static str {
    if h == "opencode" {
        "OpenCode 1.19.3 ships no documented hook/context-injection interface (its plugin event names are experimental and undocumented in the installed package); use the workmap skill or `workmap context` instead"
    } else {
        "supported harnesses: claude, codex"
    }
}

/// `env WORKMAP_HOOK=1 [WORKMAP_STATE_DIR=…] <bin> hook <Event> --harness <h>`.
/// Both hosts run `command` through a shell (Claude's examples expand
/// `${CLAUDE_PROJECT_DIR}`, Codex's use `$(git rev-parse …)`), so it is shell-quoted.
pub fn hook_command(env: &Env, bin: &Path, harness: &str, ev: Event) -> String {
    let mut parts = vec!["env".to_string(), MARKER.to_string()];
    for (k, v) in env.exports() {
        if k == "WORKMAP_GH_BIN" || k == "WORKMAP_GLAB_BIN" {
            continue; // hooks never run remote discovery
        }
        if k == "WORKMAP_STATE_DIR" && Path::new(&v) == env.home.join(".local/state/workmap") && !env.overridden {
            continue;
        }
        parts.push(sh_quote(&format!("{k}={v}")));
    }
    parts.push(sh_quote(&bin.display().to_string()));
    parts.extend(["hook".to_string(), ev.name().to_string(), "--harness".to_string(), harness.to_string()]);
    parts.join(" ")
}

fn timeout_for(ev: Event) -> Option<u64> {
    match ev {
        Event::SessionStart => Some(10),
        Event::UserPromptSubmit => Some(5),
        Event::Stop => Some(5),
        Event::SessionEnd => None, // keep the host's short default; the hook only spawns
    }
}

fn is_owned(h: &Value) -> bool {
    h.get("command").and_then(|c| c.as_str()).map(|c| c.contains(MARKER) && c.contains(" hook ")).unwrap_or(false)
}

fn read_settings(path: &Path) -> Result<Option<Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Some(json!({})));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON ({e}); not touching it", path.display()))?;
    if !v.is_object() {
        return Err(format!("{}: top level is not a JSON object; not touching it", path.display()));
    }
    if v.get("hooks").map(|h| !h.is_object()).unwrap_or(false) {
        return Err(format!("{}: \"hooks\" is not an object; not touching it", path.display()));
    }
    Ok(Some(v))
}

/// Remove workmap-owned handlers; drop groups/events/hooks emptied by that.
fn strip_owned(doc: &mut Value) -> Result<usize, String> {
    let Some(hooks) = doc.get_mut("hooks").and_then(|h| h.as_object_mut()) else { return Ok(0) };
    let mut removed = 0;
    let mut empty_events = Vec::new();
    for (ev, groups) in hooks.iter_mut() {
        let Some(arr) = groups.as_array_mut() else { continue };
        let before = arr.len();
        let mut touched = false;
        for g in arr.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(|x| x.as_array_mut()) {
                let n = hs.len();
                hs.retain(|h| !is_owned(h));
                if hs.len() != n {
                    removed += n - hs.len();
                    touched = true;
                }
            }
        }
        if touched {
            arr.retain(|g| g.get("hooks").and_then(|x| x.as_array()).map(|a| !a.is_empty()).unwrap_or(true));
            if arr.is_empty() && before > 0 {
                empty_events.push(ev.clone());
            }
        }
    }
    for e in empty_events {
        hooks.remove(&e);
    }
    if removed > 0 && hooks.is_empty() {
        if let Some(o) = doc.as_object_mut() {
            o.remove("hooks");
        }
    }
    Ok(removed)
}

fn owned_commands(doc: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(hooks) = doc.get("hooks").and_then(|h| h.as_object()) {
        for (ev, groups) in hooks {
            for g in groups.as_array().into_iter().flatten() {
                for h in g.get("hooks").and_then(|x| x.as_array()).into_iter().flatten() {
                    if is_owned(h) {
                        out.push((ev.clone(), h.get("command").and_then(|c| c.as_str()).unwrap_or("").to_string()));
                    }
                }
            }
        }
    }
    out
}

fn foreign_count(doc: &Value) -> usize {
    let mut n = 0;
    if let Some(hooks) = doc.get("hooks").and_then(|h| h.as_object()) {
        for groups in hooks.values() {
            for g in groups.as_array().into_iter().flatten() {
                n += g.get("hooks").and_then(|x| x.as_array()).map(|a| a.iter().filter(|h| !is_owned(h)).count()).unwrap_or(0);
            }
        }
    }
    n
}

/// Desired document: existing content with workmap entries replaced by `want`.
fn with_owned(doc: &Value, want: &[(Event, String)]) -> Result<Value, String> {
    let mut d = doc.clone();
    strip_owned(&mut d)?;
    let obj = d.as_object_mut().ok_or("settings is not an object")?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({})).as_object_mut().ok_or("\"hooks\" is not an object")?;
    for (ev, cmd) in want {
        let arr = hooks.entry(ev.name()).or_insert_with(|| json!([])).as_array_mut().ok_or_else(|| format!("hooks.{} is not an array; not touching it", ev.name()))?;
        let mut h = json!({"type": "command", "command": cmd});
        if let Some(t) = timeout_for(*ev) {
            h["timeout"] = json!(t);
        }
        arr.push(json!({"hooks": [h]}));
    }
    Ok(d)
}

/// Refuse to write through a symlinked settings file or directory.
fn symlinked(path: &Path) -> Option<String> {
    for p in [Some(path), path.parent()].into_iter().flatten() {
        if std::fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Some(format!("{} is a symlink", p.display()));
        }
    }
    None
}

/// Atomic replace (temp + rename in the same directory) with a one-file backup.
fn write_settings(path: &Path, v: &Value) -> Result<Option<PathBuf>, String> {
    let dir = path.parent().ok_or("settings path has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let backup = if path.exists() {
        let b = dir.join(format!("{name}.workmap-backup"));
        std::fs::copy(path, &b).map_err(|e| format!("backup {}: {e}", b.display()))?;
        Some(b)
    } else {
        None
    };
    let tmp = dir.join(format!(".{name}.workmap-tmp"));
    let body = serde_json::to_string_pretty(v).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Ok(m) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(backup)
}

pub struct HooksOpts<'a> {
    pub harness: &'a str,
    pub scope: Scope,
    pub cwd: &'a Path,
    pub bin: &'a Path,
    pub dry_run: bool,
}

/// Merge workmap's hook entries into the harness settings. Idempotent.
pub fn install(env: &Env, o: &HooksOpts) -> Result<Vec<String>, String> {
    let path = settings_path(env, o.harness, o.scope, o.cwd)?;
    if let Some(why) = symlinked(&path) {
        return Err(format!("{why}; not writing through it. Add the entries from `workmap hooks install --dry-run` by hand"));
    }
    let cur = read_settings(&path)?;
    let base = cur.clone().unwrap_or_else(|| json!({}));
    let want: Vec<(Event, String)> = Event::ALL.iter().map(|e| (*e, hook_command(env, o.bin, o.harness, *e))).collect();
    let next = with_owned(&base, &want)?;
    let mut report = Vec::new();
    if cur.as_ref() == Some(&next) {
        report.push(format!("unchanged {} ({} workmap hook(s) already installed)", path.display(), want.len()));
    } else if o.dry_run {
        report.push(format!("would update {} (dry run; nothing written):", path.display()));
        for (e, c) in &want {
            report.push(format!("  {}: {c}", e.name()));
        }
    } else {
        let b = write_settings(&path, &next)?;
        report.push(format!("installed {} workmap hook(s) in {}{}", want.len(), path.display(), b.map(|b| format!(" (previous file saved as {})", b.display())).unwrap_or_default()));
        for (e, c) in &want {
            report.push(format!("  {}: {c}", e.name()));
        }
    }
    report.push(format!("foreign hooks left untouched: {}", foreign_count(&next)));
    if o.harness == "codex" {
        report.push("codex: non-managed hooks run only after you review and trust them with /hooks inside Codex (trust is per hash; re-trust after reinstalling)".into());
        if o.scope == Scope::Project {
            report.push("codex: project-local hooks load only when the project's .codex/ layer is trusted".into());
        }
    }
    Ok(report)
}

/// Remove only workmap-owned entries. Missing file = nothing to do.
pub fn uninstall(env: &Env, harness: &str, scope: Scope, cwd: &Path, dry_run: bool) -> Result<Vec<String>, String> {
    let path = settings_path(env, harness, scope, cwd)?;
    let Some(cur) = read_settings(&path)? else { return Ok(vec![]) };
    let mut next = cur.clone();
    let n = strip_owned(&mut next)?;
    if n == 0 {
        return Ok(vec![]);
    }
    if let Some(why) = symlinked(&path) {
        return Err(format!("{why}; remove the {n} workmap hook entries (commands containing {MARKER}) by hand"));
    }
    if dry_run {
        return Ok(vec![format!("would remove {n} workmap hook(s) from {} (dry run)", path.display())]);
    }
    write_settings(&path, &next)?;
    Ok(vec![format!("removed {n} workmap hook(s) from {} ({} foreign hook(s) kept)", path.display(), foreign_count(&next))])
}

/// Remove owned user-scope hooks of every harness (`workmap uninstall`).
pub fn uninstall_user(env: &Env) -> Vec<String> {
    let mut out = Vec::new();
    for h in HARNESSES {
        match uninstall(env, h, Scope::User, Path::new("/"), false) {
            Ok(l) => out.extend(l),
            Err(e) => out.push(format!("left {h} hooks alone: {e}")),
        }
    }
    out
}

pub const SUPPORT: &str = "Host hook support (verified 2026-10 against the installed CLIs and their primary docs):
  claude   Claude Code 2.1.302   SUPPORTED  SessionStart + UserPromptSubmit add context (hookSpecificOutput.additionalContext);
                                            Stop + SessionEnd start a detached `workmap ingest --transcript <path>`
                                            user: ~/.claude/settings.json · project: .claude/settings.local.json
                                            docs: https://code.claude.com/docs/en/hooks
  codex    codex-cli 0.163.0     SUPPORTED  same events, stdin fields and JSON output (`codex features list`: hooks stable true)
                                            user: ~/.codex/hooks.json · project: .codex/hooks.json
                                            hooks stay inert until you trust them with /hooks inside Codex
                                            docs: https://learn.chatgpt.com/docs/hooks (formerly developers.openai.com/codex/hooks)
  opencode opencode 1.19.3       UNSUPPORTED no documented hook or context-injection interface in the installed package
                                            (plugin hook names there are experimental and undocumented); use the skill
Context is source excerpts and fully qualified references only, bounded by WORKMAP_HOOK_BUDGET (default 1500 bytes);
no scan, network or model runs inside a hook. Install: workmap hooks install [--harness claude|codex] [--scope user|project] [--dry-run]";

/// What is installed where, plus the support matrix.
pub fn status(env: &Env, cwd: &Path) -> String {
    let mut o = String::new();
    for h in HARNESSES {
        for (scope, name) in [(Scope::User, "user"), (Scope::Project, "project")] {
            let Ok(path) = settings_path(env, h, scope, cwd) else { continue };
            let line = match read_settings(&path) {
                Ok(None) => "no file".to_string(),
                Err(e) => format!("unreadable: {e}"),
                Ok(Some(v)) => {
                    let owned = owned_commands(&v);
                    let evs: BTreeSet<&str> = owned.iter().map(|x| x.0.as_str()).collect();
                    if owned.is_empty() {
                        format!("workmap hooks not installed · {} foreign hook(s)", foreign_count(&v))
                    } else {
                        format!("workmap hooks: {} · {} foreign hook(s)", evs.into_iter().collect::<Vec<_>>().join(", "), foreign_count(&v))
                    }
                }
            };
            let _ = writeln!(o, "{h:<6} {name:<7} {}: {line}", path.display());
        }
    }
    o.push_str(SUPPORT);
    o.push('\n');
    o
}
