//! Native agent conversation parsers: Claude Code JSONL, Codex rollout JSONL,
//! OpenCode SQLite (current) and OpenCode legacy JSON storage.
//!
//! Parsers extract only metadata and file-access evidence from *structured tool
//! calls*. Shell commands never count as confirmed access; paths found in them
//! are reported as `mentioned` candidates which the scanner keeps only if they
//! name a tracked file. Codex's own classification of a shell command
//! (`parsed_cmd`: read/search/list_files) is kept as its own, weaker class
//! (`parsed`), tied to the command's recorded exit status.

use crate::util::{clip, parse_iso, read_line_bounded, redact, Line};
use serde_json::Value;
use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};

pub const LINE_CAP: usize = 16 << 20; // bigger lines are skipped and counted
pub const FILE_CAP: u64 = 512 << 20; // stop reading a transcript after this many bytes
/// Searchable natural-language text kept per message (longer messages are cut).
pub const MSG_CHARS: usize = 4800;
/// Messages and total text kept per conversation; beyond that `messages_capped`.
pub const MAX_MSGS: usize = 5000;
pub const MAX_MSG_BYTES: usize = 2 << 20;

/// One natural-language message (user input or assistant reply). Tool calls,
/// tool results, reasoning and harness-injected context are never included.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    /// Transcript line (JSONL) or 0 for database rows.
    pub line: usize,
    /// Ordinal among kept messages of this conversation (1-based).
    pub seq: usize,
    pub ts: Option<i64>,
    /// user | assistant
    pub speaker: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEvent {
    pub path: String,
    /// read | write | mention | search | list (search/list: Codex parsed shell commands)
    pub op: &'static str,
    /// observed (tool result recorded ok) | requested (no result seen) | failed | mentioned
    /// | parsed (a shell command Codex itself parsed as read/search/list, exit 0; best effort)
    pub strength: &'static str,
    /// Line in the transcript (JSONL) or 0 for database rows.
    pub line: usize,
    pub ts: Option<i64>,
    pub tool: String,
    /// Extra locator such as an OpenCode part id.
    pub locator: String,
}

/// A child conversation (Claude Code subagent) and the evidence naming its parent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParentLink {
    /// Parent's provider session id.
    pub native_id: String,
    /// Which path/fields proved the relation.
    pub evidence: Vec<String>,
    /// Parent tool call that spawned the child (`toolUseId` in the sibling meta file).
    pub tool_use_id: Option<String>,
    pub agent_type: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub provider: &'static str,
    pub native_id: String,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub title: Option<String>,
    pub first_prompt: Option<(i64, String, usize)>,
    pub last_prompt: Option<(i64, String, usize)>,
    pub started: i64,
    pub updated: i64,
    pub source_path: String,
    pub events: Vec<FileEvent>,
    pub warnings: usize,
    pub truncated: bool,
    pub messages: Vec<Msg>,
    pub msg_bytes: usize,
    /// Message text beyond MAX_MSGS / MAX_MSG_BYTES was not kept.
    pub messages_capped: bool,
    /// Set for subagent transcripts: the conversation that spawned this one.
    pub parent: Option<ParentLink>,
    seen_msgs: std::collections::HashSet<u64>,
}

impl Session {
    fn touch(&mut self, ts: Option<i64>) {
        if let Some(t) = ts {
            if self.started == 0 || t < self.started {
                self.started = t;
            }
            if t > self.updated {
                self.updated = t;
            }
        }
    }

    fn prompt(&mut self, ts: Option<i64>, text: &str, line: usize) {
        let t = text.trim();
        if t.is_empty() || is_injected(t) {
            return;
        }
        let p = (ts.unwrap_or(0), redact(&clip(t, 240)), line);
        if self.first_prompt.is_none() {
            self.first_prompt = Some(p.clone());
        }
        self.last_prompt = Some(p);
    }

    /// Keep one natural-language message (bounded, redacted, deduplicated per speaker).
    pub fn message(&mut self, ts: Option<i64>, speaker: &'static str, text: &str, line: usize) {
        let t = text.trim();
        if t.is_empty() || (speaker == "user" && is_injected(t)) {
            return;
        }
        if !self.seen_msgs.insert(crate::util::fnv64(&format!("{speaker}\0{t}"))) {
            return; // Codex records the same text as event and as response item
        }
        if self.messages.len() >= MAX_MSGS || self.msg_bytes >= MAX_MSG_BYTES {
            self.messages_capped = true;
            return;
        }
        let kept: String = t.chars().take(MSG_CHARS).collect();
        let kept = redact(&kept);
        self.msg_bytes += kept.len();
        let seq = self.messages.len() + 1;
        self.messages.push(Msg { line, seq, ts, speaker, text: kept });
    }
}

