//! Regression tests for search, relations, remote discovery, trust lines and
//! the task-context budget. Everything runs on synthetic repos and transcripts
//! in a temp dir; the gh/glab CLIs are fake shell scripts.

use super::*;
use crate::{relations, remote, search, trust, view::more};

fn scan_with(env: &Env, opts: scan::ScanOpts) -> (Connection, scan::Stats) {
    let mut c = db::open(&env.db_path()).unwrap();
    let st = scan::run(env, &mut c, &opts).unwrap();
    (c, st)
}

fn q(text: &str) -> search::Query {
    search::Query { text: text.into(), limit: 20, ..Default::default() }
}

fn global(c: &Connection, text: &str) -> String {
    more::search_view(&v(c), &view::Req::default(), &more::SearchReq { query: q(text), this_worktree: false }).unwrap()
}

/// A Claude transcript whose interesting text sits in the middle, next to
/// tool payloads that must never become searchable.
fn long_claude(fx: &Fixture) -> PathBuf {
    let m = fx.main.display().to_string();
    let cdir = fx.home.join(".claude/projects").join(crate::util::claude_project_dir_name(&m));
    let f = cdir.join("33333333-mid.jsonl");
    let mut lines = vec![json!({"type":"summary","summary":"Long design chat"})];
    for i in 0..6 {
        lines.push(json!({"type":"user","cwd":m,"sessionId":"33333333-mid","timestamp":format!("2026-03-0{}T10:00:00Z", i + 1),"message":{"content":format!("question number {i}")}}));
        let said = if i == 3 { "The flaky zanzibar handshake comes from the retry loop.".to_string() } else { format!("answer number {i}") };
        lines.push(json!({"type":"assistant","cwd":m,"sessionId":"33333333-mid","timestamp":format!("2026-03-0{}T10:00:01Z", i + 1),"message":{"content":[
            {"type":"thinking","thinking":"hiddenthoughtword"},
            {"type":"text","text":said},
            {"type":"tool_use","id":format!("u{i}"),"name":"Bash","input":{"command":"echo toolpayloadword"}}]}}));
        lines.push(json!({"type":"user","cwd":m,"sessionId":"33333333-mid","timestamp":format!("2026-03-0{}T10:00:02Z", i + 1),"toolUseResult":{},"message":{"content":[{"type":"tool_result","tool_use_id":format!("u{i}"),"content":"resultpayloadword"}]}}));
    }
    write(&f, &jl(&lines));
    f
}

