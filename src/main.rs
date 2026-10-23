#![allow(clippy::type_complexity, clippy::too_many_arguments, clippy::format_collect)]

mod ask;
mod buildinfo;
mod config;
mod db;
mod discover;
mod docs;
mod git;
mod hook;
mod hook_prompt;
mod install;
mod overview;
mod relations;
mod remote;
mod resume;
mod scan;
mod search;
mod sessions;
mod snapshot;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod testutil;
mod trust;
mod util;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "workmap — local map of repos, worktrees, docs, commits and agent conversations

USAGE:
  workmap [overview] [--limit N] [--no-install]          every project (local and remote-only) by name
  workmap context [<project>] [--docs N] [--files N] [--commits N] [--sessions N] [--budget BYTES] [--weak]
                                                       compact map: quoted purpose (doc path + line), then typed,
                                                       directed connections grouped by source (default budget 4000
                                                       bytes, whole lines); weak/unconfirmed edges only with --weak
  workmap context [<project>] --detailed [--docs N] [--files N] [--commits N] [--sessions N]
                                                       previous sections view: docs, key files with why, commits,
                                                       conversations with access codes, related projects
  workmap context [<project>] --task 'words' [--file PATH]... [--budget BYTES]
                                                       ranked context for a task/files; each item says why
  workmap search WORDS... [--project P [--worktree W --this-worktree]] [--kind K[,K]] [--provider P]
                 [--speaker user|assistant] [--since YYYY-MM-DD] [--limit N] [--any|--exact] [--here] [--explain]
                                                       full-text search: project, doc, commit, message, remote;
                                                       tiers exact → prefix → stem (→ any with --any; substring
                                                       for sha/path/identifier queries), each hit labeled;
                                                       --here (= --this-worktree): only the current worktree;
                                                       --explain: per-tier queries/counts, why hits are missing
  workmap expand REF [--limit N]                       one item and everything related to it (alias: explore)
  workmap related [<project>] [--depth N] [--limit N]  related projects with evidence (<project> may be remote:<id>)
  workmap why <project> <other> [--depth N]            shortest evidence chain between two projects (either may be remote:<id>)
  workmap ask REF <question> [--dry-run]               read-only continuation of a stored conversation
  workmap scan [--root DIR]... [--no-home] [--full] [--no-sessions] [--no-remote] [--refresh-remote] [--[no-]snapshots]
               [--no-refresh] [--max-dirs N] [--commits N] [--quiet] [--verbose]
  workmap roots add <DIR> | roots list
  workmap remote status|list|refresh|enable|disable    gh/glab catalog of repos you can access (metadata only)
  workmap remote snapshots status|enable|disable|set KEY VALUE
                                                       opt-in: index docs/manifests of remote-only repos from
                                                       temporary shallow clones (cached per commit)
  workmap ingest --transcript FILE... [--checkout DIR]... [--quiet]
                                                       index only these sources now (no home walk, no pruning,
                                                       no remote sync); OpenCode: --transcript '<db>#session=<id>'.
                                                       `install` adds a daily systemd user timer running a full scan
  workmap hook <event> --harness claude|codex [--explain]
                                                       read a host hook event (JSON) on stdin; print bounded
                                                       context or nothing; always exits 0 (used by installed hooks);
                                                       --explain prints why each passage was kept or dropped to stderr
  workmap hooks status|install|uninstall [--harness claude|codex] [--scope user|project] [--dry-run]
                                                       opt-in host hooks: context at session start / per prompt,
                                                       ingest on stop; `hooks status` shows the support matrix
  workmap resume REF [--cwd DIR] [--dry-run]           interactive native resume of the ORIGINAL session in its
                                                       own CLI and recorded cwd (writes to it; `ask` is read-only)
  workmap install [--no-activate] | workmap uninstall
  workmap status | workmap --version

REF is <project>:<rest>, or :<rest> with a scope (--project P or WORKMAP_PROJECT=P;
WORKMAP_WORKTREE=W picks a non-main worktree). <rest> is one of:
  <path>                       a file/doc: conversations that read/wrote it, docs naming it, commits
  <doc>#<heading-slug>         one doc section, read from the file
  commit:<sha-prefix>          one commit
  conversation:<title>         one agent conversation (exact title or unique title prefix)
  session:<native-id-prefix>   the same, by the provider's session id
  message:<session-prefix>:<l<line>|p<n>>   one message with its neighbours (printed by search)
  related:<other project>      all relation evidence between the two projects