/// Concatenated `text` of content blocks whose type is one of `types`.
fn block_text(content: Option<&Value>, types: &[&str]) -> String {
    match content {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|x| x.as_str()).map(|t| types.contains(&t)).unwrap_or(false))
            .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Harness-injected "user" content that is not something a person typed.
fn is_injected(t: &str) -> bool {
    [
        "<command-",
        "<local-command",
        "<system-reminder",
        "<environment_context",
        "<user_instructions",
        "# AGENTS.md instructions",
        "<permissions",
        "Caveat:",
        "<task-notification",
        "[Request interrupted",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

/// Candidate path tokens inside a shell command (resolution happens later).
pub fn shell_path_tokens(cmd: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for tok in cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '<' | '>' | '"' | '\'' | '`')) {
        let t = tok.trim_matches(|c: char| c == ',' || c == ':');
        if t.len() < 3 || t.starts_with('-') || t.contains("://") || t.contains('*') || t.contains('$') || t.contains('=') {
            continue;
        }
        let last = t.rsplit('/').next().unwrap_or(t);
        if (t.contains('/') || last.contains('.')) && !v.iter().any(|x| x == t) {
            v.push(t.to_string());
        }
        if v.len() >= 40 {
            break;
        }
    }
    v
}

fn mention(events: &mut Vec<FileEvent>, cmd: &str, line: usize, ts: Option<i64>, tool: &str) {
    for p in shell_path_tokens(cmd) {
        events.push(FileEvent { path: p, op: "mention", strength: "mentioned", line, ts, tool: tool.into(), locator: String::new() });
    }
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}

/// Stream a JSONL file with line/byte caps, tolerating junk and partial lines.
fn for_each_json(path: &Path, sess: &mut Session, mut f: impl FnMut(&mut Session, usize, Value)) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut r = BufReader::with_capacity(1 << 16, std::io::Read::take(file, FILE_CAP));
    let mut buf = Vec::new();
    let mut n = 0usize;
    let mut bytes = 0u64;
    loop {
        n += 1;
        match read_line_bounded(&mut r, &mut buf, LINE_CAP)? {
            Line::Eof => break,
            Line::TooLong(len) => {
                bytes += len as u64 + 1;
                sess.warnings += 1;
            }
            Line::Data(d) => {
                bytes += d.len() as u64 + 1;
                if d.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                match serde_json::from_slice::<Value>(&d) {
                    Ok(v) => f(sess, n, v),
                    Err(_) => sess.warnings += 1, // malformed or partially written line
                }
            }
        }
    }
    if bytes >= FILE_CAP {
        sess.truncated = true;
    }
    Ok(())
}

// ---------------------------------------------------------------- Claude Code

pub fn claude_head_cwd(path: &Path) -> Option<String> {
    let head = crate::util::read_head(path, 64 << 10).ok()?;
    let txt = String::from_utf8_lossy(&head);
    let i = txt.find("\"cwd\":\"")? + 7;
    let rest = &txt[i..];
    let j = rest.find('"')?;
    Some(rest[..j].replace("\\\\", "\\"))
}

/// `<session-id>/subagents/agent-<agent-id>.jsonl` (Claude Code 2.x layout):
/// returns (parent session id from the directory name, agent id from the file name).
pub fn claude_subagent_path(path: &Path) -> Option<(String, String)> {
    let dir = path.parent()?;
    if dir.file_name()? != "subagents" {
        return None;
    }
    let parent = dir.parent()?.file_name()?.to_string_lossy().into_owned();
    let agent = path.file_stem()?.to_string_lossy().strip_prefix("agent-")?.to_string();
    (!parent.is_empty() && !agent.is_empty()).then_some((parent, agent))
}

