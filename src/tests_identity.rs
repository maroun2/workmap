//! Project identity: version, duplicate-clone handles, raw remote URLs, remote graph.
//! Synthetic fixtures only: temp homes, fake CLIs, no network, no real home.

use super::more_tests::{dump, git_out, global, relations_fixture, snap_scan, snapshot_env, tmp_entries};
use super::*;
use crate::testutil::commit_all;
use crate::{relations, search, view::more};

// ------------------------------------------------------------------ helpers

fn ev_expand(c: &Connection, r: &str) -> Result<String, String> {
    view::expand(&v(c), r, &view::Req::default())
}

/// Every `remote:` token printed in an output (bare, as the views print them).
fn remote_refs(out: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for w in out.split(|ch: char| ch.is_whitespace() || ch == '\'' || ch == '"' || ch == '`') {
        if let Some(i) = w.find("remote:") {
            let r = w[i..].trim_end_matches([',', ')', '.', ';']).to_string();
            if (i == 0 || w[..i].ends_with('(')) && r.len() > "remote:".len() && !v.contains(&r) {
                v.push(r);
            }
        }
    }
    v
}

/// Each `remote:` ref expands from the index; a file ref's view is titled with its canonical form.
fn assert_remote_refs_expand(c: &Connection, out: &str) -> usize {
    let refs = remote_refs(out);
    for r in &refs {
        let got = ev_expand(c, r).unwrap_or_else(|e| panic!("{r}: {e}\n--- in ---\n{out}"));
        assert!(got.starts_with(&format!("# {r} ")), "{r} expanded to another node:\n{}", got.lines().next().unwrap_or(""));
    }
    refs.len()
}

/// Evidence locators (`at <locator>`): expandable refs expand; anything else says source only.
fn assert_locators(c: &Connection, out: &str) -> usize {
    let mut n = 0;
    for line in out.lines() {
        let Some(i) = line.find("] at ") else { continue };
        let rest = &line[i + 5..];
        let tok = if let Some(q) = rest.strip_prefix('\'') { q.split('\'').next().unwrap_or("").to_string() } else { rest.split_whitespace().next().unwrap_or("").to_string() };
        n += 1;
        if tok.starts_with('/') || rest.contains("source only") {
            assert!(rest.contains("source only"), "a path locator must be marked source-only: {line}");
            // A transcript locator carries a usable conversation reference.
            if rest.contains("(transcript; source only)") {
                let conv = rest.split("— conversation ").nth(1).unwrap();
                let w = match conv.strip_prefix('\'') {
                    Some(q) => q.split('\'').next().unwrap(),
                    None => conv.split_whitespace().next().unwrap(),
                };
                let got = ev_expand(c, w).unwrap_or_else(|e| panic!("{w}: {e}\n{line}"));
                assert!(got.contains("conversation"), "{got}");
            }
            continue;
        }
        let got = ev_expand(c, &tok).unwrap_or_else(|e| panic!("locator {tok} does not expand: {e}\n{line}"));
        assert!(got.starts_with("# "), "{got}");
    }
    n
}

// ------------------------------------------------------------------ version

#[test]
fn version_string_embeds_semver_revision_and_build() {
    let v = crate::version();
    assert!(v.starts_with(&format!("{} (rev ", env!("CARGO_PKG_VERSION"))) && env!("CARGO_PKG_VERSION") == "0.3.0", "{v}");
    let rev = v.split("(rev ").nth(1).unwrap().split([';', ')']).next().unwrap();
    let hex = rev.trim_end_matches("-dirty");
    assert!(rev == "unknown" || (hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit())), "{rev}");
    assert!(v.contains("; debug;") || v.contains("; release;"), "build profile shown: {v}");
    // Fallback order used by build.rs: git revision, else WORKMAP_BUILD_REV, else unknown.
    use crate::buildinfo::{render, revision};
    assert_eq!(revision(Some("0123456789ab\n"), false, Some("nixrev")), "0123456789ab");
    assert_eq!(revision(Some("0123456789ab"), true, None), "0123456789ab-dirty");
    assert_eq!(revision(None, false, Some("0123456789ab")), "0123456789ab");
    assert_eq!(revision(None, true, Some("v0.3.0-12-gabc")), "v0.3.0-12-gabc", "dirty only applies to a git-derived revision");
    assert_eq!(revision(Some("fatal: not a git repository"), false, None), "unknown");
    assert_eq!(revision(None, false, Some("unknown")), "unknown");
    assert_eq!(revision(None, false, Some("x\"; rm -rf /")), "unknown");
    assert_eq!(revision(None, false, Some("  ")), "unknown");
    assert_eq!(revision(None, false, None), "unknown");
    assert_eq!(render("0.3.0", "abc", "release", "x86_64-unknown-linux-gnu"), "0.3.0 (rev abc; release; x86_64-unknown-linux-gnu)");
    assert_eq!(render("0.3.0", "unknown", "", ""), "0.3.0 (rev unknown)");
}

