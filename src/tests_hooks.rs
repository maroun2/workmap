//! Host hook adapters (SessionStart / UserPromptSubmit / Stop), hook install and resume.
//! Synthetic fixtures only: temp homes, fake CLIs, no network, no real home.

#[allow(unused_imports)]
use super::*;
use crate::hook::{self, Ctx, Scope};
use crate::resume;
use crate::testutil::{ledger, Ledger};
use std::time::{Duration, Instant};

fn ctx(budget: usize) -> Ctx {
    Ctx { self_exe: PathBuf::from("/nonexistent/workmap"), budget, cmd: "workmap".into() }
}

/// Event JSON shaped like the documented examples (common fields + event fields).
fn event(name: &str, session: &str, cwd: &Path, extra: serde_json::Value) -> Vec<u8> {
    let mut v = json!({
        "session_id": session,
        "transcript_path": "/tmp/nonexistent/00893aaf-19fa-41d2-8238-13269b9b3ca0.jsonl",
        "cwd": cwd.display().to_string(),
        "permission_mode": "default",
        "hook_event_name": name,
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v.to_string().into_bytes()
}

/// `additionalContext` of a hook response; asserts the documented envelope.
fn context_of(out: &str, ev: &str) -> String {
    assert!(out.ends_with('\n') && out.trim().lines().count() == 1, "one JSON line: {out:?}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], ev);
    v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string()
}

fn scanned() -> (Ledger, Env, Connection) {
    let fx = ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    (fx, env, c)
}

#[test]
fn session_start_in_an_indexed_checkout_lists_expandable_conversations_within_budget() {
    let (fx, env, c) = scanned();
    for cwd in [fx.api_fix.clone(), fx.api_fix.join("ledger")] {
        let out = hook::respond(&env, Some("SessionStart"), "claude", &event("SessionStart", "s-new", &cwd, json!({"source": "startup", "model": "m"})), &ctx(1500));
        let text = context_of(&out, "SessionStart");
        assert!(text.len() <= 1500, "{} bytes", text.len());
        assert!(text.contains("worktree fix/webhook-retry · branch fix/webhook-retry"), "{text}");
        // The conversation that ran in this worktree, pinned to it, and the sibling's.
        let refs = printed_refs(&text);
        let fix = refs.iter().find(|r| r.contains("@fix/webhook-retry:conversation:Double charge")).unwrap_or_else(|| panic!("{text}"));
        let got = expand_ref(&c, "ledger-sdk", fix).unwrap();
        assert!(got.starts_with("# ':conversation:Double charge'") && got.contains("session aaaa1111-fix"), "expands to that conversation: {got}");
        assert!(refs.iter().any(|r| r.contains(":conversation:Port retry") && !r.contains('@')), "main worktree's latest conversation: {text}");
        assert!(text.contains("Other worktrees of this project:") && text.contains(&fx.api.display().to_string()));
        assert!(assert_refs_roundtrip(&c, "ledger-sdk", &text) >= 2);
        assert!(text.contains("workmap expand '<ref>'"));
    }
    // A tight budget drops items, never cuts a line.
    let out = hook::respond(&env, Some("SessionStart"), "claude", &event("SessionStart", "s", &fx.api_fix, json!({"source": "resume"})), &ctx(300));
    let text = if out.is_empty() { String::new() } else { context_of(&out, "SessionStart") };
    assert!(text.len() <= 300, "{text}");
    assert_refs_roundtrip(&c, "ledger-sdk", &text);
    // The conversation being resumed is not offered back to itself.
    let out = hook::respond(&env, Some("SessionStart"), "codex", &event("SessionStart", "aaaa1111-fix", &fx.api_fix, json!({"source": "resume"})), &ctx(1500));
    assert!(!context_of(&out, "SessionStart").contains("Recent conversations in this checkout"), "{out}");
}

#[test]
fn unrelated_cwd_missing_index_and_bad_input_print_nothing() {
    let (fx, env, _c) = scanned();
    let elsewhere = TempDir::new("elsewhere");
    let cases: Vec<(&str, &str, Vec<u8>)> = vec![
        ("SessionStart", "claude", event("SessionStart", "s", elsewhere.path(), json!({"source": "startup"}))),
        ("UserPromptSubmit", "claude", event("UserPromptSubmit", "s", elsewhere.path(), json!({"prompt": "customers charged twice on webhook retry"}))),
        ("SessionStart", "claude", event("SessionStart", "s", Path::new("relative/dir"), json!({}))),
        ("SessionStart", "claude", b"{not json".to_vec()),
        ("SessionStart", "claude", b"[1,2]".to_vec()),
        ("SessionStart", "claude", Vec::new()),
        ("SessionStart", "claude", event("UserPromptSubmit", "s", &fx.api, json!({"prompt": "x"}))), // event mismatch
        ("SessionStart", "opencode", event("SessionStart", "s", &fx.api, json!({}))),
        ("NoSuchEvent", "claude", event("NoSuchEvent", "s", &fx.api, json!({}))),
    ];
    for (ev, h, input) in cases {
        assert_eq!(hook::respond(&env, Some(ev), h, &input, &ctx(1500)), "", "{ev} {h} {}", String::from_utf8_lossy(&input));
    }
    assert_eq!(hook::respond(&env, None, "claude", b"{}", &ctx(1500)), "");
    // No index at all (fresh state dir): nothing, and no index is created.
    let empty = TempDir::new("noindex");
    let env2 = Env::with_home(empty.path());
    assert_eq!(hook::respond(&env2, Some("SessionStart"), "claude", &event("SessionStart", "s", &fx.api, json!({})), &ctx(1500)), "");
    assert!(!env2.db_path().exists());
    // Errors were logged, not printed.
    assert!(std::fs::read_to_string(env.state_dir.join("hook.log")).unwrap().contains("event JSON"));
}

#[test]
fn prompt_context_matches_evidence_dedups_per_session_and_skips_irrelevant_prompts() {
    let (fx, env, c) = scanned();
    let ask =
        |session: &str, prompt: &str, budget: usize| hook::respond(&env, Some("UserPromptSubmit"), "claude", &event("UserPromptSubmit", session, &fx.api, json!({"prompt": prompt})), &ctx(budget));
    let out = ask("sess-1", "customers charged twice on webhook retry", 1500);
    let text = context_of(&out, "UserPromptSubmit");
    assert!(text.len() <= 1500);
    // The root-cause conversation's message, from the sibling worktree, with why.
    let refs = printed_refs(&text);
    assert!(refs.iter().any(|r| r.contains("@fix/webhook-retry:message:aaaa1111")), "{text}");
    assert!(text.contains("matched: customers, charged, twice, webhook"), "{text}");
    assert!(text.contains("Customers were charged twice") || text.contains("[Customers]"), "{text}");
    assert!(assert_refs_roundtrip(&c, "ledger-sdk", &text) >= 1);
    let first: Vec<String> = refs.clone();
    // Same session, same prompt: already delivered, nothing repeats.
    assert_eq!(ask("sess-1", "customers charged twice on webhook retry", 1500), "");
    // Another session gets it again.
    let again = context_of(&ask("sess-2", "customers charged twice on webhook retry", 1500), "UserPromptSubmit");
    assert_eq!(printed_refs(&again), first);
    // Irrelevant or single-word prompts add nothing.
    for p in ["what is the weather like in paris tomorrow", "hello", "", "please help"] {
        assert_eq!(ask("sess-3", p, 1500), "", "{p}");
    }
    // A path named in the prompt resolves to the file with the most recorded evidence.
    let t = context_of(&ask("sess-4", "why does ledger/webhooks.py look like this?", 1500), "UserPromptSubmit");
    assert!(printed_refs(&t).iter().any(|r| r.ends_with("@fix/webhook-retry:ledger/webhooks.py")), "{t}");
    assert!(t.contains("file named in the prompt · 1 recorded conversation(s)"), "{t}");
    assert_refs_roundtrip(&c, "ledger-sdk", &t);
    // Small budget: whole items only.
    let small = ask("sess-5", "customers charged twice on webhook retry", 450);
    let st = context_of(&small, "UserPromptSubmit");
    assert!(st.len() <= 450, "{}", st.len());
    for l in st.lines() {
        assert!(l.starts_with("workmap:") || l.starts_with("- ") || l.starts_with("  \"") || l.starts_with("Expand"), "partial line {l:?}");
        if l.starts_with("  \"") {
            assert!(l.ends_with('"'));
        }
    }
    // The session's own messages are never offered back to it.
    let own = ask("aaaa1111-fix", "customers charged twice on webhook retry", 1500);
    assert!(own.is_empty() || !own.contains("message:aaaa1111"), "{own}");
}

fn recorder(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let log = dir.join(format!("{name}.argv"));
    let bin = dir.join(name);
    write(&bin, &format!("#!/bin/sh\npwd > '{0}.tmp'\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{0}.tmp'; done\nmv '{0}.tmp' '{0}'\n", log.display()));
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

fn wait_for(p: &Path) -> String {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        if let Ok(s) = std::fs::read_to_string(p) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{} never written", p.display());
}

#[test]
fn stop_and_session_end_spawn_detached_ingest_of_the_transcript() {
    let (fx, env, _c) = scanned();
    let t = TempDir::new("self");
    for (ev, harness) in [("Stop", "claude"), ("SessionEnd", "codex")] {
        let (bin, log) = recorder(t.path(), ev);
        let c = Ctx { self_exe: bin.clone(), budget: 1500, cmd: "workmap".into() };
        let mut input = event(ev, "aaaa1111-fix", &fx.api_fix, json!({"stop_hook_active": false, "reason": "other"}));
        let mut v: serde_json::Value = serde_json::from_slice(&input).unwrap();
        v["transcript_path"] = json!(fx.conv_fix.display().to_string());
        input = v.to_string().into_bytes();
        let start = Instant::now();
        let out = hook::handle(&env, ev, harness, &input, &c).unwrap();
        assert!(start.elapsed() < Duration::from_secs(2), "does not wait for the child");
        assert_eq!(out.stdout, "");
        let argv = vec![bin.display().to_string(), "ingest".into(), "--transcript".into(), fx.conv_fix.display().to_string(), "--quiet".into()];
        assert_eq!(out.spawned.as_ref(), Some(&argv));
        let rec = wait_for(&log);
        let lines: Vec<&str> = rec.lines().collect();
        assert_eq!(&lines[1..], &["ingest", "--transcript", &fx.conv_fix.display().to_string(), "--quiet"]);
    }
    // Stop fires after every turn: a second Stop right away is throttled, SessionEnd is not.
    let (bin, log) = recorder(t.path(), "again");
    let c = Ctx { self_exe: bin, budget: 1500, cmd: "workmap".into() };
    let mut v: serde_json::Value = serde_json::from_slice(&event("Stop", "s", &fx.api_fix, json!({}))).unwrap();
    v["transcript_path"] = json!(fx.conv_fix.display().to_string());
    assert!(hook::handle(&env, "Stop", "claude", v.to_string().as_bytes(), &c).unwrap().spawned.is_none());
    v["hook_event_name"] = json!("SessionEnd");
    assert!(hook::handle(&env, "SessionEnd", "claude", v.to_string().as_bytes(), &c).unwrap().spawned.is_some());
    wait_for(&log);
    // Codex may send `transcript_path: null`; a missing file is skipped too.
    let (bin, log) = recorder(t.path(), "none");
    let c = Ctx { self_exe: bin, budget: 1500, cmd: "workmap".into() };
    for tp in [json!(null), json!("/no/such/transcript.jsonl"), json!("relative.jsonl")] {
        let mut v: serde_json::Value = serde_json::from_slice(&event("Stop", "s", &fx.api, json!({}))).unwrap();
        v["transcript_path"] = tp;
        assert_eq!(hook::respond(&env, Some("Stop"), "codex", v.to_string().as_bytes(), &c), "");
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!log.exists());
}

#[test]
fn hook_events_answer_fast_on_a_normal_index() {
    let (fx, env, _c) = scanned();
    for (ev, extra) in [("SessionStart", json!({"source": "startup"})), ("UserPromptSubmit", json!({"prompt": "customers charged twice on webhook retry"}))] {
        let input = event(ev, "timing", &fx.api_fix, extra);
        let _ = hook::respond(&env, Some(ev), "claude", &input, &ctx(1500)); // warm the page cache
        let t = Instant::now();
        let out = hook::respond(&env, Some(ev), "claude", &input, &ctx(1500));
        // Generous bound for unoptimized test builds on loaded CI; release target is < 100 ms.
        assert!(t.elapsed() < Duration::from_millis(1500), "{ev} took {:?}", t.elapsed());
        let _ = out;
    }
}

// ------------------------------------------------------------------ hooks install / uninstall

fn foreign_settings() -> serde_json::Value {
    json!({
        "model": "opus",
        "permissions": {"allow": ["Bash(npm test)"]},
        "hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "${CLAUDE_PROJECT_DIR}/.claude/hooks/block-rm.sh", "timeout": 30}]}],
            "SessionStart": [{"matcher": "compact", "hooks": [{"type": "command", "command": "echo reminder"}]}]
        }
    })
}

#[test]
fn hooks_install_merges_idempotently_preserves_foreign_hooks_and_uninstalls_only_its_own() {
    let t = TempDir::new("hooks");
    let home = t.path().join("home");
    let env = Env::with_home(&home);
    let settings = home.join(".claude/settings.json");
    write(&settings, &serde_json::to_string_pretty(&foreign_settings()).unwrap());
    let original = std::fs::read(&settings).unwrap();
    let bin = home.join(".local/share/workmap/bin/workmap");
    let cwd = t.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    fn opts<'a>(cwd: &'a Path, bin: &'a Path, dry: bool) -> hook::HooksOpts<'a> {
        hook::HooksOpts { harness: "claude", scope: Scope::User, cwd, bin, dry_run: dry }
    }
    let read = || serde_json::from_slice::<serde_json::Value>(&std::fs::read(&settings).unwrap()).unwrap();

    // Dry run writes nothing.
    let r = hook::install(&env, &opts(&cwd, &bin, true)).unwrap();
    assert!(r[0].starts_with("would update"));
    assert_eq!(std::fs::read(&settings).unwrap(), original);

    let r = hook::install(&env, &opts(&cwd, &bin, false)).unwrap().join("\n");
    assert!(r.contains("installed 4 workmap hook(s)") && r.contains("foreign hooks left untouched: 2"), "{r}");
    let v = read();
    assert_eq!(v["model"], "opus");
    assert_eq!(v["permissions"], foreign_settings()["permissions"]);
    assert_eq!(v["hooks"]["PreToolUse"], foreign_settings()["hooks"]["PreToolUse"]);
    let ss = v["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(ss[0], foreign_settings()["hooks"]["SessionStart"][0], "foreign group first, untouched");
    let cmd = ss[1]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.starts_with("env WORKMAP_HOOK=1 ") && cmd.ends_with(" hook SessionStart --harness claude"), "{cmd}");
    assert!(cmd.contains(&bin.display().to_string()) && cmd.contains("WORKMAP_STATE_DIR="), "fake home: same index for the hook: {cmd}");
    assert_eq!(ss[1]["hooks"][0]["type"], "command");
    for ev in ["UserPromptSubmit", "Stop", "SessionEnd"] {
        assert_eq!(v["hooks"][ev].as_array().unwrap().len(), 1, "{ev}");
    }
    // Backup holds the previous bytes.
    assert_eq!(std::fs::read(home.join(".claude/settings.json.workmap-backup")).unwrap(), original);

    // Idempotent: second install changes nothing, byte for byte.
    let before = std::fs::read(&settings).unwrap();
    assert!(hook::install(&env, &opts(&cwd, &bin, false)).unwrap()[0].starts_with("unchanged"));
    assert_eq!(std::fs::read(&settings).unwrap(), before);
    // A moved binary replaces the owned entries instead of duplicating them.
    let bin2 = t.path().join("other/workmap");
    hook::install(&env, &opts(&cwd, &bin2, false)).unwrap();
    let v = read();
    assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    assert!(v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().contains("other/workmap"));

    let st = hook::status(&env, &cwd);
    assert!(st.contains("workmap hooks: SessionEnd, SessionStart, Stop, UserPromptSubmit · 2 foreign hook(s)"), "{st}");
    assert!(st.contains("opencode") && st.contains("UNSUPPORTED") && st.contains("https://code.claude.com/docs/en/hooks"));

    // Uninstall restores the foreign content exactly (formatting-insensitive).
    let r = hook::uninstall(&env, "claude", Scope::User, &cwd, false).unwrap();
    assert!(r[0].contains("removed 4 workmap hook(s)") && r[0].contains("2 foreign hook(s) kept"), "{r:?}");
    assert_eq!(read(), foreign_settings());
    assert!(hook::uninstall(&env, "claude", Scope::User, &cwd, false).unwrap().is_empty());

    // Codex user + Claude project scopes; `workmap uninstall` removes user-scope hooks of both harnesses.
    let o = hook::HooksOpts { harness: "codex", scope: Scope::User, cwd: &cwd, bin: &bin, dry_run: false };
    let r = hook::install(&env, &o).unwrap().join("\n");
    assert!(r.contains("/hooks inside Codex"), "{r}");
    let codex = home.join(".codex/hooks.json");
    assert!(std::fs::read_to_string(&codex).unwrap().contains("hook UserPromptSubmit --harness codex"));
    let o = hook::HooksOpts { harness: "claude", scope: Scope::Project, cwd: &cwd, bin: &bin, dry_run: false };
    hook::install(&env, &o).unwrap();
    assert!(cwd.join(".claude/settings.local.json").is_file());
    hook::install(&env, &opts(&cwd, &bin, false)).unwrap();
    let lines = install::uninstall(&env).unwrap().join("\n");
    assert!(lines.contains("removed 4 workmap hook(s) from") && lines.contains(".codex/hooks.json"), "{lines}");
    assert_eq!(read(), foreign_settings());
    let cx: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&codex).unwrap()).unwrap();
    assert_eq!(cx, json!({}), "only owned entries existed");
    assert!(std::fs::read_to_string(cwd.join(".claude/settings.local.json")).unwrap().contains("WORKMAP_HOOK=1"), "project scope is explicit");
}

#[test]
fn hooks_install_refuses_broken_json_symlinks_and_unsupported_harnesses() {
    let t = TempDir::new("hooksbad");
    let home = t.path().join("home");
    let env = Env::with_home(&home);
    let bin = PathBuf::from("/opt/workmap");
    let cwd = t.path().to_path_buf();
    let settings = home.join(".claude/settings.json");
    let o = hook::HooksOpts { harness: "claude", scope: Scope::User, cwd: &cwd, bin: &bin, dry_run: false };
    for broken in ["{\"hooks\": {", "[]", "{\"hooks\": []}", "{\"hooks\": {\"Stop\": {}}}"] {
        write(&settings, broken);
        let e = hook::install(&env, &o).unwrap_err();
        assert!(e.contains("not touching it"), "{broken}: {e}");
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), broken);
        assert!(!home.join(".claude/settings.json.workmap-backup").exists());
        let _ = hook::uninstall(&env, "claude", Scope::User, &cwd, false); // refuses or finds nothing it owns
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), broken);
    }
    // Symlinked settings (dotfiles repos) are never replaced by a regular file.
    let real = t.path().join("dotfiles/settings.json");
    write(&real, "{}");
    std::fs::remove_file(&settings).unwrap();
    std::os::unix::fs::symlink(&real, &settings).unwrap();
    assert!(hook::install(&env, &o).unwrap_err().contains("symlink"));
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "{}");
    // OpenCode has no verified hook interface.
    let o = hook::HooksOpts { harness: "opencode", scope: Scope::User, cwd: &cwd, bin: &bin, dry_run: false };
    assert!(hook::install(&env, &o).unwrap_err().contains("OpenCode 1.19.3 ships no documented hook"));
    // Empty settings file counts as `{}`.
    let codex = home.join(".codex/hooks.json");
    write(&codex, "  \n");
    let o = hook::HooksOpts { harness: "codex", scope: Scope::User, cwd: &cwd, bin: &bin, dry_run: false };
    hook::install(&env, &o).unwrap();
    assert!(std::fs::read_to_string(&codex).unwrap().contains("WORKMAP_HOOK=1"));
}

