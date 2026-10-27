//! End-to-end tests over a fake home with a real git repo + worktree and
//! synthetic Claude Code, Codex and OpenCode (SQLite + legacy JSON) stores.

use crate::config::Env;
use crate::testutil::{git, write, TempDir};
use crate::{ask, db, install, overview, scan};
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};

pub(crate) struct Fixture {
    _t: TempDir,
    _outside: TempDir,
    home: PathBuf,
    main: PathBuf,
    feature: PathBuf,
    outside_wt: PathBuf,
    claude_file: PathBuf,
}

fn jl(lines: &[serde_json::Value]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn fixture() -> Fixture {
    let t = TempDir::new("e2e");
    let outside = TempDir::new("outside");
    let home = t.path().join("home");
    let main = home.join("code/proj");
    write(&main.join("README.md"), "# Proj\n\nTracks things.\n\n## Layout\nThe UI lives in `src/app.ts`; see [the guide](docs/guide.md).\n");
    write(&main.join("CLAUDE.md"), "# Agent notes\n\n## Key Files\n- **main.py** - backend\n- **Missing.tsx** - not a real file\n");
    write(&main.join("src/app.ts"), "export {}\n");
    write(&main.join("main.py"), "print(1)\n");
    write(&main.join("docs/guide.md"), "# Guide\nSee ../main.py\n");
    write(&main.join("package.json"), "{\n  \"name\": \"proj\",\n  \"main\": \"src/app.ts\"\n}\n");
    write(&main.join("other.txt"), "x\n");
    write(&main.join("node_modules/dep/.git/HEAD"), "junk\n"); // must be skipped
    git(&main, &["init", "-q"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "initial layout"]);
    let feature = home.join("wt/feature");
    git(&main, &["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
    let outside_wt = outside.path().join("elsewhere");
    git(&main, &["worktree", "add", "-q", "-b", "outside", outside_wt.to_str().unwrap()]);
    let m = main.display().to_string();
    let f = feature.display().to_string();

    // Claude Code transcript (cwd = main checkout).
    let cdir = home.join(".claude/projects").join(crate::util::claude_project_dir_name(&m));
    let claude_file = cdir.join("11111111-aaaa.jsonl");
    let mut body = jl(&[
        json!({"type":"summary","summary":"Investigate backend"}),
        json!({"type":"user","cwd":m,"sessionId":"11111111-aaaa","gitBranch":"main","timestamp":"2026-02-01T10:00:00Z","message":{"role":"user","content":"why does main.py print 1?"}}),
        json!({"type":"assistant","cwd":m,"sessionId":"11111111-aaaa","timestamp":"2026-02-01T10:00:05Z","message":{"content":[
            {"type":"tool_use","id":"t1","name":"Read","input":{"file_path": format!("{m}/main.py")}},
            {"type":"tool_use","id":"t2","name":"Write","input":{"file_path":"docs/guide.md","content":"..."}},
            {"type":"tool_use","id":"t3","name":"Edit","input":{"file_path":"src/app.ts"}},
            {"type":"tool_use","id":"t4","name":"Bash","input":{"command":"cat README.md other.txt nosuch.md"}},
            {"type":"tool_use","id":"t5","name":"Read","input":{"file_path":"/etc/hostname"}}]}}),
        json!({"type":"user","cwd":m,"sessionId":"11111111-aaaa","timestamp":"2026-02-01T10:00:06Z","toolUseResult":{},"message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":"print(1)"},
            {"type":"tool_result","tool_use_id":"t2","content":"ok"},
            {"type":"tool_result","tool_use_id":"t3","is_error":true,"content":"denied"}]}}),
    ]);
    body.push_str("{not json at all\n");
    body.push_str(&jl(&[json!({"type":"user","cwd":m,"sessionId":"11111111-aaaa","timestamp":"2026-02-02T09:00:00Z","message":{"content":[{"type":"text","text":"now add tests"}]}})]));
    body.push_str("{\"type\":\"assistant\",\"partial"); // truncated final line
    write(&claude_file, &body);
    write(&cdir.join("22222222-bbbb.jsonl"), &jl(&[json!({"type":"user","cwd":"/somewhere/else","sessionId":"22222222-bbbb","message":{"content":"unrelated"}})]));
    write(&cdir.join("memory/MEMORY.md"), "# Memory\n- prefers `main.py` untouched\n");

    // Codex rollout (cwd = main checkout).
    let patch = "text(await tools.apply_patch(\"*** Begin Patch\\n*** Update File: src/app.ts\\n@@\\n-a\\n+b\\n*** End Patch\"))".to_string();
    write(
        &home.join(".codex/sessions/2026/02/03/rollout-2026-02-03T00-00-00-cccc.jsonl"),
        &jl(&[
            json!({"timestamp":"2026-02-03T00:00:00Z","type":"session_meta","payload":{"id":"cccc-uuid","cwd":m,"timestamp":"2026-02-03T00:00:00Z","git":{"branch":"main"}}}),
            json!({"timestamp":"2026-02-03T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"refactor the app entry"}}),
            json!({"timestamp":"2026-02-03T00:00:02Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"c1","input":patch}}),
            json!({"timestamp":"2026-02-03T00:00:03Z","type":"event_msg","payload":{"type":"patch_apply_end","success":true,"changes":{format!("{m}/other.txt"):{"type":"update"}}}}),
            json!({"timestamp":"2026-02-03T00:00:04Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":json!({"cmd":"sed -n 1,5p main.py"}).to_string()}}),
            json!({"timestamp":"2026-02-03T00:00:05Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","id":"i1","status":"completed","changes":{format!("{m}/docs/guide.md"):{"type":"update"}}}}}),
        ]),
    );

    // OpenCode SQLite (cwd = feature worktree) — real schema subset.
    let odb = home.join(".local/share/opencode/opencode.db");
    std::fs::create_dir_all(odb.parent().unwrap()).unwrap();
    let c = Connection::open(&odb).unwrap();
    c.execute_batch(
        "CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
        CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
    )
    .unwrap();
    c.execute("INSERT INTO session VALUES('ses_1','p',?1,'Feature work',1767000000000,1767000500000)", [&f]).unwrap();
    c.execute("INSERT INTO message VALUES('m1','ses_1',1767000000000,'{\"role\":\"user\"}'),('m2','ses_1',1767000001000,'{\"role\":\"assistant\"}')", []).unwrap();
    for (id, mid, data) in [
        ("p1", "m1", json!({"type":"text","text":"check main.py in feature"})),
        ("p2", "m2", json!({"type":"tool","tool":"read","state":{"status":"completed","input":{"filePath": format!("{f}/main.py")}}})),
        ("p3", "m2", json!({"type":"tool","tool":"edit","state":{"status":"error","input":{"filePath": format!("{f}/src/app.ts")}}})),
        ("p4", "m2", json!({"type":"patch","hash":"h","files":[format!("{f}/README.md")]})),
    ] {
        c.execute("INSERT INTO part VALUES(?1,?2,'ses_1',1767000002000,?3)", rusqlite::params![id, mid, data.to_string()]).unwrap();
    }
    drop(c);
    // OpenCode legacy JSON storage (cwd = main checkout).
    let st = home.join(".local/share/opencode/storage");
    write(&st.join("session/info/ses_old.json"), &json!({"id":"ses_old","title":"Legacy chat","directory":m,"time":{"created":1760000000000i64,"updated":1760000100000i64}}).to_string());
    write(&st.join("message/ses_old/msg1.json"), &json!({"id":"msg1","role":"user","time":{"created":1760000000000i64}}).to_string());
    write(&st.join("message/ses_old/msg2.json"), &json!({"id":"msg2","role":"assistant","time":{"created":1760000050000i64}}).to_string());
    write(&st.join("part/msg1/prt1.json"), &json!({"type":"text","text":"what is in the guide?"}).to_string());
    write(&st.join("part/msg2/prt2.json"), &json!({"type":"tool","tool":"read","state":{"status":"completed","input":{"filePath": format!("{m}/docs/guide.md")}}}).to_string());

    Fixture { _t: t, _outside: outside, home, main, feature, outside_wt, claude_file }
}

fn scan_once(env: &Env) -> (Connection, scan::Stats) {
    let mut c = db::open(&env.db_path()).unwrap();
    let st = scan::run(env, &mut c, &scan::ScanOpts::default()).unwrap();
    (c, st)
}

fn sid(provider: &str, native: &str) -> String {
    crate::util::stable_id("session", &format!("{provider}:{native}"))
}

fn edges_from(c: &Connection, src: &str) -> Vec<(String, String, String, i64)> {
    let mut st = c.prepare("SELECT e.kind, e.strength, n.path, e.source_line FROM edges e JOIN nodes n ON n.id=e.dst WHERE e.src=?1 AND n.kind IN ('file','doc') ORDER BY 3,1").unwrap();
    let rows = st.query_map([src], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default(), r.get(3)?))).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn has(edges: &[(String, String, String, i64)], kind: &str, strength: &str, path: &Path) -> bool {
    edges.iter().any(|e| e.0 == kind && e.1 == strength && e.2 == path.to_string_lossy())
}