#[test]
fn middle_transcript_messages_are_searchable_but_tool_payloads_are_not() {
    let fx = fixture();
    let f = long_claude(&fx);
    // Codex: middle agent message in the newer item_completed form, plus an event_msg duplicate.
    let m = fx.main.display().to_string();
    write(
        &fx.home.join(".codex/sessions/2026/03/04/rollout-2026-03-04T00-00-00-dddd.jsonl"),
        &jl(&[
            json!({"timestamp":"2026-03-04T00:00:00Z","type":"session_meta","payload":{"id":"dddd-uuid","cwd":m}}),
            json!({"timestamp":"2026-03-04T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>injectedcontextword</environment_context>"}]}}),
            json!({"timestamp":"2026-03-04T00:00:02Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"u1","content":[{"type":"text","text":"please check the marmalade cache"}]}}}),
            json!({"timestamp":"2026-03-04T00:00:03Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a1","content":[{"type":"text","text":"The marmalade cache is stale after deploys."}]}}}),
            json!({"timestamp":"2026-03-04T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"The marmalade cache is stale after deploys."}}),
            json!({"timestamp":"2026-03-04T00:00:04Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"a2","content":[{"type":"text","text":"done"}]}}}),
        ]),
    );
    let env = Env::with_home(&fx.home);
    let (c, st) = scan_once(&env);
    assert!(st.messages >= 15, "{st:?}");
    let out = global(&c, "zanzibar handshake");
    assert!(out.contains("1 shown of 1 matching passage"), "{out}");
    assert!(out.contains("proj:message:33333333:l12"), "assistant reply of round 3 is on transcript line 12:\n{out}");
    assert!(out.contains("claude assistant message") && out.contains("[zanzibar]"), "{out}");
    for hidden in ["toolpayloadword", "resultpayloadword", "hiddenthoughtword", "injectedcontextword"] {
        assert!(global(&c, hidden).contains("no matches"), "{hidden} must not be indexed");
    }
    // Codex duplicate (event + item) collapses to one message.
    let (hits, total) = search::run(&c, &q("marmalade stale"), None, None).unwrap();
    assert_eq!((hits.len(), total), (1, 1), "{hits:?}");
    // Speaker filter.
    let user_only = search::Query { speaker: Some("user".into()), ..q("marmalade") };
    assert_eq!(search::run(&c, &user_only, None, None).unwrap().0.len(), 1);
    // The printed ref expands to the full message with neighbours.
    let msg = view::expand(&v(&c), "proj:message:33333333:l12", &view::Req::default()).unwrap();
    assert!(msg.starts_with("# :message:33333333:l12 — assistant message in claude conversation"), "{msg}");
    assert!(
        msg.contains("The flaky zanzibar handshake comes from the retry loop.")
            && msg.contains("Previous: :message:33333333:l11 user: \"question number 3\"")
            && msg.contains("Next: :message:33333333:l14"),
        "{msg}"
    );
    assert!(msg.contains("not a decision record"));
    drop(c);
    // Rewriting the transcript without that message removes it from the index.
    let body = std::fs::read_to_string(&f).unwrap().replace("zanzibar", "plain");
    std::fs::write(&f, body + "\n").unwrap();
    let (c, _) = scan_once(&env);
    assert!(global(&c, "zanzibar").contains("no matches"), "stale message text deleted on re-parse");
    assert!(global(&c, "plain handshake").contains("1 shown"));
}

#[test]
fn scoped_global_kind_filters_and_stale_doc_text_deletion() {
    let fx = fixture();
    write(&fx.main.join("docs/ops.md"), "# Ops\n\n## Quokka rotation\nRotate the quokka keys weekly.\n");
    git(&fx.main, &["add", "-A"]);
    git(&fx.main, &["commit", "-qm", "ops notes about quokka"]);
    let other = fx.home.join("code/second");
    write(&other.join("README.md"), "# Second\n\nA quokka lives here too.\n");
    git(&other, &["init", "-q"]);
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "second start"]);
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let g = global(&c, "quokka");
    assert!(g.contains("proj:docs/ops.md#quokka-rotation") && g.contains("second:README.md") && g.contains("proj:commit:"), "{g}");
    let s = more::search_view(&v(&c), &scoped("proj"), &more::SearchReq { query: q("quokka"), this_worktree: false }).unwrap();
    assert!(s.contains(":docs/ops.md#quokka-rotation") && !s.contains("second:") && s.contains("Scope: export WORKMAP_PROJECT=proj"), "{s}");
    let docs_only = more::search_view(&v(&c), &scoped("proj"), &more::SearchReq { query: search::Query { kinds: vec!["doc".into()], ..q("quokka") }, this_worktree: false }).unwrap();
    assert!(!docs_only.contains(":commit:"), "{docs_only}");
    // Every ref printed by a scoped search expands.
    let r = printed_refs(&s).into_iter().find(|r| r.starts_with(":docs/ops.md")).unwrap();
    assert!(expand_ref(&c, "proj", &r).unwrap().contains("Rotate the quokka keys weekly."));
    drop(c);
    // Edit the doc: old section text leaves the index on the next scan.
    let ops = fx.main.join("docs/ops.md");
    std::fs::write(&ops, "# Ops\n\n## Rotation\nRotate keys weekly.\n").unwrap();
    std::fs::File::options().write(true).open(&ops).unwrap().set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
    let (c, _) = scan_once(&env);
    let (hits, _) = search::run(&c, &search::Query { kinds: vec!["doc".into()], ..q("quokka") }, None, None).unwrap();
    assert_eq!(hits.len(), 1, "only the second project's README still mentions it: {hits:?}");
    let orphans: i64 = c.query_row("SELECT count(*) FROM passages WHERE node_id NOT IN (SELECT id FROM nodes)", [], |r| r.get(0)).unwrap();
    assert_eq!(orphans, 0);
    let fts: i64 = c.query_row("SELECT count(*) FROM passages_fts WHERE passages_fts MATCH 'quokka'", [], |r| r.get(0)).unwrap();
    let rows: i64 = c.query_row("SELECT count(*) FROM passages WHERE text LIKE '%quokka%' OR title LIKE '%quokka%'", [], |r| r.get(0)).unwrap();
    assert_eq!(fts, rows, "FTS index agrees with the passage table");
}

#[test]
fn parser_version_migration_and_missing_sources_keep_search_consistent() {
    let fx = fixture();
    let f = long_claude(&fx);
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let before: i64 = c.query_row("SELECT count(*) FROM passages WHERE kind='message'", [], |r| r.get(0)).unwrap();
    // Simulate an index written by the previous parser version without message passages.
    c.execute("DELETE FROM passages WHERE kind IN ('message','doc','commit')", []).unwrap();
    c.execute_batch("PRAGMA user_version=2;").unwrap();
    drop(c);
    let (c, st) = scan_once(&env);
    assert!(st.sessions_parsed >= 4, "version bump forces re-parse: {st:?}");
    let after: i64 = c.query_row("SELECT count(*) FROM passages WHERE kind='message'", [], |r| r.get(0)).unwrap();
    assert_eq!(after, before);
    assert!(c.query_row("SELECT count(*) FROM passages WHERE kind='doc'", [], |r| r.get::<_, i64>(0)).unwrap() > 0, "checkout docs re-indexed too");
    drop(c);
    // Deleted transcript: evidence and text kept, flagged as missing everywhere.
    std::fs::remove_file(&f).unwrap();
    let (c, st) = scan_once(&env);
    assert_eq!(st.sources_missing, 1);
    let out = global(&c, "zanzibar");
    assert!(out.contains("source transcript missing"), "{out}");
    let cov = trust::coverage(&c, None);
    assert_eq!(cov.missing, 1);
    assert!(trust::coverage_line(&cov).contains("1 source transcript missing"));
}

// ------------------------------------------------------------------ relations

struct Rels {
    _t: TempDir,
    home: PathBuf,
    app: PathBuf,
    lib: PathBuf,
}

fn init(dir: &Path, origin: Option<&str>) {
    git(dir, &["init", "-q"]);
    if let Some(o) = origin {
        git(dir, &["remote", "add", "origin", o]);
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "start"]);
}