// ------------------------------------------------------------------ resume

fn resume_help(missing: &'static str) -> impl Fn(&str, &[&str]) -> Option<String> {
    move |_p, _a| Some("-r, --resume [value]  Usage: codex resume [OPTIONS] [SESSION_ID] [PROMPT]  opencode [project]  -s, --session".replace(missing, ""))
}

#[test]
fn resume_plans_use_the_native_cli_in_the_recorded_cwd_or_refuse() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let h = resume_help("@@none@@");
    let p = resume::plan(&c, &sid("claude", "11111111-aaaa"), None, &h).unwrap();
    assert_eq!((p.program.as_str(), p.args.clone(), p.cwd.clone()), ("claude", vec!["--resume".to_string(), "11111111-aaaa".into()], fx.main.clone()));
    assert!(p.drift.is_empty());
    let r = p.render();
    assert!(r.contains("command: claude --resume 11111111-aaaa\n") && r.contains("appends to the original session"), "{r}");
    let x = resume::plan(&c, &sid("codex", "cccc-uuid"), None, &h).unwrap();
    assert_eq!((x.program.as_str(), x.args.clone()), ("codex", vec!["resume".to_string(), "cccc-uuid".into()]));
    let o = resume::plan(&c, &sid("opencode", "ses_1"), None, &h).unwrap();
    assert_eq!((o.program.as_str(), o.args.clone(), o.cwd.clone()), ("opencode", vec![fx.feature.display().to_string(), "--session".into(), "ses_1".into()], fx.feature.clone()));
    assert!(resume::plan(&c, &sid("opencode", "ses_old"), None, &h).unwrap_err().contains("legacy JSON storage"));
    // Syntax not documented by the installed CLI → refuse, never guess.
    assert!(resume::plan(&c, &sid("claude", "11111111-aaaa"), None, &resume_help("--resume")).unwrap_err().contains("refusing"));
    assert!(resume::plan(&c, &sid("codex", "cccc-uuid"), None, &resume_help("SESSION_ID")).is_err());
    assert!(resume::plan(&c, &sid("opencode", "ses_1"), None, &resume_help("--session")).is_err());
    assert!(resume::plan(&c, &sid("claude", "11111111-aaaa"), None, &|_: &str, _: &[&str]| None).is_err());
    // Ids come only from the index.
    assert_eq!(resume::find_session(&c, "11111111").unwrap(), sid("claude", "11111111-aaaa"));
    assert!(resume::find_session(&c, "1111").is_err());
    assert!(resume::find_session(&c, "deadbeef-0000").is_err());
    // `ask` stays read-only for the same conversation.
    let a = ask::plan(&c, &sid("claude", "11111111-aaaa"), "q", &fake_help("@@none@@")).unwrap();
    assert!(a.args.iter().any(|x| x == "--fork-session") && a.args.iter().any(|x| x == "--no-session-persistence"));
    assert!(ask::plan(&c, &sid("opencode", "ses_1"), "q", &fake_help("@@none@@")).unwrap_err().contains("unsupported"));
}