#[test]
fn end_to_end_scan_links_everything() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, st) = scan_once(&env);
    assert_eq!(st.repos, 1, "one repository identity for all checkouts");
    assert_eq!(st.checkouts, 3, "main + home worktree + worktree outside scanned roots");
    assert_eq!(st.sessions_parsed, 4, "claude + codex + opencode db + opencode legacy");

    // Worktree resolution: exact checkout paths, common repo identity.
    let repo: String = c.query_row("SELECT id FROM nodes WHERE kind='repo'", [], |r| r.get(0)).unwrap();
    for p in [&fx.main, &fx.feature, &fx.outside_wt] {
        let canon = std::fs::canonicalize(p).unwrap();
        let (rid, kind): (String, String) = c.query_row("SELECT repo_id, kind FROM nodes WHERE path=?1", [canon.to_string_lossy()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((rid.as_str(), kind.as_str()), (repo.as_str(), "checkout"));
    }

    // Claude: structured tool calls only; Bash paths are mentions.
    let ce = edges_from(&c, &sid("claude", "11111111-aaaa"));
    assert!(has(&ce, "read", "observed", &fx.main.join("main.py")));
    assert!(has(&ce, "write", "observed", &fx.main.join("docs/guide.md")), "relative path resolved against cwd: {ce:?}");
    assert!(has(&ce, "write", "failed", &fx.main.join("src/app.ts")), "is_error result is not an observed write");
    assert!(has(&ce, "mentioned", "mentioned", &fx.main.join("README.md")));
    assert!(!ce.iter().any(|e| e.0 == "read" && e.2.ends_with("README.md")), "shell cat is never read access");
    assert!(!ce.iter().any(|e| e.2.ends_with("nosuch.md") || e.2.contains("/etc/")), "untracked mentions and external paths dropped");
    let line = ce.iter().find(|e| e.0 == "read").unwrap().3;
    assert_eq!(line, 3, "edge points at the transcript line of the tool call");
    let meta: String = c.query_row("SELECT meta FROM nodes WHERE id=?1", [sid("claude", "11111111-aaaa")], |r| r.get(0)).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(meta["first_prompt"]["text"], "why does main.py print 1?");
    assert_eq!(meta["last_prompt"]["text"], "now add tests");
    assert!(meta["warnings"].as_u64().unwrap() >= 2, "malformed + truncated lines counted");

    // Codex: apply_patch inside exec (requested) and patch_apply_end (observed).
    let xe = edges_from(&c, &sid("codex", "cccc-uuid"));
    assert!(has(&xe, "write", "requested", &fx.main.join("src/app.ts")));
    assert!(has(&xe, "write", "observed", &fx.main.join("other.txt")));
    assert!(has(&xe, "mentioned", "mentioned", &fx.main.join("main.py")));
    assert!(has(&xe, "write", "observed", &fx.main.join("docs/guide.md")), "item_completed FileChange");
    assert!(!xe.iter().any(|e| e.0 == "read"), "codex has no structured read tool");

    // OpenCode: exact worktree, not the main checkout.
    let fe = std::fs::canonicalize(&fx.feature).unwrap();
    let oe = edges_from(&c, &sid("opencode", "ses_1"));
    assert!(has(&oe, "read", "observed", &fe.join("main.py")));
    assert!(has(&oe, "write", "failed", &fe.join("src/app.ts")));
    assert!(has(&oe, "write", "observed", &fe.join("README.md")));
    assert!(!oe.iter().any(|e| e.2.starts_with(&*fx.main.to_string_lossy())));
    let le = edges_from(&c, &sid("opencode", "ses_old"));
    assert!(has(&le, "read", "observed", &fx.main.join("docs/guide.md")));

    // Session -> checkout by cwd.
    let co: String = c.query_row("SELECT checkout_id FROM nodes WHERE id=?1", [sid("opencode", "ses_1")], |r| r.get(0)).unwrap();
    assert_eq!(co, crate::util::stable_id("checkout", &fe.to_string_lossy()));
}

#[test]
fn context_ranks_doc_referenced_files_first() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let n = overview::resolve_target(&c, &fx.main.to_string_lossy()).unwrap().unwrap();
    assert_eq!(n.kind, "checkout");
    let out = overview::context(&c, &n, &overview::Limits::default()).unwrap();
    let section = out.split("## Important files").nth(1).unwrap().split("## Recent commits").next().unwrap();
    let pos = |s: &str| section.find(&format!("\n- {s} ")).unwrap_or_else(|| panic!("{s} missing in:\n{section}"));
    assert!(pos("src/app.ts") < pos("other.txt"));
    assert!(pos("main.py") < pos("other.txt"));
    assert!(section.contains("README.md:6 (Layout)"), "reference evidence with line and heading:\n{section}");
    assert!(section.contains("CLAUDE.md:4 (Key Files)"));
    assert!(!out.contains("Missing.tsx"), "unresolvable references are not invented");
    assert!(out.contains("README.md:3 [Proj] Tracks things."), "excerpt with line ref:\n{out}");
    assert!(out.contains("claude-memory/MEMORY.md"), "claude project memory attached");
    assert!(out.contains("Investigate backend") && out.contains("first user input 2026-02-01 10:00Z"));
    assert!(out.contains("initial layout"));
    assert!(out.contains("## Checkouts (3)"));
    // Expand shows evidence locations.
    let fid = crate::util::stable_id("file", &fx.main.join("main.py").to_string_lossy());
    let ex = overview::expand(&c, &fid, 40).unwrap();
    assert!(ex.contains("references/structural") && ex.contains("CLAUDE.md:4"), "{ex}");
    assert!(ex.contains("read/observed") && ex.contains("11111111-aaaa.jsonl:3"), "{ex}");
}