// ------------------------------------------------------------------ duplicate clones

#[test]
fn duplicate_clones_get_stable_handles_in_every_listing() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let main = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap().handle;
    let clone = view::resolve_project(&c, &fx.api_clone.to_string_lossy()).unwrap().handle;
    assert!(main.starts_with("ledger-api~") && clone.starts_with("ledger-api~") && main != clone, "{main} {clone}");
    drop(c);
    // Stable across rescans (derived from the clone's git dir, not scan order).
    let (c, _) = scan_once(&env);
    assert_eq!(view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap().handle, main);
    assert_eq!(view::resolve_project(&c, &fx.api_clone.to_string_lossy()).unwrap().handle, clone);

    // The bare name is refused with both handles listed.
    let err = view::resolve_project(&c, "ledger-api").err().unwrap();
    assert!(err.contains("ambiguous") && err.contains(&main) && err.contains(&clone), "{err}");

    // Skill description: every project once, by handle; never a repeated bare name.
    let d = install::description(&c);
    let list = d.split("Indexed projects: ").nth(1).unwrap().split('.').next().unwrap();
    let mut names: Vec<&str> = list.split(", ").collect();
    names.sort();
    let mut want = vec![main.as_str(), clone.as_str(), "ledger-sdk"];
    want.sort();
    assert_eq!(names, want, "{d}");
    let skill = install::render_skill(&c, &env, Path::new("/opt/workmap"));
    assert!(!skill.contains("ledger-api, ") && !skill.contains("- ledger-api —"), "{skill}");

    // Overview lists handles, each with a command that works.
    let ov = overview::overview(&c, 40, "workmap").unwrap();
    assert!(ov.contains(&format!("- {main} — ")) && ov.contains(&format!("- {clone} — ")) && !ov.contains("- ledger-api — "), "{ov}");
    for h in [&main, &clone] {
        assert!(view::context(&v(&c), h, &view::Req::default()).unwrap().starts_with(&format!("# {h} — project map")));
    }

    // Both handles' file refs roundtrip to their own clone.
    let a = ev_expand(&c, &format!("{main}:README.md")).unwrap();
    let b = ev_expand(&c, &format!("{clone}:README.md")).unwrap();
    assert!(a.starts_with(&format!("# :README.md — document in {main} @main\n{}/README.md", fx.api.display())), "{a}");
    assert!(b.starts_with(&format!("# :README.md — document in {clone} @main\n{}/README.md", fx.api_clone.display())), "{b}");
    assert_refs_roundtrip(&c, "ledger-sdk", &format!("see {main}:README.md and {clone}:README.md\n"));

    // related / why / search name the clones by handle.
    let sdk = view::resolve_project(&c, "ledger-sdk").unwrap();
    let rel = more::related_view(&v(&c), &sdk, 2, 20).unwrap();
    assert!(rel.contains(&format!(":related:{main}")) && !rel.contains(":related:ledger-api "), "{rel}");
    assert_refs_roundtrip(&c, "ledger-sdk", &rel);
    assert_locators(&c, &rel);
    let why = more::why_cmd(&v(&c), Some(&clone), "ledger-sdk", &view::Req::default(), 4).unwrap();
    assert!(why.contains(&format!("1. {clone} → ")) && why.contains("ledger-sdk"), "{why}");
    let hits = more::search_view(
        &v(&c),
        &view::Req::default(),
        &more::SearchReq { query: search::Query { text: "ledger-api".into(), kinds: vec!["project".into()], limit: 10, ..Default::default() }, this_worktree: false },
    )
    .unwrap();
    assert!(hits.contains(&format!(" {main} — project")) && hits.contains(&format!(" {clone} — project")), "{hits}");
}

// ------------------------------------------------------------------ raw remote URLs and credentials