#[test]
fn resume_refuses_a_missing_cwd_with_choices_and_reports_drift_for_cwd() {
    let fx = ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let s = sid("claude", "aaaa1111-fix");
    let h = resume_help("@@none@@");
    // --cwd elsewhere: drift evidence before anything runs.
    let p = resume::plan(&c, &s, Some(&fx.api), &h).unwrap();
    assert_eq!(p.cwd, fx.api);
    let d = p.drift.join("\n");
    assert!(d.contains(&format!("recorded: cwd {} · branch fix/webhook-retry · HEAD ", fx.api_fix.display())), "{d}");
    assert!(d.contains(&format!("current:  cwd {} · branch main · HEAD ", fx.api.display())), "{d}");
    assert!(d.contains("branch differs") && d.contains("not verified"), "{d}");
    assert!(p.render().contains("CONTEXT DRIFT"));
    // --cwd equal to the recorded one: no drift.
    assert!(resume::plan(&c, &s, Some(&fx.api_fix), &h).unwrap().drift.is_empty());
    // The worktree is moved away: refuse, name the moved checkout and the recreate command.
    let moved = fx.home.join("work/ledger-api-fix-moved");
    git(&fx.api, &["worktree", "move", fx.api_fix.to_str().unwrap(), moved.to_str().unwrap()]);
    let (c, _) = scan_once(&env);
    let e = resume::plan(&c, &s, None, &h).unwrap_err();
    assert!(e.contains(&format!("{} no longer exists", fx.api_fix.display())), "{e}");
    assert!(e.contains(&format!("git -C {} worktree move {} {}", fx.api.display(), moved.display(), fx.api_fix.display())), "branch is checked out at the new path: {e}");
    assert!(e.contains(&format!("--cwd {}", moved.display())), "reconnection candidate on the recorded branch: {e}");
    let p = resume::plan(&c, &s, Some(&moved), &h).unwrap();
    assert!(p.drift.iter().any(|l| l.contains("recorded cwd no longer exists")));
    // Worktree deleted outright: recreate it on the recorded branch.
    git(&fx.api, &["worktree", "remove", "--force", moved.to_str().unwrap()]);
    let (c, _) = scan_once(&env);
    let e = resume::plan(&c, &s, None, &h).unwrap_err();
    assert!(e.contains(&format!("git -C {} worktree add {} fix/webhook-retry", fx.api.display(), fx.api_fix.display())), "{e}");
    assert!(!e.contains("--cwd /"), "no candidate left: {e}");
    assert!(resume::plan(&c, &s, Some(&fx.home.join("nope")), &h).unwrap_err().contains("--cwd"));
}

