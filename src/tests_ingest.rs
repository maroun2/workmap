//! Targeted ingest, subagent transcripts, Codex parsed_cmd, moved/deleted checkouts.
//! Synthetic fixtures only: temp homes, fake CLIs, no network, no real home.

#[allow(unused_imports)]
use super::*;
use crate::testutil::{commit_all, jsonl, ledger, Ledger};
use std::collections::BTreeSet;
use std::time::Instant;

fn claude_file(home: &Path, cwd: &Path, id: &str) -> PathBuf {
    home.join(".claude/projects").join(crate::util::claude_project_dir_name(&cwd.display().to_string())).join(format!("{id}.jsonl"))
}

/// A Claude transcript: title, one user prompt, optional tool calls with results, a reply.
fn claude_conv(file: &Path, sid: &str, cwd: &Path, title: &str, prompt: &str, tools: &[(&str, &str, &str)], reply: &str) {
    let c = cwd.display().to_string();
    let uses: Vec<serde_json::Value> = tools.iter().map(|(id, name, path)| json!({"type":"tool_use","id":id,"name":name,"input":{"file_path": path}})).collect();
    let results: Vec<serde_json::Value> = tools.iter().map(|(id, _, _)| json!({"type":"tool_result","tool_use_id":id,"content":"ok"})).collect();
    write(
        file,
        &jsonl(&[
            json!({"type":"summary","summary":title}),
            json!({"type":"user","cwd":c,"sessionId":sid,"gitBranch":"main","timestamp":"2026-09-10T10:00:00Z","message":{"role":"user","content":prompt}}),
            json!({"type":"assistant","cwd":c,"sessionId":sid,"timestamp":"2026-09-10T10:00:05Z","message":{"content":uses}}),
            json!({"type":"user","cwd":c,"sessionId":sid,"timestamp":"2026-09-10T10:00:06Z","toolUseResult":{},"message":{"content":results}}),
            json!({"type":"assistant","cwd":c,"sessionId":sid,"timestamp":"2026-09-10T10:01:00Z","message":{"content":[{"type":"text","text":reply}]}}),
        ]),
    );
}

fn handle_of(c: &Connection, dir: &Path) -> String {
    view::resolve_project(c, &dir.to_string_lossy()).unwrap().handle
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Every evidence row an origin (`src:<transcript>` or a checkout id) produced.
fn origin_rows(c: &Connection, origin: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut st = c.prepare("SELECT src||'|'||dst||'|'||kind||'|'||strength||'|'||source_line||'|'||note FROM edges WHERE origin=?1").unwrap();
    out.extend(st.query_map([origin], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()));
    let mut st = c.prepare("SELECT 'p|'||node_id||'|'||loc||'|'||part||'|'||text FROM passages WHERE origin=?1").unwrap();
    out.extend(st.query_map([origin], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()));
    out
}

/// The whole derived index, minus scan bookkeeping, as comparable rows.
fn index_dump(c: &Connection) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for sql in [
        "SELECT 'n|'||id||'|'||kind||'|'||label||'|'||coalesce(path,'')||'|'||coalesce(repo_id,'')||'|'||coalesce(checkout_id,'')||'|'||json_remove(meta,'$.sig','$.first_observed') FROM nodes WHERE kind NOT IN ('checkout','repo')",
        "SELECT 'e|'||src||'|'||dst||'|'||kind||'|'||strength||'|'||source_path||'|'||source_line||'|'||note||'|'||origin FROM edges",
        "SELECT 'p|'||kind||'|'||node_id||'|'||loc||'|'||part||'|'||text||'|'||origin FROM passages",
        "SELECT 's|'||path||'|'||status||'|'||coalesce(session_id,'')||'|'||coalesce(cwd,'') FROM source_files",
    ] {
        let mut st = c.prepare(sql).unwrap();
        out.extend(st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()));
    }
    out
}

fn search(c: &Connection, words: &str) -> String {
    let q = crate::search::Query { text: words.into(), kinds: vec![], provider: None, speaker: None, since: None, limit: 10, any: false, exact: false };
    view::more::search_view(&v(c), &view::Req::default(), &view::more::SearchReq { query: q, this_worktree: false }).unwrap()
}

fn append(file: &Path, lines: &[serde_json::Value]) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).open(file).unwrap();
    f.write_all(jsonl(lines).as_bytes()).unwrap();
}

fn conv_section<'a>(out: &'a str, title: &str) -> Vec<&'a str> {
    out.split(&format!("\n## {title}")).nth(1).map(|s| s.split("\n\n").next().unwrap_or("").lines().skip(1).collect()).unwrap_or_default()
}

/// Every `## heading (N)` group must list its items (the review saw `## w — written (1)` with nothing under it).
fn assert_groups_listed(out: &str) {
    let lines: Vec<&str> = out.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if !l.starts_with("## ") {
            continue;
        }
        let Some(n) = l.rsplit_once('(').and_then(|x| x.1.trim_end_matches(')').split([',', ';', ' ']).next().and_then(|d| d.parse::<usize>().ok())) else { continue };
        if n == 0 {
            continue;
        }
        let items = lines[i + 1..].iter().take_while(|x| x.starts_with("- ") || x.starts_with("  …")).filter(|x| x.starts_with("- ")).count();
        assert!(items > 0, "group {l:?} announces {n} item(s) but lists none:\n{out}");
    }
}