fn relations_fixture() -> Rels {
    let t = TempDir::new("rels");
    let home = t.path().join("home");
    let code = home.join("code");
    let shared = format!("// shared helper\n{}", "fn helper() -> u32 { 42 }\n".repeat(60));
    // app: Cargo path dep on lib, registry dep named like web's package, doc link to lib's URL.
    let app = code.join("app");
    write(&app.join("Cargo.toml"), "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\" }\nwidget = \"1\"\nlibgit = { git = \"https://github.com/acme/lib\", package = \"lib\" }\ntool = { git = \"ssh://git@github.com/acme/secret-tool.git\" }\n");
    write(&app.join("README.md"), "# App\n\nUses https://github.com/acme/lib/tree/main/src for parsing.\n");
    write(&app.join("src/main.rs"), "fn main() {}\n");
    write(&app.join("src/shared.rs"), &shared);
    write(&app.join("LICENSE"), &"MIT licence text\n".repeat(100));
    init(&app, Some("https://github.com/acme/app.git"));
    // lib: links back to app by path (a cycle), has a linked worktree (same repo).
    let lib = code.join("lib");
    write(&lib.join("Cargo.toml"), "[package]\nname = \"lib\"\n");
    write(&lib.join("README.md"), "# Lib\n\nConsumers: ../app/README.md\n");
    write(&lib.join("src/lib.rs"), "pub fn parse() {}\n");
    init(&lib, Some("git@github.com:acme/lib.git"));
    git(&lib, &["worktree", "add", "-q", "-b", "wip", code.join("lib-wip").to_str().unwrap()]);
    // web: symlink into lib, same shared file as app, package named "widget".
    let web = code.join("web");
    write(&web.join("pyproject.toml"), "[project]\nname = \"widget\"\n");
    write(&web.join("vendored.rs"), &shared);
    write(&web.join("LICENSE"), &"MIT licence text\n".repeat(100));
    std::fs::create_dir_all(&web).unwrap();
    std::os::unix::fs::symlink("../lib/src", web.join("lib-src")).unwrap();
    init(&web, None);
    // A second clone of lib: same upstream, different repository on disk.
    let clone = code.join("lib-clone");
    let out = std::process::Command::new("git").args(["clone", "-q"]).arg(&lib).arg(&clone).output().unwrap();
    assert!(out.status.success());
    git(&clone, &["remote", "set-url", "origin", "https://github.com/acme/lib"]);
    // A conversation in app that wrote a lib file.
    let a = app.display().to_string();
    let cdir = home.join(".claude/projects").join(crate::util::claude_project_dir_name(&a));
    write(
        &cdir.join("44444444-x.jsonl"),
        &jl(&[
            json!({"type":"user","cwd":a,"sessionId":"44444444-x","timestamp":"2026-04-01T10:00:00Z","message":{"content":"patch the parser in lib"}}),
            json!({"type":"assistant","cwd":a,"sessionId":"44444444-x","timestamp":"2026-04-01T10:00:01Z","message":{"content":[{"type":"tool_use","id":"w","name":"Edit","input":{"file_path": lib.join("src/lib.rs").to_string_lossy()}}]}}),
            json!({"type":"user","cwd":a,"sessionId":"44444444-x","timestamp":"2026-04-01T10:00:02Z","toolUseResult":{},"message":{"content":[{"type":"tool_result","tool_use_id":"w","content":"ok"}]}}),
        ]),
    );
    Rels { _t: t, home, app, lib }
}

fn rel_kinds(c: &Connection, a: &str, b: &str) -> Vec<(String, String)> {
    let ra = view::resolve_project(c, a).unwrap().repo.id;
    let rb = view::resolve_project(c, b).unwrap().repo.id;
    let mut v: Vec<(String, String)> = relations::between(c, &ra, &rb).unwrap().into_iter().map(|r| (r.kind, r.class)).collect();
    v.sort();
    v.dedup();
    v
}

#[test]
fn relations_keep_their_kind_and_evidence() {
    let fx = relations_fixture();
    let env = Env::with_home(&fx.home);
    let (c, st) = scan_once(&env);
    assert!(st.relations > 0);
    let lib_path = fx.lib.to_string_lossy().into_owned();
    let k = rel_kinds(&c, "app", &lib_path);
    for want in [("cargo_path", "declared"), ("cargo_git", "declared"), ("doc_link", "documented"), ("conversation_access", "observed")] {
        assert!(k.contains(&(want.0.into(), want.1.into())), "{want:?} missing: {k:?}");
    }
    let kw = rel_kinds(&c, "app", "web");
    assert!(kw.contains(&("identical_files".into(), "hash".into())) && kw.contains(&("name_match".into(), "candidate".into())), "{kw:?}");
    assert_eq!(kw.iter().filter(|x| x.0 == "identical_files").count(), 1);
    assert!(rel_kinds(&c, "web", &lib_path).contains(&("symlink".into(), "structural".into())));
    // The linked worktree of lib is the same repository, not a relation; the clone is a same-upstream relation.
    let lib = view::resolve_project(&c, &fx.lib.to_string_lossy()).unwrap();
    assert_eq!(lib.checkouts.len(), 2);
    let names: Vec<String> = c.prepare("SELECT label FROM nodes WHERE kind='repo' ORDER BY label").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(names, vec!["app", "lib", "lib", "web"], "two repos labelled lib: the original and its clone");
    let same: i64 = c.query_row("SELECT count(*) FROM edges WHERE kind='rel:same_upstream'", [], |r| r.get(0)).unwrap();
    assert_eq!(same, 1);
    // Licence boilerplate never counts as shared content.
    let ex: String = c.query_row("SELECT source_path || note FROM edges WHERE kind='rel:identical_files'", [], |r| r.get(0)).unwrap();
    assert!(ex.contains("shared.rs") && ex.contains("1 identical") && !ex.contains("LICENSE"), "{ex}");
    // Views: exact source line for declared evidence, candidates separate.
    let app = view::resolve_project(&c, "app").unwrap();
    let rv = more::related_view(&v(&c), &app, 2, 20).unwrap();
    let lib = view::resolve_project(&c, &fx.lib.to_string_lossy()).unwrap();
    let lib_handle = lib.handle.clone();
    assert!(lib_handle.starts_with("lib~"), "{lib_handle}");
    assert!(rv.contains("Cargo path dependency [declared] at") && rv.contains("Cargo.toml:5: \"lib = { path = \"../lib\" }\""), "{rv}");
    assert!(rv.contains("## Uncertain candidates (1; name matches only, not evidence)"), "{rv}");
    assert!(rv.contains(&*lib_handle), "lib is ambiguous by name, so it is shown by its compact handle:\n{rv}");
    let first = rv.lines().find(|l| l.starts_with("- ")).unwrap();
    assert!(first.contains("Cargo git dependency") || first.contains("Cargo path dependency"), "declared relations sort first:\n{rv}");
    let ev = view::expand(&v(&c), ":related:web", &scoped("app")).unwrap();
    assert!(ev.starts_with("# :related:web — evidence between app and web") && ev.contains("## Uncertain candidates (not evidence of a relation)"), "{ev}");
    let map = view::context(&v(&c), "", &scoped("app")).unwrap();
    assert!(map.contains("## Related projects"), "{map}");
    assert_refs_roundtrip(&c, "app", &map);
}