/// Review replay: a transcript without `gitBranch` whose worktree was `rm -rf`'d
/// (still registered in git) must still get a working recreate hint, using the
/// branch of the checkout it was recorded in.
#[test]
fn resume_hint_for_deleted_worktree_uses_recorded_checkout_branch() {
    let fx = crate::testutil::ledger();
    let wt = fx.home.join("work/ledger-api-old");
    git(&fx.api, &["worktree", "add", "-q", "-b", "old/experiment", wt.to_str().unwrap()]);
    let w = wt.display().to_string();
    let t = fx.home.join(".claude/projects").join(crate::util::claude_project_dir_name(&w)).join("dddd4444-old.jsonl");
    write(
        &t,
        &crate::testutil::jsonl(&[serde_json::json!({"type":"user","cwd":w,"sessionId":"dddd4444-old","timestamp":"2026-08-15T10:00:00Z","message":{"role":"user","content":"try advisory locks"}})]),
    );
    let env = Env::with_home(&fx.home);
    let (_c, _) = scan_once(&env);
    std::fs::remove_dir_all(&wt).unwrap();
    let (c, _) = scan_once(&env);
    let s = sid("claude", "dddd4444-old");
    let h = |_: &str, _: &[&str]| Some("--resume".to_string());
    let e = resume::plan(&c, &s, None, &h).unwrap_err();
    let main = fx.api.display();
    assert!(e.contains(&format!("git -C {main} worktree prune && git -C {main} worktree add {w} old/experiment")), "{e}");
}

