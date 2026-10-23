//! `install`: one stable copy of the binary, one owned skill file registered with
//! every provider, default host hooks and a user-level systemd timer for a daily scan.
//! Idempotent; never uses sudo; never writes generated content or binaries
//! through symlinked/shared skill catalogs (those get a link to the owned skill).

use crate::config::Env;
use crate::util::sh_quote;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SEED: &str = "Use before exploring an unfamiliar project or repeating an investigation. Discover related work you may not know exists: files, worktrees, commits, and previous agent conversations. Find who already investigated a problem, search past conversation messages, see related projects with evidence, and ask follow-up questions using their saved context. Indexed projects: ";
pub const MAX_DESCRIPTION: usize = 1000;
pub const MAX_BODY: usize = 24 * 1024;

pub struct InstallOpts {
    pub activate: bool,
    /// Also install user-scope host hooks (Claude Code, Codex) where the host is set up.
    pub hooks: bool,
}

/// Comment line `install` writes above an exclude entry it added (uninstall removes both).
pub const EXCLUDE_MARK: &str = "# workmap install: machine-local skill registration (link to a private generated file)";

/// Everything `install` owns lives under here.
pub fn share_dir(env: &Env) -> PathBuf {
    env.home.join(".local/share/workmap")
}

/// The one stable executable every skill, hook and unit runs.
pub fn stable_bin(env: &Env) -> PathBuf {
    share_dir(env).join("bin/workmap")
}

/// Owned skill directory that linked/shared provider catalogs point at.
pub fn stable_skill_dir(env: &Env) -> PathBuf {
    share_dir(env).join("skills/workmap")
}

/// Provider skill catalogs: (provider, catalog dir, base the dir is reached from).
/// Codex honours `CODEX_HOME`/`WORKMAP_CODEX_DIR` (see `Env::codex_home`).
pub fn skill_dirs(env: &Env) -> Vec<(&'static str, PathBuf, PathBuf)> {
    let codex = env.codex_home();
    let codex_base = if codex.starts_with(&env.home) { env.home.clone() } else { codex.clone() };
    vec![("claude", env.home.join(".claude/skills"), env.home.clone()), ("codex", codex.join("skills"), codex_base), ("opencode", env.home.join(".config/opencode/skills"), env.home.clone())]
}

