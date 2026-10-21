//! `ask`: continue a stored native conversation with a follow-up question,
//! read-only. Restrictions are enforced through CLI flags/config of each agent,
//! verified against the installed CLI's `--help` before anything runs. When a
//! provider cannot be restricted, `ask` fails closed.

use crate::overview::get_node;
use rusqlite::Connection;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct AskPlan {
    pub provider: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub enforcement: Vec<String>,
}

impl AskPlan {
    pub fn render(&self) -> String {
        let q = |s: &str| if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_=./:,".contains(c)) { s.to_string() } else { format!("'{}'", s.replace('\'', "'\\''")) };
        let mut o = format!("provider: {}\ncwd: {}\n", self.provider, self.cwd.display());
        for (k, v) in &self.env {
            o.push_str(&format!("env: {k}={v}\n"));
        }
        o.push_str(&format!("command: {} {}\n", self.program, self.args.iter().map(|a| q(a)).collect::<Vec<_>>().join(" ")));
        o.push_str("read-only enforcement:\n");
        for e in &self.enforcement {
            o.push_str(&format!("  - {e}\n"));
        }
        o
    }
}

/// Flags that must never appear in an ask plan.
pub const FORBIDDEN: &[&str] = &["--dangerously-bypass-approvals-and-sandbox", "--dangerously-skip-permissions", "bypassPermissions", "danger-full-access", "workspace-write", "--auto", "acceptEdits"];

/// `help(program, args)` returns the CLI's help text; injectable for tests.
pub type HelpFn<'a> = &'a dyn Fn(&str, &[&str]) -> Option<String>;

pub fn real_help(program: &str, args: &[&str]) -> Option<String> {
    let mut c = Command::new(program);
    c.args(args);
    // OpenCode writes help to stderr. Normal command stderr remains suppressed
    // so it cannot leak credentials into output.
    crate::util::run_cmd_both(&mut c, Duration::from_secs(15))
}

fn require(help: Option<String>, what: &str, needles: &[&str]) -> Result<(), String> {
    let h = help.ok_or_else(|| format!("{what}: could not read --help; refusing to run without verified read-only flags"))?;
    for n in needles {
        if !h.contains(n) {
            return Err(format!("{what}: installed CLI does not document `{n}`; cannot enforce read-only, refusing"));
        }
    }
    Ok(())
}

/// A subagent transcript (Claude Code `<session>/subagents/agent-*.jsonl`) has no
/// session id of its own: every line carries the parent's `sessionId`, and
/// `claude --help` (2.1.x) documents `--resume <session-id>` only. Resuming a
/// child alone is therefore unsupported; point at the parent instead of
/// inventing an id. Shared by `ask` and `resume`.
pub fn subagent_refusal(c: &Connection, node: &crate::overview::Node) -> Option<String> {
    let parent = node.meta.pointer("/parent/native_id").and_then(|x| x.as_str())?;
    let provider = node.meta.get("provider").and_then(|x| x.as_str()).unwrap_or("claude");
    let pid = crate::util::stable_id("session", &format!("{provider}:{parent}"));
    let r = get_node(c, &pid)
        .ok()
        .flatten()
        .and_then(|pn| pn.checkout_id.as_deref().and_then(|id| get_node(c, id).ok().flatten()))
        .and_then(|co| crate::view::qualified_ref(c, &co, &format!("session:{parent}")).ok())
        .unwrap_or_else(|| format!("session {parent} (parent transcript not indexed)"));
    Some(format!(
        "this is a subagent transcript (child of session {parent}); it shares its parent's session id and the provider CLI cannot resume a subagent on its own. Resume the parent {r} instead."
    ))
}