Also: remote:<host/namespace/name>[:<path>[#section]] (a repository you can access but have not cloned;
paths come from an opt-in snapshot at a recorded commit), <project>,
an absolute path, an internal id, <project> --conversation <title> [--provider P] [--date D] [--session ID].

Scans $HOME plus persisted roots. State: ~/.local/state/workmap (WORKMAP_STATE_DIR; WORKMAP_HOME replaces
$HOME for scanning, provider stores and install targets). Remote discovery uses gh/glab only when they are
already authenticated; opt out with `workmap remote disable`, WORKMAP_REMOTE=0 or scan --no-remote.
Nothing is sent anywhere else. Help never installs anything. `ask` runs the provider's own resume read-only or refuses.";

struct Args {
    pos: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(v: Vec<String>) -> Args {
        let valued = [
            "--kind",
            "--since",
            "--depth",
            "--task",
            "--file",
            "--budget",
            "--speaker",
            "--limit",
            "--root",
            "--max-dirs",
            "--commits",
            "--files",
            "--sessions",
            "--docs",
            "--worktree",
            "--conversation",
            "--provider",
            "--date",
            "--session",
            "--project",
            "--transcript",
            "--cwd",
            "--harness",
            "--checkout",
            "--scope",
        ];
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
            map: view::MapLimits {
                docs: self.num("--docs", d.docs),
                files: self.num("--files", d.files),
                commits: self.num("--commits", d.commits),
                sessions: self.num("--sessions", d.sessions),
                related: d.related,
            },
            detailed: self.has("--detailed"),
            map_opts: view::map::MapOpts { budget: self.num("--budget", view::map::DEFAULT_BUDGET).max(view::map::MIN_BUDGET), weak: self.has("--weak") },
        }
        .scoped(self.get("--project"), env("WORKMAP_PROJECT"), env("WORKMAP_WORKTREE"))
    }
    fn all(&self, k: &str) -> Vec<String> {
        self.flags.iter().filter(|f| f.0 == k).filter_map(|f| f.1.clone()).collect()
    }
}