#[test]
fn why_chains_are_bounded_and_survive_cycles() {
    let fx = relations_fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let app = view::resolve_project(&c, "app").unwrap();
    // app → lib → app is a cycle; BFS still terminates and visits each project once.
    let reach = relations::bfs(&c, &app.repo.id, 6, 100).unwrap();
    let mut ids: Vec<&String> = reach.iter().map(|x| &x.0).collect();
    ids.sort();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "no node visited twice");
    assert!(!ids.contains(&&app.repo.id), "start node never re-entered");
    let w = more::why_view(&v(&c), &app, "web", 4).unwrap();
    assert!(w.contains("1. app → web") && w.contains("identical file content"), "{w}");
    assert!(w.contains("Uncertain candidates between them (not used in the chain)"), "{w}");
    let lib_clone = c.query_row("SELECT path FROM nodes WHERE kind='checkout' AND path LIKE '%lib-clone'", [], |r| r.get::<_, String>(0)).unwrap();
    // app's README names the lib URL, which both clones carry: a direct documented link.
    let w1 = more::why_view(&v(&c), &app, &lib_clone, 4).unwrap();
    assert!(w1.contains("doc names its URL or path [documented]") && !w1.contains("2. "), "{w1}");
    let web = view::resolve_project(&c, "web").unwrap();
    let w2 = more::why_view(&v(&c), &web, &lib_clone, 4).unwrap();
    assert!(w2.contains("1. web →") && w2.contains("2. ") && !w2.contains("3. "), "two steps:\n{w2}");
    let short = more::why_view(&v(&c), &web, &lib_clone, 1).unwrap();
    assert!(short.contains("no evidence chain within 1 steps"), "{short}");
    let _ = &fx.app;
}

// ------------------------------------------------------------------ remote discovery

fn fake_cli(dir: &Path, name: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    write(&p, script);
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn remote_env(fx: &Rels, gh_ok: bool) -> Env {
    let bin = fx.home.join("bin");
    let log = fx.home.join("gh.log");
    let page1 = r#"[{"full_name":"acme/lib","html_url":"https://github.com/acme/lib","private":false,"archived":false,"default_branch":"main","updated_at":"2026-05-01T00:00:00Z","description":"the parser library"}]"#;
    let page2 = r#"[{"full_name":"acme/secret-tool","html_url":"https://github.com/acme/secret-tool","visibility":"private","archived":true,"default_branch":"dev","updated_at":"2026-05-02T00:00:00Z","description":"internal ops tool"}]"#;
    let api =
        if gh_ok { format!("printf '%s\\n' '{page1}'\nprintf '%s\\n' '{page2}'\nexit 0") } else { "echo 'HTTP 403: rate limit exceeded token=ghp_LEAKEDLEAKEDLEAKED1234' >&2\nexit 1".to_string() };
    let gh = fake_cli(&bin, "gh", &format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\necho \"GH_CONFIG_DIR=$GH_CONFIG_DIR prompt=$GH_PROMPT_DISABLED\" >> '{}'\ncase \"$1\" in\n  auth) echo 'Logged in with token ghp_LEAKEDLEAKEDLEAKED1234' >&2; exit 0;;\n  api) {api};;\nesac\nexit 2\n",
        log.display(), log.display()
    ));
    let glab = fake_cli(&bin, "glab", "#!/bin/sh\necho 'not logged in; token glpat-LEAKEDLEAKEDLEAKED' >&2\nexit 1\n");
    write(&fx.home.join(".config/gh/hosts.yml"), "github.com:\n    oauth_token: ghp_LEAKEDLEAKEDLEAKED1234\n    user: someone\n");
    write(&fx.home.join(".config/glab-cli/config.yml"), "hosts:\n    gitlab.example.org:\n        token: glpat-LEAKEDLEAKEDLEAKED\n");
    let mut env = Env::with_home(&fx.home);
    env.gh_bin = gh;
    env.glab_bin = glab;
    env
}