#[test]
fn raw_remote_url_is_the_identity_despite_insteadof_and_credentials_never_persist() {
    let t = TempDir::new("rawurl");
    let home = t.path().join("home");
    let mirror = t.path().join("mirror");
    // `dep`: a local project the manifest of `x` points at by URL.
    let dep = home.join("code/dep");
    write(&dep.join("README.md"), "# dep\n");
    git(&dep, &["init", "-q"]);
    git(&dep, &["remote", "add", "origin", "git@github.com:acme/dep.git"]);
    commit_all(&dep, "dep");
    let x = home.join("code/x");
    write(&x.join("Cargo.toml"), "[package]\nname = \"x\"\n\n[dependencies]\ndep = { git = \"https://bot:s3cr3t-pass-word@github.com/acme/dep?access_token=tok-QQQQ\" }\n");
    write(
        &x.join("README.md"),
        "# x\n\nDesign: https://gitlab.example.org/g/y/-/blob/main/d.md?private_token=glpat-LEAKEDLEAKEDLEAKED and https://ci:ghp_LEAKEDLEAKEDLEAKED1234@github.com/acme/dep\n",
    );
    git(&x, &["init", "-q"]);
    git(&x, &["remote", "add", "origin", "https://ci:ghp_LEAKEDLEAKEDLEAKED1234@github.com/acme/x.git"]);
    git(&x, &["remote", "add", "upstream", "https://gitlab.example.org/g/y.git?private_token=glpat-LEAKEDLEAKEDLEAKED"]);
    // The rewrite lives in a test-local config file pulled in by include.path (honoured
    // when reading config). Not GIT_CONFIG_GLOBAL: a process-wide env var would leak
    // into every test running in parallel.
    let rules = t.path().join("rewrite.gitconfig");
    write(&rules, &format!("[url \"file://{}/\"]\n\tinsteadOf = https://ci:ghp_LEAKEDLEAKEDLEAKED1234@github.com/acme/\n", mirror.display()));
    git(&x, &["config", "include.path", rules.to_str().unwrap()]);
    commit_all(&x, "x");
    // Sanity: git itself rewrites the fetch URL to the local mirror.
    assert!(git_out(&x, &["remote", "get-url", "origin"]).starts_with("file://"));

    let env = Env::with_home(&home);
    let (c, _) = scan_once(&env);
    let p = view::resolve_project(&c, "x").unwrap();
    let remotes = p.repo.meta["remotes"].as_array().unwrap().clone();
    let origin = remotes.iter().find(|r| r["name"] == "origin").unwrap();
    assert_eq!(origin["url"], "https://github.com/acme/x.git", "raw configured URL, credentials stripped: {remotes:?}");
    assert!(origin["fetch_url"].as_str().unwrap().starts_with("file://"), "rewritten URL kept only as a secondary candidate: {remotes:?}");
    let up = remotes.iter().find(|r| r["name"] == "upstream").unwrap();
    assert_eq!(up["url"], "https://gitlab.example.org/g/y.git");
    let ids = relations::repo_identities(&p.repo);
    assert_eq!(ids[..2], ["github.com/acme/x".to_string(), "gitlab.example.org/g/y".to_string()], "{ids:?}");
    assert_eq!(p.handle, "x");

    // The manifest dependency resolves to the local dep project; quoted lines are scrubbed.
    let rel = more::related_cmd(&v(&c), Some("x"), &view::Req::default(), 2, 20).unwrap();
    assert!(rel.contains(":related:dep") && rel.contains("Cargo git dependency [declared] at x:Cargo.toml l.5"), "{rel}");
    assert!(rel.contains("https://github.com/acme/dep\""), "credentials and query stripped from the quoted line: {rel}");
    assert_locators(&c, &rel);
    let why = more::why_cmd(&v(&c), Some("x"), "dep", &view::Req::default(), 3).unwrap();
    let ev = ev_expand(&c, "x:related:dep").unwrap();
    let all = format!("{}\n{rel}\n{why}\n{ev}\n{}", dump(&c), overview::overview(&c, 40, "workmap").unwrap());
    for secret in ["ghp_LEAKED", "glpat-LEAKED", "s3cr3t", "tok-QQQQ", "access_token", "private_token", "ci:ghp"] {
        assert!(!all.contains(secret), "{secret} leaked:\n{all}");
    }

    // URL forms normalize to one identity; credential stripping keeps distinct forms usable.
    let want = Some("github.com/acme/x".to_string());
    for u in ["ssh://git@GitHub.com:2222/Acme/X.git/", "git@github.com:acme/x", "https://u:p@github.com:443/acme/x.git?x=1", "git+ssh://git@github.com/acme/x"] {
        assert_eq!(crate::util::remote_identity(&crate::git::strip_credentials(u)), want, "{u}");
    }
    assert_eq!(crate::git::strip_credentials("user:pw@host.example:ns/r.git"), "user@host.example:ns/r.git");
    assert_eq!(crate::git::strip_credentials("ssh://git@host.example:22/ns/r.git"), "ssh://git@host.example:22/ns/r.git");
    assert_eq!(crate::git::strip_credentials("https://TOKENONLY@host.example/ns/r"), "https://host.example/ns/r");
    assert_eq!(crate::git::strip_credentials("/srv/git/a@b/r.git"), "/srv/git/a@b/r.git");
    assert_eq!(
        crate::git::parse_remote_config("remote.origin.url\nhttps://a/b\0remote.my.fork.url\ngit@h.x:n/r\0remote.origin.url\nsecond\0"),
        vec![("origin".to_string(), "https://a/b".to_string()), ("my.fork".to_string(), "git@h.x:n/r".to_string())]
    );
}