#[test]
fn second_scan_reuses_sources_and_keeps_ids_and_flags_missing() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, first) = scan_once(&env);
    let ids = |c: &Connection| -> Vec<String> {
        let mut st = c.prepare("SELECT id FROM nodes ORDER BY id").unwrap();
        let v = st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        v
    };
    let before = ids(&c);
    drop(c);
    let (c, second) = scan_once(&env);
    assert_eq!(second.sessions_parsed, 0);
    assert_eq!(second.checkouts_reused, 3, "unchanged HEAD/index/docs skip git work");
    assert_eq!(second.sessions_reused, first.sessions_parsed);
    assert_eq!(ids(&c), before, "stable ids across scans");
    drop(c);
    // Editing a doc (newer mtime) re-indexes just that checkout.
    let readme = fx.main.join("README.md");
    std::fs::write(&readme, "# Proj\n\nTracks things.\n\n## Layout\nUI in `src/app.ts`, backend `main.py`.\n").unwrap();
    let f = std::fs::File::options().write(true).open(&readme).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
    let (c, edited) = scan_once(&env);
    assert_eq!(edited.checkouts_reused, 2);
    let n: i64 = c.query_row("SELECT count(*) FROM edges WHERE source_path=?1 AND kind='references' AND source_line=6", [readme.to_string_lossy()], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "README now references app.ts and main.py on line 6");
    drop(c);
    std::fs::remove_file(&fx.claude_file).unwrap();
    std::fs::remove_dir_all(fx.home.join(".codex")).unwrap();
    let (c, third) = scan_once(&env);
    assert_eq!(third.sources_missing, 2);
    assert!(third.warnings.iter().any(|w| w.starts_with("codex: source not found")));
    let meta: String = c.query_row("SELECT meta FROM nodes WHERE id=?1", [sid("claude", "11111111-aaaa")], |r| r.get(0)).unwrap();
    assert!(meta.contains("\"source_missing\":true"));
    assert!(!edges_from(&c, &sid("claude", "11111111-aaaa")).is_empty(), "evidence kept, flagged");
}

fn fake_help(missing: &'static str) -> impl Fn(&str, &[&str]) -> Option<String> {
    move |_p, _a| {
        let all = "--resume --fork-session --no-session-persistence --tools --strict-mcp-config --permission-mode dontAsk --disallowedTools --ignore-user-config --ephemeral -c, --config --skip-git-repo-check read-only";
        Some(all.replace(missing, ""))
    }
}

#[test]
fn ask_plans_are_read_only_or_refused() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let h = fake_help("@@none@@");
    let p = ask::plan(&c, &sid("claude", "11111111-aaaa"), "what did you find?", &h).unwrap();
    assert_eq!(p.program, "claude");
    assert_eq!(p.cwd, fx.main);
    let joined = p.args.join(" ");
    for need in ["-p --resume 11111111-aaaa --fork-session --no-session-persistence", "--tools Read,Grep,Glob", "--permission-mode dontAsk", "--strict-mcp-config", "disableAllHooks"] {
        assert!(joined.contains(need), "{need} missing: {joined}");
    }
    assert!(joined.contains("--disallowedTools Bash,Edit,Write"));
    assert_eq!(p.args.last().unwrap(), "what did you find?");
    for f in ask::FORBIDDEN {
        assert!(!p.args.iter().any(|a| a == f));
    }
    let x = ask::plan(&c, &sid("codex", "cccc-uuid"), "q", &h).unwrap();
    let xj = x.args.join(" ");
    assert!(xj.starts_with("exec resume --ignore-user-config --ignore-rules --ephemeral"));
    assert!(xj.contains("-c sandbox_mode=\"read-only\"") && xj.contains("-c approval_policy=\"never\"") && xj.contains("mcp_servers={}"));
    assert!(xj.contains("cccc-uuid -- q"));
    // Fail closed when the CLI lacks a restricting flag, for OpenCode, and for non-sessions.
    assert!(ask::plan(&c, &sid("claude", "11111111-aaaa"), "q", &fake_help("--strict-mcp-config")).unwrap_err().contains("refusing"));
    assert!(ask::plan(&c, &sid("codex", "cccc-uuid"), "q", &fake_help("--ephemeral")).is_err());
    assert!(ask::plan(&c, &sid("opencode", "ses_1"), "q", &h).unwrap_err().contains("unsupported"));
    assert!(ask::plan(&c, "nope", "q", &h).is_err());
    assert!(ask::plan(&c, &sid("claude", "11111111-aaaa"), "q", &|_: &str, _: &[&str]| None).is_err());
}

