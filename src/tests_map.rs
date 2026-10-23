//! Compact project map (`context` default): purpose, typed directed
//! connections grouped by source, weak-edge hiding, stable references,
//! budget and `--detailed` compatibility. Synthetic fixtures only.

#[allow(unused_imports)]
use super::*;
use crate::view::map::{classify, compact_map, is_template, MapOpts, MAX_TARGETS};
use crate::view::more;

/// Claude transcript in `cwd` with Read (observed) results for `reads` and Edit results for `writes`.
fn conv(home: &Path, cwd: &Path, id: &str, title: &str, ts: &str, reads: &[PathBuf], writes: &[PathBuf]) {
    let c = cwd.display().to_string();
    let mut uses = Vec::new();
    let mut results = Vec::new();
    for (i, (p, tool)) in reads.iter().map(|p| (p, "Read")).chain(writes.iter().map(|p| (p, "Edit"))).enumerate() {
        uses.push(json!({"type":"tool_use","id":format!("t{i}"),"name":tool,"input":{"file_path": p.display().to_string()}}));
        results.push(json!({"type":"tool_result","tool_use_id":format!("t{i}"),"content":"ok"}));
    }
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

fn map_of(c: &Connection, handle: &str) -> String {
    view::context(&v(c), "", &scoped(handle)).unwrap()
}

/// Lines of the group whose header line starts with `head`.
fn group<'a>(out: &'a str, head: &str) -> Vec<&'a str> {
    let mut it = out.lines().skip_while(|l| !l.starts_with(head));
    let mut g: Vec<&str> = it.next().into_iter().collect();
    g.extend(it.take_while(|l| l.starts_with("  ")));
    assert!(!g.is_empty(), "no group {head:?} in:\n{out}");
    g
}

/// Every reference printed in adjacency lines and group headers.
fn map_refs(out: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for l in out.lines() {
        if let Some((_, rest)) = l.strip_prefix("  ").and_then(|x| x.split_once(" → ").or_else(|| x.split_once(" ← "))) {
            for t in rest.split(", ") {
                let t = t.rsplit_once(" +").filter(|x| x.1.chars().all(|c| c.is_ascii_digit())).map(|x| x.0).unwrap_or(t);
                refs.push(t.to_string());
            }
        } else if l.starts_with(':') || l.starts_with("':") {
            refs.push(l.split(' ').next().unwrap().to_string());
        } else if l.starts_with('"') {
            refs.extend(l.split(' ').find(|w| w.starts_with(":session:") || w.starts_with("':")).map(str::to_string));
        }
    }
    refs
}

#[test]
fn classification_keeps_confirmed_and_weak_apart() {
    for (k, s, t) in [
        ("read", "observed", "read"),
        ("write", "observed", "write"),
        ("read", "parsed", "parsed-read"),
        ("references", "structural", "ref"),
        ("touched", "structural", "changes"),
        ("history", "structural", "history"),
        ("rel:cargo_path", "declared", "dependency"),
        ("rel:conversation_access", "observed", "co-use"),
        ("rel:doc_link", "documented", "ref"),
        ("rel:same_upstream", "identity", "same-upstream"),
        ("rel:identical_files", "hash", "same-content"),
        ("rel:symlink", "structural", "symlink"),
    ] {
        assert_eq!(classify(k, s), Some((t, false)), "{k}/{s}");
    }
    for (k, s) in [
        ("read", "requested"),
        ("write", "requested"),
        ("read", "failed"),
        ("write", "failed"),
        ("mentioned", "mentioned"),
        ("searched", "parsed"),
        ("listed", "parsed"),
        ("reference_candidate", "candidate"),
        ("rel:name_match", "candidate"),
    ] {
        assert!(matches!(classify(k, s), Some((_, true))), "{k}/{s} must be weak");
    }
    for k in ["worktree_of", "doc", "memory", "entrypoint", "ran_in", "subagent_of"] {
        assert_eq!(classify(k, "structural"), None, "{k} is bookkeeping, not a map edge");
    }
    assert!(is_template("This project was bootstrapped with [Create React App](https://x)."));
    assert!(is_template("TODO") && !is_template("Payments ledger HTTP service."));
}

