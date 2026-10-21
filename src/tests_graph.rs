//! Cross-project / cross-worktree file views and task graph expansion.
//! Synthetic fixtures only: temp homes, fake CLIs, no network, no real home.

#[allow(unused_imports)]
use super::*;

#[test]
fn ledger_fixture_scans_and_qualified_refs_resolve_without_scope() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let p = view::resolve_project(&c, &fx.api_fix.to_string_lossy()).unwrap();
    assert!(p.handle.starts_with("ledger-api~"), "two clones of one remote get compact handles: {}", p.handle);
    let r = view::qualified_ref(&c, &p.focus, "ledger/webhooks.py").unwrap();
    assert!(r.ends_with("@fix/webhook-retry:ledger/webhooks.py"), "{r}");
    let out = view::expand(&v(&c), &r, &view::Req::default()).unwrap();
    assert!(out.starts_with("# :ledger/webhooks.py — file in ") && out.contains("Double charge on webhook retry"), "{out}");
    assert!(fx.conv_cross.exists() && fx.sdk.exists() && fx.api.exists() && fx.api_clone.exists() && fx.conv_fix.exists());
}

/// Extra synthetic Claude transcript: `cwd` checkout, reading `reads` (absolute paths).
fn claude_conv(home: &Path, cwd: &Path, id: &str, title: &str, ts: &str, reads: &[PathBuf]) {
    let c = cwd.display().to_string();
    let uses: Vec<serde_json::Value> = reads.iter().enumerate().map(|(i, p)| json!({"type":"tool_use","id":format!("x{i}"),"name":"Read","input":{"file_path": p.display().to_string()}})).collect();
    let results: Vec<serde_json::Value> = reads.iter().enumerate().map(|(i, _)| json!({"type":"tool_result","tool_use_id":format!("x{i}"),"content":"..."})).collect();
    let f = home.join(".claude/projects").join(crate::util::claude_project_dir_name(&c)).join(format!("{id}.jsonl"));
    write(
        &f,
        &crate::testutil::jsonl(&[
            json!({"type":"summary","summary":title}),
            json!({"type":"user","cwd":c,"sessionId":id,"timestamp":ts,"message":{"role":"user","content":format!("{title} please")}}),
            json!({"type":"assistant","cwd":c,"sessionId":id,"timestamp":ts,"message":{"content":uses}}),
            json!({"type":"user","cwd":c,"sessionId":id,"timestamp":ts,"toolUseResult":{},"message":{"content":results}}),
        ]),
    );
}

/// Section of an output starting at `## <title>` up to the next `## `.
fn section<'a>(out: &'a str, title: &str) -> &'a str {
    let i = out.find(&format!("\n## {title}")).unwrap_or_else(|| panic!("no section {title}:\n{out}"));
    let rest = &out[i + 1..];
    match rest[3..].find("\n## ") {
        Some(j) => &rest[..j + 4],
        None => rest,
    }
}

/// `## Title (N …` → N.
fn header_count(sec: &str) -> usize {
    let h = sec.lines().next().unwrap();
    h.split('(').nth(1).unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

fn items(sec: &str) -> Vec<&str> {
    sec.lines().filter(|l| l.starts_with("- ")).collect()
}

#[test]
fn cross_project_file_view_lists_conversations_from_other_projects() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let out = view::expand(&v(&c), "ledger-sdk:src/retry.ts", &view::Req::default()).unwrap();
    let sec = section(&out, "Conversations");
    assert_eq!(header_count(sec), 1, "{out}");
    assert_eq!(items(sec).len(), 1, "count equals listed items:\n{out}");
    let api = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap();
    let want = format!("{}:conversation:Port retry backoff", api.handle);
    let line = items(sec)[0];
    assert!(line.contains(&format!("'{want}'")) && line.contains(&format!("[cwd {} @main]", api.handle)) && line.contains("r Read l.3"), "{line}");
    // The printed ref expands without any scope to that conversation.
    let conv = view::expand(&v(&c), &want, &view::Req::default()).unwrap();
    assert!(conv.starts_with("# ':conversation:Port retry backoff'") && conv.contains("bbbb2222-cross"), "{conv}");
    assert_refs_roundtrip(&c, "ledger-sdk", &out);
    // Scoped view of the same file prints the same qualified ref.
    let scoped_out = view::expand(&v(&c), ":src/retry.ts", &scoped("ledger-sdk")).unwrap();
    assert!(scoped_out.contains(&format!("'{want}'")), "{scoped_out}");
}