fn dump(c: &Connection) -> String {
    let mut all = String::new();
    for t in ["nodes", "edges", "passages", "remote_repos", "remote_hosts", "scans"] {
        let mut st = c.prepare(&format!("SELECT * FROM {t}")).unwrap();
        let n = st.column_count();
        let rows = st.query_map([], |r| Ok((0..n).map(|i| format!("{:?}", r.get_ref(i).unwrap())).collect::<Vec<_>>().join("|"))).unwrap();
        for r in rows {
            all.push_str(&r.unwrap());
            all.push('\n');
        }
    }
    all
}

#[test]
fn remote_catalog_dedups_clones_isolates_failures_and_never_logs_secrets() {
    let fx = relations_fixture();
    let env = remote_env(&fx, true);
    let (c, st) = scan_with(&env, scan::ScanOpts { remote: remote::Mode::Auto, ..Default::default() });
    assert!(st.remote.iter().any(|l| l == "gh github.com: 2 repositories"), "{:?}", st.remote);
    assert!(st.remote.iter().any(|l| l.starts_with("glab gitlab.example.org: skipped")), "{:?}", st.remote);
    let log = std::fs::read_to_string(fx.home.join("gh.log")).unwrap();
    assert!(
        log.contains("auth status --hostname github.com") && log.contains("api --hostname github.com --paginate user/repos?per_page=100&affiliation=owner,collaborator,organization_member"),
        "{log}"
    );
    assert!(log.contains(&format!("GH_CONFIG_DIR={}", fx.home.join(".config/gh").display())) && log.contains("prompt=1"), "CLI pinned to the fake home's config, prompts off:\n{log}");
    // acme/lib is cloned locally (two clones): it attaches to them, only secret-tool is remote-only.
    let only = remote::remote_only(&c, 10).unwrap();
    assert_eq!(only.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["github.com/acme/secret-tool"]);
    assert!(only[0].2 && only[0].1 == "private");
    let cat: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='repo' AND json_extract(meta,'$.catalog.identity')='github.com/acme/lib'", [], |r| r.get(0)).unwrap();
    assert_eq!(cat, 2);
    let ov = overview::overview(&c, 30, "workmap").unwrap();
    assert!(ov.contains("Remote-only, not cloned (1;") && ov.contains("remote:github.com/acme/secret-tool — private, archived — internal ops tool"), "{ov}");
    assert!(global(&c, "internal ops").contains("remote:github.com/acme/secret-tool"));
    let rv = view::expand(&v(&c), "remote:github.com/acme/secret-tool", &view::Req::default()).unwrap();
    assert!(rv.contains("not cloned locally") && rv.contains("default branch dev"), "{rv}");
    assert!(
        rv.contains("## Projects related to it (1)") && rv.contains("app ← Cargo git dependency [declared]") && rv.contains("secret-tool.git"),
        "git dependency on a non-cloned repo points at the remote entry:\n{rv}"
    );
    let map = view::context(&v(&c), "", &scoped("app")).unwrap();
    assert!(map.contains(":related:remote:github.com/acme/secret-tool — → Cargo git dependency [declared]"), "remote-only repo appears only as an evidenced relation:\n{map}");
    assert_refs_roundtrip(&c, "app", &map);
    assert!(!dump(&c).contains("LEAKED"), "no token text stored anywhere");
    drop(c);
    // Within the TTL nothing is fetched again.
    let calls = std::fs::read_to_string(fx.home.join("gh.log")).unwrap().lines().filter(|l| l.starts_with("api")).count();
    let (c, st) = scan_with(&env, scan::ScanOpts { remote: remote::Mode::Auto, ..Default::default() });
    assert!(st.remote.iter().any(|l| l.starts_with("gh github.com: cached")), "{:?}", st.remote);
    assert_eq!(std::fs::read_to_string(fx.home.join("gh.log")).unwrap().lines().filter(|l| l.starts_with("api")).count(), calls);
    drop(c);
    // Forced refresh that fails (rate limit): old rows kept, marked stale, no secret in messages.
    let env2 = remote_env(&fx, false);
    let (c, st) = scan_with(&env2, scan::ScanOpts { remote: remote::Mode::Force, ..Default::default() });
    assert!(st.remote.iter().any(|l| l.contains("refresh failed (gh api exit code 1)")), "{:?}", st.remote);
    let only = remote::remote_only(&c, 10).unwrap();
    assert!(only.len() == 1 && only[0].4, "kept and stale: {only:?}");
    let status = remote::status(&env2, &c).unwrap();
    assert!(status.contains("gh github.com: failed") && status.contains("(STALE)"), "{status}");
    assert!(!dump(&c).contains("LEAKED") && !status.contains("LEAKED") && !st.remote.join(" ").contains("LEAKED"));
    // Opting out deletes cached remote metadata.
    remote::clear(&c).unwrap();
    assert!(remote::remote_only(&c, 10).unwrap().is_empty());
    assert_eq!(c.query_row("SELECT count(*) FROM remote_repos", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
}

#[test]
fn remote_discovery_skips_cleanly_without_auth_cli_or_opt_in() {
    let fx = relations_fixture();
    // No config files, no CLI: nothing happens, no error.
    let env = Env::with_home(&fx.home);
    let (c, st) = scan_with(&env, scan::ScanOpts { remote: remote::Mode::Auto, ..Default::default() });
    assert!(st.remote.is_empty() && remote::remote_only(&c, 5).unwrap().is_empty());
    drop(c);
    // Configured host but the CLI is missing: reported as unavailable, scan succeeds.
    write(&fx.home.join(".config/gh/hosts.yml"), "github.com:\n    user: x\n");
    let (c, st) = scan_with(&env, scan::ScanOpts { remote: remote::Mode::Auto, ..Default::default() });
    assert!(st.remote.iter().any(|l| l.contains("skipped, gh auth status not installed")), "{:?}", st.remote);
    assert!(remote::status(&env, &c).unwrap().contains("gh github.com: unavailable"));
    drop(c);
    // Mode Off (scan --no-remote / opt-out) never runs the CLI.
    let env = remote_env(&fx, true);
    let _ = std::fs::remove_file(fx.home.join("gh.log"));
    let (_c, st) = scan_with(&env, scan::ScanOpts { remote: remote::Mode::Off, ..Default::default() });
    assert!(st.remote.is_empty() && !fx.home.join("gh.log").exists());
    std::fs::create_dir_all(&env.state_dir).unwrap();
    std::fs::write(env.state_dir.join("no-remote"), "").unwrap();
    assert_eq!(remote::mode(&env, false, true), remote::Mode::Off, "opt-out file beats --refresh-remote");
}

// ------------------------------------------------------------------ trust, budget, ask

#[test]
fn freshness_and_coverage_are_reported() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_with(&env, scan::ScanOpts { max_parts: 2, commits: 7, ..Default::default() });
    let line = trust::index_line(&c);
    assert!(line.starts_with("Index: last scan ") && line.contains("newest 7 commits per worktree") && !line.contains("WARNING"), "{line}");
    let cov = trust::coverage(&c, None);
    assert!(cov.truncated >= 1 && cov.with_warnings >= 1, "opencode capped at 2 parts, claude has junk lines: {cov:?}");
    let m = view::context(&v(&c), "", &scoped("proj")).unwrap();
    assert!(m.contains("Coverage: ") && m.contains("truncated at the size cap") && m.contains("Index: last scan"), "{m}");
    let conv = view::expand(&v(&c), ":conversation:Investigate backend", &scoped("proj")).unwrap();
    assert!(conv.contains("latest user input (as typed; not a decision record)") && conv.contains("coverage: ") && conv.contains("some transcript lines unparseable"), "{conv}");
    assert_refs_roundtrip(&c, "proj", &conv);
    c.execute("UPDATE scans SET finished=finished-10*86400", []).unwrap();
    let stale = trust::index_line(&c);
    assert!(stale.contains("WARNING: index is stale (10d ago > 48h)"), "{stale}");
    let s = global(&c, "backend");
    assert!(s.contains("WARNING: index is stale"), "{s}");
}

#[test]
fn task_context_ranks_with_reasons_and_respects_the_budget() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let files = vec!["main.py".to_string()];
    let t = more::TaskReq { task: Some("why does main print"), files: &files, budget: 6000 };
    let out = view::task(&v(&c), "proj", &view::Req::default(), &t).unwrap();
    assert_eq!(out, view::task(&v(&c), "proj", &view::Req::default(), &t).unwrap(), "deterministic");
    let ranked: Vec<&str> = out.split("## Ranked").nth(1).unwrap().split("\n## ").next().unwrap().lines().filter(|l| l.starts_with("- ")).collect();
    assert!(ranked[0].contains(":conversation:Investigate") && ranked[0].contains("read main.py (observed, Read) at transcript l.3"), "observed access ranks first:\n{out}");
    assert!(out.contains("names main.py at CLAUDE.md:4") && out.contains("changed main.py (A)") && out.contains("text matches"), "{out}");
    assert!(ranked.iter().all(|l| l.contains(" — ")), "every item says why");
    let mut scores: Vec<i64> = ranked.iter().map(|l| l.split('[').nth(1).unwrap().split(']').next().unwrap().parse().unwrap()).collect();
    let sorted = {
        let mut s = scores.clone();
        s.sort_by(|a, b| b.cmp(a));
        s
    };
    assert_eq!(scores, sorted);
    scores.clear();
    let small = view::task(&v(&c), "proj", &view::Req::default(), &more::TaskReq { budget: 1100, ..t }).unwrap();
    assert!(small.len() <= 1100 + 120, "budget bounds the output: {} bytes\n{small}", small.len());
    assert!(small.contains("… omitted to stay within --budget 1100: ") && small.contains("ranked item(s)"), "{small}");
    let missing = view::task(&v(&c), "proj", &view::Req::default(), &more::TaskReq { task: None, files: &["nope.rs".to_string()], budget: 3000 }).unwrap();
    assert!(missing.contains("note: nope.rs: no indexed evidence"), "{missing}");
}

