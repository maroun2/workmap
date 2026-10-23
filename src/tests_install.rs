//! `workmap install` on linked homes: shared skill catalogs in Git checkouts,
//! a symlinked Codex home, earlier manual registrations, conflicts, hooks,
//! service environment and uninstall.

use super::*;
use crate::hook::{self, Scope};
use std::os::unix::fs::symlink;

struct Linked {
    _t: TempDir,
    home: PathBuf,
    repo: PathBuf,
    catalog: PathBuf,
    codex: PathBuf,
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// The layout seen on a real host: `~/.claude/skills` -> shared repo catalog,
/// `~/.codex` -> a home on another disk whose `skills` -> the same catalog.
fn linked(manual_exclude: bool, legacy: bool) -> Linked {
    let t = TempDir::new("linked");
    let home = t.path().join("home");
    let repo = t.path().join("repos/skills");
    let catalog = repo.join("skills");
    write(&catalog.join("other/SKILL.md"), "---\nname: other\ndescription: tracked\n---\n");
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "catalog"]);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    symlink(&catalog, home.join(".claude/skills")).unwrap();
    let codex = t.path().join("disk/home/.codex");
    std::fs::create_dir_all(&codex).unwrap();
    symlink(&codex, home.join(".codex")).unwrap();
    symlink(&catalog, codex.join("skills")).unwrap();
    if manual_exclude {
        let ex = repo.join(".git/info/exclude");
        let mut text = std::fs::read_to_string(&ex).unwrap_or_default();
        text.push_str("# machine-local registration\n/skills/workmap\n");
        std::fs::write(&ex, text).unwrap();
    }
    if legacy {
        // Earlier install: a real OpenCode copy with a binary, and a hand-made link to it.
        let oc = home.join(".config/opencode/skills/workmap");
        write(&oc.join("SKILL.md"), &format!("---\nname: workmap\ndescription: \"old\"\n---\n\n{}\nold body\n", install::MARKER));
        write(&oc.join("bin/workmap"), "old binary copy");
        symlink(&oc, catalog.join("workmap")).unwrap();
    }
    Linked { _t: t, home, repo, catalog, codex }
}

fn env_for(l: &Linked) -> Env {
    let mut env = Env::with_home(&l.home);
    // As `Env::from_process` resolves a symlinked ~/.codex.
    env.codex_dir = std::fs::canonicalize(l.home.join(".codex")).unwrap();
    env
}

fn exe(l: &Linked) -> PathBuf {
    let p = l.home.join("build/workmap");
    write(&p, "#!/bin/sh\necho workmap\n");
    p
}

fn opts() -> install::InstallOpts {
    install::InstallOpts { activate: true, hooks: false }
}

/// Every regular file under `dir` that is not inside `.git`, without following links.
fn files_without_links(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let ft = e.file_type().unwrap();
        if e.file_name() == ".git" || ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            files_without_links(&e.path(), out);
        } else {
            out.push(e.path());
        }
    }
}

fn changed(r: &[String]) -> Vec<&String> {
    r.iter().filter(|l| ["updated", "wrote ", "linked ", "migrated", "excluded", "removed"].iter().any(|p| l.starts_with(p))).collect()
}

#[test]
fn install_on_linked_home_migrates_manual_registration_and_keeps_shared_repo_clean() {
    let l = linked(true, true);
    let env = env_for(&l);
    let c = db::open(&env.db_path()).unwrap();
    let exclude_before = std::fs::read_to_string(l.repo.join(".git/info/exclude")).unwrap();
    let r1 = install::install(&env, &c, &exe(&l), &opts()).unwrap();
    assert!(!r1.iter().any(|x| x.starts_with(install::ERROR)), "{r1:?}");
    let stable = l.home.join(".local/share/workmap/skills/workmap");
    let entry = l.catalog.join("workmap");
    assert_eq!(std::fs::read_link(&entry).unwrap(), stable, "manual link repointed at the stable skill");
    assert!(r1.iter().any(|x| x.starts_with("migrated claude skill")), "{r1:?}");
    assert!(r1.iter().any(|x| x.starts_with("shared codex skill catalog")), "{r1:?}");
    // OpenCode keeps a real copy, now without the binary.
    let oc = l.home.join(".config/opencode/skills/workmap");
    assert!(!oc.join("bin").exists() && std::fs::read_to_string(oc.join("SKILL.md")).unwrap().contains(install::MARKER));
    // All three providers read the same generated text.
    let s = std::fs::read_to_string(stable.join("SKILL.md")).unwrap();
    for p in [l.home.join(".claude/skills/workmap/SKILL.md"), l.home.join(".codex/skills/workmap/SKILL.md"), oc.join("SKILL.md")] {
        assert_eq!(std::fs::read_to_string(&p).unwrap(), s, "{}", p.display());
    }
    assert!(s.contains(&env.home.join(".local/share/workmap/bin/workmap").display().to_string()));
    assert!(s.contains(&format!("WORKMAP_CODEX_DIR={}", l.codex.display())), "symlinked codex home pinned canonically without the user setting it");
    for v in r1.iter().filter(|x| x.starts_with("verify: ") && x.contains(" reads ")) {
        assert!(v.ends_with("(workmap skill, ok)"), "{v}");
    }
    // Shared repo: nothing untracked, no generated content, exclude not duplicated.
    assert_eq!(git_out(&l.repo, &["status", "--porcelain", "--untracked-files=all"]), "");
    assert_eq!(std::fs::read_to_string(l.repo.join(".git/info/exclude")).unwrap(), exclude_before);
    let mut files = Vec::new();
    files_without_links(&l.repo, &mut files);
    assert!(files.iter().all(|f| !std::fs::read_to_string(f).unwrap_or_default().contains(install::MARKER)), "{files:?}");

    // Idempotent: second run changes nothing.
    let r2 = install::install(&env, &c, &exe(&l), &opts()).unwrap();
    assert!(changed(&r2).is_empty(), "{:?}", changed(&r2));

    // Uninstall: links before targets, nothing dangling, the user's exclude line kept.
    let u = install::uninstall(&env).unwrap();
    assert!(std::fs::symlink_metadata(&entry).is_err() && !oc.exists() && !stable.exists(), "{u:?}");
    assert!(l.catalog.join("other/SKILL.md").is_file());
    assert_eq!(std::fs::read_to_string(l.repo.join(".git/info/exclude")).unwrap(), exclude_before);
    assert_eq!(git_out(&l.repo, &["status", "--porcelain"]), "");
}

