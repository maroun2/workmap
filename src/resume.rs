//! Explicit, user-triggered interactive resume of a recorded conversation in its
//! own provider CLI. Distinct from the read-only `ask`: this continues (and
//! appends to) the ORIGINAL session, so it is only ever started by the user.
//!
//! Resume syntax, verified against the installed CLIs' `--help` (2026-10) and
//! re-checked at run time before anything executes:
//! - claude 2.1.302: `-r, --resume [value]  Resume a conversation by session ID`.
//!   Sessions are "tied to your current directory"
//!   (<https://code.claude.com/docs/en/how-claude-code-works>), so it runs in the
//!   recorded cwd; whether a UUID resolves from another directory is unverified.
//! - codex-cli 0.163.0: `codex resume [OPTIONS] [SESSION_ID] [PROMPT]`, "Session id
//!   (UUID) or session name"; `--all` only widens the picker ("disables cwd filtering").
//! - opencode 1.19.3: `opencode [project]` ("path to start opencode in") with
//!   `-s, --session  session id to continue`.
//!
//! Commands are built as argv (no shell). Session ids come only from the index.

use crate::ask::HelpFn;
use crate::overview::{get_node, nodes, Node};
use crate::util::sh_quote;
use rusqlite::Connection;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ResumePlan {
    pub provider: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Recorded-vs-current evidence printed before running (empty when the cwd is the recorded one).
    pub drift: Vec<String>,
}

impl ResumePlan {
    pub fn render(&self) -> String {
        let mut o = format!("provider: {}\ncwd: {}\n", self.provider, self.cwd.display());
        if !self.drift.is_empty() {
            o.push_str("CONTEXT DRIFT: --cwd differs from the recorded working directory:\n");
            for d in &self.drift {
                let _ = writeln!(o, "  {d}");
            }
        }
        let _ = writeln!(o, "command: {} {}", sh_quote(&self.program), self.args.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" "));
        o.push_str("mode: interactive; continues and appends to the original session (use `workmap ask` for a read-only question)\n");
        o
    }
}

fn meta<'a>(n: &'a Node, k: &str) -> &'a str {
    n.meta.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn require(help: Option<String>, what: &str, needles: &[&str]) -> Result<(), String> {
    let h = help.ok_or_else(|| format!("{what}: could not read its --help; refusing to guess the resume syntax"))?;
    for n in needles {
        if !h.contains(n) {
            return Err(format!("{what}: installed CLI does not document `{n}`; refusing to guess the resume syntax"));
        }
    }
    Ok(())
}