#[test]
fn map_groups_by_source_with_exact_direction_and_counts() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let api = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap();
    let h = api.handle.clone();
    let m = map_of(&c, &h);
    assert!(m.starts_with(&format!("# {h} — map · main @ ")), "{m}");
    assert!(m.contains("Purpose (:README.md l.3): \"Payments ledger HTTP service.\""), "{m}");
    assert!(m.contains("Selected (confirmed edges only): docs 1/1 · files 3/3 · conversations 2/2 · commits 1/1 · projects 2/2 · weak edges hidden: 0"), "{m}");
    assert!(m.contains("Worktrees (2): main*, fix/webhook-retry"), "{m}");
    // Source printed once, then targets per type; types with the same targets share a line.
    let port = group(&m, "\"Port retry backoff from the sdk\" :session:bbbb2222 (claude@main ");
    assert_eq!(port[1..], ["  read → :ledger/charges.py, ledger-sdk:src/retry.ts"]);
    assert_eq!(group(&m, "\"Double charge on webhook retry\"")[1], format!("  read, write → {h}@fix/webhook-retry:ledger/webhooks.py"));
    assert_eq!(group(&m, ":README.md")[1], "  ref → :ledger/charges.py, :ledger/webhooks.py");
    let cm = group(&m, ":commit:");
    assert!(
        cm[0].ends_with("\"Initial ledger service\" (2026-01-02)") && cm[1].starts_with("  changes → :README.md, ") && !cm[1].contains("@fix"),
        "a commit under main lists main's files only: {cm:?}"
    );
    let pg = group(&m, &format!("{h} (this project)"));
    assert!(
        pg.contains(&"  co-use → :related:ledger-sdk") && pg.iter().any(|l| (l.starts_with("  same-upstream → ") || l.starts_with("  same-upstream ← ")) && l.contains(":related:ledger-api~")),
        "{pg:?}"
    );
    assert_eq!(group(&m, "worktree main")[1], "  history → :commit:".to_string() + &cm[0][":commit:".len()..cm[0].find(' ').unwrap()]);
    // Seen from the sdk, the same stored relation row points the other way.
    let sdk = map_of(&c, "ledger-sdk");
    assert!(group(&sdk, "ledger-sdk (this project)").iter().any(|l| l.starts_with("  co-use ← ") && l.contains(&format!(":related:{h}"))), "{sdk}");
    // Nothing weak, no opaque ids, no why prose, one Next line with each command once.
    assert!(!m.contains("session-") && !m.contains("file-") && !m.contains("score=") && !m.contains("named in"), "{m}");
    assert_eq!(m.matches("workmap ").count(), 3, "{m}");
    assert_eq!(m.lines().filter(|l| l.starts_with("Scope: ") || l.starts_with("Types ")).count(), 2);
}

#[test]
fn every_emitted_ref_roundtrips_including_sibling_and_cross_project() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let h = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap().handle;
    let m = map_of(&c, &h);
    let refs = map_refs(&m);
    for want in [":README.md", ":ledger/charges.py", "ledger-sdk:src/retry.ts", ":session:aaaa1111", ":session:bbbb2222", ":related:ledger-sdk"] {
        assert!(refs.iter().any(|r| r == want), "{want} not emitted: {refs:?}\n{m}");
    }
    assert!(refs.iter().any(|r| r.starts_with(":commit:")) && refs.iter().any(|r| r.contains("@fix/webhook-retry:ledger/webhooks.py")), "{refs:?}");
    for r in &refs {
        let w = shell_split(r);
        let req = if w[0].starts_with(':') { scoped(&h) } else { view::Req::default() };
        let got = view::expand(&v(&c), &w[0], &req).unwrap_or_else(|e| panic!("{r}: {e}\n{m}"));
        if let Some(pre) = w[0].strip_prefix(":session:") {
            assert!(got.lines().any(|l| l.starts_with(&format!("session {pre}"))), "{r}:\n{got}");
        } else if w[0].contains("@fix/webhook-retry:") {
            assert!(got.contains(&format!("{}/ledger/webhooks.py", fx.api_fix.display())), "sibling ref must open the sibling's file:\n{got}");
        } else if w[0] == "ledger-sdk:src/retry.ts" {
            assert!(got.starts_with("# :src/retry.ts — file in ledger-sdk"), "{got}");
        }
    }
    assert!(assert_refs_roundtrip(&c, &h, &m) >= 6);
}

