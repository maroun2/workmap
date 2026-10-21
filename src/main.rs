#![allow(clippy::type_complexity, clippy::too_many_arguments, clippy::format_collect)]

mod ask;
mod config;
mod db;
mod discover;
mod docs;
mod git;
mod install;
mod overview;
mod relations;
mod remote;
mod scan;
mod sessions;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod testutil;
mod util;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "workmap — local map of repos, worktrees, docs, commits and agent conversations

USAGE:
  workmap [overview] [--limit N] [--no-install]
  workmap context [<project>] [--project P] [--worktree W] [--docs N] [--files N] [--commits N] [--sessions N]
  workmap expand REF [--project P] [--worktree W] [--limit N]      (alias: explore)
  workmap ask REF <question> [--dry-run]

  REF is <project>:<rest>, or :<rest> when a scope is set with --project P or WORKMAP_PROJECT=P
  (WORKMAP_WORKTREE=W picks a non-main worktree of the scope project). <rest> is one of:
    <path>                     a file/doc: conversations that read/wrote it, docs naming it, commits
    <doc>#<heading-slug>       one doc section, read from the file
    commit:<sha-prefix>        one commit
    conversation:<title>       one agent conversation (exact title or unique title prefix)
    session:<native-id-prefix> the same, by the provider's session id
  Also accepted: <project> (project map), an absolute path, an internal id,
  <project> --conversation <title> [--provider P] [--date YYYY-MM-DD] [--session ID],
  and `ask <session-id> <question>`.
  workmap scan [--root DIR]... [--no-home] [--full] [--no-sessions] [--no-refresh] [--max-dirs N] [--commits N] [--quiet] [--verbose]
  workmap roots add <DIR> | roots list
  workmap install [--no-activate]
  workmap status

Scans $HOME plus persisted roots. State: ~/.local/state/workmap (override WORKMAP_STATE_DIR;
WORKMAP_HOME replaces $HOME for scanning, provider sources and install targets).
Projects are named by their label (see `workmap overview`); ambiguous names fail with choices.
--worktree <branch|dir> picks a checkout of the project. Help never installs anything. `ask` runs the provider's own resume read-only or refuses.";

struct Args {
    pos: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(v: Vec<String>) -> Args {
        let valued = ["--limit", "--root", "--max-dirs", "--commits", "--files", "--sessions", "--docs", "--worktree", "--conversation", "--provider", "--date", "--session", "--project"];
        let mut a = Args { pos: vec![], flags: vec![] };
        let mut it = v.into_iter();
        while let Some(x) = it.next() {
            if x == "--" {
                a.pos.extend(it.by_ref());
                break;
            }
            if let Some((k, val)) = x.split_once('=').filter(|_| x.starts_with("--")) {
                a.flags.push((k.to_string(), Some(val.to_string())));
            } else if valued.contains(&x.as_str()) {
                let val = it.next();
                a.flags.push((x, val));
            } else if x.starts_with("--") || x == "-h" {
                a.flags.push((x, None));
            } else {
                a.pos.push(x);
            }
        }
        a
    }
    fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|f| f.0 == k)
    }
    fn num(&self, k: &str, d: usize) -> usize {
        self.flags.iter().rev().find(|f| f.0 == k).and_then(|f| f.1.as_ref()).and_then(|v| v.parse().ok()).unwrap_or(d)
    }
    fn get(&self, k: &str) -> Option<String> {
        self.flags.iter().rev().find(|f| f.0 == k).and_then(|f| f.1.clone())
    }
    fn req(&self) -> view::Req {
        let d = view::MapLimits::default();
        let env = |k: &str| std::env::var(k).ok();
        view::Req {
            project: None,
            scope_worktree: None,
            worktree: self.get("--worktree"),
            conv: view::ConvSel { title: self.get("--conversation"), provider: self.get("--provider"), date: self.get("--date"), session: self.get("--session") },
            limit: self.get("--limit").and_then(|x| x.parse().ok()),
            map: view::MapLimits { docs: self.num("--docs", d.docs), files: self.num("--files", d.files), commits: self.num("--commits", d.commits), sessions: self.num("--sessions", d.sessions) },
        }
        .scoped(self.get("--project"), env("WORKMAP_PROJECT"), env("WORKMAP_WORKTREE"))
    }
    fn all(&self, k: &str) -> Vec<String> {
        self.flags.iter().filter(|f| f.0 == k).filter_map(|f| f.1.clone()).collect()
    }
}