#[test]
fn ask_note_separates_history_from_current_disk() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let n = more::ask_note(&c, &sid("claude", "11111111-aaaa"));
    assert!(n.starts_with("historical session: last activity 2026-02-02") && n.contains("indexed commit(s) on main") && n.contains("file(s) it wrote changed or vanished since"), "{n}");
    assert!(n.contains("not a decision record"));
}

#[test]
fn deleted_checkouts_are_pruned_but_unmounted_parents_are_not() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    drop(c);
    // `wt/` vanishes entirely (like an unmounted drive): its checkout is kept.
    let feature = std::fs::canonicalize(&fx.feature).unwrap();
    std::fs::remove_dir_all(fx.home.join("wt")).unwrap();
    // The outside worktree is deleted while its parent stays: pruned.
    let outside = std::fs::canonicalize(&fx.outside_wt).unwrap();
    std::fs::remove_dir_all(&outside).unwrap();
    let (c, st) = scan_once(&env);
    let has = |p: &Path| c.query_row("SELECT count(*) FROM nodes WHERE kind='checkout' AND path=?1", [p.to_string_lossy()], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(has(&feature), 1, "parent dir missing: keep");
    assert_eq!(has(&outside), 0, "deleted with parent present: pruned");
    assert_eq!(st.checkouts_pruned, 1);
    let orphans: i64 = c.query_row("SELECT count(*) FROM passages WHERE node_id NOT IN (SELECT id FROM nodes)", [], |r| r.get(0)).unwrap();
    assert_eq!(orphans, 0);
}