pub fn parse_claude(path: &Path) -> std::io::Result<Session> {
    let mut sess = Session { provider: "claude", source_path: path.display().to_string(), ..Default::default() };
    sess.native_id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut pending: HashMap<String, usize> = HashMap::new();
    // Subagent transcripts carry the *parent's* `sessionId` on every line plus
    // `isSidechain: true` and their own `agentId`.
    let mut line_sid: Option<String> = None;
    let mut agent_id: Option<String> = None;
    let mut sidechain = false;
    for_each_json(path, &mut sess, |sess, line, v| {
        let ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_iso);
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if sess.cwd.is_none() {
            sess.cwd = s(&v, "cwd");
        }
        if sess.branch.is_none() {
            sess.branch = s(&v, "gitBranch").filter(|b| !b.is_empty() && b != "HEAD");
        }
        if let Some(id) = s(&v, "sessionId") {
            line_sid = Some(id);
        }
        if agent_id.is_none() {
            agent_id = s(&v, "agentId").filter(|a| !a.is_empty());
        }
        sidechain |= v.get("isSidechain").and_then(|x| x.as_bool()) == Some(true);
        if ty == "summary" {
            if let Some(t) = s(&v, "summary") {
                sess.title = Some(clip(&t, 120));
            }
        } else if ty.contains("title") {
            if let Some(t) = s(&v, "title").or_else(|| s(&v, "aiTitle")).or_else(|| s(&v, "customTitle")) {
                sess.title = Some(clip(&t, 120));
            }
        }
        if ty != "user" && ty != "assistant" {
            return;
        }
        sess.touch(ts);
        let content = v.pointer("/message/content");
        if ty == "user" {
            if v.get("isMeta").and_then(|x| x.as_bool()) == Some(true) {
                return;
            }
            match content {
                Some(Value::String(t)) => {
                    sess.prompt(ts, t, line);
                    sess.message(ts, "user", t, line);
                }
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match b.get("type").and_then(|x| x.as_str()) {
                            Some("text") => {
                                if v.get("toolUseResult").is_none() {
                                    let t = b.get("text").and_then(|x| x.as_str()).unwrap_or("");
                                    sess.prompt(ts, t, line);
                                    sess.message(ts, "user", t, line);
                                }
                            }
                            Some("tool_result") => {
                                if let Some(i) = b.get("tool_use_id").and_then(|x| x.as_str()).and_then(|id| pending.remove(id)) {
                                    let err = b.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                                    sess.events[i].strength = if err { "failed" } else { "observed" };
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        let said = block_text(content, &["text"]);
        sess.message(ts, "assistant", &said, line);
        let Some(Value::Array(blocks)) = content else { return };
        for b in blocks {
            if b.get("type").and_then(|x| x.as_str()) != Some("tool_use") {
                continue;
            }
            let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let input = b.get("input").cloned().unwrap_or(Value::Null);
            let id = s(b, "id").unwrap_or_default();
            let (op, key) = match name {
                "Read" => ("read", "file_path"),
                "NotebookRead" => ("read", "notebook_path"),
                "Write" | "Edit" | "MultiEdit" => ("write", "file_path"),
                "NotebookEdit" => ("write", "notebook_path"),
                "Bash" => {
                    mention(&mut sess.events, input.get("command").and_then(|x| x.as_str()).unwrap_or(""), line, ts, name);
                    continue;
                }
                "Grep" | "Glob" => {
                    if let Some(p) = s(&input, "path") {
                        sess.events.push(FileEvent { path: p, op: "mention", strength: "mentioned", line, ts, tool: name.into(), locator: String::new() });
                    }
                    continue;
                }
                _ => continue,
            };
            if let Some(p) = s(&input, key) {
                sess.events.push(FileEvent { path: p, op, strength: "requested", line, ts, tool: name.into(), locator: String::new() });
                if !id.is_empty() {
                    pending.insert(id, sess.events.len() - 1);
                }
            }
        }
    })?;
    let by_path = claude_subagent_path(path);
    if by_path.is_some() || (sidechain && agent_id.is_some()) {
        let mut ev = Vec::new();
        let parent = match (&by_path, &line_sid) {
            (Some((dir_sid, _)), Some(l)) if l == dir_sid => {
                ev.push(format!("path {dir_sid}/subagents/{}", path.file_name().unwrap_or_default().to_string_lossy()));
                ev.push("sessionId field names the parent".to_string());
                Some(dir_sid.clone())
            }
            (Some((dir_sid, _)), _) => {
                ev.push(format!("path {dir_sid}/subagents/{}", path.file_name().unwrap_or_default().to_string_lossy()));
                Some(dir_sid.clone())
            }
            (None, Some(l)) => {
                ev.push("sessionId field names the parent".to_string());
                Some(l.clone())
            }
            (None, None) => None,
        };
        if sidechain {
            ev.push("isSidechain=true".to_string());
        }
        sess.native_id = agent_id.clone().or_else(|| by_path.as_ref().map(|b| b.1.clone())).unwrap_or_default();
        if let Some(parent) = parent.filter(|p| *p != sess.native_id) {
            let mut link = ParentLink { native_id: parent, ..Default::default() };
            // Sibling `agent-<id>.meta.json`: {agentType, description, toolUseId, spawnDepth}.
            let meta_path = path.with_file_name(format!("{}.meta.json", path.file_stem().unwrap_or_default().to_string_lossy()));
            if let Some(m) = crate::util::read_head(&meta_path, 64 << 10).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                link.tool_use_id = s(&m, "toolUseId");
                link.agent_type = s(&m, "agentType");
                link.description = s(&m, "description").map(|d| redact(&clip(&d, 100)));
                if link.tool_use_id.is_some() {
                    ev.push("meta.json toolUseId = the parent's Agent tool call".to_string());
                }
            }
            link.evidence = ev;
            if sess.title.is_none() {
                if let Some(d) = link.description.as_deref().filter(|d| !d.trim().is_empty()) {
                    sess.title = Some(clip(&format!("subagent: {d}"), 120));
                }
            }
            sess.parent = Some(link);
        }
    } else if let Some(l) = line_sid {
        sess.native_id = l;
    }
    Ok(sess)
}

// ---------------------------------------------------------------- Codex

/// Paths named by `*** Add/Update/Delete File:` and `*** Move to:` headers in an
/// apply_patch body, which may be raw text or embedded in a JS string literal.
pub fn patch_paths(body: &str) -> Vec<String> {
    let mut v = Vec::new();
    for marker in ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "] {
        let mut rest = body;
        while let Some(i) = rest.find(marker) {
            let after = &rest[i + marker.len()..];
            let end = after.find(['\n', '\\', '"', '`', '\r']).unwrap_or(after.len());
            let p = after[..end].trim();
            if !p.is_empty() && !v.iter().any(|x: &String| x == p) {
                v.push(p.to_string());
            }
            rest = &after[end..];
        }
    }
    v
}

fn exec_js_commands(js: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut rest = js;
    while let Some(i) = rest.find("cmd:") {
        let after = rest[i + 4..].trim_start();
        let q = after.chars().next().unwrap_or(' ');
        if q == '"' || q == '\'' || q == '`' {
            if let Some(j) = after[1..].find(q) {
                v.push(after[1..1 + j].to_string());
                rest = &after[1 + j..];
                continue;
            }
        }
        rest = after;
    }
    v
}

pub fn codex_head_cwd(path: &Path) -> Option<String> {
    let head = crate::util::read_head(path, 256 << 10).ok()?;
    let first = head.split(|b| *b == b'\n').next()?;
    let v: Value = serde_json::from_slice(first).ok()?;
    v.pointer("/payload/cwd").and_then(|x| x.as_str()).map(str::to_string)
}

/// `file:///a%20b` -> `/a b`; plain paths unchanged.
fn uri_path(s: &str) -> String {
    let Some(rest) = s.strip_prefix("file://") else { return s.to_string() };
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(x) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(x);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Outcome of a Codex command execution: `parsed` only for a recorded exit code 0
/// (or status completed without an exit code), `failed` for a non-zero exit code
/// or a failed/declined status, otherwise `requested` (no result seen).
fn cmd_strength(status: Option<&str>, exit: Option<i64>) -> &'static str {
    match (status, exit) {
        (Some("failed") | Some("declined"), _) => "failed",
        (_, Some(0)) => "parsed",
        (_, Some(_)) => "failed",
        (Some("completed"), None) => "parsed",
        _ => "requested",
    }
}

/// Codex's own classification of a shell command (`parsed_cmd`, codex-rs
/// `ParsedCommand`): `read {cmd,name,path}`, `search {cmd,query,path}`,
/// `list_files {cmd,path}`; `unknown` is ignored. Paths are best effort.
#[allow(clippy::too_many_arguments)]
fn codex_parsed_cmd(sess: &mut Session, pc: Option<&Value>, cwd: Option<&str>, strength: &'static str, line: usize, ts: Option<i64>, cmd: &str, keys: &mut HashMap<usize, u64>) -> Vec<usize> {
    let mut idx = Vec::new();
    let Some(Value::Array(items)) = pc else { return idx };
    for it in items {
        let (op, tool) = match it.get("type").and_then(|x| x.as_str()) {
            Some("read") => ("read", "shell:read"),
            Some("search") => ("search", "shell:search"),
            Some("list_files") => ("list", "shell:list"),
            _ => continue,
        };
        let Some(p) = s(it, "path").filter(|p| !p.is_empty()).or_else(|| if op == "read" { s(it, "name").filter(|p| !p.is_empty()) } else { None }) else { continue };
        let abs = match cwd {
            Some(c) if !Path::new(&p).is_absolute() && !c.is_empty() => Path::new(c).join(&p).to_string_lossy().into_owned(),
            _ => p,
        };
        sess.events.push(FileEvent { path: abs.clone(), op, strength, line, ts, tool: tool.into(), locator: String::new() });
        keys.insert(sess.events.len() - 1, crate::util::fnv64(&format!("{cmd}\0{op}\0{abs}")));
        idx.push(sess.events.len() - 1);
    }
    idx
}

fn cmd_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(c)) => c.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

pub fn parse_codex(path: &Path) -> std::io::Result<Session> {
    let mut sess = Session { provider: "codex", source_path: path.display().to_string(), ..Default::default() };
    // Parsed-command events by execution id (item id / call_id), and a
    // command+path key per event for deduplication across event shapes.
    let mut by_exec: HashMap<String, Vec<usize>> = HashMap::new();
    let mut keys: HashMap<usize, u64> = HashMap::new();
    let mut exec = |sess: &mut Session, id: Option<String>, done: bool, status: Option<&str>, exit: Option<i64>, pc: Option<&Value>, cwd: Option<String>, cmd: String, line: usize, ts: Option<i64>| {
        let strength = if done { cmd_strength(status, exit) } else { "requested" };
        if let Some(prev) = id.as_ref().and_then(|i| by_exec.get(i)).filter(|v| !v.is_empty()) {
            if done {
                for &i in prev {
                    sess.events[i].strength = strength;
                }
            }
            return;
        }
        let cwd = cwd.map(|c| uri_path(&c)).or_else(|| sess.cwd.clone());
        let added = codex_parsed_cmd(sess, pc, cwd.as_deref(), strength, line, ts, &cmd, &mut keys);
        if let Some(id) = id {
            by_exec.insert(id, added);
        }
    };
    for_each_json(path, &mut sess, |sess, line, v| {
        let ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_iso);
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let p = v.get("payload").cloned().unwrap_or(Value::Null);
        let pty = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match (ty, pty) {
            ("session_meta", _) => {
                sess.native_id = s(&p, "id").or_else(|| s(&p, "session_id")).unwrap_or_default();
                sess.cwd = s(&p, "cwd");
                sess.branch = p.pointer("/git/branch").and_then(|x| x.as_str()).map(str::to_string);
                sess.touch(s(&p, "timestamp").and_then(|t| parse_iso(&t)).or(ts));
            }
            ("turn_context", _) => {
                if sess.cwd.is_none() {
                    sess.cwd = s(&p, "cwd");
                }
            }
            ("event_msg", "user_message") => {
                sess.touch(ts);
                let t = p.get("message").and_then(|x| x.as_str()).unwrap_or("");
                sess.prompt(ts, t, line);
                sess.message(ts, "user", t, line);
            }
            ("event_msg", "agent_message") => {
                sess.touch(ts);
                sess.message(ts, "assistant", p.get("message").and_then(|x| x.as_str()).unwrap_or(""), line);
            }
            ("response_item", "message") => {
                // `developer` items and injected user context are not conversation.
                match p.get("role").and_then(|x| x.as_str()) {
                    Some("assistant") => sess.message(ts, "assistant", &block_text(p.get("content"), &["output_text", "text"]), line),
                    Some("user") => sess.message(ts, "user", &block_text(p.get("content"), &["input_text", "text"]), line),
                    _ => {}
                }
            }
            ("event_msg", "patch_apply_end") => {
                let ok = p.get("success").and_then(|x| x.as_bool()).unwrap_or(false);
                if let Some(Value::Object(ch)) = p.get("changes") {
                    for k in ch.keys() {
                        sess.events.push(FileEvent {
                            path: k.clone(),
                            op: "write",
                            strength: if ok { "observed" } else { "failed" },
                            line,
                            ts,
                            tool: "patch_apply_end".into(),
                            locator: String::new(),
                        });
                    }
                }
            }
            ("event_msg", "exec_command_begin") | ("event_msg", "exec_command_end") => {
                // Older rollouts: {call_id, command, cwd, parsed_cmd, exit_code, ...}.
                let done = pty == "exec_command_end";
                exec(
                    sess,
                    s(&p, "call_id"),
                    done,
                    p.get("status").and_then(|x| x.as_str()),
                    p.get("exit_code").and_then(|x| x.as_i64()),
                    p.get("parsed_cmd"),
                    s(&p, "cwd"),
                    cmd_text(p.get("command")),
                    line,
                    ts,
                );
            }
            ("event_msg", "item_started") => {
                let item = p.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(|x| x.as_str()) == Some("CommandExecution") {
                    exec(sess, s(&item, "id"), false, None, None, item.get("parsed_cmd"), s(&item, "cwd"), cmd_text(item.get("command")), line, ts);
                }
            }
            ("event_msg", "item_completed") => {
                // Newer rollouts: {item: {type: "FileChange", status, changes: {<abs path>: {...}}}}
                let item = p.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(|x| x.as_str()) == Some("CommandExecution") {
                    // {id, command: [..], cwd: "file:///..", parsed_cmd: [..], status, exit_code, aggregated_output}
                    exec(
                        sess,
                        s(&item, "id"),
                        true,
                        item.get("status").and_then(|x| x.as_str()),
                        item.get("exit_code").and_then(|x| x.as_i64()),
                        item.get("parsed_cmd"),
                        s(&item, "cwd"),
                        cmd_text(item.get("command")),
                        line,
                        ts,
                    );
                }
                match item.get("type").and_then(|x| x.as_str()) {
                    Some("UserMessage") => {
                        sess.touch(ts);
                        let t = block_text(item.get("content"), &["text", "input_text"]);
                        sess.prompt(ts, &t, line);
                        sess.message(ts, "user", &t, line);
                    }
                    Some("AgentMessage") => {
                        sess.touch(ts);
                        sess.message(ts, "assistant", &block_text(item.get("content"), &["text", "output_text"]), line);
                    }
                    _ => {}
                }
                if item.get("type").and_then(|x| x.as_str()) == Some("FileChange") {
                    let strength = match item.get("status").and_then(|x| x.as_str()) {
                        Some("completed") => "observed",
                        Some("failed") | Some("declined") => "failed",
                        _ => "requested",
                    };
                    let paths: Vec<String> = match item.get("changes") {
                        Some(Value::Object(ch)) => ch.keys().cloned().collect(),
                        Some(Value::Array(a)) => a.iter().filter_map(|x| s(x, "path")).collect(),
                        _ => vec![],
                    };
                    for k in paths {
                        sess.events.push(FileEvent { path: k, op: "write", strength, line, ts, tool: "FileChange".into(), locator: String::new() });
                    }
                }
            }
            ("response_item", "custom_tool_call") | ("response_item", "function_call") => {
                sess.touch(ts);
                let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let body = s(&p, "input").or_else(|| s(&p, "arguments")).unwrap_or_default();
                let args: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                match name {
                    "apply_patch" => {
                        let patch = s(&args, "input").unwrap_or(body.clone());
                        for f in patch_paths(&patch) {
                            sess.events.push(FileEvent { path: f, op: "write", strength: "requested", line, ts, tool: "apply_patch".into(), locator: String::new() });
                        }
                    }
                    "exec" => {
                        if body.contains("apply_patch") {
                            for f in patch_paths(&body) {
                                sess.events.push(FileEvent { path: f, op: "write", strength: "requested", line, ts, tool: "exec:apply_patch".into(), locator: String::new() });
                            }
                        }
                        for c in exec_js_commands(&body) {
                            mention(&mut sess.events, &c, line, ts, "exec_command");
                        }
                    }
                    "exec_command" | "shell" | "local_shell" => {
                        let cmd = match args.get("cmd").or_else(|| args.get("command")) {
                            Some(Value::String(c)) => c.clone(),
                            Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
                            _ => String::new(),
                        };
                        mention(&mut sess.events, &cmd, line, ts, name);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    })?;
    dedup_parsed_cmds(&mut sess, &keys);
    // A patch both requested and confirmed applied keeps only the observed record.
    let observed: Vec<String> = sess.events.iter().filter(|e| e.strength == "observed" && e.op == "write").map(|e| e.path.clone()).collect();
    let cwd = sess.cwd.clone().unwrap_or_default();
    sess.events.retain(|e| !(e.strength == "requested" && e.op == "write" && observed.iter().any(|o| o == &e.path || Path::new(&cwd).join(&e.path) == Path::new(o))));
    Ok(sess)
}

/// Parsed-command evidence cleanup: search/list only when the command succeeded;
/// one event per (command, op, path) keeping the strongest outcome (the same
/// command can be recorded by several event shapes); and a path Codex parsed
/// from a command is no longer also reported as a bare shell mention.
fn dedup_parsed_cmds(sess: &mut Session, keys: &HashMap<usize, u64>) {
    let cwd = sess.cwd.clone().unwrap_or_default();
    let abs = |p: &str| crate::util::resolve_against(Path::new(&cwd), p);
    let rank = |s: &str| match s {
        "parsed" => 3,
        "failed" => 2,
        _ => 1,
    };
    let parsed_paths: std::collections::HashSet<PathBuf> = keys.keys().map(|&i| abs(&sess.events[i].path)).collect();
    let mut best: HashMap<u64, usize> = HashMap::new();
    for (&i, &k) in keys {
        let e = best.entry(k).or_insert(i);
        let (a, b) = (&sess.events[i], &sess.events[*e]);
        if rank(a.strength) > rank(b.strength) || (rank(a.strength) == rank(b.strength) && i < *e) {
            *e = i;
        }
    }
    let keep: Vec<bool> = sess
        .events
        .iter()
        .enumerate()
        .map(|(i, e)| match keys.get(&i) {
            Some(k) => best.get(k) == Some(&i) && (e.op == "read" || e.strength == "parsed"),
            None => !(e.op == "mention" && parsed_paths.contains(&abs(&e.path))),
        })
        .collect();
    let mut it = keep.into_iter();
    sess.events.retain(|_| it.next().unwrap_or(true));
}

/// Fast `rollout_path -> (cwd, title, first_user_message)` from Codex's own
/// state database, so unrelated rollouts are never opened.
pub fn codex_thread_index(codex_home: &Path) -> HashMap<String, (String, String, String)> {
    let mut map = HashMap::new();
    let Ok(rd) = std::fs::read_dir(codex_home) else { return map };
    let mut dbs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy();
            n.starts_with("state_") && n.ends_with(".sqlite")
        })
        .collect();
    dbs.sort();
    let Some(db) = dbs.last() else { return map };
    let Ok(c) = open_ro(db) else { return map };
    let Ok(mut st) = c.prepare("SELECT rollout_path, cwd, title, first_user_message FROM threads") else { return map };
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2).unwrap_or_default(), r.get::<_, String>(3).unwrap_or_default())));
    if let Ok(rows) = rows {
        for (p, cwd, t, f) in rows.flatten() {
            map.insert(p, (cwd, t, f));
        }
    }
    map
}