#[test]
fn conversation_ref_stays_valid_after_a_same_title_conversation_appears() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let h = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap().handle;
    let before = map_of(&c, &h);
    let line = group(&before, "\"Double charge on webhook retry\"")[0];
    let r = line.split(' ').find(|w| w.starts_with(":session:")).unwrap().to_string();
    assert_eq!(r, ":session:aaaa1111");
    // The title-prefix form the detailed view prints today ...
    let detailed = view::context(&v(&c), "", &view::Req { detailed: true, ..scoped(&h) }).unwrap();
    assert!(detailed.contains("':conversation:Double charge'"), "{detailed}");
    // ... becomes ambiguous once another conversation gets the same title; the session ref does not.
    conv(&fx.home, &fx.api, "eeee5555-again", "Double charge on webhook retry", "2026-09-20T08:00:00Z", &[fx.api.join("ledger/charges.py")], &[]);
    let (c2, _) = scan_once(&env);
    assert!(view::expand(&v(&c2), ":conversation:Double charge", &scoped(&h)).unwrap_err().contains("2 conversations"));
    let got = view::expand(&v(&c2), &r, &scoped(&h)).unwrap();
    assert!(got.lines().any(|l| l.starts_with("session aaaa1111-fix ")), "{got}");
    let after = map_of(&c2, &h);
    assert!(after.contains(&format!("{} (claude@fix/webhook-retry", r)) && after.contains("\"Double charge on webhook retry\" :session:eeee5555 (claude@main"), "{after}");
}