#[test]
fn install_adds_one_marked_exclude_when_the_shared_repo_does_not_ignore_the_link() {
    let l = linked(false, false);
    let env = env_for(&l);
    let c = db::open(&env.db_path()).unwrap();
    let r1 = install::install(&env, &c, &exe(&l), &opts()).unwrap();
    assert!(r1.iter().any(|x| x.starts_with("excluded /skills/workmap")), "{r1:?}");
    assert!(r1.iter().any(|x| x.starts_with("linked claude skill")), "{r1:?}");
    assert_eq!(git_out(&l.repo, &["status", "--porcelain", "--untracked-files=all"]), "");
    let ex = std::fs::read_to_string(l.repo.join(".git/info/exclude")).unwrap();
    assert_eq!(ex.matches("/skills/workmap").count(), 1);
    install::install(&env, &c, &exe(&l), &opts()).unwrap();
    assert_eq!(std::fs::read_to_string(l.repo.join(".git/info/exclude")).unwrap(), ex);
    install::uninstall(&env).unwrap();
    assert!(!std::fs::read_to_string(l.repo.join(".git/info/exclude")).unwrap().contains("workmap"));
    assert_eq!(git_out(&l.repo, &["status", "--porcelain", "--untracked-files=all"]), "");
}

#[test]
fn install_reports_conflicts_and_never_overwrites_foreign_entries() {
    // Foreign link at the registration name.
    let l = linked(true, false);
    let env = env_for(&l);
    let c = db::open(&env.db_path()).unwrap();
    let mine = l.home.join("my-skill");
    write(&mine.join("SKILL.md"), "---\nname: workmap\ndescription: mine\n---\n");
    symlink(&mine, l.catalog.join("workmap")).unwrap();
    let r = install::install(&env, &c, &exe(&l), &opts()).unwrap();
    assert!(r.iter().any(|x| x.starts_with(install::ERROR) && x.contains("did not generate")), "{r:?}");
    assert_eq!(std::fs::read_link(l.catalog.join("workmap")).unwrap(), mine);
    assert!(std::fs::read_to_string(mine.join("SKILL.md")).unwrap().contains("mine"));
    // Uninstall leaves it too.
    let u = install::uninstall(&env).unwrap();
    assert!(u.iter().any(|x| x.contains("left claude skill link")), "{u:?}");
    assert_eq!(std::fs::read_link(l.catalog.join("workmap")).unwrap(), mine);

    // Real unowned directory in a plain catalog, and an owned copy with extra files in a linked one.
    let l = linked(true, false);
    let env = env_for(&l);
    let c = db::open(&env.db_path()).unwrap();
    write(&l.home.join(".config/opencode/skills/workmap/SKILL.md"), "hand written\n");
    write(&l.catalog.join("workmap/SKILL.md"), &format!("---\nname: workmap\n---\n{}\n", install::MARKER));
    write(&l.catalog.join("workmap/notes.md"), "user notes\n");
    let r = install::install(&env, &c, &exe(&l), &opts()).unwrap();
    assert_eq!(r.iter().filter(|x| x.starts_with(install::ERROR)).count(), 2, "{r:?}");
    assert_eq!(std::fs::read_to_string(l.home.join(".config/opencode/skills/workmap/SKILL.md")).unwrap(), "hand written\n");
    assert_eq!(std::fs::read_to_string(l.catalog.join("workmap/notes.md")).unwrap(), "user notes\n");
}