#[test]
fn help_probe_captures_stderr_for_opencode_style_clis() {
    let dir = crate::testutil::TempDir::new("help-stderr");
    let fake = dir.path().join("opencode");
    std::fs::write(&fake, "#!/bin/sh\necho 'opencode --session [id] [project]' >&2\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let text = ask::real_help(fake.to_str().unwrap(), &["--help"]).unwrap();
    assert!(text.contains("--session") && text.contains("[project]"), "{text}");
}

#[test]
fn install_is_idempotent_and_links_symlinked_skill_dirs() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    // A shared skills directory linked into home for codex.
    let shared = fx.home.join("shared-skills");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::create_dir_all(fx.home.join(".codex")).unwrap();
    std::os::unix::fs::symlink(&shared, fx.home.join(".codex/skills")).unwrap();
    let exe = fx.home.join("fake-bin");
    std::fs::write(&exe, b"#!/bin/sh\necho workmap\n").unwrap();
    let opts = install::InstallOpts { activate: true, hooks: false };
    let r1 = install::install(&env, &c, &exe, &opts).unwrap();
    let r2 = install::install(&env, &c, &exe, &opts).unwrap();
    let stable = fx.home.join(".local/share/workmap/skills/workmap");
    let bin = fx.home.join(".local/share/workmap/bin/workmap");
    assert!(r1.iter().any(|l| l.starts_with("updated claude skill")), "{r1:?}");
    assert!(r1.iter().any(|l| l.starts_with("linked codex skill") && l.contains("symlink")), "{r1:?}");
    assert!(!r1.iter().any(|l| l.starts_with(install::ERROR)), "{r1:?}");
    // Only a link lands in the shared dir; generated text and binaries stay private.
    let names: Vec<_> = std::fs::read_dir(&shared).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(names, vec![std::ffi::OsString::from("workmap")]);
    assert_eq!(std::fs::read_link(shared.join("workmap")).unwrap(), stable);
    assert!(r2.iter().all(|l| !l.starts_with("updated") && !l.starts_with("wrote ") && !l.starts_with("linked") && !l.starts_with("migrated")), "second run changes nothing: {r2:?}");
    assert!(r1.iter().any(|l| l.contains("timer not activated")), "fake home never touches systemctl");
    let skill = std::fs::read_to_string(fx.home.join(".claude/skills/workmap/SKILL.md")).unwrap();
    assert_eq!(std::fs::read_to_string(stable.join("SKILL.md")).unwrap(), skill, "one generated text everywhere");
    let desc_line = skill.lines().find(|l| l.starts_with("description: ")).unwrap();
    let desc: String = serde_json::from_str(desc_line.trim_start_matches("description: ")).unwrap();
    assert!(desc == install::DESCRIPTION && desc.len() <= install::MAX_DESCRIPTION, "{desc}");
    assert!(skill.contains("workmap overview") && skill.contains("export WORKMAP_PROJECT=<project>") && skill.contains("expand REF"));
    assert!(!skill.contains("proj —") && skill.len() <= install::MAX_SKILL, "no embedded project inventory:\n{skill}");
    assert!(!fx.home.join(".claude/skills/workmap/bin").exists(), "no per-provider binary copies");
    assert!(skill.contains(&bin.display().to_string()), "skill runs the stable binary");
    assert!(fx.home.join(".config/opencode/skills/workmap/SKILL.md").is_file());
    assert!(skill.contains(install::MARKER) && skill.contains(" roots list"));
    let state = env.state_dir.display().to_string();
    assert!(skill.contains(&format!("env WORKMAP_STATE_DIR={state} WORKMAP_HOME={}", fx.home.display())), "generated commands keep state/home");
    assert!(r1.iter().any(|l| l.starts_with("next: register every project root")));
    assert!(r1.iter().any(|l| l.starts_with("verify: codex reads") && l.ends_with("(workmap skill, ok)")), "{r1:?}");
    let svc = std::fs::read_to_string(fx.home.join(".config/systemd/user/workmap-scan.service")).unwrap();
    assert!(svc.contains(&format!("Environment=\"WORKMAP_STATE_DIR={state}\"")) && svc.contains(&format!("Environment=\"WORKMAP_HOME={}\"", fx.home.display())));
    assert!(svc.contains(&format!("ExecStart={} scan --quiet", bin.display())));
    assert!(svc.contains("Environment=\"PATH=") && svc.contains(":/usr/bin:/bin\"") && svc.contains("Environment=\"WORKMAP_GIT_BIN=/"), "{svc}");
    let timer = std::fs::read_to_string(fx.home.join(".config/systemd/user/workmap-scan.timer")).unwrap();
    assert!(timer.contains("OnCalendar=daily") && timer.contains("Persistent=true"));
    assert_eq!(std::fs::read_link(fx.home.join(".local/bin/workmap")).unwrap(), bin);
}

#[test]
fn scan_refreshes_only_installed_owned_skills() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let shared = fx.home.join("shared-skills");
    std::fs::create_dir_all(&shared).unwrap();
    std::os::unix::fs::symlink(&shared, fx.home.join(".codex/skills")).unwrap();
    let exe = fx.home.join("fake-bin");
    std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
    install::install(&env, &c, &exe, &install::InstallOpts { activate: true, hooks: false }).unwrap();
    drop(c);
    let claude_dir = fx.home.join(".claude/skills/workmap");
    let claude_skill = claude_dir.join("SKILL.md");
    let stable_skill = fx.home.join(".local/share/workmap/skills/workmap/SKILL.md");
    // The binary is never rewritten by scans: a sentinel must survive.
    let bin = fx.home.join(".local/share/workmap/bin/workmap");
    std::fs::write(&bin, b"SENTINEL").unwrap();
    // A hand-written (unowned) skill must not be touched.
    let oc_skill = fx.home.join(".config/opencode/skills/workmap/SKILL.md");
    std::fs::write(&oc_skill, "---\nname: workmap\ndescription: mine\n---\nhand written\n").unwrap();
    let unit = fx.home.join(".config/systemd/user/workmap-scan.service");
    let unit_before = std::fs::read(&unit).unwrap();
    assert!(!install::description(&db::open(&env.db_path()).unwrap()).contains("proj2"));

    // New project appears, scan runs, refresh follows (as `workmap scan` does).
    let p2 = fx.home.join("code/proj2");
    write(&p2.join("README.md"), "# Proj2\n\nSecond project intro.\n");
    git(&p2, &["init", "-q"]);
    git(&p2, &["add", "-A"]);
    git(&p2, &["commit", "-qm", "start proj2"]);
    let stable_before = std::fs::read_to_string(&stable_skill).unwrap();
    // An owned copy in an older format (e.g. the 0.4 inventory-sized skill) is rewritten.
    std::fs::write(&claude_skill, format!("---\nname: workmap\ndescription: \"old\"\n---\n\n{}\nold body with every project map\n", install::MARKER)).unwrap();
    let (c, _) = scan_once(&env);
    let r = install::refresh_skills(&env, &c).unwrap();
    assert!(r.iter().any(|l| l.starts_with("refreshed claude")), "{r:?}");
    // The text does not depend on the index: a new project leaves the stable skill byte-identical.
    assert!(r.iter().any(|l| l.starts_with("unchanged stable")), "{r:?}");
    assert!(r.iter().any(|l| l.starts_with("linked codex")), "{r:?}");
    assert!(r.iter().any(|l| l.contains("opencode") && l.contains("not generated by workmap")), "{r:?}");
    let skill = std::fs::read_to_string(&claude_skill).unwrap();
    assert_eq!(skill, stable_before);
    assert!(!skill.contains("proj2") && !skill.contains("Second project intro.") && !skill.contains("old body"), "{skill}");
    assert_eq!(std::fs::read_to_string(shared.join("workmap/SKILL.md")).unwrap(), skill, "codex reads the refreshed stable skill through its link");
    assert_eq!(std::fs::read_to_string(&stable_skill).unwrap(), skill);

    // No-op scan: nothing rewritten.
    drop(c);
    let (c, _) = scan_once(&env);
    let r2 = install::refresh_skills(&env, &c).unwrap();
    assert!(r2.iter().any(|l| l.starts_with("unchanged claude")), "{r2:?}");
    assert_eq!(std::fs::read_to_string(&claude_skill).unwrap(), skill);
    // Nothing else changed or appeared.
    assert_eq!(std::fs::read(&bin).unwrap(), b"SENTINEL");
    assert!(std::fs::read_to_string(&oc_skill).unwrap().contains("hand written"));
    assert_eq!(std::fs::read_dir(&shared).unwrap().count(), 1);
    assert_eq!(std::fs::read(&unit).unwrap(), unit_before);
    // Removing the install means refresh creates nothing.
    std::fs::remove_dir_all(&claude_dir).unwrap();
    let r3 = install::refresh_skills(&env, &c).unwrap();
    assert!(!claude_skill.exists() && !r3.iter().any(|l| l.contains("claude")));
}