#[test]
fn weak_edges_are_hidden_by_default_and_never_shown_as_confirmed() {
    let fx = fixture();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let m = map_of(&c, "proj");
    let hidden: usize = m.split("weak edges hidden: ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
    assert!(hidden > 0, "{m}");
    for t in ["mention", "requested", "failed", "searched", "listed →", "listed,", "candidate", "Unconfirmed"] {
        assert!(!m.contains(t), "{t} leaked into the default map:\n{m}");
    }
    let mut req = scoped("proj");
    req.map_opts.weak = true;
    let w = view::context(&v(&c), "", &req).unwrap();
    let (confirmed, weak) = w.split_once("Unconfirmed (--weak; NOT evidence of access or relation):").unwrap_or_else(|| panic!("{w}"));
    assert!(weak.contains("mention → ") && weak.contains("write-failed → :src/app.ts"), "{weak}");
    for l in confirmed.lines().filter(|l| l.starts_with("  ")) {
        assert!(!["mention", "requested", "failed", "searched", "listed", "candidate"].iter().any(|t| l.contains(t)), "weak type among confirmed: {l}");
    }
    // The confirmed part is identical with or without --weak, apart from the header count.
    let strip = |s: &str| s.lines().filter(|l| !l.starts_with("Selected ") && !l.starts_with("Types ")).collect::<Vec<_>>().join("\n");
    assert_eq!(strip(confirmed).trim_end(), strip(m.split("\nNext:").next().unwrap()).trim_end());
}

#[test]
fn purpose_falls_back_honestly_for_template_and_missing_docs() {
    let t = TempDir::new("purpose");
    let home = t.path().join("home");
    let tpl = home.join("code/tpl");
    write(&tpl.join("README.md"), "# tpl\n\n![badge](x.svg)\n\nThis project was bootstrapped with Create React App.\n");
    write(&tpl.join("src/index.js"), "x\n");
    let both = home.join("code/both");
    write(&both.join("README.md"), "# both\n\nGetting Started with Create React App\n");
    write(&both.join("CLAUDE.md"), "# both\n\nWarehouse billing reconciler for nightly exports. More text here.\n");
    let none = home.join("code/nodoc");
    write(&none.join("main.go"), "package main\n");
    for p in [&tpl, &both, &none] {
        git(p, &["init", "-q"]);
        crate::testutil::commit_all(p, "start");
    }
    let env = Env::with_home(&home);
    let (c, _) = scan_once(&env);
    let m = map_of(&c, "tpl");
    assert!(m.contains("Purpose: not stated; :README.md l.5 is generic template text (\"This project was bootstrapped with Create React App.\")"), "{m}");
    let m = map_of(&c, "both");
    assert!(m.contains("Purpose (:CLAUDE.md l.3): \"Warehouse billing reconciler for nightly exports.\""), "{m}");
    let m = map_of(&c, "nodoc");
    assert!(m.contains("Purpose: not stated (no README/doc intro indexed in this worktree)") && m.contains("docs 0/0"), "{m}");
    assert!(m.contains("  changes → :main.go"), "{m}");
}

#[test]
fn dense_graph_caps_targets_and_reports_omissions() {
    let t = TempDir::new("dense");
    let home = t.path().join("home");
    let p = home.join("code/dense");
    let mut readme = String::from("# dense\n\nMany small modules under one roof.\n\n## Files\n");
    for i in 0..30 {
        write(&p.join(format!("src/m{i:02}.rs")), "pub fn f() {}\n");
        readme.push_str(&format!("- `src/m{i:02}.rs`\n"));
    }
    write(&p.join("README.md"), &readme);
    git(&p, &["init", "-q"]);
    crate::testutil::commit_all(&p, "thirty modules");
    for k in 0..10 {
        let reads: Vec<PathBuf> = (0..20).map(|i| p.join(format!("src/m{:02}.rs", (i + k) % 30))).collect();
        conv(&home, &p, &format!("{k:02}dd{k:04}-dense"), &format!("Dense task {k}"), &format!("2026-03-{:02}T10:00:00Z", k + 1), &reads, &[p.join(format!("src/m{k:02}.rs"))]);
    }
    let env = Env::with_home(&home);
    let (c, _) = scan_once(&env);
    let m = map_of(&c, "dense");
    assert!(m.contains("files 8/31 ") || m.contains("files 8/30 "), "default --files 8 of all ranked files: {m}");
    assert!(m.contains("conversations 6/10"), "{m}");
    for l in m.lines().filter(|l| l.starts_with("  ") && l.contains(" → ")) {
        let n = l.split_once(" → ").unwrap().1.split(", ").count();
        assert!(n <= MAX_TARGETS, "{l}");
    }
    assert!(m.lines().any(|l| l.starts_with("  read → ") && l.ends_with(" +14")), "{m}");
    assert!(m.contains("more: workmap context --files 24 --sessions 10"), "{m}");
    // Bigger limits select more, still deterministic.
    let wide = view::context(&v(&c), "", &view::Req { map: view::MapLimits { files: 30, sessions: 10, ..Default::default() }, map_opts: MapOpts { budget: 100_000, weak: false }, ..scoped("dense") })
        .unwrap();
    assert!(wide.contains("files 30/") && wide.contains("conversations 10/10") && !wide.contains("omitted at --budget"), "{wide}");
    assert_eq!(
        wide,
        view::context(&v(&c), "", &view::Req { map: view::MapLimits { files: 30, sessions: 10, ..Default::default() }, map_opts: MapOpts { budget: 100_000, weak: false }, ..scoped("dense") })
            .unwrap()
    );
}

#[test]
fn budget_drops_whole_groups_and_keeps_unicode_intact() {
    let t = TempDir::new("budget");
    let home = t.path().join("home");
    let p = home.join("code/uni");
    write(&p.join("README.md"), "# uni\n\nŽluťoučký kůň: přehled úloh — 日本語のメモ.\n");
    for i in 0..8 {
        write(&p.join(format!("src/ž{i}.rs")), "x\n");
    }
    git(&p, &["init", "-q"]);
    crate::testutil::commit_all(&p, "start");
    for k in 0..6 {
        conv(&home, &p, &format!("{k:02}aa{k:04}-uni"), &format!("Úkol {k}: ďábelské ódy 日本語のタイトル"), &format!("2026-04-{:02}T10:00:00Z", k + 1), &[p.join(format!("src/ž{k}.rs"))], &[]);
    }
    let env = Env::with_home(&home);
    let (c, _) = scan_once(&env);
    let full = map_of(&c, "uni");
    assert!(!full.contains("omitted at --budget"), "{full}");
    let budget = full.len() - 250;
    let cut = view::context(&v(&c), "", &view::Req { map_opts: MapOpts { budget, weak: false }, ..scoped("uni") }).unwrap();
    assert!(cut.len() <= budget, "{} > {budget}", cut.len());
    let omitted = cut.lines().find(|l| l.contains(&format!("omitted at --budget {budget} bytes"))).unwrap_or_else(|| panic!("{cut}"));
    assert!(omitted.starts_with("… ") && omitted.contains("group(s)"), "{omitted}");
    let full_lines: std::collections::BTreeSet<&str> = full.lines().collect();
    for l in cut.lines().filter(|l| *l != omitted) {
        assert!(full_lines.contains(l), "line not whole: {l:?}");
    }
    // A group header is never printed without its adjacency lines.
    let lines: Vec<&str> = cut.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with("\"Úkol") {
            assert!(lines.get(i + 1).is_some_and(|n| n.starts_with("  read → ")), "{cut}");
        }
    }
    assert!(cut.lines().last().unwrap().starts_with("Next: "), "Next line kept:\n{cut}");
    // Even below the header size: header, omission note and Next line, nothing cut mid-line.
    let tiny = compact_map(&v(&c), &view::resolve_project(&c, "uni").unwrap(), &view::MapLimits::default(), &MapOpts { budget: 1, weak: false }).unwrap();
    assert!(tiny.contains("Purpose (:README.md l.3): \"Žluťoučký kůň: přehled úloh — 日本語のメモ.\"") && tiny.contains("omitted at --budget 1 bytes"), "{tiny}");
}