/// Session node by internal id, or by a unique native-id prefix (>= 8 chars) across the index.
pub fn find_session(c: &Connection, id: &str) -> Result<String, String> {
    if let Some(n) = get_node(c, id).map_err(|e| e.to_string())? {
        return if n.kind == "session" { Ok(n.id) } else { Err(format!("{id} is a {}, not a conversation", n.kind)) };
    }
    if id.chars().count() < 8 {
        return Err(format!("{id:?}: give a reference (:conversation:<title>, :session:<id-prefix>) or at least 8 characters of a session id"));
    }
    let like = format!("{}%", id.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let hits = nodes(c, "kind='session' AND json_extract(meta,'$.native_id') LIKE ?1 ESCAPE '\\' LIMIT 11", &[&like]).map_err(|e| e.to_string())?;
    match hits.len() {
        1 => Ok(hits[0].id.clone()),
        0 => Err(format!("no indexed conversation has a session id starting with {id:?}")),
        n => Err(format!("{n} conversations have a session id starting with {id:?}; give more characters or a reference")),
    }
}

/// Subagent transcripts have no resumable id of their own. Tolerates absent keys.
/// `Some(parent native id or "")` when the node is a subagent transcript.
fn parent_of(s: &Node) -> Option<String> {
    let path = s.path.as_deref().unwrap_or("");
    let by_path = path.find("/subagents/").map(|i| path[..i].rsplit('/').next().unwrap_or("").to_string()); // <project>/<parent id>/subagents/<agent>.jsonl
    let flagged = ["is_subagent", "subagent", "sidechain", "is_sidechain"].iter().any(|k| s.meta.get(*k).and_then(|x| x.as_bool()).unwrap_or(false));
    if by_path.is_none() && !flagged {
        return None;
    }
    let named = ["parent_native_id", "parent_session", "parent"].iter().find_map(|k| s.meta.get(*k).and_then(|x| x.as_str()).filter(|p| !p.is_empty()).map(str::to_string));
    Some(named.or(by_path).unwrap_or_default())
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(args);
    crate::util::run_cmd(&mut c, Duration::from_secs(5)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn handle_of(c: &Connection, s: &Node) -> Option<String> {
    let repo = get_node(c, s.repo_id.as_deref()?).ok()??;
    crate::view::project_handle(c, &repo).ok()
}

/// Checkouts of the same project on the recorded branch that still exist.
/// Branch the session worked on: the transcript's own field, else the branch of
/// the checkout it was recorded in (snapshot kept even after the checkout is gone).
fn recorded_branch(s: &Node) -> &str {
    let b = meta(s, "branch");
    if !b.is_empty() {
        return b;
    }
    s.meta.pointer("/checkout/branch").and_then(|x| x.as_str()).unwrap_or("")
}

fn candidates(c: &Connection, s: &Node) -> Vec<Node> {
    let (Some(repo), b) = (s.repo_id.as_deref(), recorded_branch(s)) else { return vec![] };
    let all = nodes(c, "kind='checkout' AND repo_id=?1 ORDER BY json_extract(meta,'$.main') DESC, path", &[&repo]).unwrap_or_default();
    all.into_iter().filter(|co| !b.is_empty() && meta(co, "branch") == b && co.path.as_deref().map(|p| Path::new(p).is_dir()).unwrap_or(false)).collect()
}

fn main_checkout(c: &Connection, s: &Node) -> Option<Node> {
    let repo = s.repo_id.as_deref()?;
    nodes(c, "kind='checkout' AND repo_id=?1 ORDER BY json_extract(meta,'$.main') DESC, path", &[&repo]).ok()?.into_iter().find(|co| co.path.as_deref().map(|p| Path::new(p).is_dir()).unwrap_or(false))
}

pub fn plan(c: &Connection, sid: &str, cwd_override: Option<&Path>, help: HelpFn) -> Result<ResumePlan, String> {
    let s = get_node(c, sid).map_err(|e| e.to_string())?.ok_or_else(|| format!("unknown session id {sid}"))?;
    if s.kind != "session" {
        return Err(format!("{sid} is a {}, not a conversation", s.kind));
    }
    let provider = meta(&s, "provider").to_string();
    let native = meta(&s, "native_id").to_string();
    if native.is_empty() {
        return Err("the transcript records no native session id; nothing to resume".into());
    }
    if let Some(parent) = parent_of(&s) {
        let hint = match (parent.is_empty(), handle_of(c, &s)) {
            (false, Some(h)) => format!(": resume the parent conversation instead, e.g. workmap resume {}", sh_quote(&format!("{h}:session:{}", parent.chars().take(12).collect::<String>()))),
            (false, None) => format!(": resume the parent session {parent} instead"),
            _ => String::new(),
        };
        return Err(format!("this is a subagent transcript; subagents have no session of their own to resume{hint}"));
    }
    let recorded = PathBuf::from(meta(&s, "cwd"));
    let cwd = match cwd_override {
        Some(d) => std::fs::canonicalize(d).map_err(|e| format!("--cwd {}: {e}", d.display()))?,
        None => {
            if !recorded.is_dir() {
                return Err(missing_cwd(c, &s, &recorded));
            }
            recorded.clone()
        }
    };
    if !cwd.is_dir() {
        return Err(format!("--cwd {} is not a directory", cwd.display()));
    }
    let drift = if cwd_override.is_some() && std::fs::canonicalize(&recorded).ok().as_deref() != Some(cwd.as_path()) { drift(c, &s, &recorded, &cwd, &provider) } else { vec![] };
    let (program, args) = match provider.as_str() {
        "claude" => {
            require(help("claude", &["--help"]), "claude", &["--resume"])?;
            ("claude", vec!["--resume".to_string(), native])
        }
        "codex" => {
            require(help("codex", &["resume", "--help"]), "codex resume", &["SESSION_ID"])?;
            ("codex", vec!["resume".to_string(), native])
        }
        "opencode" if s.path.as_deref().map(|p| p.contains("/storage/session/")).unwrap_or(false) => {
            return Err("opencode: this conversation was indexed from OpenCode's legacy JSON storage; the installed OpenCode keeps sessions in its SQLite database and workmap cannot verify the id still exists there. Check `opencode session list`, then run `opencode <dir> --session <id>` yourself".into());
        }
        "opencode" => {
            require(help("opencode", &["--help"]), "opencode", &["--session", "[project]"])?;
            ("opencode", vec![cwd.display().to_string(), "--session".to_string(), native])
        }
        other => return Err(format!("unsupported provider {other:?}")),
    };
    Ok(ResumePlan { provider, program: program.into(), args, cwd, drift })
}

fn missing_cwd(c: &Connection, s: &Node, recorded: &Path) -> String {
    let mut m = format!("recorded working directory {} no longer exists; the native session is keyed by it, so workmap will not guess. Choices:", recorded.display());
    let cands = candidates(c, s);
    for co in &cands {
        let _ = write!(m, "\n  workmap resume <same ref> --cwd {}   # existing worktree on the recorded branch {}", sh_quote(co.path.as_deref().unwrap_or("")), meta(co, "branch"));
    }
    let b = recorded_branch(s);
    match (main_checkout(c, s), b.is_empty()) {
        // The branch is checked out elsewhere, so `worktree add` would fail; move it back instead.
        (Some(main), false) if !cands.is_empty() && cands.iter().all(|co| co.path != main.path) => {
            let _ = write!(
                m,
                "\n  git -C {} worktree repair && git -C {} worktree move {} {}   # repair the moved checkout registration, restore its recorded path, then rerun",
                sh_quote(main.path.as_deref().unwrap_or("")),
                sh_quote(cands[0].path.as_deref().unwrap_or("")),
                sh_quote(cands[0].path.as_deref().unwrap_or("")),
                sh_quote(&recorded.display().to_string())
            );
        }
        (Some(main), false) if cands.is_empty() => {
            let _ = write!(
                m,
                "\n  git -C {0} worktree prune && git -C {0} worktree add {1} {2}   # recreate the worktree at the recorded path (prune drops the stale registration of the deleted one), then rerun",
                sh_quote(main.path.as_deref().unwrap_or("")),
                sh_quote(&recorded.display().to_string()),
                sh_quote(b)
            );
        }
        (_, _) if cands.is_empty() => m.push_str("\n  (no indexed checkout of this project on the recorded branch; pass --cwd <dir> to run elsewhere and accept the drift)"),
        _ => {}
    }
    m
}

fn drift(c: &Connection, s: &Node, recorded: &Path, cwd: &Path, provider: &str) -> Vec<String> {
    let co = s.checkout_id.as_deref().and_then(|id| get_node(c, id).ok().flatten());
    let rb = recorded_branch(s);
    let rh = co.as_ref().map(|co| meta(co, "head").to_string()).unwrap_or_default();
    let cb = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "(not a git checkout)".into());
    let ch = git_out(cwd, &["rev-parse", "--short=10", "HEAD"]).unwrap_or_default();
    let mut d = vec![
        format!("recorded: cwd {} · branch {} · HEAD {} (at last index)", recorded.display(), if rb.is_empty() { "(unknown)" } else { rb }, if rh.is_empty() { "(unknown)" } else { &rh }),
        format!("current:  cwd {} · branch {cb} · HEAD {}", cwd.display(), if ch.is_empty() { "(unknown)" } else { &ch }),
    ];
    if !recorded.exists() {
        d.push("recorded cwd no longer exists".into());
    }
    if !rb.is_empty() && rb != cb {
        d.push(format!("branch differs: the session worked on {rb}, this directory is on {cb}"));
    }
    if provider == "claude" {
        d.push("claude stores sessions per project directory; resuming by id from another directory is not verified to work".into());
    }
    d
}

pub fn execute(p: &ResumePlan) -> std::io::Result<i32> {
    let st = Command::new(&p.program).args(&p.args).current_dir(&p.cwd).status()?;
    Ok(st.code().unwrap_or(1))
}