// ---------------------------------------------------------------- OpenCode

pub fn open_ro(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    let c = rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(c)
}

pub struct OcSessionRow {
    pub id: String,
    pub directory: String,
    pub title: String,
    pub created: i64,
    pub updated: i64,
}

/// Session list from the current OpenCode SQLite store (read-only).
pub fn opencode_sessions(db: &Path) -> rusqlite::Result<Vec<OcSessionRow>> {
    let c = open_ro(db)?;
    let mut st = c.prepare("SELECT id, directory, title, time_created, time_updated FROM session")?;
    let rows = st.query_map([], |r| Ok(OcSessionRow { id: r.get(0)?, directory: r.get(1)?, title: r.get(2)?, created: r.get::<_, i64>(3)? / 1000, updated: r.get::<_, i64>(4)? / 1000 }))?;
    Ok(rows.flatten().collect())
}

fn opencode_part(sess: &mut Session, data: &Value, ts: Option<i64>, locator: &str) {
    match data.get("type").and_then(|x| x.as_str()) {
        Some("tool") => {
            let tool = data.get("tool").and_then(|x| x.as_str()).unwrap_or("");
            let input = data.pointer("/state/input").cloned().unwrap_or(Value::Null);
            let status = data.pointer("/state/status").and_then(|x| x.as_str()).unwrap_or("");
            let strength = match status {
                "completed" => "observed",
                "error" => "failed",
                _ => "requested",
            };
            let op = match tool {
                "read" => "read",
                "edit" | "write" | "multiedit" => "write",
                "bash" => {
                    mention(&mut sess.events, input.get("command").and_then(|x| x.as_str()).unwrap_or(""), 0, ts, "bash");
                    return;
                }
                "patch" | "apply_patch" => {
                    let body = s(&input, "patchText").or_else(|| s(&input, "patch")).unwrap_or_default();
                    for f in patch_paths(&body) {
                        sess.events.push(FileEvent { path: f, op: "write", strength, line: 0, ts, tool: tool.into(), locator: locator.into() });
                    }
                    return;
                }
                "grep" | "glob" | "list" => {
                    if let Some(p) = s(&input, "path") {
                        sess.events.push(FileEvent { path: p, op: "mention", strength: "mentioned", line: 0, ts, tool: tool.into(), locator: locator.into() });
                    }
                    return;
                }
                _ => return,
            };
            if let Some(p) = s(&input, "filePath").or_else(|| s(&input, "file_path")) {
                sess.events.push(FileEvent { path: p, op, strength, line: 0, ts, tool: tool.into(), locator: locator.into() });
            }
        }
        Some("patch") => {
            if let Some(Value::Array(files)) = data.get("files") {
                for f in files.iter().filter_map(|x| x.as_str()) {
                    sess.events.push(FileEvent { path: f.into(), op: "write", strength: "observed", line: 0, ts, tool: "patch".into(), locator: locator.into() });
                }
            }
        }
        _ => {}
    }
}