#[test]
fn uninstall_of_an_old_layout_leaves_no_dangling_link() {
    let l = linked(true, true);
    let env = env_for(&l);
    install::uninstall(&env).unwrap();
    assert!(std::fs::symlink_metadata(l.catalog.join("workmap")).is_err(), "manual link to the owned copy removed with it");
    assert!(!l.home.join(".config/opencode/skills/workmap").exists());
}

#[test]
fn install_adds_hooks_by_default_idempotently_and_keeps_backups_out_of_settings_dirs() {
    let l = linked(true, false);
    let env = env_for(&l);
    let c = db::open(&env.db_path()).unwrap();
    let foreign = json!({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo foreign"}]}]}, "other": 1});
    write(&l.codex.join("hooks.json"), &serde_json::to_string_pretty(&foreign).unwrap());
    let o = install::InstallOpts { activate: true, hooks: true };
    let r1 = install::install(&env, &c, &exe(&l), &o).unwrap();
    assert!(r1.iter().any(|x| x.starts_with("codex hooks: installed 4 workmap hook(s)")), "{r1:?}");
    assert!(r1.iter().any(|x| x.starts_with("claude hooks: installed 4 workmap hook(s)")), "{r1:?}");
    assert!(r1.iter().any(|x| x.starts_with("verify: codex trust records")), "{r1:?}");
    let before = std::fs::read(l.codex.join("hooks.json")).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(v["other"], 1);
    assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    assert_eq!(v["hooks"]["SessionStart"][0], foreign["hooks"]["SessionStart"][0]);
    let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.contains(&env.home.join(".local/share/workmap/bin/workmap").display().to_string()) && !cmd.contains("/build/"), "stable binary, not the build path: {cmd}");
    // Backups live in the state dir, not in ~/.codex.
    assert!(!std::fs::read_dir(&l.codex).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("backup")));
    assert!(hook::backup_path(&env, &l.codex.join("hooks.json")).is_file());

    let r2 = install::install(&env, &c, &exe(&l), &o).unwrap();
    assert!(r2.iter().any(|x| x.starts_with("codex hooks: unchanged") && x.contains("existing Codex trust still applies")), "{r2:?}");
    assert_eq!(std::fs::read(l.codex.join("hooks.json")).unwrap(), before, "same definitions, same bytes: trust hashes stay valid");

    // --no-hooks writes none; project scope backups do not land in the project.
    let l2 = linked(true, false);
    let env2 = env_for(&l2);
    let r = install::install(&env2, &c, &exe(&l2), &opts()).unwrap();
    assert!(r.iter().any(|x| x.contains("--no-hooks")) && !l2.codex.join("hooks.json").exists());
    let proj = l2.home.join("proj");
    write(&proj.join(".codex/hooks.json"), "{}");
    let bin = env2.home.join(".local/share/workmap/bin/workmap");
    hook::install(&env2, &hook::HooksOpts { harness: "codex", scope: Scope::Project, cwd: &proj, bin: &bin, dry_run: false }).unwrap();
    assert_eq!(std::fs::read_dir(proj.join(".codex")).unwrap().count(), 1, "only hooks.json in the project");
}

#[test]
fn service_unit_carries_git_and_path_and_scan_requires_git() {
    let l = linked(true, false);
    let env = env_for(&l);
    let path = std::env::var_os("PATH");
    let tools = install::resolve_tools(&env, path.as_deref());
    let git = tools.git.clone().expect("git on PATH for tests");
    let (svc, _) = install::units(&env, Path::new("/opt/work map/workmap"), &tools);
    assert!(svc.contains(&format!("Environment=\"WORKMAP_GIT_BIN={}\"", git.display())), "{svc}");
    assert!(svc.contains(&format!("Environment=\"PATH={}", git.parent().unwrap().display())), "{svc}");
    assert!(svc.contains("ExecStart=\"/opt/work map/workmap\" scan --quiet"), "{svc}");
    // A PATH that lists only a temp dir contributes nothing temporary.
    let tmp_git = l.home.join("t/git");
    assert!(install::resolve_tools(&env, Some(std::ffi::OsStr::new("/nonexistent"))).git.is_none());
    assert!(!install::resolve_tools(&env, Some(tmp_git.parent().unwrap().as_os_str())).path.contains("/t:"));
    let e = crate::util::check_git(Path::new("/nonexistent/git")).unwrap_err();
    assert!(e.contains("git is required") && e.contains("WORKMAP_GIT_BIN"), "{e}");
    assert!(crate::util::check_git(&git).unwrap().starts_with("git version"));
}

#[test]
fn codex_trust_records_are_read_not_written() {
    let l = linked(true, false);
    let env = env_for(&l);
    let cfg = l.codex.join("config.toml");
    let hooks = l.codex.join("hooks.json");
    let text = format!("model = \"m\"\n[hooks.state.\"{}:stop:0:0\"]\ntrusted_hash = \"sha256:abc\"\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n", hooks.display());
    write(&cfg, &text);
    assert_eq!(hook::codex_trust_records(&env), vec![format!("{}:stop:0:0", hooks.display())]);
    let lines = hook::trust_lines(&env).join("\n");
    assert!(lines.contains("trust records for") && lines.contains(": 1 "), "{lines}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
}