#[test]
fn detailed_is_the_previous_project_map_byte_for_byte() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let p = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap();
    let req = view::Req { detailed: true, ..scoped(&p.handle) };
    let d = view::context(&v(&c), "", &req).unwrap();
    assert_eq!(d, view::project_map(&v(&c), &p, &view::MapLimits::default()).unwrap());
    assert!(d.contains("## Key files") && d.contains("## Conversations (") && d.contains("Codes: r/w"), "{d}");
    // `expand <project>` keeps showing the detailed view.
    assert_eq!(view::expand(&v(&c), &p.handle, &view::Req::default()).unwrap(), d);
    // Task context is untouched by the map options.
    let t = more::TaskReq { task: Some("webhook retry"), files: &[], budget: 6000 };
    let task = view::task(&v(&c), &p.handle, &view::Req::default(), &t).unwrap();
    let task_weak = view::task(&v(&c), &p.handle, &view::Req { map_opts: MapOpts { budget: 700, weak: true }, ..Default::default() }, &t).unwrap();
    assert_eq!(task, task_weak);
    assert!(!task.contains("— map ·") && task.len() <= 6000, "{task}");
}

#[test]
fn skill_prints_the_command_prefix_once_and_compact_maps() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let bin = fx.home.join(".claude/skills/workmap/bin/workmap");
    let skill = install::render_skill(&c, &env, &bin);
    let prefix = install::command_prefix(&env, &bin);
    // Printed once for reading, and only on the commands that scan or change roots.
    assert_eq!(skill.matches(&prefix).count(), 5, "{skill}");
    assert_eq!(skill.lines().filter(|l| l.starts_with(&prefix) && !l.contains(" scan") && !l.contains(" roots ")).count(), 1, "{skill}");
    assert!(skill.contains("— map ·") && skill.contains("workmap context --detailed") && skill.contains("workmap expand"), "{skill}");
    assert!(!skill.contains("## Key files"), "skill uses the compact map:\n{skill}");
    assert!(skill.len() < install::MAX_BODY + 4096);
}