pub fn parse_opencode_db(db: &Path, row: &OcSessionRow, max_parts: usize) -> rusqlite::Result<Session> {
    let c = open_ro(db)?;
    let mut sess = Session {
        provider: "opencode",
        native_id: row.id.clone(),
        cwd: Some(row.directory.clone()),
        title: Some(clip(&row.title, 120)),
        started: row.created,
        updated: row.updated,
        source_path: format!("{}#session={}", db.display(), row.id),
        ..Default::default()
    };
    let mut st = c.prepare(
        "SELECT p.id, p.data, p.time_created, json_extract(m.data,'$.role') FROM part p JOIN message m ON m.id=p.message_id
         WHERE p.session_id=?1 ORDER BY p.time_created, p.id LIMIT ?2",
    )?;
    let rows = st.query_map(rusqlite::params![row.id, max_parts as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<String>>(3)?)))?;
    let mut n = 0;
    for (pid, data, t, role) in rows.flatten() {
        n += 1;
        let ts = Some(t / 1000);
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            sess.warnings += 1;
            continue;
        };
        let is_text = v.get("type").and_then(|x| x.as_str()) == Some("text") && v.get("synthetic").and_then(|x| x.as_bool()) != Some(true);
        if role.as_deref() == Some("user") && is_text {
            let t = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
            sess.prompt(ts, t, 0);
            sess.message(ts, "user", t, 0);
        } else if role.as_deref() == Some("assistant") && is_text {
            sess.message(ts, "assistant", v.get("text").and_then(|x| x.as_str()).unwrap_or(""), 0);
        } else {
            opencode_part(&mut sess, &v, ts, &pid);
        }
    }
    sess.truncated = n >= max_parts;
    Ok(sess)
}