// ------------------------------------------------------------------ remote graph

#[test]
fn remote_endpoints_in_related_why_and_expand_from_the_cache() {
    let fx = relations_fixture();
    let (env, bare, work) = snapshot_env(&fx);
    let id = "github.com/acme/secret-tool";
    let rref = format!("remote:{id}");
    // A local project linked to the remote only by the remote's README.
    let runbooks = fx.home.join("code/runbooks");
    write(&runbooks.join("README.md"), "# runbooks\n\nOperational notes, nothing shared.\n");
    git(&runbooks, &["init", "-q"]);
    git(&runbooks, &["remote", "add", "origin", "https://github.com/ops/runbooks.git"]);
    commit_all(&runbooks, "runbooks");
    write(
        &work.join("README.md"),
        "# Secret tool\n\nInternal.\n\n## Usage\nThe pelican deploy runbook lives here.\nSee https://github.com/acme/app for the app.\nOn-call steps: https://github.com/ops/runbooks\n",
    );
    git_out(&work, &["commit", "-qam", "link runbooks"]);
    git_out(&work, &["push", "-q", bare.to_str().unwrap(), "dev"]);

    // Metadata only: expand says so, and the remote is already a graph endpoint.
    let (c, _) = snap_scan(&env);
    let meta_only = ev_expand(&c, &rref).unwrap();
    assert!(meta_only.contains("## Catalog metadata (from gh on github.com, fetched") && meta_only.contains("none: only catalog metadata is indexed"), "{meta_only}");
    let rel0 = more::related_cmd(&v(&c), Some(&rref), &view::Req::default(), 1, 20).unwrap();
    assert!(rel0.starts_with(&format!("# {rref} — related projects")) && rel0.contains("- app — Cargo git dependency"), "app's manifest names it:\n{rel0}");
    drop(c);

    crate::snapshot::set(&env, "snapshots", "on").unwrap();
    let (c, _) = snap_scan(&env);
    let sha = git_out(&bare, &["rev-parse", "dev"]);
    // The temporary clone is gone; everything below reads the index only.
    assert_eq!(tmp_entries(&env), 0);

    // expand remote:<id>: catalog metadata vs cached snapshot, with expandable doc/manifest refs.
    let rv = ev_expand(&c, &rref).unwrap();
    assert!(rv.contains("## Catalog metadata (from gh on github.com, fetched") && rv.contains("## Cached snapshot (opt-in"), "{rv}");
    assert!(rv.contains(&format!("dev @ {sha} · indexed ")) && rv.contains("1 docs, 1 manifests cached"), "{rv}");
    assert!(!rv.contains("Only metadata is indexed"), "{rv}");
    for want in [format!("{rref}:README.md"), format!("{rref}:README.md#usage"), format!("{rref}:Cargo.toml")] {
        assert!(rv.contains(&format!("{want} ")), "{want} listed:\n{rv}");
    }
    assert!(assert_remote_refs_expand(&c, &rv) >= 4);
    let manifest = ev_expand(&c, &format!("{rref}:Cargo.toml")).unwrap();
    assert!(manifest.contains("manifest in the REMOTE SNAPSHOT") && manifest.contains("app = { git = \"https://github.com/acme/app\" }"), "{manifest}");

    // Provenance alias `remote:<id>@<sha>/<path>:<line>` resolves to the canonical ref and the line.
    let alias = ev_expand(&c, &format!("{rref}@{}/Cargo.toml:5", &sha[..12])).unwrap();
    assert!(
        alias.starts_with(&format!("# {rref}:Cargo.toml — manifest")) && alias.contains("→ the indexed commit") && alias.contains("line 5: \"app = { git = \"https://github.com/acme/app\" }\""),
        "{alias}"
    );
    let alias_doc = ev_expand(&c, &format!("{rref}@{}/README.md:7", &sha[..12])).unwrap();
    assert!(alias_doc.starts_with(&format!("# {rref}:README.md — doc")) && alias_doc.contains("line 7: \"See https://github.com/acme/app for the app.\""), "{alias_doc}");

    // search --kind remote covers snapshot docs, with the cached-doc ref.
    let hits = more::search_view(
        &v(&c),
        &view::Req::default(),
        &more::SearchReq { query: search::Query { text: "pelican".into(), kinds: vec!["remote".into()], limit: 10, ..Default::default() }, this_worktree: false },
    )
    .unwrap();
    assert!(hits.contains(&format!("{rref}:README.md#usage")), "{hits}");
    assert_remote_refs_expand(&c, &hits);

    // related remote:<id>: one hop (app, runbooks) and two hops via app.
    let rel = more::related_cmd(&v(&c), Some(&rref), &view::Req::default(), 2, 20).unwrap();
    assert!(rel.contains("- app — ") && rel.contains("- runbooks — doc names its URL or path") && rel.contains("- web — identical file content (shared content, not lineage) (via app)"), "{rel}");
    assert!(rel.contains(&format!("at {rref}:README.md l.8")), "snapshot evidence points at the cached doc:\n{rel}");
    assert!(assert_locators(&c, &rel) >= 3);
    assert_remote_refs_expand(&c, &rel);

    // why, both directions, through and to the remote.
    let w1 = more::why_cmd(&v(&c), Some("web"), &rref, &view::Req::default(), 4).unwrap();
    assert!(w1.contains("1. web → app") && w1.contains(&format!("2. app → {rref}")) && !w1.contains("3. "), "{w1}");
    let w2 = more::why_cmd(&v(&c), Some(&rref), "web", &view::Req::default(), 4).unwrap();
    assert!(w2.starts_with(&format!("# why {rref} → web")) && w2.contains(&format!("1. {rref} → app")) && w2.contains("2. app → web"), "{w2}");
    // Local → remote → local: runbooks and app are related only through the remote.
    let w3 = more::why_cmd(&v(&c), Some("runbooks"), "app", &view::Req::default(), 4).unwrap();
    assert!(w3.contains(&format!("1. runbooks → {rref}")) && w3.contains(&format!("2. {rref} → app")) && w3.contains("not local disk"), "{w3}");
    for o in [&w1, &w2, &w3] {
        assert_locators(&c, o);
        assert_remote_refs_expand(&c, o);
    }
    // The scoped relation view accepts a remote endpoint too.
    let ev = view::expand(&v(&c), &format!(":related:{rref}"), &scoped("app")).unwrap();
    assert!(ev.starts_with(&format!("# :related:{rref} — evidence between app and {rref}")) && ev.contains("Cargo git dependency"), "{ev}");
    assert_locators(&c, &ev);
    // A conversation's evidence carries a usable conversation ref.
    let lib = view::resolve_project(&c, &fx.lib.to_string_lossy()).unwrap().handle;
    let app_lib = view::expand(&v(&c), &format!(":related:{lib}"), &scoped("app")).unwrap();
    assert!(app_lib.contains("(transcript; source only) — conversation"), "{app_lib}");
    assert!(assert_locators(&c, &app_lib) >= 4);
    assert_locators(&c, &more::related_cmd(&v(&c), Some("app"), &view::Req::default(), 1, 20).unwrap());
    drop(c);

    // HEAD moves: the old provenance still resolves but says it is not the indexed commit.
    write(&work.join("README.md"), "# Secret tool\n\n## Usage\nThe albatross runbook.\n");
    git_out(&work, &["commit", "-qam", "moved"]);
    git_out(&work, &["push", "-q", bare.to_str().unwrap(), "dev"]);
    let (c, _) = snap_scan(&env);
    let old = ev_expand(&c, &format!("{rref}@{}/Cargo.toml:5", &sha[..12])).unwrap();
    assert!(old.contains("NOT the indexed commit"), "{old}");
    assert!(global(&c, "albatross").contains(&format!("{rref}:README.md#usage")));
    assert_eq!(tmp_entries(&env), 0);
    let err = ev_expand(&c, "remote:github.com/acme/nope").unwrap_err();
    assert!(err.contains("no remote repository github.com/acme/nope in the catalog"), "{err}");
    // A catalog identity that is cloned locally points at the clone(s) by handle.
    let local = ev_expand(&c, "remote:github.com/acme/app").unwrap();
    assert!(local.contains("cloned locally") && local.contains("- app — "), "{local}");
}