fn run() -> Result<(), String> {
    let a = Args::parse(std::env::args().skip(1).collect());
    if a.has("--help") || a.has("-h") || a.pos.first().map(|s| s == "help").unwrap_or(false) {
        println!("{HELP}");
        return Ok(());
    }
    let env = config::Env::from_process();
    let mut c = db::open(&env.db_path()).map_err(|e| format!("open index {}: {e}", env.db_path().display()))?;
    let cmd = a.pos.first().cloned().unwrap_or_else(|| "overview".into());
    match cmd.as_str() {
        "overview" => {
            // Optional self-install on first default invocation, only when configured.
            let wants = std::env::var_os("WORKMAP_AUTO_INSTALL").is_some() || env.state_dir.join("auto-install").exists();
            if a.pos.is_empty() && wants && !a.has("--no-install") && !env.state_dir.join("installed").exists() {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                for l in install::install(&env, &c, &exe, &install::InstallOpts { activate: true }).map_err(|e| e.to_string())? {
                    eprintln!("install: {l}");
                }
            }
            print!("{}{}", env_note(), overview::overview(&c, a.num("--limit", 30), &cli_cmd()).map_err(|e| e.to_string())?);
        }
        "context" | "expand" | "explore" => {
            let req = a.req();
            // No target: the scope project (if any); `context` alone keeps the old cwd behaviour.
            let t = a.pos.get(1).cloned().unwrap_or_else(|| {
                if req.project.is_some() {
                    String::new()
                } else if cmd == "context" {
                    ".".into()
                } else {
                    "?".into()
                }
            });
            if t == "?" {
                return Err(format!(
                    "usage: workmap expand REF   (REF = <project>[:<path>[#section] | :commit:<sha> | :conversation:<title> | :session:<id>], or :<…> with --project/WORKMAP_PROJECT)\n{}",
                    view::SCOPE_HELP
                ));
            }
            if cmd == "context" && a.has("--ids") {
                // Legacy evidence view with internal ids.
                let n = overview::resolve_target(&c, &t).map_err(|e| e.to_string())?.ok_or_else(|| format!("nothing indexed matches {t:?}"))?;
                print!("{}", overview::context(&c, &n, &overview::Limits::default()).map_err(|e| e.to_string())?);
                return Ok(());
            }
            let v = view::View { c: &c, cmd: cli_cmd(), state: state_vars() };
            let out = if cmd == "context" { view::context(&v, &t, &req)? } else { view::expand(&v, &t, &req)? };
            print!("{out}");
        }
        "scan" => {
            let opts = scan::ScanOpts {
                roots: a.all("--root").iter().map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| PathBuf::from(r))).collect(),
                include_home: !a.has("--no-home"),
                max_dirs: a.num("--max-dirs", 300_000),
                commits: a.num("--commits", 30),
                verbose: a.has("--verbose"),
                full: a.has("--full"),
                sessions: !a.has("--no-sessions"),
                ..Default::default()
            };
            let st = scan::run(&env, &mut c, &opts).map_err(|e| e.to_string())?;
            let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            let bytes = std::fs::metadata(env.db_path()).map(|m| m.len()).unwrap_or(0) + std::fs::metadata(env.db_path().with_extension("db-wal")).map(|m| m.len()).unwrap_or(0);
            if !a.has("--quiet") {
                println!(
                    "scan: {} ms, {} repos, {} checkouts ({} unchanged), {} docs, sessions parsed={} reused={} unrelated-sources={} missing-sources={} parse-warnings={} dirs={} db={} bytes",
                    st.ms,
                    st.repos,
                    st.checkouts,
                    st.checkouts_reused,
                    st.docs,
                    st.sessions_parsed,
                    st.sessions_reused,
                    st.sources_unrelated,
                    st.sources_missing,
                    st.parse_warnings,
                    st.dirs_visited,
                    bytes
                );
                for w in &st.warnings {
                    println!("warning: {w}");
                }
            }
            // Keep installed skill overviews current; only existing workmap-owned files.
            if !a.has("--no-refresh") {
                for l in install::refresh_skills(&env, &c).map_err(|e| format!("skill refresh: {e}"))? {
                    if !a.has("--quiet") || !l.starts_with("unchanged") {
                        println!("{l}");
                    }
                }
            }
        }
        "roots" => match a.pos.get(1).map(String::as_str) {
            Some("add") => {
                let p = a.pos.get(2).ok_or("usage: workmap roots add <DIR>")?;
                let added = env.add_root(std::path::Path::new(p)).map_err(|e| format!("{p}: {e}"))?;
                println!("{} {}", if added { "added" } else { "already present" }, p);
            }
            Some("list") | None => {
                println!("home (default): {}", env.home.display());
                for r in env.load_roots() {
                    println!("{}", r.display());
                }
            }
            Some(x) => return Err(format!("unknown roots subcommand {x}")),
        },
        "ask" => {
            let req = a.req();
            let target = a.pos.get(1).ok_or(
                "usage: workmap ask REF <question> [--dry-run]   (REF = :conversation:<title> | :session:<id-prefix> with a project scope; or <project> --conversation <title>; or <session-id>)",
            )?;
            let q = a.pos[2..].join(" ");
            if q.trim().is_empty() {
                return Err("missing question".into());
            }
            let is_ref = target.starts_with(':') || target.contains(":conversation:") || target.contains(":session:");
            let sid = if is_ref || req.conv.title.is_some() || req.conv.session.is_some() {
                view::ask_session(&view::View { c: &c, cmd: cli_cmd(), state: state_vars() }, target, &req)?
            } else {
                target.clone()
            };
            let sid = &sid;
            let p = ask::plan(&c, sid, &q, &ask::real_help).map_err(|e| format!("ask refused: {e}"))?;
            print!("{}", p.render());
            if !a.has("--dry-run") {
                let code = ask::execute(&p).map_err(|e| e.to_string())?;
                if code != 0 {
                    return Err(format!("{} exited with {code}", p.program));
                }
            }
        }
        "install" => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            for l in install::install(&env, &c, &exe, &install::InstallOpts { activate: !a.has("--no-activate") }).map_err(|e| e.to_string())? {
                println!("{l}");
            }
        }
        "status" => {
            let q = |s: &str| c.query_row(s, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
            println!("db: {}", env.db_path().display());
            for k in ["repo", "checkout", "doc", "memory", "file", "commit", "session"] {
                println!("{k}: {}", q(&format!("SELECT count(*) FROM nodes WHERE kind='{k}'")));
            }
            println!("edges: {}", q("SELECT count(*) FROM edges"));
            let last: String = c.query_row("SELECT stats FROM scans WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap_or_default();
            println!("last scan: {last}");
        }
        other => return Err(format!("unknown command {other:?}\n\n{HELP}")),
    }
    Ok(())
}