#[test]
fn paths_resolve_to_exact_nodes_and_deleted_files_are_marked() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let main_py = fx.main.join("main.py");
    let n = overview::resolve_target(&c, &main_py.to_string_lossy()).unwrap().unwrap();
    assert_eq!((n.kind.as_str(), n.path.as_deref()), ("file", Some(&*main_py.to_string_lossy())));
    assert!(overview::expand(&c, &main_py.to_string_lossy(), 40).unwrap().starts_with("file main.py"));
    // context <file> still picks its project and checkout.
    let ctx = overview::context(&c, &n, &overview::Limits::default()).unwrap();
    assert!(ctx.contains("## Checkouts (3)") && ctx.contains(&format!("* {}", fx.main.display())));
    // A subdirectory that is not a node falls back to the deepest checkout.
    let d = overview::resolve_target(&c, &fx.main.join("src").to_string_lossy()).unwrap().unwrap();
    assert_eq!(d.kind, "checkout");
    // Deleted file: still resolvable, flagged as historical, commit evidence kept.
    std::fs::remove_file(fx.main.join("other.txt")).unwrap();
    let ctx = overview::context(&c, &n, &overview::Limits { files: 30, ..Default::default() }).unwrap();
    let line = ctx.lines().find(|l| l.starts_with("- other.txt ")).unwrap();
    assert!(line.contains("NOT ON DISK") && line.contains("recent commit"), "{line}");
    let ex = overview::expand(&c, &fx.main.join("other.txt").to_string_lossy(), 40).unwrap();
    assert!(ex.contains("(not on disk now") && ex.contains("touched/structural"), "{ex}");
    // Intro excerpt survives a tight excerpt budget even though Layout has more refs.
    let tight = overview::context(&c, &n, &overview::Limits { excerpts: 1, ..Default::default() }).unwrap();
    assert!(tight.contains("README.md:3 [Proj] Tracks things."), "{tight}");
    assert!(!tight.contains("README.md:6 [Layout]"));
}

// ------------------------------------------------------------------ named views and scoped references

use crate::view;

fn v(c: &Connection) -> view::View<'_> {
    view::View { c, cmd: "workmap".into(), state: String::new() }
}

fn scoped(project: &str) -> view::Req {
    view::Req::default().scoped(Some(project.into()), None, None)
}