fn write_if_changed(path: &Path, data: &[u8]) -> std::io::Result<bool> {
    if std::fs::read(path).map(|d| d == data).unwrap_or(false) {
        return Ok(false);
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("workmap-tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

fn set_exec(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

/// Why a catalog must not receive generated files: a symlink between `base`
/// and `dir`, or `dir` living inside a Git checkout (a shared skills repo).
pub fn unsafe_target(home: &Path, base: &Path, dir: &Path) -> Option<String> {
    if let Ok(rel) = dir.strip_prefix(base) {
        let mut cur = base.to_path_buf();
        for comp in rel.components() {
            cur.push(comp);
            if is_symlink(&cur) {
                return Some(format!("{} is a symlink", cur.display()));
            }
        }
    }
    git_top(home, dir).map(|t| format!("{} is inside the git checkout {}", dir.display(), t.display()))
}

/// Nearest existing ancestor of `dir`, canonical.
fn canonical_existing(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find_map(|a| std::fs::canonicalize(a).ok())
}

/// Top of the Git work tree containing `dir` (canonical), found by `.git` entries;
/// the home directory itself (dotfiles repos) does not count.
fn git_top(home: &Path, dir: &Path) -> Option<PathBuf> {
    let c = canonical_existing(dir)?;
    let home = std::fs::canonicalize(home).ok();
    for a in c.ancestors() {
        if home.as_deref() == Some(a) || a == Path::new("/") {
            return None;
        }
        if std::fs::symlink_metadata(a.join(".git")).is_ok() {
            return Some(a.to_path_buf());
        }
    }
    None
}

fn git_ok(top: &Path, args: &[&str]) -> Option<String> {
    let out = crate::util::git_cmd().arg("-C").arg(top).args(args).env("GIT_OPTIONAL_LOCKS", "0").stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Keep a link inside a shared checkout out of `git status`: add it to that
/// repo's local `info/exclude` (never to tracked files) unless already ignored.
fn ensure_excluded(home: &Path, entry: &Path, report: &mut Vec<String>) -> Result<(), String> {
    let Some(top) = entry.parent().and_then(|p| git_top(home, p)) else { return Ok(()) };
    let parent = std::fs::canonicalize(entry.parent().unwrap()).map_err(|e| format!("{}: {e}", entry.display()))?;
    let rel = parent.strip_prefix(&top).unwrap_or(Path::new("")).join("workmap");
    let rel_s = rel.to_string_lossy().to_string();
    if git_ok(&top, &["ls-files", "--error-unmatch", "--", &rel_s]).is_some() {
        return Err(format!("{} is tracked in {}; not replacing a shared file", rel_s, top.display()));
    }
    if git_ok(&top, &["check-ignore", "-q", "--no-index", "--", &rel_s]).is_some() {
        return Ok(());
    }
    let ex = git_ok(&top, &["rev-parse", "--path-format=absolute", "--git-path", "info/exclude"]).ok_or_else(|| format!("cannot locate info/exclude of {}", top.display()))?;
    let ex = PathBuf::from(ex);
    let mut text = std::fs::read_to_string(&ex).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{EXCLUDE_MARK}\n/{rel_s}\n"));
    write_if_changed(&ex, text.as_bytes()).map_err(|e| format!("{}: {e}", ex.display()))?;
    report.push(format!("excluded /{rel_s} in {} (local only, not committed)", ex.display()));
    Ok(())
}

/// Drop the exclude lines `ensure_excluded` wrote (marked), nothing else.
fn remove_exclude(home: &Path, entry: &Path, report: &mut Vec<String>) {
    let Some(top) = entry.parent().and_then(|p| git_top(home, p)) else { return };
    let Some(ex) = git_ok(&top, &["rev-parse", "--path-format=absolute", "--git-path", "info/exclude"]) else { return };
    let Ok(text) = std::fs::read_to_string(&ex) else { return };
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    let mut removed = false;
    while let Some(l) = lines.next() {
        if l == EXCLUDE_MARK && lines.peek().map(|n| n.ends_with("/workmap")).unwrap_or(false) {
            lines.next();
            removed = true;
            continue;
        }
        out.push(l);
    }
    if removed {
        let body = if out.is_empty() { String::new() } else { out.join("\n") + "\n" };
        if std::fs::write(&ex, body).is_ok() {
            report.push(format!("removed workmap exclude entry from {ex}"));
        }
    }
}

fn read_owned(skill: &Path) -> bool {
    std::fs::read_to_string(skill).map(|t| t.starts_with("---\nname: workmap\n") && t.contains(MARKER)).unwrap_or(false)
}

/// A symlinked registration is ours when it resolves to a workmap-generated
/// skill, or (dangling) points at a location `install` has written before.
fn owned_link(env: &Env, entry: &Path) -> bool {
    let Ok(target) = std::fs::read_link(entry) else { return false };
    let target = if target.is_absolute() { target } else { entry.parent().unwrap_or(Path::new("/")).join(target) };
    if target.exists() {
        return read_owned(&target.join("SKILL.md"));
    }
    let mut known: Vec<PathBuf> = skill_dirs(env).into_iter().map(|(_, d, _)| d.join("workmap")).collect();
    known.push(stable_skill_dir(env));
    known.contains(&target)
}

fn links_to(entry: &Path, target: &Path) -> bool {
    match (std::fs::canonicalize(entry), std::fs::canonicalize(target)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Atomically point `entry` at `target` (temp link + rename: never a gap).
fn replace_link(entry: &Path, target: &Path) -> std::io::Result<()> {
    let tmp = entry.with_file_name(".workmap.workmap-tmp-link");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, entry)
}

/// Contents of an owned real skill dir that may be removed/converted.
fn owned_dir_removable(dir: &Path) -> bool {
    let ok_top = ["SKILL.md", "bin", "SKILL.workmap-tmp"];
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if !ok_top.contains(&n.as_str()) {
            return false;
        }
        if n == "bin" && std::fs::read_dir(e.path()).map(|r| r.flatten().any(|f| f.file_name() != "workmap")).unwrap_or(true) {
            return false;
        }
    }
    true
}

fn remove_owned_dir(dir: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(dir.join("bin/workmap"));
    let _ = std::fs::remove_dir(dir.join("bin"));
    let _ = std::fs::remove_file(dir.join("SKILL.workmap-tmp"));
    std::fs::remove_file(dir.join("SKILL.md"))?;
    std::fs::remove_dir(dir)
}

/// Register the owned skill with one provider catalog. Plain catalogs get a
/// real `workmap/SKILL.md`; linked/shared catalogs get `workmap -> <stable skill>`.
/// Returns a report line; `Err` is a genuine conflict (left untouched).
fn register(env: &Env, provider: &str, catalog: &Path, base: &Path, skill: &str, report: &mut Vec<String>) -> Result<String, String> {
    let entry = catalog.join("workmap");
    let stable = stable_skill_dir(env);
    let linked = unsafe_target(&env.home, base, catalog);
    let io = |e: std::io::Error| format!("{}: {e}", entry.display());
    if is_symlink(&entry) {
        if links_to(&entry, &stable) {
            return Ok(format!("unchanged {provider} skill {} -> {}", entry.display(), stable.display()));
        }
        if !owned_link(env, &entry) {
            return Err(format!(
                "{provider}: {} is a symlink to {} which workmap did not generate; leaving it (remove it to let workmap register)",
                entry.display(),
                std::fs::read_link(&entry).map(|t| t.display().to_string()).unwrap_or_default()
            ));
        }
        ensure_excluded(&env.home, &entry, report)?;
        replace_link(&entry, &stable).map_err(io)?;
        return Ok(format!("migrated {provider} skill {} -> {} (was an earlier workmap registration)", entry.display(), stable.display()));
    }
    let exists = std::fs::symlink_metadata(&entry).is_ok();
    if exists && !entry.is_dir() {
        return Err(format!("{provider}: {} exists and is not a directory; leaving it", entry.display()));
    }
    let empty = exists && std::fs::read_dir(&entry).map(|mut r| r.next().is_none()).unwrap_or(false);
    if exists && !empty && !read_owned(&entry.join("SKILL.md")) {
        return Err(format!("{provider}: {} holds a skill workmap did not generate; leaving it", entry.display()));
    }
    match linked {
        None => {
            let ch = write_if_changed(&entry.join("SKILL.md"), skill.as_bytes()).map_err(io)?;
            let legacy = entry.join("bin/workmap");
            let mut note = String::new();
            if std::fs::symlink_metadata(&legacy).is_ok() {
                std::fs::remove_file(&legacy).map_err(io)?;
                let _ = std::fs::remove_dir(entry.join("bin"));
                note = format!(" (removed old binary copy; skill runs {})", stable_bin(env).display());
            }
            Ok(format!("{} {provider} skill {}{note}", if ch || !note.is_empty() { "updated" } else { "unchanged" }, entry.join("SKILL.md").display()))
        }
        Some(why) => {
            if exists && !empty && !owned_dir_removable(&entry) {
                return Err(format!("{provider}: {} ({why}) holds extra files besides the generated skill; leaving it", entry.display()));
            }
            ensure_excluded(&env.home, &entry, report)?;
            if exists {
                if empty {
                    std::fs::remove_dir(&entry).map_err(io)?;
                } else {
                    remove_owned_dir(&entry).map_err(io)?;
                }
            }
            std::fs::create_dir_all(catalog).map_err(|e| format!("{}: {e}", catalog.display()))?;
            replace_link(&entry, &stable).map_err(io)?;
            Ok(format!("linked {provider} skill {} -> {} ({why}; generated file stays private)", entry.display(), stable.display()))
        }
    }
}

pub fn description(c: &Connection) -> String {
    // Project handles, not bare labels: two clones named alike are listed as
    // `name~hex` each (the same names `overview` prints and commands accept).
    let mut labels: Vec<String> = Vec::new();
    for r in crate::overview::nodes(c, "kind='repo' ORDER BY ts DESC, label, id", &[]).unwrap_or_default() {
        let h = crate::view::project_handle(c, &r).unwrap_or(r.label);
        if !labels.contains(&h) {
            labels.push(h);
        }
    }
    let mut d = SEED.to_string();
    let mut shown = 0;
    for l in &labels {
        let piece = if shown == 0 { l.clone() } else { format!(", {l}") };
        if d.len() + piece.len() + 16 > MAX_DESCRIPTION {
            break;
        }
        d.push_str(&piece);
        shown += 1;
    }
    if shown < labels.len() {
        d.push_str(&format!(" (+{} more)", labels.len() - shown));
    }
    if labels.is_empty() {
        d.push_str("(none yet; run workmap scan)");
    }
    d.push('.');
    let remote: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='remote'", [], |r| r.get(0)).unwrap_or(0);
    let extra = format!(" Plus {remote} accessible remote repositories not cloned here.");
    if remote > 0 && d.len() + extra.len() <= MAX_DESCRIPTION {
        d.push_str(&extra);
    }
    d
}

/// Ownership marker: only files carrying it are ever rewritten by `refresh_skills`.
pub const MARKER: &str = "<!-- generated by workmap install; refreshed after each scan -->";

/// `env WORKMAP_STATE_DIR=… [WORKMAP_HOME=…] <bin>` so generated commands use the same index.
pub fn command_prefix(env: &Env, bin: &Path) -> String {
    let vars: Vec<String> = env.exports().iter().map(|(k, v)| sh_quote(&format!("{k}={v}"))).collect();
    format!("env {} {}", vars.join(" "), sh_quote(&bin.display().to_string()))
}

pub fn skill_body(c: &Connection, env: &Env, bin: &Path) -> String {
    let b = command_prefix(env, bin);
    // The full prefix (same index, same provider dirs) is printed once; commands below say `workmap`.
    let w = "workmap";
    let mut o = format!(
        "{MARKER}\n\n# workmap\n\nLocal index of Git repos, worktrees, docs, commits and native agent conversations (Claude Code, Codex, OpenCode). Output lists evidence with source paths, lines and timestamps; it does not decide what is authoritative.\n\n\
         ## Commands\n\nRead-only commands below say `{w}`; the full command (same index and agent stores) is:\n\n```\n{b}\n```\n\nUse that full form for `scan` and `roots`, which read agent stores and write the index. Start map-first, then go deeper only where needed:\n\n```\n{w} overview                                   # all indexed projects by name\nexport WORKMAP_PROJECT=<project>             # or add --project <project>; WORKMAP_WORKTREE=<branch> for a non-main worktree\n{w} context                                    # compact map: quoted purpose, typed directed connections grouped by source\n{w} expand REF                                 # details and evidence for one ref printed by the map (file, doc, commit, conversation, :related:<project>)\n{w} search 'words'                             # no obvious starting node? full text over docs, commits, conversation messages\n{w} context --detailed                         # previous sections view: docs, key files with reasons, commits, conversations\n{w} context --weak                             # also list unconfirmed edges (mentions, requested/failed access, name matches)\n{w} context --task 'what you are doing' --file <path> --budget 6000   # ranked context for a task; every item says why\n{w} related                                    # related projects with evidence\n{w} why <project> <other>                      # shortest evidence chain between two projects\n{w} ask ':session:<id-prefix>' 'question' --dry-run   # read-only resume plan; drop --dry-run to run\n{b} scan   # refresh (incremental; also refreshes this file)\n```\n\nMap lines read `A type → B` (stored edge from A to B; `←` the reverse). Only confirmed edges are listed; weak ones are counted in the `Selected` line. Conversation titles come with a `:session:<native-id-prefix>` ref that stays valid when another conversation gets the same title. Conversation text is historical; a latest user input is not a decision. Check the `Index:` line for staleness. Entries marked remote: are repositories you can access but have not cloned.\n\n"
    );
    o.push_str(&format!(
        "## Registering project roots (do this once after install)\n\nThe default scan walks {} (symlinks followed two levels deep; caches, `.config`, `.local`, `tmp` skipped). Ask the user which directories hold their projects, and also look for project locations named in their agent configs, shell history or the repos listed below. Register every root directory outside that walk, then rescan:\n\n```\n{b} roots list\n{b} roots add <dir>          # repeat per project root\n{b} scan\n```\n\nRegistered roots:\n",
        env.home.display()
    ));
    let roots = env.load_roots();
    if roots.is_empty() {
        o.push_str("- (none yet)\n");
    }
    for r in roots {
        o.push_str(&format!("- {}\n", r.display()));
    }
    o.push_str("\n## Indexed overview\n\n");
    // Drop the scan clock from the header so an unchanged index renders byte-identical.
    let ov = crate::overview::overview(c, 40, w).unwrap_or_default();
    let ov: Vec<String> = ov.lines().enumerate().map(|(i, l)| if i == 0 { l.split("; last scan").next().unwrap_or(l).to_string() } else { l.to_string() }).collect();
    o.push_str(&ov.join("\n"));
    o.push('\n');
    let v = crate::view::View { c, cmd: w.to_string(), state: String::new() };
    let lim = crate::view::MapLimits { docs: 3, files: 4, commits: 2, sessions: 3, related: 3 };
    let opts = crate::view::map::MapOpts { budget: 1800, weak: false };
    if let Ok(mut st) = c.prepare("SELECT id FROM nodes WHERE kind='repo' ORDER BY ts DESC LIMIT 8") {
        let ids: Vec<String> = st.query_map([], |r| r.get(0)).map(|r| r.flatten().collect()).unwrap_or_default();
        for id in ids {
            if o.len() > MAX_BODY {
                o.push_str(&format!("\n… truncated; run `{w} context <project>` for the rest.\n"));
                break;
            }
            if let Ok(p) = crate::view::resolve_project(c, &id) {
                // Index freshness changes every scan (keep the file stable); scope and next
                // commands are in the Commands section above, printed once.
                let m = crate::view::map::compact_map(&v, &p, &lim, &opts).unwrap_or_default();
                let m: String = m.lines().filter(|l| !["Index: ", "WARNING: index is stale", "Scope: ", "Next: "].iter().any(|x| l.starts_with(x))).map(|l| format!("{l}\n")).collect();
                o.push('\n');
                o.push_str(&m.replacen("# ", "### ", 1));
            }
        }
    }
    if o.len() > MAX_BODY + 4096 {
        let mut cut = MAX_BODY + 4096;
        while !o.is_char_boundary(cut) {
            cut -= 1;
        }
        o.truncate(cut);
        o.push_str("\n… truncated\n");
    }
    o
}

pub fn render_skill(c: &Connection, env: &Env, bin: &Path) -> String {
    format!("---\nname: workmap\ndescription: {}\n---\n\n{}", yaml_quote(&description(c)), skill_body(c, env, bin))
}

/// After a scan: rewrite workmap-owned SKILL.md files only (the stable skill and
/// plain provider copies). Never creates registrations, copies binaries or touches systemd.
pub fn refresh_skills(env: &Env, c: &Connection) -> std::io::Result<Vec<String>> {
    let mut report = Vec::new();
    let bin = stable_bin(env);
    let stable = stable_skill_dir(env).join("SKILL.md");
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    if std::fs::symlink_metadata(&stable).is_ok() {
        targets.push(("stable".into(), stable.clone()));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (provider, catalog, _) in skill_dirs(env) {
        let entry = catalog.join("workmap");
        let skill = entry.join("SKILL.md");
        if std::fs::symlink_metadata(&entry).is_err() {
            continue; // not installed there: nothing to refresh
        }
        if is_symlink(&entry) {
            let to = std::fs::canonicalize(&entry).ok();
            if to.is_some() && to == std::fs::canonicalize(stable_skill_dir(env)).ok() {
                report.push(format!("linked {provider} skill {} -> {}", entry.display(), stable_skill_dir(env).display()));
            } else if !seen.contains(&to) && to.is_some() && read_owned(&skill) {
                // Older registration linking to another owned copy: that copy is refreshed on its own.
                report.push(format!("linked {provider} skill {} -> {} (run workmap install to point it at the stable skill)", entry.display(), to.as_ref().unwrap().display()));
            } else if to.is_none() {
                report.push(format!("skipped {provider} skill refresh: {} is a dangling link (run workmap install)", entry.display()));
            } else {
                report.push(format!("skipped {provider} skill refresh: {} not generated by workmap", entry.display()));
            }
            seen.insert(to);
            continue;
        }
        let canon = std::fs::canonicalize(&skill).ok();
        if !seen.insert(canon) {
            continue;
        }
        if !read_owned(&skill) {
            report.push(format!("skipped {provider} skill refresh: {} not generated by workmap", skill.display()));
            continue;
        }
        targets.push((provider.to_string(), skill));
    }
    if !targets.is_empty() && !bin.is_file() {
        report.push(format!("skipped skill refresh: {} missing (run workmap install)", bin.display()));
        return Ok(report);
    }
    let body = render_skill(c, env, &bin);
    for (who, skill) in targets {
        if !read_owned(&skill) {
            report.push(format!("skipped {who} skill refresh: {} not generated by workmap", skill.display()));
            continue;
        }
        let changed = write_if_changed(&skill, body.as_bytes())?;
        report.push(format!("{} {who} skill {}", if changed { "refreshed" } else { "unchanged" }, skill.display()));
    }
    Ok(report)
}

fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Commands the scan service needs, resolved where `install` runs (the user's
/// PATH): systemd user managers often have a minimal PATH without git.
pub struct Tools {
    pub git: Option<PathBuf>,
    pub path: String,
}

pub fn resolve_tools(env: &Env, path: Option<&std::ffi::OsStr>) -> Tools {
    let explicit = std::env::var_os("WORKMAP_GIT_BIN").filter(|v| !v.is_empty()).map(PathBuf::from);
    let git = explicit.or_else(|| crate::util::find_on_path("git", path));
    let tmp = std::env::temp_dir();
    // Tool dirs first, then the standard ones in a fixed order (always present).
    let standard: Vec<PathBuf> = ["/usr/local/bin", "/usr/bin", "/bin"].iter().map(PathBuf::from).collect();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut add = |p: Option<PathBuf>| {
        if let Some(d) = p.as_deref().filter(|p| p.is_absolute()).and_then(Path::parent) {
            let d = d.to_path_buf();
            if !d.starts_with(&tmp) && !dirs.contains(&d) && !standard.contains(&d) {
                dirs.push(d);
            }
        }
    };
    add(git.clone());
    for (bin, name) in [(&env.gh_bin, "gh"), (&env.glab_bin, "glab")] {
        add(if bin.is_absolute() { Some(bin.clone()) } else { crate::util::find_on_path(name, path) });
    }
    dirs.extend(standard.iter().cloned());
    let path = dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(":");
    Tools { git, path }
}

fn unit_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%")
}

pub fn units(env: &Env, bin: &Path, tools: &Tools) -> (String, String) {
    let mut vars = env.exports();
    vars.push(("PATH".into(), tools.path.clone()));
    if let Some(g) = &tools.git {
        vars.push(("WORKMAP_GIT_BIN".into(), g.display().to_string()));
    }
    let envs: String = vars.iter().map(|(k, v)| format!("Environment=\"{k}={}\"\n", unit_quote(v))).collect();
    let exec = bin.display().to_string();
    let exec = if exec.contains(' ') { format!("\"{}\"", unit_quote(&exec)) } else { exec };
    let service = format!("[Unit]\nDescription=workmap daily index scan\n\n[Service]\nType=oneshot\n{envs}ExecStart={exec} scan --quiet\nNice=10\nIOSchedulingClass=idle\n");
    let timer = "[Unit]\nDescription=Run workmap scan daily\n\n[Timer]\nOnCalendar=daily\nPersistent=true\nRandomizedDelaySec=15min\n\n[Install]\nWantedBy=timers.target\n".to_string();
    (service, timer)
}

/// `systemctl --user` with the session bus found even when the caller's
/// environment lacks it (agent shells, ssh without a login session).
fn systemctl(args: &[&str]) -> Option<(bool, String)> {
    let mut c = Command::new("systemctl");
    c.arg("--user").args(args);
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata("/proc/self").map(|m| m.uid()).ok()?;
        let run = PathBuf::from(format!("/run/user/{uid}"));
        if !run.is_dir() {
            return None;
        }
        c.env("XDG_RUNTIME_DIR", &run);
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() && run.join("bus").exists() {
            c.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", run.join("bus").display()));
        }
    }
    let o = c.stdin(std::process::Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&o.stdout).trim().to_string() + String::from_utf8_lossy(&o.stderr).trim();
    Some((o.status.success(), text))
}

fn bus_available() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some() || {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").map(|m| Path::new(&format!("/run/user/{}", m.uid())).join("bus").exists()).unwrap_or(false)
    }
}

/// Remove what `install` wrote: the stable binary and skill, provider
/// registrations (links before their target, so nothing dangles), owned
/// exclude entries, hooks and the user timer. The index (state dir) is kept.
pub fn uninstall(env: &Env) -> std::io::Result<Vec<String>> {
    let mut report = Vec::new();
    let unit_dir = env.home.join(".config/systemd/user");
    if !env.overridden && bus_available() && unit_dir.join("workmap-scan.timer").exists() {
        let ok = systemctl(&["disable", "--now", "workmap-scan.timer"]).map(|r| r.0).unwrap_or(false);
        report.push(format!("systemctl --user disable --now workmap-scan.timer: {}", if ok { "ok" } else { "FAILED" }));
    }
    for f in ["workmap-scan.service", "workmap-scan.timer"] {
        let p = unit_dir.join(f);
        if p.is_file() && std::fs::read_to_string(&p).map(|t| t.contains("workmap")).unwrap_or(false) {
            std::fs::remove_file(&p)?;
            report.push(format!("removed {}", p.display()));
        }
    }
    if unit_dir.join("workmap-scan.service.d").is_dir() {
        report.push(format!("kept your drop-in overrides in {}", unit_dir.join("workmap-scan.service.d").display()));
    }
    // Links first (they may point into owned real dirs or the stable skill).
    let dirs = skill_dirs(env);
    for (provider, catalog, _) in &dirs {
        let entry = catalog.join("workmap");
        if !is_symlink(&entry) {
            continue;
        }
        if links_to(&entry, &stable_skill_dir(env)) || owned_link(env, &entry) {
            std::fs::remove_file(&entry)?;
            remove_exclude(&env.home, &entry, &mut report);
            report.push(format!("removed {provider} skill link {}", entry.display()));
        } else {
            report.push(format!("left {provider} skill link {} alone: not generated by workmap", entry.display()));
        }
    }
    for (provider, catalog, _) in &dirs {
        let dir = catalog.join("workmap");
        if !dir.is_dir() || is_symlink(&dir) {
            continue;
        }
        if !read_owned(&dir.join("SKILL.md")) || !owned_dir_removable(&dir) {
            report.push(format!("left {} alone: not generated by workmap", dir.display()));
            continue;
        }
        remove_owned_dir(&dir)?;
        report.push(format!("removed {provider} skill {}", dir.display()));
    }
    let link = env.home.join(".local/bin/workmap");
    if is_symlink(&link) && std::fs::read_link(&link).map(|t| t == stable_bin(env)).unwrap_or(false) {
        std::fs::remove_file(&link)?;
        report.push(format!("removed {}", link.display()));
    }
    let share = share_dir(env);
    if share.is_dir() && !is_symlink(&share) {
        std::fs::remove_dir_all(&share)?;
        report.push(format!("removed {}", share.display()));
    }
    // User-scope host hooks; project-scope entries are listed by `workmap hooks status`.
    report.extend(crate::hook::uninstall_user(env));
    let _ = std::fs::remove_file(env.state_dir.join("installed"));
    report.push(format!("kept the index at {} — delete that directory to remove all indexed data", env.state_dir.display()));
    Ok(report)
}

/// Report lines starting with this are conflicts/failures; `workmap install` exits non-zero.
pub const ERROR: &str = "error: ";

pub fn install(env: &Env, c: &Connection, self_exe: &Path, opts: &InstallOpts) -> std::io::Result<Vec<String>> {
    let mut report = Vec::new();
    let bin = stable_bin(env);
    // Running the installed copy itself (upgrade in place) reads and writes the same bytes.
    let exe = std::fs::read(self_exe)?;
    let ch = write_if_changed(&bin, &exe)?;
    set_exec(&bin)?;
    report.push(format!("{} {}", if ch { "wrote" } else { "unchanged" }, bin.display()));
    let link = env.home.join(".local/bin/workmap");
    match std::fs::read_link(&link) {
        Ok(t) if t == bin => {}
        Ok(t) => report.push(format!("left {} -> {} alone (not the stable binary)", link.display(), t.display())),
        Err(_) if std::fs::symlink_metadata(&link).is_ok() => report.push(format!("left {} alone (a regular file)", link.display())),
        Err(_) => {
            std::fs::create_dir_all(link.parent().unwrap())?;
            std::os::unix::fs::symlink(&bin, &link)?;
            report.push(format!("linked {} -> {}", link.display(), bin.display()));
        }
    }

    let skill = render_skill(c, env, &bin);
    let ch = write_if_changed(&stable_skill_dir(env).join("SKILL.md"), skill.as_bytes())?;
    report.push(format!("{} stable skill {}", if ch { "wrote" } else { "unchanged" }, stable_skill_dir(env).join("SKILL.md").display()));
    let mut seen: Vec<(PathBuf, &str)> = Vec::new();
    for (provider, catalog, base) in skill_dirs(env) {
        // Claude and Codex may share one catalog (both linked to the same repo).
        let key = std::fs::canonicalize(&catalog).unwrap_or_else(|_| catalog.clone());
        if let Some((_, first)) = seen.iter().find(|(k, _)| *k == key) {
            report.push(format!("shared {provider} skill catalog {} (same directory as {first})", catalog.display()));
            continue;
        }
        let mut extra = Vec::new();
        match register(env, provider, &catalog, &base, &skill, &mut extra) {
            Ok(l) => {
                report.extend(extra);
                report.push(l);
            }
            Err(e) => {
                report.extend(extra);
                report.push(format!("{ERROR}{e}"));
            }
        }
        seen.push((std::fs::canonicalize(&catalog).unwrap_or(key), provider)); // created by register
    }

    if opts.hooks {
        for h in crate::hook::HARNESSES {
            let root = if *h == "codex" { env.codex_home() } else { env.home.join(".claude") };
            if !root.is_dir() {
                report.push(format!("skipped {h} hooks: {} does not exist ({h} not set up here)", root.display()));
                continue;
            }
            let cwd = env.home.clone();
            match crate::hook::install(env, &crate::hook::HooksOpts { harness: h, scope: crate::hook::Scope::User, cwd: &cwd, bin: &bin, dry_run: false }) {
                Ok(l) => report.extend(l.into_iter().map(|l| format!("{h} hooks: {l}"))),
                Err(e) => report.push(format!("{ERROR}{h} hooks: {e}")),
            }
        }
    } else {
        report.push("hooks not installed (--no-hooks); `workmap hooks install` adds them later".into());
    }

    let tools = resolve_tools(env, std::env::var_os("PATH").as_deref());
    match &tools.git {
        Some(g) => match crate::util::check_git(g) {
            Ok(v) => report.push(format!("git: {} ({v})", g.display())),
            Err(e) => report.push(format!("{ERROR}{e}")),
        },
        None => report.push(format!("{ERROR}git not found on PATH; scans need it (install git or set WORKMAP_GIT_BIN)")),
    }
    let unit_dir = env.home.join(".config/systemd/user");
    if let Some(why) = unsafe_target(&env.home, &env.home, &unit_dir).filter(|w| w.contains("symlink")) {
        report.push(format!("skipped systemd units: {why}"));
    } else {
        let (svc, tim) = units(env, &bin, &tools);
        let a = write_if_changed(&unit_dir.join("workmap-scan.service"), svc.as_bytes())?;
        let b = write_if_changed(&unit_dir.join("workmap-scan.timer"), tim.as_bytes())?;
        report.push(format!("{} systemd user units in {} (PATH={})", if a || b { "wrote" } else { "unchanged" }, unit_dir.display(), tools.path));
        let dropin = unit_dir.join("workmap-scan.service.d");
        if let Ok(rd) = std::fs::read_dir(&dropin) {
            let names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            report.push(format!("kept drop-in overrides {}: {} (they apply after the generated unit)", dropin.display(), names.join(", ")));
        }
        if opts.activate && !env.overridden && bus_available() {
            let r = systemctl(&["daemon-reload"]).map(|r| r.0).unwrap_or(false) && systemctl(&["enable", "--now", "workmap-scan.timer"]).map(|r| r.0).unwrap_or(false);
            report.push(format!("systemctl --user enable --now workmap-scan.timer: {}", if r { "ok" } else { "FAILED" }));
            if !r {
                report.push(format!("{ERROR}could not enable workmap-scan.timer"));
            }
            let enabled = systemctl(&["is-enabled", "workmap-scan.timer"]).map(|r| r.1).unwrap_or_default();
            let svc_state = systemctl(&["is-active", "workmap-scan.service"]).map(|r| r.1).unwrap_or_default();
            report.push(format!("timer: {enabled}; scan service now: {svc_state} (install never starts a scan)"));
        } else {
            report.push("timer not activated (fake home, --no-activate, or no user session bus)".into());
        }
    }
    std::fs::create_dir_all(&env.state_dir)?;
    write_if_changed(&env.state_dir.join("installed"), b"1\n")?;
    report.extend(verify(env));
    report.push(format!("next: register every project root outside {} with `workmap roots add <dir>`, then `workmap scan` (the skill lists this step for agents)", env.home.display()));
    Ok(report)
}

/// What a provider will actually read, checked from the filesystem after install.
pub fn verify(env: &Env) -> Vec<String> {
    let mut out = Vec::new();
    for (provider, catalog, _) in skill_dirs(env) {
        let skill = catalog.join("workmap/SKILL.md");
        let ok = read_owned(&skill);
        let shown = std::fs::canonicalize(&skill).map(|p| p.display().to_string()).unwrap_or_else(|_| "missing".into());
        let line = format!("verify: {provider} reads {} -> {shown}", skill.display());
        out.push(if ok { format!("{line} (workmap skill, ok)") } else { format!("{line} (NOT a workmap skill)") });
    }
    out.extend(crate::hook::trust_lines(env));
    out
}