/// How printed follow-up commands invoke this binary.
fn cli_cmd() -> String {
    if let Ok(c) = std::env::var("WORKMAP_CMD") {
        return c;
    }
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok());
    let on_path = std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join("workmap")).find(|p| p.is_file())).and_then(|p| std::fs::canonicalize(p).ok());
    match (exe, on_path) {
        (Some(e), Some(p)) if e == p => "workmap".into(),
        (Some(e), _) => util::sh_quote(&e.display().to_string()),
        _ => "workmap".into(),
    }
}

/// Non-default environment the printed references depend on (`WORKMAP_STATE_DIR=…`).
fn state_vars() -> String {
    ["WORKMAP_STATE_DIR", "WORKMAP_HOME"].iter().filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={}", util::sh_quote(&v)))).collect::<Vec<_>>().join(" ")
}

/// Printed commands rely on the same index; say so when it is not the default.
fn env_note() -> String {
    let vars: Vec<String> = ["WORKMAP_STATE_DIR", "WORKMAP_HOME"].iter().filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={}", util::sh_quote(&v)))).collect();
    if vars.is_empty() {
        String::new()
    } else {
        format!("(commands below assume: export {})\n", vars.join(" "))
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("workmap: {e}");
            ExitCode::FAILURE
        }
    }
}