/// Every reference printed in an output, exactly as printed (quoted or bare),
/// with an optional trailing `--worktree X`.
fn printed_refs(out: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for line in out.lines() {
        if line.starts_with("Scope:") || line.starts_with("Codes:") {
            continue;
        }
        let b: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < b.len() {
            let start_ok = i == 0 || b[i - 1] == ' ' || b[i - 1] == '(';
            let mut tok = String::new();
            if start_ok && b[i] == '\'' && b.get(i + 1) == Some(&':') {
                // '...' with '\'' escapes
                let mut j = i;
                loop {
                    tok.push(b[j]);
                    j += 1;
                    if j >= b.len() {
                        break;
                    }
                    if b[j] == '\'' {
                        tok.push('\'');
                        if b.get(j + 1) == Some(&'\\') && b.get(j + 2) == Some(&'\'') && b.get(j + 3) == Some(&'\'') {
                            tok.push_str("\\''");
                            j += 4;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                }
                i = j;
            } else if start_ok && fq_len(&b[i..]) > 0 {
                // Fully qualified `<handle>[~hex][@worktree]:<rest>`, optionally '…'-quoted.
                let n = fq_len(&b[i..]);
                tok.extend(&b[i..i + n]);
                i += n;
            } else if start_ok && b[i] == ':' && b.get(i + 1).map(|c| !c.is_whitespace()).unwrap_or(false) {
                let mut j = i;
                while j < b.len() && !b[j].is_whitespace() && b[j] != ',' {
                    tok.push(b[j]);
                    j += 1;
                }
                i = j;
            } else {
                i += 1;
                continue;
            }
            let rest: String = b[i..].iter().collect();
            if let Some(w) = rest.strip_prefix(" --worktree ") {
                tok.push_str(" --worktree ");
                tok.push_str(w.split_whitespace().next().unwrap_or(""));
            }
            refs.push(tok);
        }
    }
    refs
}

/// Length of a fully qualified reference token at the start of `b` (0 if none):
/// a handle containing `~` or `@` before the first `:`, followed by a non-space.
fn fq_len(b: &[char]) -> usize {
    let quoted = b.first() == Some(&'\'');
    let s = if quoted { 1 } else { 0 };
    let Some(colon) = b[s..].iter().position(|&c| c == ':' || c.is_whitespace() || c == '\'').map(|k| k + s) else { return 0 };
    if b[colon] != ':' || colon == s || !b[s..colon].iter().any(|&c| c == '@' || c == '~') || !b[s..colon].iter().all(|&c| c.is_alphanumeric() || "._-~@/+".contains(c)) {
        return 0;
    }
    if b.get(colon + 1).map(|c| c.is_whitespace()).unwrap_or(true) {
        return 0;
    }
    let mut j = colon + 1;
    if quoted {
        while j < b.len() {
            if b[j] == '\'' {
                if b.get(j + 1) == Some(&'\\') && b.get(j + 2) == Some(&'\'') && b.get(j + 3) == Some(&'\'') {
                    j += 4;
                    continue;
                }
                return j + 1;
            }
            j += 1;
        }
        return 0;
    }
    while j < b.len() && !b[j].is_whitespace() && b[j] != ',' {
        j += 1;
    }
    j
}

/// Minimal POSIX word splitter for bare words and '...' quoting.
fn shell_split(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                in_word = true;
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        break;
                    }
                    cur.push(c2);
                }
            }
            '\\' => {
                in_word = true;
                if let Some(c2) = chars.next() {
                    cur.push(c2);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// Expand a printed reference under a scope, like `workmap expand <ref> --project <scope>`.
fn expand_ref(c: &Connection, scope: &str, printed: &str) -> Result<String, String> {
    let w = shell_split(printed);
    if !w[0].starts_with(':') {
        // Fully qualified references must resolve without any scope or flag.
        return view::expand(&v(c), &w[0], &view::Req::default());
    }
    let mut req = scoped(scope);
    if w.len() == 3 && w[1] == "--worktree" {
        req.worktree = Some(w[2].clone());
    }
    view::expand(&v(c), &w[0], &req)
}

/// Every printed reference resolves, and to the node it names (its view's title repeats the reference).
fn assert_refs_roundtrip(c: &Connection, scope: &str, out: &str) -> usize {
    let refs = printed_refs(out);
    for r in &refs {
        let got = expand_ref(c, scope, r).unwrap_or_else(|e| panic!("{r}: {e}\n--- in ---\n{out}"));
        let plain = r.split(" --worktree").next().unwrap();
        let w = shell_split(plain);
        if !w[0].starts_with(':') {
            // `<handle>[@wt]:<rest>` → the view titles it with the scoped form `:<rest>`.
            let rest = &w[0][w[0].find(':').unwrap()..];
            let want = crate::util::sh_quote(rest);
            let want = want.split('#').next().unwrap();
            assert!(got.starts_with(&format!("# {want}")), "{r} expanded to a different node:\n{}", got.lines().next().unwrap_or(""));
        } else if let Some(pre) = plain.strip_prefix(":session:") {
            // Session-id refs (the compact map's stable form) name the conversation by its native id.
            assert!(got.lines().any(|l| l.starts_with(&format!("session {pre}"))), "{r} expanded to a different conversation:\n{got}");
        } else if !plain.contains("(other") {
            assert!(got.starts_with(&format!("# {plain} ")), "{r} expanded to a different node:\n{}", got.lines().next().unwrap_or(""));
        }
    }
    refs.len()
}

#[test]
fn scoped_refs_for_file_section_commit_and_project() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let p = scoped("proj");
    let m = view::context(&v(&c), "", &view::Req { detailed: true, ..scoped("proj") }).unwrap();
    assert!(m.starts_with("# proj — project map"), "{m}");
    let compact = view::context(&v(&c), "", &p).unwrap();
    assert!(compact.starts_with("# proj — map · main @ ") && compact.contains("Purpose (:README.md l.3): \"Tracks things.\""), "{compact}");
    assert_eq!(compact.matches("Scope: export WORKMAP_PROJECT=proj").count(), 1, "scope printed once");
    assert_eq!(compact.matches("workmap ").count(), 3, "commands printed once, in the Next line:\n{compact}");
    assert!(assert_refs_roundtrip(&c, "proj", &compact) >= 5);
    assert_eq!(m.matches("Scope: export WORKMAP_PROJECT=proj").count(), 1, "scope printed once");
    assert!(!m.contains("workmap expand proj") && !m.contains("file-") && !m.contains("session-"), "no repeated commands or ids:\n{m}");
    assert!(m.contains("- :README.md l.3 \"Tracks things.\""), "{m}");
    assert!(m.contains("':conversation:Investigate backend'") || m.contains("':conversation:Investigate'"), "{m}");
    let f = view::expand(&v(&c), ":main.py", &p).unwrap();
    assert!(f.starts_with("# :main.py — file in proj @main"), "{f}");
    assert!(f.contains("codex@main: sh exec_command l.5") || f.contains("sh exec_command"), "{f}");
    assert!(f.contains("claude@main: r Read l.3"), "{f}");
    assert!(f.contains("- :CLAUDE.md#key-files l.4: \"- **main.py** - backend\""), "{f}");
    let s = view::expand(&v(&c), ":CLAUDE.md#key-files", &p).unwrap();
    assert!(s.contains("   4 | - **main.py** - backend") && s.contains("Links (1): :main.py l.4"), "{s}");
    assert!(view::expand(&v(&c), ":CLAUDE.md#nope", &p).unwrap_err().contains(":CLAUDE.md#key-files"));
    let sha = std::process::Command::new("git").arg("-C").arg(&fx.main).args(["rev-parse", "HEAD"]).output().unwrap();
    let sha = String::from_utf8_lossy(&sha.stdout).trim().to_string();
    let cm = view::expand(&v(&c), &format!(":commit:{}", &sha[..8]), &p).unwrap();
    assert!(cm.starts_with(&format!("# :commit:{} — \"initial layout\"", &sha[..8])) && cm.contains("- A :main.py"), "{cm}");
    // Full references keep working without a scope, and equal the scoped ones.
    assert_eq!(view::expand(&v(&c), "proj:main.py", &view::Req::default()).unwrap(), f);
    for out in [&m, &f, &s, &cm] {
        assert_refs_roundtrip(&c, "proj", out);
    }
    assert!(assert_refs_roundtrip(&c, "proj", &m) >= 10);
}

#[test]
fn missing_scope_is_a_clear_error_and_flag_beats_env() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let e = view::expand(&v(&c), ":main.py", &view::Req::default()).unwrap_err();
    assert!(e.contains("--project") && e.contains("WORKMAP_PROJECT"), "{e}");
    assert!(view::ask_session(&v(&c), ":conversation:Investigate", &view::Req::default()).unwrap_err().contains("WORKMAP_PROJECT"));
    // env scope works; an explicit flag overrides it.
    let by_env = view::Req::default().scoped(None, Some("proj".into()), None);
    assert!(view::expand(&v(&c), ":main.py", &by_env).is_ok());
    let flag_wins = view::Req::default().scoped(Some("nosuch".into()), Some("proj".into()), None);
    assert!(view::expand(&v(&c), ":main.py", &flag_wins).unwrap_err().contains("no project named nosuch"));
    // Env worktree applies to the scope project.
    let wt = view::Req::default().scoped(None, Some("proj".into()), Some("feature".into()));
    let f = view::expand(&v(&c), ":main.py", &wt).unwrap();
    assert!(f.starts_with("# :main.py — file in proj @feature") && f.contains("WORKMAP_WORKTREE=feature"), "{f}");
    let bad = view::Req::default().scoped(None, Some("proj".into()), Some("nope".into()));
    assert!(view::expand(&v(&c), ":main.py", &bad).unwrap_err().starts_with("WORKMAP_WORKTREE: no worktree nope"));
}