// ------------------------------------------------------------------ remote snapshots

fn git_out(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=T", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false"]).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A bare "GitHub" repository acme/secret-tool on branch dev, reachable through
/// `url.<file>.insteadOf=https://github.com/`, plus a work clone to push from.
fn snapshot_env(fx: &Rels) -> (Env, PathBuf, PathBuf) {
    // Outside the scanned home, so neither is indexed as a local project.
    let remotes = fx._t.path().join("remotes");
    let bare = remotes.join("acme/secret-tool.git");
    std::fs::create_dir_all(&bare).unwrap();
    git_out(&bare, &["init", "-q", "--bare", "-b", "dev"]);
    let work = fx._t.path().join("work-secret-tool");
    write(&work.join("README.md"), "# Secret tool\n\nInternal.\n\n## Usage\nThe pelican deploy runbook lives here.\nSee https://github.com/acme/app for the app.\n");
    write(&work.join("Cargo.toml"), "[package]\nname = \"secret-tool\"\n\n[dependencies]\napp = { git = \"https://github.com/acme/app\" }\nlocal = { path = \"../somewhere\" }\n");
    std::os::unix::fs::symlink("/etc/passwd", work.join("escape")).unwrap();
    git_out(&work, &["init", "-q", "-b", "dev"]);
    git_out(&work, &["add", "-A"]);
    git_out(&work, &["commit", "-qm", "first"]);
    git_out(&work, &["push", "-q", bare.to_str().unwrap(), "dev"]);
    let mut env = remote_env(fx, true);
    // The catalog's gh: same listing; the branches endpoint answers from a file when present.
    let log = fx.home.join("gh.log");
    let page1 = r#"[{"full_name":"acme/lib","html_url":"https://github.com/acme/lib","private":false,"archived":false,"default_branch":"main","updated_at":"2026-05-01T00:00:00Z","description":"the parser library"}]"#;
    let page2 = r#"[{"full_name":"acme/secret-tool","html_url":"https://github.com/acme/secret-tool","visibility":"private","archived":false,"default_branch":"dev","updated_at":"2026-05-02T00:00:00Z","description":"internal ops tool"}]"#;
    env.gh_bin = fake_cli(&fx.home.join("bin"), "gh2", &format!(
        "#!/bin/sh\necho \"$@\" >> '{log}'\ncase \"$1\" in\n  auth) exit 0;;\n  api) case \"$4\" in\n    */branches/*) [ -f '{sha}' ] || exit 1; printf '{{\"commit\":{{\"sha\":\"%s\"}}}}' \"$(cat '{sha}')\";;\n    *) printf '%s\\n' '{page1}'; printf '%s\\n' '{page2}';;\n  esac;;\nesac\n",
        log = log.display(), sha = fx.home.join("head-sha").display()
    ));
    env.snapshot_git_config = vec![(format!("url.file://{}/.insteadOf", remotes.display()), "https://github.com/".into())];
    (env, bare, work)
}

fn snap_scan(env: &Env) -> (Connection, scan::Stats) {
    scan_with(env, scan::ScanOpts { remote: remote::Mode::Force, snapshots: None, ..Default::default() })
}

fn tmp_entries(env: &Env) -> usize {
    std::fs::read_dir(crate::snapshot::tmp_root(env)).map(|r| r.count()).unwrap_or(0)
}