// ------------------------------------------------------------------ item 5: targeted ingest

#[test]
fn targeted_ingest_is_fast_touches_only_named_sources_and_is_visible_to_other_connections() {
    let fx: Ledger = ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let cross_origin = format!("src:{}", fx.conv_cross.display());
    let cross_before = origin_rows(&c, &cross_origin);
    assert!(!cross_before.is_empty());
    let checkouts_before = count(&c, "SELECT count(*) FROM nodes WHERE kind='checkout'");
    let doc_passages_before = count(&c, "SELECT count(*) FROM passages WHERE kind IN ('doc','commit','project')");

    // conv_fix grows (new reply + a read), a new conversation appears, and one runs elsewhere.
    let f = fx.api_fix.display().to_string();
    append(
        &fx.conv_fix,
        &[
            json!({"type":"assistant","cwd":f,"sessionId":"aaaa1111-fix","timestamp":"2026-09-02T11:00:00Z","message":{"content":[
                {"type":"text","text":"Follow-up: the zanzibarquux retry window also needs a test."},
                {"type":"tool_use","id":"r9","name":"Read","input":{"file_path": format!("{f}/ledger/charges.py")}}]}}),
            json!({"type":"user","cwd":f,"sessionId":"aaaa1111-fix","timestamp":"2026-09-02T11:00:01Z","toolUseResult":{},"message":{"content":[{"type":"tool_result","tool_use_id":"r9","content":"..."}]}}),
        ],
    );
    let fresh = claude_file(&fx.home, &fx.api, "cccc3333-new");
    claude_conv(&fresh, "cccc3333-new", &fx.api, "Quarantine flaky ledger test", "the ledger test flakes with a qorvexian timeout", &[("x1", "Read", "README.md")], "Marked it.");
    let elsewhere = claude_file(&fx.home, Path::new("/nowhere/at/all"), "dddd4444-else");
    claude_conv(&elsewhere, "dddd4444-else", Path::new("/nowhere/at/all"), "Unrelated", "unrelated work", &[], "ok");

    let mut c2 = db::open(&env.db_path()).unwrap();
    let t0 = Instant::now();
    let r = scan::ingest(&env, &mut c2, &scan::IngestOpts { transcripts: vec![fx.conv_fix.clone(), fresh.clone(), elsewhere.clone()], checkouts: vec![] }).unwrap();
    let ms = t0.elapsed().as_millis();
    eprintln!("targeted ingest of 3 transcripts: {ms} ms (reported {} ms)", r.ms);
    assert!(ms < 2000, "ingest must be fast, took {ms} ms");
    assert!(r.lines.iter().any(|l| l.starts_with("parsed claude") && l.ends_with("cccc3333-new.jsonl")), "{:?}", r.lines);
    assert!(r.lines.iter().any(|l| l.starts_with("unrelated claude") && l.contains("/nowhere/at/all")), "{:?}", r.lines);

    // Another connection (as another process would) sees the new text right away.
    let reader = db::open(&env.db_path()).unwrap();
    let hits = search(&reader, "zanzibarquux");
    assert!(hits.contains("Double charge on webhook retry"), "{hits}");
    let hits = search(&reader, "qorvexian");
    assert!(hits.contains("Quarantine flaky ledger test"), "{hits}");
    let api = handle_of(&reader, &fx.api);
    let conv = view::expand(&v(&reader), ":conversation:Quarantine flaky", &scoped(&api)).unwrap();
    assert!(conv.contains("## r — read (1)") && conv.contains(":README.md"), "{conv}");
    assert_refs_roundtrip(&reader, &api, &conv);
    let fix_sid = sid("claude", "aaaa1111-fix");
    assert!(edges_from(&reader, &fix_sid).iter().any(|e| e.0 == "read" && e.1 == "observed" && e.2.ends_with("ledger-api-fix/ledger/charges.py")));
    // Unknown cwd is recorded as such, truthfully.
    let st: (String, Option<String>, Option<String>) =
        reader.query_row("SELECT status, cwd, session_id FROM source_files WHERE path=?1", [elsewhere.to_string_lossy()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(st, ("unrelated".to_string(), Some("/nowhere/at/all".to_string()), None));

    // Nothing else was touched: another transcript's rows, checkouts, docs and commits.
    assert_eq!(origin_rows(&reader, &cross_origin), cross_before);
    assert_eq!(count(&reader, "SELECT count(*) FROM nodes WHERE kind='checkout'"), checkouts_before);
    assert_eq!(count(&reader, "SELECT count(*) FROM passages WHERE kind IN ('doc','commit','project')"), doc_passages_before);

    // Same rows as a full scan would write: a scan afterwards changes nothing.
    let after_ingest = index_dump(&reader);
    let mut c3 = db::open(&env.db_path()).unwrap();
    scan::run(&env, &mut c3, &scan::ScanOpts::default()).unwrap();
    let after_scan = index_dump(&c3);
    let only_ingest: Vec<_> = after_ingest.difference(&after_scan).collect();
    let only_scan: Vec<_> = after_scan.difference(&after_ingest).collect();
    assert!(only_ingest.is_empty() && only_scan.is_empty(), "ingest and scan disagree:\ningest-only {only_ingest:#?}\nscan-only {only_scan:#?}");

    // An unchanged source is not re-parsed; a relative spelling hits the same row.
    let r = scan::ingest(&env, &mut c3, &scan::IngestOpts { transcripts: vec![fresh.clone()], checkouts: vec![] }).unwrap();
    assert!(r.lines[0].starts_with("unchanged claude"), "{:?}", r.lines);
    assert_eq!(count(&c3, "SELECT count(*) FROM source_files WHERE path LIKE '%cccc3333-new.jsonl'"), 1);
    // A transcript deleted since: evidence kept, flagged missing.
    std::fs::remove_file(&fresh).unwrap();
    let r = scan::ingest(&env, &mut c3, &scan::IngestOpts { transcripts: vec![fresh.clone()], checkouts: vec![] }).unwrap();
    assert!(r.lines[0].contains("flagged missing"), "{:?}", r.lines);
    let conv = view::expand(&v(&c3), ":conversation:Quarantine flaky", &scoped(&api)).unwrap();
    assert!(conv.contains("source transcript missing") && conv.contains(":README.md"), "{conv}");
    // OpenCode databases need a session; the message says how.
    let e = scan::ingest(&env, &mut c3, &scan::IngestOpts { transcripts: vec![fx.home.join(".local/share/opencode/opencode.db")], checkouts: vec![] }).unwrap_err();
    assert!(e.contains("#session=<id>"), "{e}");
}

#[test]
fn ingest_checkout_reindexes_only_that_checkout() {
    let fx = ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let sdk_co = crate::util::stable_id("checkout", &fx.sdk.to_string_lossy());
    let sdk_before = origin_rows(&c, &sdk_co);
    write(&fx.api.join("docs/runbook.md"), "# Runbook\n\nRestart the vexillary worker after a deploy.\n");
    commit_all(&fx.api, "Add runbook");
    let mut c2 = db::open(&env.db_path()).unwrap();
    let r = scan::ingest(&env, &mut c2, &scan::IngestOpts { transcripts: vec![], checkouts: vec![fx.api.join("ledger")] }).unwrap();
    assert_eq!(r.checkouts, 1, "{:?}", r.lines);
    assert!(search(&c2, "vexillary").contains("docs/runbook.md"));
    assert_eq!(origin_rows(&c2, &sdk_co), sdk_before, "other checkouts are not re-indexed");
}

#[test]
fn commit_ref_expands_in_both_clones_after_shared_sha_node_is_overwritten() {
    let fx = ledger();
    let clone = fx.home.join("mirror/exact-ledger-api");
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git(&fx.home, &["clone", "-q", fx.api.to_str().unwrap(), clone.to_str().unwrap()]);
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let sha = String::from_utf8(std::process::Command::new("git").arg("-C").arg(&fx.api).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    let prefix = sha.trim().chars().take(8).collect::<String>();
    for root in [&fx.api, &clone] {
        let handle = handle_of(&c, root);
        let out = view::expand(&v(&c), &format!("{handle}:commit:{prefix}"), &scoped(&handle)).unwrap_or_else(|e| panic!("{root:?}: {e}"));
        assert!(out.contains(&prefix), "{root:?}: {out}");
    }
}

#[test]
fn concurrent_ingest_and_scan_do_not_corrupt_the_index() {
    let fx = ledger();
    let env = Env::with_home(&fx.home);
    drop(scan_once(&env));
    let env2 = env.clone();
    let scanner = std::thread::spawn(move || {
        for _ in 0..3 {
            let mut c = db::open(&env2.db_path()).unwrap();
            scan::run(&env2, &mut c, &scan::ScanOpts { full: true, ..Default::default() }).unwrap();
        }
    });
    let f = fx.api_fix.display().to_string();
    for i in 0..12 {
        append(
            &fx.conv_fix,
            &[
                json!({"type":"assistant","cwd":f,"sessionId":"aaaa1111-fix","timestamp":"2026-09-02T12:00:00Z","message":{"content":[{"type":"text","text":format!("note number {i} on the retry fix")}]}}),
            ],
        );
        let mut c = db::open(&env.db_path()).unwrap();
        scan::ingest(&env, &mut c, &scan::IngestOpts { transcripts: vec![fx.conv_fix.clone()], checkouts: vec![] }).unwrap();
        // A reader always sees one whole version of the conversation: no duplicated messages.
        let dup = count(&c, "SELECT count(*) FROM (SELECT node_id, loc, part FROM passages WHERE kind='message' GROUP BY 1,2,3 HAVING count(*) > 1)");
        assert_eq!(dup, 0);
    }
    scanner.join().unwrap();
    let c = db::open(&env.db_path()).unwrap();
    let ok: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    assert_eq!(ok, "ok");
    c.execute("INSERT INTO passages_fts(passages_fts) VALUES('integrity-check')", []).unwrap();
    assert_eq!(count(&c, "SELECT count(*) FROM (SELECT node_id, loc, part FROM passages WHERE kind='message' GROUP BY 1,2,3 HAVING count(*) > 1)"), 0);
    assert!(search(&c, "note number 11").contains("Double charge on webhook retry"));
    assert_eq!(count(&c, "SELECT count(*) FROM source_files WHERE path LIKE '%aaaa1111-fix.jsonl'"), 1);
}

#[test]
fn partial_root_scan_keeps_projects_and_sources_outside_the_roots() {
    let fx = ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let cos = count(&c, "SELECT count(*) FROM nodes WHERE kind='checkout'");
    let cross = sid("claude", "bbbb2222-cross");
    let edges_before = edges_from(&c, &cross);
    let m = fx.api.display().to_string();
    append(
        &fx.conv_cross,
        &[json!({"type":"assistant","cwd":m,"sessionId":"bbbb2222-cross","timestamp":"2026-09-03T10:00:00Z","message":{"content":[{"type":"text","text":"Ported; the plinthwise jitter stays."}]}})],
    );
    let mut c2 = db::open(&env.db_path()).unwrap();
    let st = scan::run(&env, &mut c2, &scan::ScanOpts { include_home: false, roots: vec![fx.sdk.clone()], ..Default::default() }).unwrap();
    assert_eq!(st.checkouts_pruned + st.checkouts_missing, 0);
    assert_eq!(count(&c2, "SELECT count(*) FROM nodes WHERE kind='checkout'"), cos, "checkouts outside the roots stay");
    let row: (String, Option<String>) = c2.query_row("SELECT status, session_id FROM source_files WHERE path=?1", [fx.conv_cross.to_string_lossy()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(row, ("parsed".to_string(), Some(cross.clone())), "a changed transcript of an unscanned project is not demoted to unrelated");
    assert_eq!(edges_from(&c2, &cross), edges_before, "its evidence (incl. the cross-project read) is intact");
    let co: Option<String> = c2.query_row("SELECT checkout_id FROM nodes WHERE id=?1", [&cross], |r| r.get(0)).unwrap();
    assert_eq!(co, Some(crate::util::stable_id("checkout", &m)));
    assert!(search(&c2, "plinthwise").contains("Port retry backoff"));
}

// ------------------------------------------------------------------ item 7: subagents

#[test]
fn subagent_transcripts_are_child_sessions_with_parent_edge_and_file_discovery() {
    let fx = ledger();
    let m = fx.api.display().to_string();
    let parent = claude_file(&fx.home, &fx.api, "eeee5555-parent");
    write(
        &parent,
        &jsonl(&[
            json!({"type":"summary","summary":"Audit the charge pipeline"}),
            json!({"type":"user","cwd":m,"sessionId":"eeee5555-parent","timestamp":"2026-09-11T09:00:00Z","message":{"role":"user","content":"audit how charges are posted"}}),
            json!({"type":"assistant","cwd":m,"sessionId":"eeee5555-parent","timestamp":"2026-09-11T09:00:05Z","message":{"content":[{"type":"tool_use","id":"toolu_A1","name":"Agent","input":{"description":"Trace charge path","prompt":"trace it","subagent_type":"Explore"}}]}}),
            json!({"type":"user","cwd":m,"sessionId":"eeee5555-parent","timestamp":"2026-09-11T09:02:00Z","toolUseResult":{"agentId":"a1b2c3d4e5f6a7b8c","status":"completed"},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_A1","content":"done"}]}}),
        ]),
    );
    // <parent-session-id>/subagents/agent-<agentId>.jsonl: every line carries the PARENT's sessionId.
    let sub = parent.with_extension("").join("subagents/agent-a1b2c3d4e5f6a7b8c.jsonl");
    let line = |extra: serde_json::Value| {
        let mut b = json!({"cwd":m,"sessionId":"eeee5555-parent","isSidechain":true,"agentId":"a1b2c3d4e5f6a7b8c","gitBranch":"main"});
        b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        b
    };
    write(
        &sub,
        &jsonl(&[
            line(json!({"type":"user","parentUuid":null,"timestamp":"2026-09-11T09:00:06Z","message":{"role":"user","content":"Trace how a charge reaches post_charge"}})),
            line(
                json!({"type":"assistant","timestamp":"2026-09-11T09:00:10Z","message":{"content":[{"type":"tool_use","id":"s1","name":"Read","input":{"file_path": format!("{m}/ledger/charges.py")}}]}}),
            ),
            line(json!({"type":"user","timestamp":"2026-09-11T09:00:11Z","toolUseResult":{},"message":{"content":[{"type":"tool_result","tool_use_id":"s1","content":"..."}]}})),
            line(json!({"type":"assistant","timestamp":"2026-09-11T09:01:00Z","message":{"content":[{"type":"text","text":"Childfinding: post_charge never checks the quillbrook token."}]}})),
        ]),
    );
    write(&sub.with_file_name("agent-a1b2c3d4e5f6a7b8c.meta.json"), &json!({"agentType":"Explore","description":"Trace charge path","toolUseId":"toolu_A1","spawnDepth":1}).to_string());
    // Tool output dirs next to subagents/ are not transcripts.
    write(&parent.with_extension("").join("tool-results/x.jsonl"), "{\"type\":\"user\"}\n");

    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let p_sid = sid("claude", "eeee5555-parent");
    let c_sid = sid("claude", "a1b2c3d4e5f6a7b8c");
    let label: String = c.query_row("SELECT label FROM nodes WHERE id=?1", [&p_sid], |r| r.get(0)).unwrap();
    assert_eq!(label, "Audit the charge pipeline", "the child must not overwrite its parent's node");
    let (clabel, cmeta): (String, String) = c.query_row("SELECT label, meta FROM nodes WHERE id=?1", [&c_sid], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(clabel, "subagent: Trace charge path");
    let cmeta: serde_json::Value = serde_json::from_str(&cmeta).unwrap();
    assert_eq!(cmeta.pointer("/parent/native_id").and_then(|x| x.as_str()), Some("eeee5555-parent"));
    let note: String = c.query_row("SELECT note FROM edges WHERE src=?1 AND dst=?2 AND kind='subagent_of'", [&c_sid, &p_sid], |r| r.get(0)).unwrap();
    assert!(note.contains("path eeee5555-parent/subagents/agent-a1b2c3d4e5f6a7b8c.jsonl") && note.contains("sessionId") && note.contains("toolUseId"), "{note}");
    assert_eq!(count(&c, "SELECT count(*) FROM source_files WHERE path LIKE '%tool-results%'"), 0);

    // The child's read is discoverable from the file view, marked as a subagent of the parent.
    let api = handle_of(&c, &fx.api);
    let f = view::expand(&v(&c), ":ledger/charges.py", &scoped(&api)).unwrap();
    let child_line = f.lines().find(|l| l.contains("subagent: Trace charge path")).unwrap_or_else(|| panic!("{f}"));
    assert!(child_line.contains("r Read") && child_line.contains("(subagent of ':conversation:Audit the charge')"), "{child_line}");
    assert_refs_roundtrip(&c, &api, &f);
    // Parent lists its children; the child names its parent and refuses ask.
    let pv = view::expand(&v(&c), ":conversation:Audit the charge pipeline", &scoped(&api)).unwrap();
    let kids = conv_section(&pv, "Subagents (1)");
    assert!(kids.len() == 1 && kids[0].contains("subagent: Trace charge path") && kids[0].contains("r1"), "{pv}");
    assert_refs_roundtrip(&c, &api, &pv);
    let child_ref = printed_refs(kids[0])[0].clone();
    let cv = expand_ref(&c, &api, &child_ref).unwrap();
    assert!(cv.contains("subagent of ':conversation:Audit the charge' — evidence: path eeee5555-parent/subagents/"), "{cv}");
    assert!(cv.contains("Ask: not available for a subagent transcript") && !cv.contains("ask :"), "{cv}");
    assert_refs_roundtrip(&c, &api, &cv);
    let h = |_: &str, _: &[&str]| Some("--resume --fork-session --no-session-persistence --tools --strict-mcp-config --permission-mode dontAsk --disallowedTools".to_string());
    let e = ask::plan(&c, &c_sid, "q", &h).unwrap_err();
    assert!(e.contains("Resume the parent") && e.contains("session:eeee5555-parent") && !e.contains("--resume"), "{e}");
    assert!(ask::plan(&c, &p_sid, "q", &h).is_ok(), "the parent itself stays resumable");
    // Child messages are searchable.
    assert!(search(&c, "quillbrook").contains("subagent: Trace charge path"));

    // Targeted ingest of the child alone gives the same rows.
    let before = origin_rows(&c, &format!("src:{}", sub.display()));
    let mut c2 = db::open(&env.db_path()).unwrap();
    c2.execute("UPDATE source_files SET mtime=-1 WHERE path=?1", [sub.to_string_lossy()]).unwrap();
    let r = scan::ingest(&env, &mut c2, &scan::IngestOpts { transcripts: vec![sub.clone()], checkouts: vec![] }).unwrap();
    assert!(r.lines[0].starts_with("parsed claude"), "{:?}", r.lines);
    assert_eq!(origin_rows(&c2, &format!("src:{}", sub.display())), before);
}

// ------------------------------------------------------------------ item 7: moved / deleted checkouts

/// ledger + a `spike/batching` worktree with a committed file and a conversation that wrote it.
fn with_spike(fx: &Ledger) -> (PathBuf, PathBuf) {
    let spike = fx.home.join("work/ledger-api-spike");
    git(&fx.api, &["worktree", "add", "-q", "-b", "spike/batching", spike.to_str().unwrap()]);
    write(&spike.join("ledger/batch.py"), "def batch(charges):\n    return charges\n");
    commit_all(&spike, "Batch charges");
    let conv = claude_file(&fx.home, &spike, "ffff6666-spike");
    claude_conv(
        &conv,
        "ffff6666-spike",
        &spike,
        "Batch charges spike",
        "try batching charges per merchant",
        &[("b1", "Write", &format!("{}/ledger/batch.py", spike.display()))],
        "Batching works for the gloamrush case.",
    );
    (spike, conv)
}

#[test]
fn moved_worktree_keeps_historical_evidence_and_reconnects_only_with_identity_evidence() {
    let fx = ledger();
    let (spike, _) = with_spike(&fx);
    let env = Env::with_home(&fx.home);
    drop(scan_once(&env));
    let api = handle_of(&db::open(&env.db_path()).unwrap(), &fx.api);
    let before = db::open(&env.db_path()).unwrap();
    let old_ref = view::expand(&v(&before), ":conversation:Batch charges spike", &scoped(&api)).unwrap();
    let old_file_ref = printed_refs(conv_section(&old_ref, "w — written (1)")[0])[0].clone();
    let moved = fx.home.join("archive/ledger-api-batching");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&spike, &moved).unwrap();
    let (c, st) = scan_once(&env);
    assert_eq!((st.checkouts_missing, st.checkouts_pruned), (0, 0), "{:?}", st.warnings);
    let old_co = crate::util::stable_id("checkout", &spike.to_string_lossy());
    let s = sid("claude", "ffff6666-spike");
    let co: Option<String> = c.query_row("SELECT checkout_id FROM nodes WHERE id=?1", [&s], |r| r.get(0)).unwrap();
    assert_eq!(co.as_deref(), Some(old_co.as_str()), "the conversation keeps its historical checkout");
    let (path, meta): (String, String) = c.query_row("SELECT path,meta FROM nodes WHERE id=?1", [&old_co], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(meta.get("missing"), None);
    assert!(meta["moved_paths"].as_array().is_some_and(|a| a.iter().any(|x| x == &json!(spike.to_string_lossy().to_string()))), "{meta}");
    assert_eq!(path, moved.to_string_lossy());
    assert!(count(&c, &format!("SELECT count(*) FROM nodes WHERE path='{}'", moved.join("ledger/batch.py").display())) > 0);
    let moved_id = crate::util::stable_id("checkout", &moved.to_string_lossy());
    assert!(overview::get_node(&c, &moved_id).unwrap().is_none(), "move must reuse prior checkout identity without a duplicate");

    // A repeated full scan must retain historical identity after path has been
    // updated to the moved location; otherwise it would create a second node.
    let (c, st) = scan_once(&env);
    assert_eq!((st.checkouts_missing, st.checkouts_pruned), (0, 0), "{:?}", st.warnings);
    assert!(overview::get_node(&c, &moved_id).unwrap().is_none(), "second full scan must not create a duplicate checkout");
    let (path, meta): (String, String) = c.query_row("SELECT path,meta FROM nodes WHERE id=?1", [&old_co], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(path, moved.to_string_lossy());
    assert!(serde_json::from_str::<serde_json::Value>(&meta).unwrap()["moved_paths"].as_array().is_some());

    let api = handle_of(&c, &fx.api);
    let cv = view::expand(&v(&c), ":conversation:Batch charges spike", &scoped(&api)).unwrap();
    assert!(!cv.contains("@?"), "{cv}");
    // Session keeps its original cwd as historical evidence, while its checkout
    // ref resolves to the moved live worktree.
    assert!(cv.contains(&spike.display().to_string()), "{cv}");
    assert!(cv.contains("try batching charges per merchant"), "recorded text excerpt: {cv}");
    let w = conv_section(&cv, "w — written (1)");
    assert!(w.len() == 1 && w[0].contains("ledger/batch.py"), "{cv}");
    assert_groups_listed(&cv);
    assert_refs_roundtrip(&c, &api, &cv);
    // The file ref printed there opens the historical file with the same note.
    let fref = printed_refs(w[0])[0].clone();
    let fv = expand_ref(&c, &api, &fref).unwrap();
    assert!(fv.contains("Batch charges spike") && !fv.contains("NOT ON DISK"), "{fv}");
    // A ref printed before the move still resolves, and subsequent refs use the
    // same checkout identity now attached to the moved path.
    let old_fv = expand_ref(&c, &api, &old_file_ref).unwrap();
    assert!(old_fv.contains("Batch charges spike"), "{old_fv}");
    let live_fv = view::expand(&v(&c), &format!("{api}@spike/batching:ledger/batch.py"), &scoped(&api)).unwrap();
    assert!(live_fv.contains("Batch charges spike") && !live_fv.contains("NOT ON DISK"), "{live_fv}");
    // Every ref in the view remains scoped to its live branch after the move.
    for r in printed_refs(&fv) {
        let mut req = scoped(&api);
        req.scope_worktree = Some("spike/batching".into());
        let w = shell_split(&r);
        let got = view::expand(&v(&c), &w[0], &req).unwrap_or_else(|e| panic!("{r}: {e}"));
        if w[0].starts_with(':') {
            assert!(got.starts_with(&format!("# {r} ")), "{r} -> {}", got.lines().next().unwrap_or(""));
        }
    }
    // The moved checkout is a live worktree of its own, with its branch read from git.
    let map = view::context(&v(&c), "", &scoped(&api)).unwrap();
    assert!(!map.contains("MISSING since") && map.contains(&format!("@ {}", moved.display())), "{map}");
    // A partial scan elsewhere keeps the reconnection.
    let mut cp = db::open(&env.db_path()).unwrap();
    scan::run(&env, &mut cp, &scan::ScanOpts { include_home: false, roots: vec![fx.sdk.clone()], ..Default::default() }).unwrap();
    let path: String = cp.query_row("SELECT path FROM nodes WHERE id=?1", [&old_co], |r| r.get(0)).unwrap();
    assert_eq!(path, moved.to_string_lossy());
    // Moving it back makes it live again.
    std::fs::rename(&moved, &spike).unwrap();
    let (c, st) = scan_once(&env);
    assert_eq!(st.checkouts_missing, 0);
    let path: String = c.query_row("SELECT path FROM nodes WHERE id=?1", [&old_co], |r| r.get(0)).unwrap();
    assert_eq!(path, spike.to_string_lossy());
}

#[test]
fn deleted_worktree_stays_expandable_and_ambiguous_successors_are_refused() {
    let fx = ledger();
    let (spike, conv) = with_spike(&fx);
    let env = Env::with_home(&fx.home);
    drop(scan_once(&env));
    let head = String::from_utf8(std::process::Command::new("git").arg("-C").arg(&spike).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap().trim().to_string();
    std::fs::remove_dir_all(&spike).unwrap();
    git(&fx.api, &["worktree", "prune"]);
    // Two unrelated clones that happen to sit on the same branch and commit: not proof of identity.
    for d in ["clones/one", "clones/two"] {
        let dst = fx.home.join(d);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        git(&fx.home, &["clone", "-q", fx.api.to_str().unwrap(), dst.to_str().unwrap()]);
        git(&dst, &["checkout", "-q", "-b", "spike/batching", &head]);
    }
    let (c, st) = scan_once(&env);
    assert_eq!(st.checkouts_missing, 1, "{:?}", st.warnings);
    let api = handle_of(&c, &fx.api);
    let cv = view::expand(&v(&c), ":conversation:Batch charges spike", &scoped(&api)).unwrap();
    assert!(cv.contains(&format!("recorded at {} on branch spike/batching", spike.display())), "{cv}");
    assert!(cv.contains("NOT reconnected, 2 possible new locations (ambiguous)") && cv.contains("same HEAD") && !cv.contains("; now at "), "{cv}");
    assert!(cv.contains("clones/one") && cv.contains("clones/two"), "{cv}");
    assert!(cv.contains("try batching charges per merchant") && conv_section(&cv, "w — written (1)").len() == 1, "{cv}");
    assert_groups_listed(&cv);
    assert_refs_roundtrip(&c, &api, &cv);
    let s = sid("claude", "ffff6666-spike");
    let h = |_: &str, _: &[&str]| Some(String::new());
    let e = ask::plan(&c, &s, "q", &h).unwrap_err();
    assert!(e.contains("no longer exists") && e.contains("checkout missing since") && e.contains("git worktree add"), "{e}");
    // Still searchable, and a re-parse (e.g. parser upgrade) keeps the historical association.
    assert!(search(&c, "gloamrush").contains("Batch charges spike"));
    let mut c2 = db::open(&env.db_path()).unwrap();
    c2.execute("UPDATE source_files SET mtime=-1 WHERE path=?1", [conv.to_string_lossy()]).unwrap();
    scan::ingest(&env, &mut c2, &scan::IngestOpts { transcripts: vec![conv.clone()], checkouts: vec![] }).unwrap();
    let w = edges_from(&c2, &s);
    assert!(w.iter().any(|e| e.0 == "write" && e.1 == "observed" && e.2 == spike.join("ledger/batch.py").to_string_lossy()), "{w:?}");
    let co: Option<String> = c2.query_row("SELECT checkout_id FROM nodes WHERE id=?1", [&s], |r| r.get(0)).unwrap();
    assert_eq!(co, Some(crate::util::stable_id("checkout", &spike.to_string_lossy())));
}

// ------------------------------------------------------------------ item 10: Codex parsed_cmd

#[test]
fn codex_parsed_shell_reads_are_their_own_evidence_class() {
    let fx = ledger();
    write(&fx.api.join("ledger/__init__.py"), "");
    commit_all(&fx.api, "Package marker");
    let m = fx.api.display().to_string();
    let uri = format!("file://{m}");
    let item = |id: &str, cmd: &str, parsed: serde_json::Value, status: &str, exit: serde_json::Value| {
        json!({"timestamp":"2026-09-12T08:00:02Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":id,
            "command":["/bin/bash","-lc",cmd],"cwd":uri,"parsed_cmd":parsed,"status":status,"exit_code":exit,"aggregated_output":"..."}}})
    };
    write(
        &fx.home.join(".codex/sessions/2026/09/12/rollout-2026-09-12T08-00-00-abcd.jsonl"),
        &jsonl(&[
            json!({"timestamp":"2026-09-12T08:00:00Z","type":"session_meta","payload":{"id":"abcd-codex","cwd":m,"git":{"branch":"main"}}}),
            json!({"timestamp":"2026-09-12T08:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"why do charges double post"}}),
            // Function call and its parsed result for the same command: one read, no `sh` duplicate.
            json!({"timestamp":"2026-09-12T08:00:02Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_9","arguments":json!({"cmd":"sed -n 1,5p ledger/charges.py"}).to_string()}}),
            item("exec-1", "sed -n 1,5p ledger/charges.py", json!([{"type":"read","cmd":"sed -n 1,5p ledger/charges.py","name":"charges.py","path":"ledger/charges.py"}]), "completed", json!(0)),
            json!({"timestamp":"2026-09-12T08:00:03Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_9","command":["/bin/bash","-lc","sed -n 1,5p ledger/charges.py"],"cwd":m,
                "parsed_cmd":[{"type":"read","cmd":"sed -n 1,5p ledger/charges.py","name":"charges.py","path":"ledger/charges.py"}],"exit_code":0}}),
            item("exec-2", "cat README.md", json!([{"type":"read","cmd":"cat README.md","name":"README.md","path":format!("{m}/README.md")}]), "failed", json!(1)),
            json!({"timestamp":"2026-09-12T08:00:04Z","type":"event_msg","payload":{"type":"item_started","item":{"type":"CommandExecution","id":"exec-3","command":["/bin/bash","-lc","cat docs/adr/0003-charge-dedup.md"],"cwd":uri,
                "parsed_cmd":[{"type":"read","cmd":"cat docs/adr/0003-charge-dedup.md","name":"0003-charge-dedup.md","path":"docs/adr/0003-charge-dedup.md"}],"status":"inProgress"}}}),
            item("exec-4", "rg token ledger/webhooks.py", json!([{"type":"search","cmd":"rg token ledger/webhooks.py","query":"token","path":"ledger/webhooks.py"}]), "completed", json!(0)),
            item("exec-5", "ls ledger", json!([{"type":"list_files","cmd":"ls ledger","path":"ledger"}]), "completed", json!(0)),
            // Older shape without parsed_cmd: still only a shell mention.
            json!({"timestamp":"2026-09-12T08:00:06Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_10","arguments":json!({"cmd":"wc -l ledger/__init__.py"}).to_string()}}),
        ]),
    );
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let s = sid("codex", "abcd-codex");
    let e = edges_from(&c, &s);
    let at = |rel: &str| fx.api.join(rel);
    assert!(has(&e, "read", "parsed", &at("ledger/charges.py")), "{e:?}");
    assert_eq!(e.iter().filter(|x| x.2.ends_with("/ledger/charges.py")).count(), 1, "same command from function_call + item_completed + exec_command_end counts once: {e:?}");
    assert!(has(&e, "read", "failed", &at("README.md")), "{e:?}");
    assert!(has(&e, "read", "requested", &at("docs/adr/0003-charge-dedup.md")), "{e:?}");
    assert!(has(&e, "searched", "parsed", &at("ledger/webhooks.py")) && has(&e, "mentioned", "mentioned", &at("ledger/__init__.py")), "{e:?}");
    assert!(!e.iter().any(|x| x.0 == "listed"), "a listed directory is not a file: {e:?}");
    assert!(!e.iter().any(|x| x.0 == "read" && x.1 == "observed"), "never a confirmed read from parsed intent alone: {e:?}");

    let api = handle_of(&c, &fx.api);
    let cv = view::expand(&v(&c), ":session:abcd-codex", &scoped(&api)).unwrap();
    assert!(cv.contains("access: cr1 req1 fail1 srch1 sh1"), "{cv}");
    let cr = conv_section(&cv, "cr — read via shell command (parsed by Codex, best effort; exit 0, not proof of a full read) (1)");
    assert!(cr.len() == 1 && cr[0].contains(":ledger/charges.py") && cr[0].contains("shell:read"), "{cv}");
    assert!(cv.contains("## r-fail — read failed (1)") && cv.contains("## r-req — read requested, no result (1)") && cv.contains("## srch — searched via shell"), "{cv}");
    assert!(cv.contains("Codes: r/w = read/written") && cv.contains("cr = read via shell command (parsed by Codex, best effort"), "{cv}");
    assert_groups_listed(&cv);
    assert_refs_roundtrip(&c, &api, &cv);
    let fv = view::expand(&v(&c), ":ledger/charges.py", &scoped(&api)).unwrap();
    assert!(fv.lines().any(|l| l.contains("cr shell:read") && l.contains("codex")), "{fv}");
    assert_refs_roundtrip(&c, &api, &fv);
    // Ranking: a confirmed provider Read (conv_cross) outranks a parsed shell read.
    let t = view::task(&v(&c), &api, &view::Req::default(), &view::more::TaskReq { task: None, files: &["ledger/charges.py".to_string()], budget: 6000 }).unwrap();
    let pos = |needle: &str| t.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle} missing:\n{t}"));
    assert!(pos("[90]") < pos("[70]"), "{t}");
    assert!(t.lines().any(|l| l.contains("[70]") && l.contains("via shell command (parsed by Codex, best effort)")), "{t}");
    let graph = view::task(&v(&c), &api, &view::Req::default(), &view::more::TaskReq { task: Some("why do charges"), files: &[], budget: 6000 }).unwrap();
    assert!(graph.contains("[hop1] read via shell command (parsed by Codex, best effort) by"), "parsed read must keep its best-effort meaning in task graph hops:\n{graph}");
    assert!(!graph.lines().any(|l| l.contains(":ledger/charges.py") && l.contains("[hop1] failed access by")), "{graph}");
}