#[test]
fn two_projects_interleaved_never_cross_resolve() {
    let fx = fixture();
    let other = fx.home.join("code/second");
    write(&other.join("README.md"), "# Second\n\nThe second project.\n");
    write(&other.join("main.py"), "print(2)\n");
    git(&other, &["init", "-q"]);
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "second start"]);
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    for _ in 0..2 {
        let a = view::expand(&v(&c), ":main.py", &scoped("proj")).unwrap();
        let b = view::expand(&v(&c), ":main.py", &scoped("second")).unwrap();
        assert!(a.contains(&format!("{}/main.py", fx.main.display())) && a.contains("Investigate backend"), "{a}");
        assert!(b.contains(&format!("{}/main.py", std::fs::canonicalize(&other).unwrap().display())) && !b.contains("Investigate backend"), "{b}");
        let rb = view::expand(&v(&c), ":README.md#second", &scoped("second")).unwrap();
        assert!(rb.contains("The second project."));
        assert!(view::expand(&v(&c), ":README.md#second", &scoped("proj")).is_err());
    }
}

#[test]
fn worktree_scope_selects_exact_checkout_for_same_path() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let mut feat = scoped("proj");
    feat.worktree = Some("feature".into());
    let fe = std::fs::canonicalize(&fx.feature).unwrap();
    let f = view::expand(&v(&c), ":main.py", &feat).unwrap();
    let f_own = f.split("\n## Same path in other checkouts").next().unwrap();
    assert!(f.contains(&format!("{}/main.py", fe.display())) && f_own.contains("\"Feature work\"") && !f_own.contains("Investigate backend"), "{f}");
    assert!(f.contains("--worktree feature"), "scope keeps the worktree:\n{f}");
    let main = view::expand(&v(&c), ":main.py", &scoped("proj")).unwrap();
    // main's own sections never list the feature conversation; the separate
    // "Same path in other checkouts" section may (fully qualified, labelled).
    let (own, others) = main.split_once("\n## Same path in other checkouts").unwrap_or((&main, ""));
    assert!(own.contains("Investigate backend") && !own.contains("Feature work"), "{main}");
    assert!(others.contains("proj@feature:main.py") && others.contains("\"Feature work\""), "{main}");
    let mf = view::context(&v(&c), "", &feat).unwrap();
    assert!(mf.contains("WORKMAP_WORKTREE=feature") && mf.contains("— map · feature @ ") && mf.contains("feature*"), "{mf}");
    let md = view::context(&v(&c), "", &view::Req { detailed: true, ..feat }).unwrap();
    assert!(md.contains("WORKMAP_WORKTREE=feature") && md.contains("* feature @"), "{md}");
    let e = view::expand(&v(&c), "proj:main.py", &view::Req { worktree: Some("nope".into()), ..Default::default() }).unwrap_err();
    assert!(e.contains("--worktree main") && e.contains("--worktree feature"), "{e}");
}

#[test]
fn duplicate_project_names_refuse_and_path_handle_works() {
    let fx = fixture();
    let other = fx.home.join("other/proj");
    write(&other.join("README.md"), "# Other\n\nAnother repo with the same name.\n");
    git(&other, &["init", "-q"]);
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "other start"]);
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let e = view::expand(&v(&c), ":main.py", &scoped("proj")).unwrap_err();
    assert!(e.contains("ambiguous") && e.contains(&*fx.main.to_string_lossy()) && e.contains(&*other.to_string_lossy()), "{e}");
    // The error lists stable compact handles; the checkout path still works too.
    let path = fx.main.to_string_lossy().into_owned();
    let by_path = view::context(&v(&c), "", &scoped(&path)).unwrap();
    let handle = by_path.split_whitespace().nth(1).unwrap().to_string();
    assert!(handle.starts_with("proj~") && handle.len() >= 9 && e.contains(&handle), "{handle}\n{e}");
    let m = view::context(&v(&c), "", &scoped(&handle)).unwrap();
    assert!(m.starts_with(&format!("# {handle} — map · ")) && m.contains(&format!("WORKMAP_PROJECT={handle}")), "{m}");
    assert_eq!(m, by_path, "handle and path name the same project");
    let (c2, _) = scan_once(&env);
    assert_eq!(view::context(&v(&c2), "", &scoped(&handle)).unwrap(), m, "handle is stable across scans");
    assert_refs_roundtrip(&c, &handle, &m);
    assert!(view::context(&v(&c), "nosuch", &view::Req::default()).unwrap_err().contains("no project named"));
}

