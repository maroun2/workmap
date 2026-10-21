//! Remote catalog: repositories you can access on GitHub/GitLab hosts where the
//! `gh` / `glab` CLI is already authenticated, including self-hosted hosts in
//! their config. Metadata only (name, URL, description, visibility, archived,
//! default branch, last update). Nothing is cloned or fetched, no login is
//! attempted, and the CLIs' stderr (which can echo tokens) is never read.
//!
//! Scope: GitHub `user/repos` with owner, collaborator and organization-member
//! affiliation; GitLab `projects?membership=true`. Not every public repository.

use crate::config::Env;
use crate::db;
use crate::scan::Stats;
use crate::util::{now_unix, remote_identity, run_status, stable_id};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Never call gh/glab; cached rows stay as they are.
    Off,
    /// Refresh hosts whose catalog is older than the TTL.
    Auto,
    /// Refresh every authenticated host now.
    Force,
}

pub const ORIGIN: &str = "remote-catalog";
const AUTH_T: Duration = Duration::from_secs(20);
const API_T: Duration = Duration::from_secs(180);
const MAX_PER_HOST: usize = 20_000;

/// Opt-out: `WORKMAP_REMOTE=0|off|false|no` or `<state>/no-remote`.
pub fn disabled(env: &Env) -> bool {
    let off = std::env::var("WORKMAP_REMOTE").map(|v| matches!(v.to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no")).unwrap_or(false);
    off || env.state_dir.join("no-remote").exists()
}

pub fn mode(env: &Env, no_remote: bool, refresh: bool) -> Mode {
    if no_remote || disabled(env) {
        Mode::Off
    } else if refresh {
        Mode::Force
    } else {
        Mode::Auto
    }
}

pub fn ttl_secs() -> i64 {
    std::env::var("WORKMAP_REMOTE_TTL_HOURS").ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or(24) * 3600
}

/// Top-level keys of gh's `hosts.yml`.
pub fn gh_hosts(cfg_dir: &Path) -> Vec<String> {
    let txt = std::fs::read_to_string(cfg_dir.join("hosts.yml")).unwrap_or_default();
    txt.lines()
        .filter(|l| !l.starts_with([' ', '\t', '#', '-']) && l.trim_end().ends_with(':'))
        .map(|l| l.trim_end().trim_end_matches(':').trim_matches('"').to_string())
        .filter(|h| h.contains('.'))
        .collect()
}

/// Keys directly under `hosts:` in glab's `config.yml`.
pub fn glab_hosts(cfg_dir: &Path) -> Vec<String> {
    let txt = std::fs::read_to_string(cfg_dir.join("config.yml")).unwrap_or_default();
    let mut out = Vec::new();
    let mut inside = false;
    let mut indent: Option<usize> = None;
    for l in txt.lines() {
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let ind = l.len() - l.trim_start().len();
        if ind == 0 {
            inside = l.trim_end() == "hosts:";
            continue;
        }
        if !inside {
            continue;
        }
        let i = *indent.get_or_insert(ind);
        if ind == i && l.trim_end().ends_with(':') {
            let h = l.trim().trim_end_matches(':').trim_matches('"').to_string();
            if h.contains('.') {
                out.push(h);
            }
        }
    }
    out
}

fn cli(env: &Env, provider: &str) -> Command {
    let mut c = Command::new(if provider == "gh" { &env.gh_bin } else { &env.glab_bin });
    // Same config the host list came from; never prompt, never self-update.
    c.env("GH_CONFIG_DIR", &env.gh_config)
        .env("GLAB_CONFIG_DIR", &env.glab_config)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GLAB_CHECK_UPDATE", "false")
        .env("NO_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    if env.overridden {
        // A fake home must not reach real accounts through tokens in the environment.
        for k in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GH_HOST", "GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN", "GITLAB_HOST"] {
            c.env_remove(k);
        }
    }
    c
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteRepo {
    pub identity: String,
    pub namespace: String,
    pub name: String,
    pub url: String,
    pub description: String,
    pub visibility: String,
    pub archived: bool,
    pub default_branch: String,
    pub updated_at: String,
}

fn st(v: &Value, k: &str) -> String {
    v.pointer(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Parse concatenated JSON pages (`[..][..]`, one array, or NDJSON objects).
pub fn parse_listing(provider: &str, host: &str, out: &str) -> Result<Vec<RemoteRepo>, String> {
    let mut v = Vec::new();
    for item in serde_json::Deserializer::from_str(out).into_iter::<Value>() {
        let item = item.map_err(|_| "unparseable output".to_string())?;
        let objs = match item {
            Value::Array(a) => a,
            o @ Value::Object(_) => vec![o],
            _ => continue,
        };
        for o in objs {
            let r = if provider == "gh" {
                let full = st(&o, "/full_name");
                let url = st(&o, "/html_url");
                let vis = match st(&o, "/visibility") {
                    s if !s.is_empty() => s,
                    _ if o.get("private").and_then(|x| x.as_bool()) == Some(true) => "private".into(),
                    _ => "public".into(),
                };
                let (ns, name) = full.rsplit_once('/').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or_default();
                RemoteRepo {
                    identity: remote_identity(&url).unwrap_or_else(|| format!("{host}/{full}").to_lowercase()),
                    namespace: ns,
                    name,
                    url,
                    description: st(&o, "/description"),
                    visibility: vis,
                    archived: o.get("archived").and_then(|x| x.as_bool()).unwrap_or(false),
                    default_branch: st(&o, "/default_branch"),
                    updated_at: st(&o, "/pushed_at").max(st(&o, "/updated_at")),
                }
            } else {
                let full = st(&o, "/path_with_namespace");
                let url = st(&o, "/web_url");
                let (ns, name) = full.rsplit_once('/').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or_default();
                RemoteRepo {
                    identity: remote_identity(&url).unwrap_or_else(|| format!("{host}/{full}").to_lowercase()),
                    namespace: ns,
                    name,
                    url,
                    description: st(&o, "/description"),
                    visibility: st(&o, "/visibility"),
                    archived: o.get("archived").and_then(|x| x.as_bool()).unwrap_or(false),
                    default_branch: st(&o, "/default_branch"),
                    updated_at: st(&o, "/last_activity_at"),
                }
            };
            if !r.name.is_empty() && v.len() < MAX_PER_HOST {
                v.push(r);
            }
        }
    }
    Ok(v)
}

fn set_host(tx: &Connection, provider: &str, host: &str, status: &str, fetched: Option<i64>, count: Option<i64>, msg: &str) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO remote_hosts(provider,host,status,fetched_at,checked_at,count,message) VALUES(?1,?2,?3,coalesce(?4,0),?5,coalesce(?6,0),?7)
         ON CONFLICT(provider,host) DO UPDATE SET status=excluded.status, checked_at=excluded.checked_at, message=excluded.message,
           fetched_at=coalesce(?4, remote_hosts.fetched_at), count=coalesce(?6, remote_hosts.count)",
        params![provider, host, status, fetched, now_unix(), count, msg],
    )?;
    Ok(())
}

/// Refresh the catalog (per mode), then rebuild remote-only nodes and passages.
pub fn sync(env: &Env, tx: &Connection, mode: Mode, stats: &mut Stats) -> rusqlite::Result<()> {
    if mode != Mode::Off {
        let mut hosts: Vec<(&'static str, String)> = gh_hosts(&env.gh_config).into_iter().map(|h| ("gh", h)).collect();
        hosts.extend(glab_hosts(&env.glab_config).into_iter().map(|h| ("glab", h)));
        hosts.extend(env.env_token_hosts.iter().cloned());
        hosts.sort();
        hosts.dedup();
        for (provider, host) in hosts {
            let prev: Option<(String, i64)> =
                tx.query_row("SELECT status, fetched_at FROM remote_hosts WHERE provider=?1 AND host=?2", params![provider, host], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if mode == Mode::Auto {
                if let Some((_, f)) = prev.as_ref().filter(|p| p.1 > 0 && now_unix() - p.1 < ttl_secs()) {
                    stats.remote.push(format!("{provider} {host}: cached (fetched {})", crate::util::fmt_ts(*f)));
                    continue;
                }
            }
            if let Err(e) = run_status(cli(env, provider).args(["auth", "status", "--hostname", &host]), AUTH_T) {
                let status = if e == "not installed" { "unavailable" } else { "unauthenticated" };
                set_host(tx, provider, &host, status, None, None, &format!("{provider} auth status: {e}"))?;
                stats.remote.push(format!("{provider} {host}: skipped, {provider} auth status {e} (cached entries kept, marked stale)"));
                continue;
            }
            let endpoint = if provider == "gh" {
                "user/repos?per_page=100&affiliation=owner,collaborator,organization_member&sort=full_name"
            } else {
                "projects?membership=true&per_page=100&order_by=id&sort=asc"
            };
            let res = run_status(cli(env, provider).args(["api", "--hostname", &host, "--paginate", endpoint]), API_T).and_then(|out| parse_listing(provider, &host, &out));
            match res {
                Ok(list) => {
                    tx.execute("DELETE FROM remote_repos WHERE provider=?1 AND host=?2", params![provider, host])?;
                    let now = now_unix();
                    for r in &list {
                        tx.execute(
                            "INSERT OR REPLACE INTO remote_repos(provider,host,identity,namespace,name,url,description,visibility,archived,default_branch,updated_at,fetched_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                            params![provider, host, r.identity, r.namespace, r.name, crate::git::strip_credentials(&r.url), crate::util::redact(&crate::util::clip(&r.description, 300)), r.visibility, r.archived, r.default_branch, r.updated_at, now],
                        )?;
                    }
                    set_host(tx, provider, &host, "ok", Some(now), Some(list.len() as i64), "")?;
                    stats.remote.push(format!("{provider} {host}: {} repositories", list.len()));
                }
                Err(e) => {
                    set_host(tx, provider, &host, "failed", None, None, &format!("{provider} api: {e}"))?;
                    stats.remote.push(format!("{provider} {host}: refresh failed ({provider} api {e}); cached entries kept, marked stale"));
                }
            }
        }
    }
    rebuild(tx)
}

/// Remote-only nodes (not cloned locally) and catalog metadata on cloned repos.
pub fn rebuild(tx: &Connection) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM passages WHERE origin=?1", [ORIGIN])?;
    tx.execute("DELETE FROM edges WHERE src IN (SELECT id FROM nodes WHERE kind='remote') OR dst IN (SELECT id FROM nodes WHERE kind='remote')", [])?;
    tx.execute("DELETE FROM nodes WHERE kind='remote'", [])?;
    tx.execute("UPDATE nodes SET meta=json_remove(meta,'$.catalog') WHERE kind='repo'", [])?;
    let mut local: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for r in crate::overview::nodes(tx, "kind='repo' ORDER BY id", &[])? {
        for id in crate::relations::repo_identities(&r) {
            local.entry(id).or_default().push(r.id.clone());
        }
    }
    let ttl = ttl_secs();
    let rows: Vec<(String, String, String, String, String, String, String, String, bool, String, String, i64, String)> = {
        let mut st = tx.prepare(
            "SELECT r.provider, r.host, r.identity, r.namespace, r.name, r.url, r.description, r.visibility, r.archived, r.default_branch, r.updated_at, r.fetched_at, h.status
             FROM remote_repos r LEFT JOIN remote_hosts h ON h.provider=r.provider AND h.host=r.host ORDER BY r.identity, r.provider",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get::<_, Option<String>>(12)?.unwrap_or_default(),
            ))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let mut done = std::collections::HashSet::new();
    for (provider, host, identity, ns, name, url, desc, vis, archived, branch, updated, fetched, status) in rows {
        if !done.insert(identity.clone()) {
            continue;
        }
        let stale = now_unix() - fetched > ttl || status != "ok";
        let meta = json!({"identity": identity, "provider": provider, "host": host, "namespace": ns, "name": name, "url": url, "description": desc,
            "visibility": vis, "archived": archived, "default_branch": branch, "updated_at": updated, "fetched_at": fetched, "stale": stale});
        if let Some(repos) = local.get(&identity) {
            for repo in repos {
                tx.execute("UPDATE nodes SET meta=json_set(meta,'$.catalog',json(?2)) WHERE id=?1", params![repo, meta.to_string()])?;
            }
            continue;
        }
        let id = stable_id("remote", &identity);
        let ts = crate::util::parse_iso(&updated);
        db::upsert_node(tx, &db::NodeRow { id: &id, kind: "remote", label: &identity, path: None, repo_id: None, checkout_id: None, ts, meta: &meta })?;
        let text = format!("{identity} {name} {desc}");
        db::add_passage(
            tx,
            &db::Passage {
                kind: "remote",
                node_id: &id,
                repo_id: None,
                checkout_id: None,
                source_path: "",
                line: 0,
                loc: "",
                part: 0,
                speaker: &provider,
                ts,
                title: &identity,
                heading: "",
                text: &text,
                origin: ORIGIN,
            },
        )?;
    }
    Ok(())
}

/// Delete every cached remote row and node (used when opting out).
pub fn clear(tx: &Connection) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM remote_repos", [])?;
    tx.execute("DELETE FROM remote_hosts", [])?;
    rebuild(tx)?;
    tx.execute("DELETE FROM edges WHERE kind LIKE 'rel:%' AND (dst LIKE 'remote-%' OR src LIKE 'remote-%')", [])?;
    Ok(())
}

/// `workmap remote status` text.
pub fn status(env: &Env, c: &Connection) -> rusqlite::Result<String> {
    let mut o = String::new();
    o.push_str(&format!(
        "remote discovery: {} (opt out: `workmap remote disable`, WORKMAP_REMOTE=0 or scan --no-remote); refresh after {}h\n",
        if disabled(env) { "DISABLED" } else { "enabled" },
        ttl_secs() / 3600
    ));
    o.push_str(&format!("gh: {} (hosts from {}/hosts.yml) · glab: {} (hosts from {}/config.yml)\n", env.gh_bin.display(), env.gh_config.display(), env.glab_bin.display(), env.glab_config.display()));
    let mut st = c.prepare("SELECT provider, host, status, fetched_at, checked_at, count, message FROM remote_hosts ORDER BY provider, host")?;
    let rows: Vec<(String, String, String, i64, i64, i64, String)> =
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<Result<_, _>>()?;
    if rows.is_empty() {
        o.push_str("no hosts checked yet (none configured, or discovery has not run)\n");
    }
    for (p, h, s, f, ch, n, m) in rows {
        let stale = s != "ok" || now_unix() - f > ttl_secs();
        o.push_str(&format!(
            "- {p} {h}: {s}; {n} repositories from {}{}; last check {}{}\n",
            if f > 0 { crate::util::fmt_ts(f) } else { "never".into() },
            if stale && f > 0 { " (STALE)" } else { "" },
            crate::util::fmt_ts(ch),
            if m.is_empty() { String::new() } else { format!(" — {m}") }
        ));
    }
    let cloned: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='repo' AND json_extract(meta,'$.catalog') IS NOT NULL", [], |r| r.get(0))?;
    let only: i64 = c.query_row("SELECT count(*) FROM nodes WHERE kind='remote'", [], |r| r.get(0))?;
    o.push_str(&format!("catalog: {only} remote-only (not cloned), {cloned} matched to local clones\n"));
    Ok(o)
}

/// Remote-only entries for listings: (identity, visibility, archived, description, stale).
pub fn remote_only(c: &Connection, limit: usize) -> rusqlite::Result<Vec<(String, String, bool, String, bool)>> {
    let mut st = c.prepare("SELECT label, meta FROM nodes WHERE kind='remote' ORDER BY ts DESC, label LIMIT ?1")?;
    let rows = st.query_map([limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut v = Vec::new();
    for row in rows {
        let (l, m) = row?;
        let m: Value = serde_json::from_str(&m).unwrap_or(Value::Null);
        v.push((l, st_v(&m, "visibility"), m.get("archived").and_then(|x| x.as_bool()).unwrap_or(false), st_v(&m, "description"), m.get("stale").and_then(|x| x.as_bool()).unwrap_or(false)));
    }
    Ok(v)
}

fn st_v(m: &Value, k: &str) -> String {
    m.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_configs_and_listings_parse() {
        let t = crate::testutil::TempDir::new("hosts");
        crate::testutil::write(&t.path().join("gh/hosts.yml"), "github.com:\n    user: someone\n    oauth_token: SECRET\ngit.example.com:\n    user: x\n");
        assert_eq!(gh_hosts(&t.path().join("gh")), vec!["github.com", "git.example.com"]);
        crate::testutil::write(
            &t.path().join("gl/config.yml"),
            "git_protocol: ssh\nhosts:\n    gitlab.com:\n        token: SECRET\n        api_host: gitlab.com\n    gitlab.example.org:\n        token: x\neditor: vi\n",
        );
        assert_eq!(glab_hosts(&t.path().join("gl")), vec!["gitlab.com", "gitlab.example.org"]);
        let pages = r#"[{"full_name":"acme/a","html_url":"https://github.com/acme/a","private":true,"archived":false,"default_branch":"main","updated_at":"2026-01-01T00:00:00Z","description":"A"}]
[{"full_name":"acme/b","html_url":"https://github.com/acme/b","visibility":"public","archived":true,"default_branch":"dev","updated_at":"2026-01-02T00:00:00Z","description":null}]"#;
        let v = parse_listing("gh", "github.com", pages).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].identity.as_str(), v[0].visibility.as_str()), ("github.com/acme/a", "private"));
        assert!(v[1].archived && v[1].default_branch == "dev");
        let gl = r#"[{"path_with_namespace":"grp/sub/c","web_url":"https://gitlab.example.org/grp/sub/c","visibility":"internal","archived":false,"default_branch":"main","last_activity_at":"2026-02-02T00:00:00Z","description":"C"}]"#;
        let v = parse_listing("glab", "gitlab.example.org", gl).unwrap();
        assert_eq!((v[0].identity.as_str(), v[0].namespace.as_str()), ("gitlab.example.org/grp/sub/c", "grp/sub"));
        assert!(parse_listing("gh", "github.com", "not json").is_err());
    }
}