#[test]
fn file_view_pagination_counts_and_more_lines_work() {
    let fx = crate::testutil::ledger();
    let retry = fx.sdk.join("src/retry.ts");
    claude_conv(&fx.home, &fx.sdk, "cccc3333-sdk", "Tune sdk backoff jitter", "2026-09-05T08:00:00Z", std::slice::from_ref(&retry));
    claude_conv(&fx.home, &fx.api_fix, "dddd4444-fixwt", "Compare retry helpers", "2026-09-06T08:00:00Z", &[retry]);
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let req = |limit: Option<usize>| view::Req { limit, ..scoped("ledger-sdk") };
    let full = view::expand(&v(&c), ":src/retry.ts", &req(None)).unwrap();
    let sec = section(&full, "Conversations");
    assert_eq!(header_count(sec), 3, "{full}");
    assert_eq!(items(sec).len(), 3, "{full}");
    assert!(sec.contains("Tune sdk backoff") && sec.contains("@fix/webhook-retry]") && sec.contains("[cwd ledger-api~"), "{sec}");
    let one = view::expand(&v(&c), ":src/retry.ts", &req(Some(1))).unwrap();
    let sec1 = section(&one, "Conversations");
    assert_eq!(header_count(sec1), 3);
    assert_eq!(items(sec1).len(), 1, "{one}");
    let more = sec1.lines().find(|l| l.contains("… 2 more (expand")).unwrap_or_else(|| panic!("{one}"));
    let n: usize = more.rsplit("--limit ").next().unwrap().trim_end_matches(')').parse().unwrap();
    let all = view::expand(&v(&c), ":src/retry.ts", &req(Some(n))).unwrap();
    assert_eq!(items(section(&all, "Conversations")).len(), 3, "{all}");
    for o in [&full, &one, &all] {
        assert_refs_roundtrip(&c, "ledger-sdk", o);
    }
}

fn head_sha(dir: &Path) -> String {
    let o = std::process::Command::new("git").arg("-C").arg(dir).args(["rev-parse", "HEAD"]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[test]
fn main_file_view_shows_same_path_in_sibling_worktree_only_of_same_repo() {
    let fx = crate::testutil::ledger();
    // Same relative path in a different repository (second clone, other repo id).
    write(&fx.api_clone.join("ledger/webhooks.py"), "def handle(event):\n    pass\n");
    git(&fx.api_clone, &["add", "-A"]);
    git(&fx.api_clone, &["commit", "-qm", "Clone-only webhook stub"]);
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let api = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap();
    let h = api.handle.clone();
    let out = view::expand(&v(&c), ":ledger/webhooks.py", &scoped(&h)).unwrap();
    let (own, others) = out.split_once("\n## Same path in other checkouts").unwrap_or_else(|| panic!("{out}"));
    assert!(!own.contains("Double charge"), "main's own sections stay main's:\n{out}");
    let fix_sha = head_sha(&fx.api_fix);
    let file = format!("{h}@fix/webhook-retry:ledger/webhooks.py");
    let commit = format!("{h}@fix/webhook-retry:commit:{}", &fix_sha[..8]);
    let conv = format!("'{h}@fix/webhook-retry:conversation:Double charge");
    assert!(others.contains(&format!("- {file} — DIFFERS from main (blob ")), "{out}");
    assert!(others.contains(&conv) && others.contains("w Edit l.3"), "{out}");
    assert!(others.contains(&commit) && others.contains("Reuse provider idempotency key") && others.contains("not in main's indexed history"), "{out}");
    let initial = head_sha(&fx.api);
    assert!(!others.contains(&initial[..8]), "commits shared with main are not repeated:\n{out}");
    assert!(!out.contains("Clone-only") && !out.contains(&fx.api_clone.display().to_string()) && !out.contains("mirror"), "other repo never shown:\n{out}");
    // Every emitted ref is fully qualified and resolves with no scope at all.
    for r in [&file, &commit] {
        view::expand(&v(&c), r, &view::Req::default()).unwrap_or_else(|e| panic!("{r}: {e}"));
    }
    assert!(assert_refs_roundtrip(&c, &h, others) >= 3);
    assert_refs_roundtrip(&c, &h, &out);
    // Uncommitted on-disk change in the sibling is reported as such.
    std::fs::write(fx.api_fix.join("ledger/webhooks.py"), "# local edit\n").unwrap();
    let dirty = view::expand(&v(&c), ":ledger/webhooks.py", &scoped(&h)).unwrap();
    assert!(dirty.contains("uncommitted change (dirty)"), "{dirty}");
    // A file only in the sibling is discoverable from main instead of an error.
    write(&fx.api_fix.join("ledger/retry_policy.py"), "MAX = 3\n");
    git(&fx.api_fix, &["add", "-A"]);
    git(&fx.api_fix, &["commit", "-qm", "Add retry policy"]);
    drop(c);
    let (c, _) = scan_once(&env);
    let only = view::expand(&v(&c), ":ledger/retry_policy.py", &scoped(&h)).unwrap();
    assert!(only.contains("NOT in this checkout") && only.contains(&format!("{h}@fix/webhook-retry:ledger/retry_policy.py")) && only.contains("Add retry policy"), "{only}");
    assert_refs_roundtrip(&c, &h, &only);
}

#[test]
fn conversation_access_lists_match_counts_after_worktree_removal() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    drop(c);
    std::fs::remove_dir_all(&fx.api_fix).unwrap();
    let (c, _) = scan_once(&env);
    let api = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap();
    let out = view::expand(&v(&c), ":conversation:Double charge", &scoped(&api.handle)).unwrap();
    for title in ["w — written", "r — read"] {
        if let Some(i) = out.find(&format!("\n## {title}")) {
            let sec = section(&out[i..], title);
            assert_eq!(header_count(sec), items(sec).len(), "{out}");
        }
    }
    assert_refs_roundtrip(&c, &api.handle, &out);
}

fn task_out(c: &Connection, project: &str, task: &str, files: &[String], budget: usize) -> String {
    let t = view::more::TaskReq { task: Some(task), files, budget };
    view::task(&v(c), project, &view::Req::default(), &t).unwrap()
}

fn ranked_lines(out: &str) -> Vec<&str> {
    section(out, "Ranked").lines().filter(|l| l.starts_with("- ")).collect()
}

#[test]
fn task_without_file_reaches_written_file_and_fix_commit_with_reason_chains() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let h = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap().handle;
    let out = task_out(&c, &h, "customers charged twice", &[], 6000);
    let lines = ranked_lines(&out);
    let file = format!("- {h}@fix/webhook-retry:ledger/webhooks.py [");
    let fl = lines.iter().find(|l| l.starts_with(&file)).unwrap_or_else(|| panic!("hop1 file missing:\n{out}"));
    assert!(fl.contains("[hop1] written by ':conversation:Double charge' (w Edit l.3) which matched") && fl.contains("'charged'") && fl.contains(" in :message:aaaa1111:l2"), "{fl}");
    assert!(out.contains("DIFFERS from main's :ledger/webhooks.py"), "same-path signal for the reached file:\n{out}");
    let sha = head_sha(&fx.api_fix);
    let commit = format!("- {h}@fix/webhook-retry:commit:{} [", &sha[..8]);
    let cl = lines.iter().find(|l| l.starts_with(&commit)).unwrap_or_else(|| panic!("hop2 commit missing:\n{out}"));
    assert!(cl.contains("[hop2] changed") && cl.contains("not in main's indexed history") && cl.contains("that file was written by"), "{cl}");
    // Files reached by observed writes outrank the commits derived from them.
    let pos = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap();
    assert!(pos(&file) < pos(&commit));
    assert!(assert_refs_roundtrip(&c, &h, &out) >= 4);
    assert_eq!(out, task_out(&c, &h, "customers charged twice", &[], 6000), "deterministic");
    // Cross-project hop: the conversation in ledger-api read the sdk's file.
    let x = task_out(&c, &h, "exponential backoff", &[], 6000);
    let xl = ranked_lines(&x);
    let sdk = xl.iter().find(|l| l.starts_with("- ledger-sdk:src/retry.ts (other project) [")).unwrap_or_else(|| panic!("{x}"));
    assert!(sdk.contains("[hop1] read by ':conversation:Port retry backoff' (r Read l.3)"), "{sdk}");
    assert!(view::expand(&v(&c), "ledger-sdk:src/retry.ts", &view::Req::default()).is_ok());
    let sdk_sha = head_sha(&fx.sdk);
    let sc = format!("- ledger-sdk:commit:{} (other project) [", &sdk_sha[..8]);
    assert!(xl.iter().any(|l| l.starts_with(&sc) && l.contains("[hop2]")), "{x}");
    assert!(view::expand(&v(&c), &format!("ledger-sdk:commit:{}", &sdk_sha[..8]), &view::Req::default()).is_ok());
    assert_refs_roundtrip(&c, &h, &x);
}