#[test]
fn conversation_refs_duplicates_special_chars_and_ask_equivalence() {
    let fx = fixture();
    let m = fx.main.display().to_string();
    let cdir = fx.home.join(".claude/projects").join(crate::util::claude_project_dir_name(&m));
    let conv = |id: &str, title: &str, day: &str| {
        jl(&[
            json!({"type":"summary","summary":title}),
            json!({"type":"user","cwd":m,"sessionId":id,"timestamp":format!("{day}T10:00:00Z"),"message":{"content":"go"}}),
            json!({"type":"assistant","cwd":m,"sessionId":id,"timestamp":format!("{day}T10:00:01Z"),"message":{"content":[{"type":"tool_use","id":"x","name":"Read","input":{"file_path":format!("{m}/main.py")}}]}}),
            json!({"type":"user","cwd":m,"sessionId":id,"timestamp":format!("{day}T10:00:02Z"),"toolUseResult":{},"message":{"content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}}),
        ])
    };
    write(&cdir.join("aaaa1111-dup.jsonl"), &conv("aaaa1111-dup", "Fix build", "2026-03-01"));
    write(&cdir.join("bbbb2222-dup.jsonl"), &conv("bbbb2222-dup", "Fix build", "2026-03-01"));
    write(&cdir.join("cccc3333-q.jsonl"), &conv("cccc3333-q", "it's \"quoted\": $HOME `x` #1", "2026-03-02"));
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let p = scoped("proj");
    let e = view::expand(&v(&c), ":conversation:Fix build", &p).unwrap_err();
    assert!(e.contains("2 conversations in proj match --conversation 'Fix build'") && e.contains("proj:session:aaaa1111") && e.contains("proj:session:bbbb2222"), "{e}");
    for want in ["aaaa1111", "bbbb2222"] {
        let out = view::expand(&v(&c), &format!(":session:{want}"), &p).unwrap();
        assert!(out.contains(&format!("session {want}-dup")), "{out}");
    }
    // Title with quotes, colon, $ and # round-trips through shell quoting.
    let f = view::expand(&v(&c), ":main.py", &view::Req { limit: Some(20), ..scoped("proj") }).unwrap();
    let q = printed_refs(&f).into_iter().find(|r| r.contains("it'\\''s")).unwrap_or_else(|| panic!("{f}"));
    let out = expand_ref(&c, "proj", &q).unwrap();
    assert!(out.contains("session cccc3333-q"), "{out}");
    assert_refs_roundtrip(&c, "proj", &f);
    // Every conversation ref in the map: expand and ask resolve to the same session.
    let mp = view::context(&v(&c), "", &view::Req { map: view::MapLimits { sessions: 20, ..Default::default() }, ..scoped("proj") }).unwrap();
    let convs: Vec<String> = printed_refs(&mp).into_iter().filter(|r| r.contains(":conversation:") || r.contains(":session:")).collect();
    assert!(convs.len() >= 6, "{mp}");
    for r in convs {
        let w = shell_split(&r);
        let sid_a = view::ask_session(&v(&c), &w[0], &p).unwrap();
        let shown = view::expand(&v(&c), &w[0], &p).unwrap();
        let native = shown.lines().find_map(|l| l.strip_prefix("session ")).unwrap().split(' ').next().unwrap().to_string();
        let prov = shown.lines().next().unwrap().split(" — ").nth(1).unwrap().split(' ').next().unwrap().to_string();
        assert_eq!(sid_a, sid(&prov, &native), "{r}");
    }
    // ask plan equivalence: shorthand vs legacy flags vs session id.
    let a = view::ask_session(&v(&c), ":conversation:Investigate backend", &p).unwrap();
    let b = view::ask_session(&v(&c), "proj", &view::Req { conv: view::ConvSel { title: Some("Investigate backend".into()), ..Default::default() }, ..Default::default() }).unwrap();
    let full = view::ask_session(&v(&c), "proj:conversation:Investigate backend", &view::Req::default()).unwrap();
    assert!(a == b && b == full && a == sid("claude", "11111111-aaaa"));
    let h = fake_help("@@none@@");
    assert!(ask::plan(&c, &a, "what next?", &h).unwrap().args.join(" ").contains("--resume 11111111-aaaa --fork-session"));
}

#[test]
fn file_with_many_commits_still_lists_conversations() {
    let fx = fixture();
    for i in 0..14 {
        write(&fx.main.join("main.py"), &format!("print({i})\n"));
        git(&fx.main, &["commit", "-qam", &format!("tweak {i}")]);
    }
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let f = view::expand(&v(&c), ":main.py", &scoped("proj")).unwrap();
    assert!(f.contains("## Commits changing it (15 in indexed history)") && f.contains("… 7 more (expand :main.py --limit 15)"), "{f}");
    let conv = f.split("## Conversations").nth(1).unwrap().split("## Docs").next().unwrap();
    assert!(conv.contains("Investigate backend") && conv.contains(":conversation:"), "{f}");
    assert_refs_roundtrip(&c, "proj", &f);
}

#[test]
fn unique_name_matches_are_unverified_candidates_not_references() {
    let fx = fixture();
    write(&fx.main.join("docs/notes.md"), "# Notes\n\n## Sources\nSee research.md for data and [guide](guide.md).\n");
    write(&fx.main.join("deep/x/research.md"), "# unrelated research\n");
    git(&fx.main, &["add", "-A"]);
    git(&fx.main, &["commit", "-qm", "notes and research"]);
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let research = fx.main.join("deep/x/research.md").to_string_lossy().into_owned();
    let kinds: Vec<(String, String, String)> = {
        let mut st = c.prepare("SELECT e.kind, e.strength, e.note FROM edges e JOIN nodes n ON n.id=e.dst WHERE n.path=?1 AND e.kind LIKE 'reference%'").unwrap();
        let v = st.query_map([&research], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|r| r.unwrap()).collect();
        v
    };
    assert_eq!(kinds.len(), 1, "{kinds:?}");
    assert_eq!((kinds[0].0.as_str(), kinds[0].1.as_str()), ("reference_candidate", "candidate"));
    assert!(kinds[0].2.contains("token `research.md`"));
    let guide: i64 = c
        .query_row("SELECT count(*) FROM edges e JOIN nodes n ON n.id=e.dst WHERE n.path=?1 AND e.kind='references' AND e.source_line=4", [fx.main.join("docs/guide.md").to_string_lossy()], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(guide, 1);
    let m = view::context(&v(&c), "", &view::Req { map: view::MapLimits { files: 50, ..Default::default() }, ..scoped("proj") }).unwrap();
    if let Some(l) = m.lines().find(|l| l.starts_with("- :deep/x/research.md")) {
        assert!(!l.contains("named in"), "{l}");
    }
    let top_of: std::collections::BTreeMap<String, String> = [(crate::util::stable_id("checkout", &fx.main.to_string_lossy()), fx.main.to_string_lossy().into_owned())].into_iter().collect();
    let ranked = crate::overview::rank_files(&c, &top_of.keys().cloned().collect::<Vec<_>>(), &top_of).unwrap();
    let r = ranked.iter().find(|r| r.node.path.as_deref() == Some(research.as_str())).unwrap();
    assert!(r.refs.is_empty() && r.score == 4, "only its commit scores (4 per recent commit)");
    let f = view::expand(&v(&c), ":deep/x/research.md", &scoped("proj")).unwrap();
    assert!(f.contains("## Docs naming it (exact path) (0)"), "{f}");
    assert!(f.contains("## Unverified mentions") && f.contains("- :docs/notes.md#sources l.4 `research.md`:"), "{f}");
    let s = view::expand(&v(&c), ":docs/notes.md#sources", &scoped("proj")).unwrap();
    assert!(s.contains("Links (1): :docs/guide.md l.4"), "{s}");
    assert!(s.contains("Unverified name matches") && s.contains("l.4 `research.md` → maybe :deep/x/research.md"), "{s}");
    assert_refs_roundtrip(&c, "proj", &f);
    assert_refs_roundtrip(&c, "proj", &s);
}

#[path = "tests_more.rs"]
mod more_tests;
// One module per workstream; shared fixtures live in testutil.rs.
#[path = "tests_graph.rs"]
mod graph_tests;
#[path = "tests_hooks.rs"]
mod hook_tests;
#[path = "tests_identity.rs"]
mod identity_tests;
#[path = "tests_ingest.rs"]
mod ingest_tests;
#[path = "tests_install.rs"]
mod install_tests;
#[path = "tests_map.rs"]
mod map_tests;
#[path = "tests_prompt.rs"]
mod prompt_tests;
#[path = "tests_search.rs"]
mod search_tests;