#[test]
fn resume_refuses_subagent_transcripts_and_runs_argv_without_a_shell() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    // A Claude subagent transcript next to its parent session.
    let m = fx.main.display().to_string();
    let sub = fx.claude_file.parent().unwrap().join("11111111-aaaa/subagents/agent-a1b2.jsonl");
    write(
        &sub,
        &crate::testutil::jsonl(&[
            json!({"type":"user","isSidechain":true,"agentId":"a1b2","cwd":m,"sessionId":"agent-a1b2","timestamp":"2026-02-01T10:00:01Z","message":{"role":"user","content":"search the repo"}}),
        ]),
    );
    let (c, _) = scan_once(&env);
    let h = resume_help("@@none@@");
    // Indexed by the scan once subagent transcripts are parsed; until then, the
    // node a parser would write (subagent lines carry the parent's sessionId).
    let id: String = c.query_row("SELECT id FROM nodes WHERE kind='session' AND path LIKE '%/subagents/%'", [], |r| r.get(0)).unwrap_or_else(|_| {
        let id = crate::util::stable_id("session", "claude:agent-a1b2");
        let repo: String = c.query_row("SELECT repo_id FROM nodes WHERE id=?1", [sid("claude", "11111111-aaaa")], |r| r.get(0)).unwrap();
        let meta = json!({"provider": "claude", "native_id": "11111111-aaaa", "cwd": m, "is_subagent": true});
        db::upsert_node(&c, &db::NodeRow { id: &id, kind: "session", label: "search the repo", path: Some(sub.to_str().unwrap()), repo_id: Some(&repo), checkout_id: None, ts: Some(1), meta: &meta })
            .unwrap();
        id
    });
    let e = resume::plan(&c, &id, None, &h).unwrap_err();
    assert!(e.contains("subagent") && e.contains("workmap resume proj:session:11111111-aaa"), "{e}");
    // Execution: argv passed verbatim (spaces, quotes), in the plan's cwd, no shell.
    let t = TempDir::new("fakecli");
    let (bin, log) = recorder(t.path(), "fake-claude");
    let mut p = resume::plan(&c, &sid("claude", "11111111-aaaa"), None, &h).unwrap();
    p.program = bin.display().to_string();
    p.args.push("it's a \"test\"; rm -rf /".into());
    assert_eq!(resume::execute(&p).unwrap(), 0);
    let rec = wait_for(&log);
    let lines: Vec<&str> = rec.lines().collect();
    assert_eq!(lines, vec![fx.main.display().to_string().as_str(), "--resume", "11111111-aaaa", "it's a \"test\"; rm -rf /"]);
}