pub fn plan(c: &Connection, session: &str, question: &str, help: HelpFn) -> Result<AskPlan, String> {
    let node = get_node(c, session).map_err(|e| e.to_string())?.ok_or_else(|| format!("unknown session id {session}"))?;
    if node.kind != "session" {
        return Err(format!("{session} is a {}, not a session", node.kind));
    }
    let provider = node.meta.get("provider").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let native = node.meta.get("native_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let cwd = PathBuf::from(node.meta.get("cwd").and_then(|x| x.as_str()).unwrap_or(""));
    if native.is_empty() {
        return Err("session has no native id; cannot resume".into());
    }
    if let Some(e) = subagent_refusal(c, &node) {
        return Err(e);
    }
    if !cwd.is_dir() {
        let mut m = format!("recorded cwd {} no longer exists; native resume is keyed by it, refusing", cwd.display());
        if let Some(note) = node.checkout_id.as_deref().and_then(|id| get_node(c, id).ok().flatten()).and_then(|co| crate::view::missing_note(c, &co)) {
            m.push_str(&format!("\n{note}\n(to resume, recreate the checkout at the recorded path, e.g. `git worktree repair` after moving it back, or `git worktree add <recorded path> <branch>`)"));
        }
        return Err(m);
    }
    let plan = match provider.as_str() {
        "claude" => {
            require(
                help("claude", &["--help"]),
                "claude",
                &["--resume", "--fork-session", "--no-session-persistence", "--tools", "--strict-mcp-config", "--permission-mode", "dontAsk", "--disallowedTools"],
            )?;
            AskPlan {
                provider,
                program: "claude".into(),
                args: vec![
                    "-p".into(),
                    "--resume".into(),
                    native,
                    "--fork-session".into(),
                    "--no-session-persistence".into(),
                    "--tools".into(),
                    "Read,Grep,Glob".into(),
                    "--allowedTools".into(),
                    "Read,Grep,Glob".into(),
                    "--disallowedTools".into(),
                    "Bash,Edit,Write,MultiEdit,NotebookEdit,WebFetch,WebSearch,Agent,Task".into(),
                    "--permission-mode".into(),
                    "dontAsk".into(),
                    "--strict-mcp-config".into(),
                    "--mcp-config".into(),
                    r#"{"mcpServers":{}}"#.into(),
                    "--settings".into(),
                    r#"{"disableAllHooks":true}"#.into(),
                    "--".into(),
                    question.into(),
                ],
                cwd,
                env: vec![],
                enforcement: vec![
                    "--tools Read,Grep,Glob: only read/search built-ins exist in the session (no Bash/Edit/Write)".into(),
                    "--disallowedTools denies write/exec/network tools even if user settings allow them".into(),
                    "--permission-mode dontAsk: anything not pre-approved is denied instead of prompting".into(),
                    "--strict-mcp-config with an empty server list: no MCP tools".into(),
                    "--settings disableAllHooks: user hooks do not run".into(),
                    "--fork-session --no-session-persistence: original transcript is not appended to".into(),
                ],
            }
        }
        "codex" => {
            require(help("codex", &["exec", "resume", "--help"]), "codex exec resume", &["--ignore-user-config", "--ephemeral", "--config", "--skip-git-repo-check"])?;
            require(help("codex", &["exec", "--help"]), "codex exec", &["read-only"])?;
            AskPlan {
                provider,
                program: "codex".into(),
                args: vec![
                    "exec".into(),
                    "resume".into(),
                    "--ignore-user-config".into(),
                    "--ignore-rules".into(),
                    "--ephemeral".into(),
                    "--skip-git-repo-check".into(),
                    "-c".into(),
                    "sandbox_mode=\"read-only\"".into(),
                    "-c".into(),
                    "approval_policy=\"never\"".into(),
                    "-c".into(),
                    "mcp_servers={}".into(),
                    native,
                    "--".into(),
                    question.into(),
                ],
                cwd,
                env: vec![],
                enforcement: vec![
                    "sandbox_mode=read-only via -c (`exec resume` has no --sandbox flag in this CLI; -c is its documented override)".into(),
                    "approval_policy=never: no escalation prompts, sandbox denials stay denials".into(),
                    "--ignore-user-config + mcp_servers={}: no user MCP servers (MCP tools run outside the sandbox)".into(),
                    "--ephemeral: the stored rollout is not appended to".into(),
                ],
            }
        }
        "opencode" | "opencode-legacy" => {
            return Err("opencode: unsupported. `opencode run --session` has no read-only flag; permission config cannot be verified to cover MCP servers and plugins, so workmap refuses rather than run a writable agent. Open the session in OpenCode's plan agent manually.".into());
        }
        other => return Err(format!("unsupported provider {other}")),
    };
    for a in &plan.args {
        if FORBIDDEN.iter().any(|f| a == f) {
            return Err(format!("internal error: forbidden flag {a} in plan"));
        }
    }
    Ok(plan)
}

pub fn execute(p: &AskPlan) -> std::io::Result<i32> {
    let st = Command::new(&p.program).args(&p.args).current_dir(&p.cwd).envs(p.env.iter().cloned()).status()?;
    Ok(st.code().unwrap_or(1))
}