/// Legacy OpenCode JSON storage: `storage/session/**/<ses>.json`,
/// `storage/message/<ses>/<msg>.json`, `storage/part/<msg>/<part>.json`.
pub fn legacy_opencode_session_files(storage: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![storage.join("session")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(p),
                Ok(ft) if ft.is_file() && p.extension().map(|x| x == "json").unwrap_or(false) => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

pub fn legacy_opencode_info(path: &Path) -> Option<(String, String, String, i64, i64)> {
    let v: Value = serde_json::from_slice(&crate::util::read_head(path, 1 << 20).ok()?).ok()?;
    let id = s(&v, "id")?;
    let dir = s(&v, "directory")?;
    let created = v.pointer("/time/created").and_then(|x| x.as_i64()).unwrap_or(0) / 1000;
    let updated = v.pointer("/time/updated").and_then(|x| x.as_i64()).unwrap_or(0) / 1000;
    Some((id, dir, s(&v, "title").unwrap_or_default(), created, updated))
}

fn sorted_json(dir: &Path, cap: usize) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "json").unwrap_or(false)).collect()).unwrap_or_default();
    v.sort();
    v.truncate(cap);
    v
}

pub fn parse_opencode_legacy(storage: &Path, info_path: &Path, max_parts: usize) -> Option<Session> {
    let (id, dir, title, created, updated) = legacy_opencode_info(info_path)?;
    let mut sess = Session {
        provider: "opencode",
        native_id: id.clone(),
        cwd: Some(dir),
        title: Some(clip(&title, 120)),
        started: created,
        updated,
        source_path: info_path.display().to_string(),
        ..Default::default()
    };
    let mut seen = 0;
    for msg in sorted_json(&storage.join("message").join(&id), 10_000) {
        let Some(m) = crate::util::read_head(&msg, 4 << 20).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
            sess.warnings += 1;
            continue;
        };
        let mid = s(&m, "id").unwrap_or_default();
        let role = s(&m, "role").unwrap_or_default();
        let mts = m.pointer("/time/created").and_then(|x| x.as_i64()).map(|t| t / 1000);
        for part in sorted_json(&storage.join("part").join(&mid), 10_000) {
            seen += 1;
            if seen > max_parts {
                sess.truncated = true;
                return Some(sess);
            }
            let Some(v) = crate::util::read_head(&part, 16 << 20).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
                sess.warnings += 1;
                continue;
            };
            let loc = part.display().to_string();
            let is_text = v.get("type").and_then(|x| x.as_str()) == Some("text") && v.get("synthetic").and_then(|x| x.as_bool()) != Some(true);
            if role == "user" && is_text {
                let t = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
                sess.prompt(mts, t, 0);
                sess.message(mts, "user", t, 0);
            } else if role == "assistant" && is_text {
                sess.message(mts, "assistant", v.get("text").and_then(|x| x.as_str()).unwrap_or(""), 0);
            } else {
                opencode_part(&mut sess, &v, mts, &loc);
            }
        }
    }
    Some(sess)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_headers_from_js_literal() {
        let js = r#"text(await tools.apply_patch("*** Begin Patch\n*** Add File: /r/a.md\n+x\n*** Update File: src/b.rs\n@@\n*** End Patch"))"#;
        assert_eq!(patch_paths(js), vec!["/r/a.md", "src/b.rs"]);
    }

    #[test]
    fn shell_tokens_are_candidates_only() {
        let t = shell_path_tokens("rg -n foo src/main.rs && cat README.md | head -5 > /tmp/x.txt");
        assert!(t.contains(&"src/main.rs".to_string()) && t.contains(&"README.md".to_string()));
        assert!(!t.iter().any(|x| x == "-n" || x == "foo"));
    }

    #[test]
    fn injected_prompts_are_ignored() {
        let mut s = Session::default();
        s.prompt(Some(1), "<system-reminder>x</system-reminder>", 1);
        assert!(s.first_prompt.is_none());
        s.prompt(Some(2), "fix the build", 2);
        assert_eq!(s.first_prompt.unwrap().1, "fix the build");
    }
}