#[test]
fn task_budget_drops_whole_items_and_says_how_to_get_them() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let h = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap().handle;
    let files = ["ledger/webhooks.py".to_string()];
    let full = task_out(&c, &h, "charged twice", &files, 100_000);
    assert!(!full.contains("… omitted"), "{full}");
    assert!(full.contains(&format!("{h}@fix/webhook-retry:conversation:Double charge")) && full.contains("## ledger/webhooks.py: same path in other checkouts"), "{full}");
    let full_lines: std::collections::HashSet<&str> = full.lines().collect();
    let mut saw_omission = false;
    for budget in (800..full.len() + 400).step_by(211) {
        let out = task_out(&c, &h, "charged twice", &files, budget);
        assert!(out.ends_with('\n'), "{out}");
        assert_eq!(out, task_out(&c, &h, "charged twice", &files, budget), "deterministic at {budget}");
        let mut footer = None;
        for l in out.lines() {
            if l.starts_with("… omitted") {
                footer = Some(l);
            } else if !l.starts_with("## Ranked") {
                assert!(full_lines.contains(l), "line cut or invented at budget {budget}: {l:?}\n{out}");
            }
        }
        assert_refs_roundtrip(&c, &h, &out);
        if out.len() > budget {
            assert!(budget < 1400, "only a header longer than a tiny budget may overflow ({budget}, {}):\n{out}", out.len());
        }
        if let Some(f) = footer {
            saw_omission = true;
            assert_eq!(out.lines().last(), Some(f), "omission note is the final line");
            let n: usize = f.split("rerun with --budget ").nth(1).unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
            let again = task_out(&c, &h, "charged twice", &files, n);
            assert!(!again.contains("… omitted"), "suggested budget {n} shows everything:\n{again}");
        }
    }
    assert!(saw_omission);
}