#[test]
fn remote_snapshots_are_opt_in_cached_per_commit_and_survive_failures() {
    use crate::snapshot;
    let fx = relations_fixture();
    let (env, bare, work) = snapshot_env(&fx);
    // Off by default: metadata only, no clone.
    let (c, st) = snap_scan(&env);
    assert!(!st.remote.iter().any(|l| l.starts_with("snapshot")), "{:?}", st.remote);
    assert!(global(&c, "pelican").contains("no matches"));
    drop(c);

    snapshot::set(&env, "snapshots", "on").unwrap();
    let (c, st) = snap_scan(&env);
    let sha1 = git_out(&bare, &["rev-parse", "dev"]);
    assert!(st.remote.iter().any(|l| l.starts_with(&format!("snapshot github.com/acme/secret-tool: indexed {}", &sha1[..12]))), "{:?}", st.remote);
    assert_eq!(tmp_entries(&env), 0, "temp clone removed after success");
    let out = global(&c, "pelican runbook");
    assert!(out.contains("remote:github.com/acme/secret-tool:README.md#usage") && out.contains("REMOTE SNAPSHOT at") && out.contains("not on local disk"), "{out}");
    // The ref resolves from the index alone (the clone is gone) to cached text and a browser link at the indexed commit.
    let doc = view::expand(&v(&c), "remote:github.com/acme/secret-tool:README.md#usage", &view::Req::default()).unwrap();
    assert!(doc.contains("REMOTE SNAPSHOT of github.com/acme/secret-tool at") && doc.contains("The pelican deploy runbook lives here."), "{doc}");
    assert!(doc.contains(&format!("https://github.com/acme/secret-tool/blob/{sha1}/README.md#L5")), "{doc}");
    assert!(!dump(&c).contains("snapshots-tmp"), "no temp paths stored");
    // Relations from the snapshot: git dependency and doc URL point at the local app; the local path dep and the symlink are ignored.
    let app = view::resolve_project(&c, "app").unwrap();
    let rid = crate::util::stable_id("remote", "github.com/acme/secret-tool");
    let ev = relations::between(&c, &rid, &app.repo.id).unwrap();
    assert!(ev.iter().any(|r| r.kind == "cargo_git" && r.source_path.starts_with("remote:github.com/acme/secret-tool@") && r.note.contains("app = { git")), "{ev:?}");
    assert!(ev.iter().any(|r| r.kind == "doc_link"));
    let raws: i64 = c.query_row("SELECT count(*) FROM raw_links WHERE origin='snap:github.com/acme/secret-tool' AND (kind LIKE '%_path' OR kind='symlink')", [], |r| r.get(0)).unwrap();
    assert_eq!(raws, 0);
    let rv = view::expand(&v(&c), "remote:github.com/acme/secret-tool", &view::Req::default()).unwrap();
    assert!(rv.contains("## Remote snapshot (opt-in") && rv.contains("Only dev is indexed; other branches are not."), "{rv}");
    let at: i64 = c.query_row("SELECT indexed_at FROM remote_snapshots", [], |r| r.get(0)).unwrap();
    drop(c);

    // Unchanged HEAD (resolved via ls-remote, then via the API): reused, no new clone.
    let (c, st) = snap_scan(&env);
    assert!(st.remote.iter().any(|l| l.contains("unchanged at")), "{:?}", st.remote);
    drop(c);
    std::fs::write(fx.home.join("head-sha"), &sha1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (c, st) = snap_scan(&env);
    assert!(st.remote.iter().any(|l| l.contains("unchanged at")), "{:?}", st.remote);
    assert_eq!(c.query_row("SELECT indexed_at FROM remote_snapshots", [], |r| r.get::<_, i64>(0)).unwrap(), at);
    assert!(std::fs::read_to_string(fx.home.join("gh.log")).unwrap().contains("repos/acme/secret-tool/branches/dev"));
    drop(c);

    // HEAD moves: re-snapshot, old text gone, new text searchable.
    write(&work.join("README.md"), "# Secret tool\n\n## Usage\nThe albatross deploy runbook lives here.\n");
    git_out(&work, &["commit", "-qam", "second"]);
    git_out(&work, &["push", "-q", bare.to_str().unwrap(), "dev"]);
    let sha2 = git_out(&bare, &["rev-parse", "dev"]);
    std::fs::write(fx.home.join("head-sha"), &sha2).unwrap();
    let (c, st) = snap_scan(&env);
    assert!(st.remote.iter().any(|l| l.contains(&format!("indexed {}", &sha2[..12]))), "{:?}", st.remote);
    assert!(global(&c, "pelican").contains("no matches") && global(&c, "albatross").contains("README.md#usage"));
    drop(c);

    // A failing refresh (size cap) keeps the last good snapshot and leaves no temp files.
    let mut x: u64 = 0x9e3779b97f4a7c15;
    let big: Vec<u8> = (0..400_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    std::fs::write(work.join("data.bin"), big).unwrap();
    write(&work.join("README.md"), "# Secret tool\n\n## Usage\nThe condor runbook.\n");
    git_out(&work, &["add", "-A"]);
    git_out(&work, &["commit", "-qm", "third"]);
    git_out(&work, &["push", "-q", bare.to_str().unwrap(), "dev"]);
    std::fs::remove_file(fx.home.join("head-sha")).unwrap();
    snapshot::set(&env, "snapshot_max_kb", "120").unwrap();
    let (c, st) = snap_scan(&env);
    assert!(st.remote.iter().any(|l| l.contains("failed (git clone exceeded the size cap of 120 KiB); previous snapshot kept")), "{:?}", st.remote);
    assert_eq!(tmp_entries(&env), 0, "temp clone removed after failure");
    assert!(global(&c, "albatross").contains("README.md#usage") && global(&c, "condor").contains("no matches"));
    let status = snapshot::status(&env, &c);
    assert!(status.contains(&format!("failed; {} indexed", &sha2[..12])) && status.contains("last error: git clone exceeded"), "{status}");
    drop(c);

    // An unreachable remote: same, error recorded, data kept.
    std::fs::rename(&bare, fx._t.path().join("moved.git")).unwrap();
    snapshot::set(&env, "snapshot_max_kb", "204800").unwrap();
    let (c, st) = snap_scan(&env);
    assert!(st.remote.iter().any(|l| l.contains("cannot resolve dev HEAD") && l.contains("previous snapshot kept")), "{:?}", st.remote);
    assert!(global(&c, "albatross").contains("README.md#usage"));
    drop(c);

    // Leftovers of an interrupted run (owner process gone) are removed; opting out deletes snapshot data.
    std::fs::create_dir_all(crate::snapshot::tmp_root(&env).join("snap-999999999-1-0/repo")).unwrap();
    let (c, st) = snap_scan(&env);
    assert!(st.remote.iter().any(|l| l.contains("removed 1 leftover temp clone")), "{:?}", st.remote);
    assert_eq!(tmp_entries(&env), 0);
    snapshot::clear_all(&c).unwrap();
    assert!(global(&c, "albatross").contains("no matches"));
}