fn run() -> Result<(), String> {
    let a = Args::parse(std::env::args().skip(1).collect());
    if a.has("--version") || a.has("-V") || a.pos.first().map(|s| s == "version").unwrap_or(false) {
        println!("workmap {}", version());
        return Ok(());
    }
    if a.has("--help") || a.has("-h") || a.pos.first().map(|s| s == "help").unwrap_or(false) {
        println!("{HELP}");
        return Ok(());
    }
    let env = config::Env::from_process();
    if a.pos.first().map(|s| s == "hook").unwrap_or(false) {
        // Host hook: must never fail or block the host session, so it runs before
        // (and instead of) the regular index open, which creates/migrates and waits.
        hook::main(&env, a.pos.get(1).map(String::as_str), a.get("--harness").as_deref().unwrap_or("claude"), cli_cmd(), a.has("--explain"));
        return Ok(());
    }
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
            if cmd == "context" && (a.has("--task") || a.has("--file")) {
                let files = a.all("--file");
                let task = a.get("--task");
                let t = if t == "." && req.project.is_none() { ".".to_string() } else { t };
                print!("{}", view::task(&v, &t, &req, &view::more::TaskReq { task: task.as_deref(), files: &files, budget: a.num("--budget", 6000).max(800) })?);
                return Ok(());
            }
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
                remote: remote::mode(&env, a.has("--no-remote"), a.has("--refresh-remote")),
                snapshots: if a.has("--no-snapshots") || a.has("--no-remote") {
                    Some(false)
                } else if a.has("--snapshots") {
                    Some(true)
                } else {
                    None
                },
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
                println!(
                    "indexed: {} searchable messages, {} truncated/capped conversations, {} relation evidence rows, {} checkouts pruned, {} missing (kept: conversations recorded there)",
                    st.messages, st.sessions_truncated, st.relations, st.checkouts_pruned, st.checkouts_missing
                );
                for r in &st.remote {
                    println!("remote: {r}");
                }
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
        "search" | "find" => {
            let v = view::View { c: &c, cmd: cli_cmd(), state: state_vars() };
            print!("{}", search_cmd(&v, &a)?);
        }
        "related" => {
            let req = a.req();
            let v = view::View { c: &c, cmd: cli_cmd(), state: state_vars() };
            print!("{}", view::more::related_cmd(&v, a.pos.get(1).map(String::as_str), &req, a.num("--depth", 2).min(6), a.num("--limit", 15))?);
        }
        "why" => {
            let req = a.req();
            let v = view::View { c: &c, cmd: cli_cmd(), state: state_vars() };
            let (from, to) = match (a.pos.get(1), a.pos.get(2)) {
                (Some(x), Some(y)) => (Some(x.as_str()), y.clone()),
                (Some(y), None) if req.project.is_some() => (None, y.clone()),
                _ => return Err("usage: workmap why <project> <other> [--depth N]   (or: why <other> with --project/WORKMAP_PROJECT)".into()),
            };
            print!("{}", view::more::why_cmd(&v, from, &to, &req, a.num("--depth", 4).min(8))?);
        }
        "remote" => match a.pos.get(1).map(String::as_str).unwrap_or("status") {
            "status" => {
                print!("{}", remote::status(&env, &c).map_err(|e| e.to_string())?);
                print!("{}", snapshot::status(&env, &c));
            }
            "snapshots" => match a.pos.get(2).map(String::as_str).unwrap_or("status") {
                "status" => print!("{}", snapshot::status(&env, &c)),
                "enable" => {
                    snapshot::set(&env, "snapshots", "on")?;
                    println!("remote snapshots enabled: the next scan takes temporary shallow clones of in-scope remote-only repositories (see `workmap remote snapshots status`)");
                }
                "disable" => {
                    snapshot::set(&env, "snapshots", "off")?;
                    snapshot::clear_all(&c).map_err(|e| e.to_string())?;
                    println!("remote snapshots disabled; snapshot data deleted from the index");
                }
                "set" => {
                    let (Some(k), Some(v)) = (a.pos.get(3), a.pos.get(4)) else {
                        let keys: Vec<String> = snapshot::KEYS.iter().map(|(k, d)| format!("  {k}: {d}")).collect();
                        return Err(format!("usage: workmap remote snapshots set KEY VALUE\n{}", keys.join("\n")));
                    };
                    snapshot::set(&env, k, v)?;
                    println!("{k} = {v}");
                }
                x => return Err(format!("unknown snapshots subcommand {x}; use status|enable|disable|set KEY VALUE")),
            },
            "list" => {
                let rows = remote::remote_only(&c, a.num("--limit", 200)).map_err(|e| e.to_string())?;
                println!("{} remote-only repositories (not cloned; metadata from gh/glab):", rows.len());
                for (id, vis, archived, desc, stale) in rows {
                    println!("- remote:{id} {vis}{}{} {}", if archived { " archived" } else { "" }, if stale { " (stale)" } else { "" }, util::clip(&desc, 80));
                }
            }
            "refresh" => {
                if remote::disabled(&env) {
                    return Err("remote discovery is disabled (workmap remote enable to turn it back on)".into());
                }
                let tx = c.transaction().map_err(|e| e.to_string())?;
                let mut st = scan::Stats::default();
                remote::sync(&env, &tx, remote::Mode::Force, &mut st).map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                for l in st.remote {
                    println!("{l}");
                }
                println!("relations to remote-only entries update on the next `workmap scan`");
            }
            "disable" => {
                std::fs::create_dir_all(&env.state_dir).map_err(|e| e.to_string())?;
                std::fs::write(env.state_dir.join("no-remote"), "remote discovery disabled by `workmap remote disable`\n").map_err(|e| e.to_string())?;
                remote::clear(&c).map_err(|e| e.to_string())?;
                println!("remote discovery disabled; cached remote metadata deleted from {}", env.db_path().display());
            }
            "enable" => {
                let _ = std::fs::remove_file(env.state_dir.join("no-remote"));
                println!("remote discovery enabled (takes effect on the next scan; WORKMAP_REMOTE=0 still overrides)");
            }
            x => return Err(format!("unknown remote subcommand {x}; use status|list|refresh|enable|disable|snapshots")),
        },
        "uninstall" => {
            for l in install::uninstall(&env).map_err(|e| e.to_string())? {
                println!("{l}");
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
            if a.pos.get(1).map(|t| t.starts_with("remote:")).unwrap_or(false) {
                return Err("ask needs a conversation recorded in a local native transcript; remote snapshots contain no conversations".into());
            }
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
            print!("{}", view::more::ask_note(&c, sid));
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
            println!("db: {} ({} bytes)", env.db_path().display(), std::fs::metadata(env.db_path()).map(|m| m.len()).unwrap_or(0));
            print!("{}", trust::index_line(&c));
            for k in ["repo", "checkout", "doc", "memory", "file", "commit", "session", "remote"] {
                println!("{k}: {}", q(&format!("SELECT count(*) FROM nodes WHERE kind='{k}'")));
            }
            println!("edges: {} (relation evidence: {})", q("SELECT count(*) FROM edges"), q("SELECT count(*) FROM edges WHERE kind LIKE 'rel:%'"));
            println!("searchable passages: {}", q("SELECT count(*) FROM passages"));
            for k in search::KINDS {
                println!("  {k}: {}", q(&format!("SELECT count(*) FROM passages WHERE kind='{k}'")));
            }
            print!("{}", trust::coverage_line(&trust::coverage(&c, None)));
            for (p, s, n) in trust::sources(&c) {
                println!("  sources {p} {s}: {n}");
            }
            println!("  (unrelated = transcript cwd outside every indexed checkout; missing = deleted since indexed, evidence kept; error = could not be parsed)");
            print!("{}", remote::status(&env, &c).map_err(|e| e.to_string())?);
            let last: String = c.query_row("SELECT stats FROM scans WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap_or_default();
            println!("last scan stats: {last}");
        }
        "ingest" => {
            let o = scan::IngestOpts { transcripts: a.all("--transcript").iter().map(PathBuf::from).collect(), checkouts: a.all("--checkout").iter().map(PathBuf::from).collect() };
            if o.transcripts.is_empty() && o.checkouts.is_empty() {
                return Err("usage: workmap ingest --transcript FILE... [--checkout DIR]... [--quiet]".into());
            }
            let r = scan::ingest(&env, &mut c, &o)?;
            if !a.has("--quiet") {
                for l in &r.lines {
                    println!("{l}");
                }
                println!("ingest: {} ms, {} source(s), {} checkout(s); nothing else was walked, pruned or synced (full refresh: workmap scan)", r.ms, o.transcripts.len(), r.checkouts);
            }
        }
        "hook" => unreachable!("dispatched before the index is opened"),
        "hooks" => {
            let harness = a.get("--harness").unwrap_or_else(|| "claude".into());
            let scope = match a.get("--scope").as_deref() {
                None | Some("user") => hook::Scope::User,
                Some("project") => hook::Scope::Project,
                Some(x) => return Err(format!("--scope {x}: use user or project")),
            };
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            match a.pos.get(1).map(String::as_str).unwrap_or("status") {
                "status" => print!("{}", hook::status(&env, &cwd)),
                "install" => {
                    // Prefer the stable copy `workmap install` maintains over a build directory.
                    let canonical = env.home.join(".local/share/workmap/bin/workmap");
                    let bin = if canonical.is_file() { canonical } else { std::env::current_exe().map_err(|e| e.to_string())? };
                    if !bin.starts_with(env.home.join(".local/share/workmap")) {
                        eprintln!("note: hooks will run {} (no `workmap install` copy found); run `workmap install` first if this is a build directory", bin.display());
                    }
                    for l in hook::install(&env, &hook::HooksOpts { harness: &harness, scope, cwd: &cwd, bin: &bin, dry_run: a.has("--dry-run") })? {
                        println!("{l}");
                    }
                }
                "uninstall" => {
                    let l = hook::uninstall(&env, &harness, scope, &cwd, a.has("--dry-run"))?;
                    if l.is_empty() {
                        println!("no workmap hooks in {}", hook::settings_path(&env, &harness, scope, &cwd)?.display());
                    }
                    for l in l {
                        println!("{l}");
                    }
                }
                x => return Err(format!("unknown hooks subcommand {x}; use status|install|uninstall [--harness claude|codex] [--scope user|project] [--dry-run]")),
            }
        }
        "resume" => {
            let req = a.req();
            let target =
                a.pos.get(1).ok_or("usage: workmap resume REF [--cwd DIR] [--dry-run]   (REF = :conversation:<title> | :session:<id-prefix> with a scope, <project>:session:<id>, or a session id)")?;
            let is_ref = target.starts_with(':') || target.contains(":conversation:") || target.contains(":session:");
            let sid = if is_ref || req.conv.title.is_some() || req.conv.session.is_some() {
                view::ask_session(&view::View { c: &c, cmd: cli_cmd(), state: state_vars() }, target, &req)?
            } else {
                resume::find_session(&c, target)?
            };
            let cwd = a.get("--cwd").map(PathBuf::from);
            let p = resume::plan(&c, &sid, cwd.as_deref(), &ask::real_help).map_err(|e| format!("resume refused: {e}"))?;
            print!("{}", view::more::ask_note(&c, &sid));
            print!("{}", p.render());
            if !a.has("--dry-run") {
                let code = resume::execute(&p).map_err(|e| format!("{}: {e}", p.program))?;
                if code != 0 {
                    return Err(format!("{} exited with {code}", p.program));
                }
            }
        }
        other => return Err(format!("unknown command {other:?}\n\n{HELP}")),
    }
    Ok(())
}

/// `<semver> (<revision>[, <build>])`; see build.rs for where the revision comes from.
fn version() -> String {
    buildinfo::render(env!("CARGO_PKG_VERSION"), env!("WORKMAP_BUILD_REV"), env!("WORKMAP_BUILD_PROFILE"), env!("WORKMAP_BUILD_TARGET"))
}

/// How printed follow-up commands invoke this binary.
/// `workmap search …` (kept out of `run` so tests can execute printed commands).
fn search_cmd(v: &view::View, a: &Args) -> Result<String, String> {
    let words = a.pos[1..].join(" ");
    if words.trim().is_empty() {
        return Err("usage: workmap search WORDS... [--project P [--worktree W] [--here]] [--kind doc,message,commit,project,remote,file] [--any|--exact] [--explain]".into());
    }
    let kinds: Vec<String> = a.all("--kind").iter().flat_map(|k| k.split(',')).map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
    if let Some(bad) = kinds.iter().find(|k| !search::SEARCH_KINDS.contains(&k.as_str())) {
        return Err(format!("unknown --kind {bad}; kinds: {}", search::SEARCH_KINDS.join(", ")));
    }
    let since = match a.get("--since") {
        Some(d) => Some(search::date_to_ts(&d).ok_or_else(|| format!("--since {d}: expected YYYY-MM-DD"))?),
        None => None,
    };
    let query = search::Query { text: words, kinds, provider: a.get("--provider"), speaker: a.get("--speaker"), since, limit: a.num("--limit", 10), any: a.has("--any"), exact: a.has("--exact") };
    let here = a.has("--this-worktree") || a.has("--here");
    let mut req = a.req();
    let env_project = a.get("--project").is_none() && req.project.is_some();
    if here && req.project.is_none() && a.get("--project").is_none() {
        // `--here` without a scope: the checkout containing the current directory.
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let p = view::resolve_project(v.c, &cwd.to_string_lossy()).map_err(|e| format!("--here: {e}"))?;
        req.project = Some(p.focus.path.clone().unwrap_or(p.handle));
    }
    if here && req.project.is_none() {
        return Err("--this-worktree/--here needs a project: add --project P or run inside an indexed checkout".into());
    }
    let opts = view::more::SearchOpts { explain: a.has("--explain"), env_project, env_worktree: a.get("--worktree").is_none() && req.scope_worktree.is_some() };
    view::more::search_view_opts(v, &req, &view::more::SearchReq { query, this_worktree: here }, &opts)
}

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
