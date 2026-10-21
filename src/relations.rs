//! Evidence-backed relations between projects.
//!
//! Extraction (per main checkout, while indexing) records raw link candidates:
//! manifest path/Git dependencies (Cargo, package.json, pyproject), Git
//! submodules, symlinks pointing out of the checkout, doc lines naming another
//! repository's URL or path, declared package names, and Git blob ids of
//! substantive files. The relation pass resolves them against every indexed
//! project on each scan, adds observed cross-project conversation access, and
//! stores one edge per piece of evidence (`rel:<kind>`, class as strength).
//!
//! Each kind keeps its meaning: identical content is not lineage, a shared
//! conversation is not a dependency, and a name match is only a candidate.

use crate::db::{self, EdgeRow, RawLink};
use crate::overview::{get_node, nodes, Node};
use crate::scan::Matcher;
use crate::util::{clip, identity_candidates, normalize, remote_identity, resolve_against};
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

pub const ORIGIN: &str = "relations";

/// (kind, class, human label). Class is stored as the edge strength.
pub const KINDS: &[(&str, &str, &str)] = &[
    ("cargo_path", "declared", "Cargo path dependency"),
    ("cargo_git", "declared", "Cargo git dependency"),
    ("npm_path", "declared", "package.json local (file:/link:) dependency"),
    ("npm_git", "declared", "package.json git dependency"),
    ("py_path", "declared", "pyproject local path dependency"),
    ("py_git", "declared", "pyproject git dependency"),
    ("submodule", "declared", "Git submodule"),
    ("symlink", "structural", "symlink pointing into it"),
    ("doc_link", "documented", "doc names its URL or path"),
    ("conversation_access", "observed", "a conversation here read/wrote its files (co-use, not a dependency)"),
    ("identical_files", "hash", "identical file content (shared content, not lineage)"),
    ("same_upstream", "identity", "separate clone of the same remote repository"),
    ("name_match", "candidate", "UNVERIFIED: a registry dependency has the same name as a package declared there"),
];

pub fn label(kind: &str) -> &'static str {
    KINDS.iter().find(|k| k.0 == kind).map(|k| k.2).unwrap_or("related")
}

fn class(kind: &str) -> &'static str {
    KINDS.iter().find(|k| k.0 == kind).map(|k| k.1).unwrap_or("candidate")
}

const MAX_MANIFESTS: usize = 60;
const MAX_LINKS: usize = 400;
pub const MAX_HASHED: usize = 20_000;
pub const HASH_MIN: u64 = 1024;
pub const HASH_MAX: u64 = 4 << 20;
/// A blob present in more projects than this is boilerplate, not a signal.
pub const HASH_MAX_REPOS: i64 = 5;

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map(|i| i + 1).unwrap_or(1)
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches(|c| c == '"' || c == '\'')
}

/// Value of `key = "..."` inside an inline table or line.
fn toml_key(line: &str, key: &str) -> Option<String> {
    let mut rest = line;
    while let Some(i) = rest.find(key) {
        let before_ok = i == 0 || matches!(rest.as_bytes()[i - 1], b' ' | b'{' | b',' | b'\t');
        let after = rest[i + key.len()..].trim_start();
        if before_ok && after.starts_with('=') {
            let v = after[1..].trim_start();
            let q = v.chars().next()?;
            if q == '"' || q == '\'' {
                let end = v[1..].find(q)?;
                return Some(v[1..1 + end].to_string());
            }
        }
        rest = &rest[i + key.len()..];
    }
    None
}

/// What one manifest declares: (kind, raw, line).
fn parse_cargo(text: &str) -> Vec<(&'static str, String, usize)> {
    let mut out = Vec::new();
    let mut table = String::new();
    let mut table_dep: Option<(String, usize)> = None; // [dependencies.foo]
    let mut table_dep_found = false;
    let is_dep_table = |t: &str| t.ends_with("dependencies") || t.contains("dependencies.");
    for (i, raw) in text.lines().enumerate() {
        let l = raw.split(" #").next().unwrap_or(raw).trim();
        if l.starts_with('[') {
            if let (Some((name, line)), false) = (&table_dep, table_dep_found) {
                out.push(("dep_name", name.clone(), *line));
            }
            table = l.trim_matches(|c| c == '[' || c == ']').trim().to_string();
            table_dep = None;
            table_dep_found = false;
            if let Some((t, name)) = table.rsplit_once('.') {
                if t.ends_with("dependencies") {
                    table_dep = Some((unquote(name).to_string(), i + 1));
                }
            }
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let k = unquote(k);
        if table == "package" && k == "name" {
            out.push(("pkg_name", unquote(v).to_string(), i + 1));
        } else if table_dep.is_some() {
            if k == "path" {
                out.push(("cargo_path", unquote(v).to_string(), i + 1));
                table_dep_found = true;
            } else if k == "git" {
                out.push(("cargo_git", unquote(v).to_string(), i + 1));
                table_dep_found = true;
            }
        } else if is_dep_table(&table) {
            if let Some(p) = toml_key(v, "path") {
                out.push(("cargo_path", p, i + 1));
            } else if let Some(g) = toml_key(v, "git") {
                out.push(("cargo_git", g, i + 1));
            } else if toml_key(v, "workspace").is_none() && !v.contains("workspace = true") {
                out.push(("dep_name", toml_key(v, "package").unwrap_or_else(|| k.to_string()), i + 1));
            }
        }
    }
    if let (Some((name, line)), false) = (table_dep, table_dep_found) {
        out.push(("dep_name", name, line));
    }
    out
}

fn npm_spec(spec: &str) -> Option<(&'static str, String)> {
    for p in ["file:", "link:", "portal:"] {
        if let Some(x) = spec.strip_prefix(p) {
            return Some(("npm_path", x.to_string()));
        }
    }
    if spec.starts_with("workspace:") || spec.starts_with("npm:") {
        return None;
    }
    if let Some(x) = spec.strip_prefix("github:") {
        return Some(("npm_git", format!("https://github.com/{}", x.split('#').next().unwrap_or(x))));
    }
    if let Some(x) = spec.strip_prefix("gitlab:") {
        return Some(("npm_git", format!("https://gitlab.com/{}", x.split('#').next().unwrap_or(x))));
    }
    if spec.starts_with("git") || spec.contains("://") {
        return Some(("npm_git", spec.split('#').next().unwrap_or(spec).to_string()));
    }
    let core = spec.split('#').next().unwrap_or(spec);
    let parts: Vec<&str> = core.split('/').collect();
    if parts.len() == 2 && !spec.starts_with('@') && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))) {
        return Some(("npm_git", format!("https://github.com/{core}")));
    }
    if spec.starts_with('.') || spec.starts_with('/') {
        return Some(("npm_path", spec.to_string()));
    }
    None
}

fn parse_package_json(text: &str) -> Vec<(&'static str, String, usize)> {
    let mut out = Vec::new();
    let Ok(j) = serde_json::from_str::<serde_json::Value>(text) else { return out };
    if let Some(n) = j.get("name").and_then(|x| x.as_str()) {
        out.push(("pkg_name", n.to_string(), line_of(text, "\"name\"")));
    }
    for table in ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"] {
        let Some(m) = j.get(table).and_then(|x| x.as_object()) else { continue };
        for (name, spec) in m {
            let Some(spec) = spec.as_str() else { continue };
            let line = line_of(text, &format!("\"{name}\""));
            match npm_spec(spec) {
                Some((k, raw)) => out.push((k, raw, line)),
                None if !spec.starts_with("workspace:") => out.push(("dep_name", name.clone(), line)),
                None => {}
            }
        }
    }
    out
}

fn pep508_name(s: &str) -> String {
    s.chars().take_while(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect()
}

fn parse_pyproject(text: &str) -> Vec<(&'static str, String, usize)> {
    let mut out = Vec::new();
    let mut table = String::new();
    let mut in_deps = false;
    for (i, raw) in text.lines().enumerate() {
        let l = raw.split(" #").next().unwrap_or(raw).trim();
        if l.starts_with('[') && !in_deps {
            table = l.trim_matches(|c| c == '[' || c == ']').trim().to_string();
            continue;
        }
        let mut items: Vec<String> = Vec::new();
        if in_deps {
            if l.starts_with(']') {
                in_deps = false;
                continue;
            }
            items.push(l.to_string());
        } else if let Some((k, v)) = l.split_once('=') {
            let k = unquote(k);
            if (table == "project" || table == "tool.poetry") && k == "name" {
                out.push(("pkg_name", unquote(v).to_string(), i + 1));
            } else if table == "project" && (k == "dependencies") {
                let v = v.trim();
                if v.starts_with('[') {
                    let inner = v.trim_start_matches('[');
                    if inner.contains(']') {
                        items.push(inner.trim_end_matches(']').to_string());
                    } else {
                        in_deps = true;
                        items.push(inner.to_string());
                    }
                }
            } else if table.ends_with("dependencies") || table == "tool.uv.sources" {
                if let Some(p) = toml_key(v, "path") {
                    out.push(("py_path", p, i + 1));
                } else if let Some(g) = toml_key(v, "git") {
                    out.push(("py_git", g, i + 1));
                } else if k != "python" && table.ends_with("dependencies") {
                    out.push(("dep_name", k.to_string(), i + 1));
                }
            }
        }
        for item in items {
            for spec in item.split(',') {
                let spec = unquote(spec.trim());
                if spec.is_empty() {
                    continue;
                }
                if let Some((_, url)) = spec.split_once(" @ ") {
                    let url = url.trim();
                    if let Some(p) = url.strip_prefix("file://") {
                        out.push(("py_path", p.to_string(), i + 1));
                    } else {
                        out.push(("py_git", url.to_string(), i + 1));
                    }
                } else {
                    let n = pep508_name(spec);
                    if !n.is_empty() {
                        out.push(("dep_name", n, i + 1));
                    }
                }
            }
        }
    }
    out
}

fn parse_gitmodules(text: &str) -> Vec<(&'static str, String, usize)> {
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let (k, v) = l.trim().split_once('=')?;
            (k.trim() == "url").then(|| ("submodule", v.trim().to_string(), i + 1))
        })
        .collect()
}

/// Build output, lockfiles and licence boilerplate never count as shared content.
pub fn hash_excluded(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let upper = name.to_ascii_uppercase();
    let dir_skip = ["node_modules/", "vendor/", "dist/", "build/", "third_party/", "target/", ".git/"];
    upper.starts_with("LICENSE")
        || upper.starts_with("LICENCE")
        || upper.starts_with("COPYING")
        || upper.starts_with("CODE_OF_CONDUCT")
        || name.starts_with(".git")
        || name.ends_with(".lock")
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
        || name.ends_with(".min.js")
        || name.ends_with(".map")
        || name == "go.sum"
        || dir_skip.iter().any(|d| rel.starts_with(d) || rel.contains(&format!("/{d}")))
}

/// Record raw relation candidates of one (main) checkout. Bounded: manifests,
/// links and hashed files are capped; only `stat` is used for sizes.
pub fn extract_checkout(tx: &Connection, top: &Path, repo_id: &str, co_id: &str, staged: &[(String, String, String)]) -> rusqlite::Result<()> {
    let mut n_links = 0usize;
    let mut add = |kind: &str, raw: &str, resolved: &str, src: &Path, line: usize, note: &str| -> rusqlite::Result<()> {
        if n_links >= MAX_LINKS {
            return Ok(());
        }
        n_links += 1;
        db::add_raw_link(tx, &RawLink { origin: co_id, repo_id, checkout_id: co_id, kind, raw, resolved, source_path: &src.to_string_lossy(), line: line as i64, note })
    };
    let mut manifests = 0usize;
    for (mode, _, rel) in staged {
        if !mode.starts_with("100") {
            continue;
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let depth = rel.matches('/').count();
        let parser: fn(&str) -> Vec<(&'static str, String, usize)> = match name {
            "Cargo.toml" => parse_cargo,
            "package.json" => parse_package_json,
            "pyproject.toml" => parse_pyproject,
            ".gitmodules" if depth == 0 => parse_gitmodules,
            _ => continue,
        };
        if depth > 3 || hash_excluded(rel) || manifests >= MAX_MANIFESTS {
            continue;
        }
        manifests += 1;
        let abs = top.join(rel);
        let Ok(bytes) = crate::util::read_head(&abs, 512 << 10) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let dir = abs.parent().unwrap_or(top).to_path_buf();
        for (kind, raw, line) in parser(&text) {
            let resolved = if kind.ends_with("_path") { resolve_against(&dir, &raw).to_string_lossy().into_owned() } else { String::new() };
            add(kind, &raw, &resolved, &abs, line, name)?;
        }
    }
    // Symlinks: tracked (mode 120000) anywhere, plus untracked ones at the top level.
    let mut links: Vec<PathBuf> = staged.iter().filter(|(m, _, _)| m == "120000").map(|(_, _, r)| top.join(r)).take(200).collect();
    if let Ok(rd) = std::fs::read_dir(top) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) && !links.contains(&e.path()) {
                links.push(e.path());
            }
        }
    }
    links.sort();
    for l in links {
        let Ok(target) = std::fs::read_link(&l) else { continue };
        let abs = normalize(&l.parent().unwrap_or(top).join(&target));
        if abs.starts_with(top) {
            continue;
        }
        let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
        add("symlink", &target.to_string_lossy(), &abs.to_string_lossy(), &l, 0, "symlink")?;
    }
    // Identical-content candidates from Git blob ids (no hashing of our own).
    let mut hashed = 0usize;
    for (mode, blob, rel) in staged {
        if hashed >= MAX_HASHED {
            break;
        }
        if !(mode == "100644" || mode == "100755") || hash_excluded(rel) {
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(top.join(rel)) else { continue };
        if !md.is_file() || md.len() < HASH_MIN || md.len() > HASH_MAX {
            continue;
        }
        hashed += 1;
        tx.execute("INSERT OR REPLACE INTO file_hashes(checkout_id,repo_id,rel,blob,size) VALUES(?1,?2,?3,?4,?5)", params![co_id, repo_id, rel, blob, md.len() as i64])?;
    }
    Ok(())
}

/// Doc lines naming a URL or a path outside the checkout (main checkout docs).
pub fn extract_doc_links(tx: &Connection, home: &Path, doc: &Path, text: &str, top: &Path, repo_id: &str, co_id: &str) -> rusqlite::Result<()> {
    let dir = doc.parent().unwrap_or(top);
    for (line, raw, kind) in crate::docs::outside_links(text, 100) {
        let resolved = if kind == "doc_path" {
            let p = if let Some(r) = raw.strip_prefix("~/") { home.join(r) } else { resolve_against(dir, &raw) };
            if p.starts_with(top) {
                continue;
            }
            p.to_string_lossy().into_owned()
        } else {
            String::new()
        };
        db::add_raw_link(tx, &RawLink { origin: co_id, repo_id, checkout_id: co_id, kind, raw: &raw, resolved: &resolved, source_path: &doc.to_string_lossy(), line: line as i64, note: "doc" })?;
    }
    Ok(())
}

/// Normalized remote identities of a repository node (from `git remote -v`).
pub fn repo_identities(repo: &Node) -> Vec<String> {
    let mut v: Vec<String> =
        repo.meta.get("remotes").and_then(|r| r.as_array()).map(|a| a.iter().filter_map(|r| r.get("url").and_then(|u| u.as_str())).filter_map(remote_identity).collect()).unwrap_or_default();
    v.sort();
    v.dedup();
    v
}

/// `../x.git` style submodule URLs are relative to the superproject's remote.
fn relative_url_identity(base: &str, raw: &str) -> Option<String> {
    let mut segs: Vec<&str> = base.split('/').collect();
    for part in raw.trim_end_matches(".git").split('/') {
        match part {
            ".." => {
                segs.pop();
            }
            "." | "" => {}
            p => segs.push(p),
        }
    }
    (segs.len() >= 3).then(|| segs.join("/").to_lowercase())
}

struct Ev {
    src: String,
    dst: String,
    kind: &'static str,
    source_path: String,
    line: i64,
    note: String,
}

/// Recompute all relation edges from raw links, hashes, sessions and identities.
pub fn resolve(tx: &Connection, matcher: &mut Matcher) -> rusqlite::Result<usize> {
    tx.execute("DELETE FROM edges WHERE origin=?1", [ORIGIN])?;
    let repos = nodes(tx, "kind='repo' ORDER BY id", &[])?;
    let mut by_identity: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ids_of: HashMap<String, Vec<String>> = HashMap::new();
    for r in &repos {
        let ids = repo_identities(r);
        for i in &ids {
            by_identity.entry(i.clone()).or_default().push(r.id.clone());
        }
        ids_of.insert(r.id.clone(), ids);
    }
    let remote_only: HashMap<String, String> = {
        let mut st = tx.prepare("SELECT json_extract(meta,'$.identity'), id FROM nodes WHERE kind='remote'")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.flatten().collect()
    };
    let target_of = |identity: &str, src: &str| -> Vec<String> {
        if let Some(v) = by_identity.get(identity) {
            return v.iter().filter(|x| *x != src).cloned().collect();
        }
        remote_only.get(identity).map(|x| vec![x.clone()]).unwrap_or_default()
    };
    let mut evs: Vec<Ev> = Vec::new();
    let raw: Vec<(String, String, String, String, String, i64, String)> = {
        let mut st = tx.prepare("SELECT repo_id, kind, raw, resolved, source_path, line, note FROM raw_links ORDER BY repo_id, source_path, line, raw")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut pkg: HashMap<String, Vec<(String, String, i64)>> = HashMap::new();
    for (repo, kind, rawv, _, sp, line, _) in &raw {
        if kind == "pkg_name" {
            pkg.entry(rawv.to_lowercase()).or_default().push((repo.clone(), sp.clone(), *line));
        }
    }
    for (repo, kind, rawv, resolved, sp, line, _) in &raw {
        let k: &'static str = match kind.as_str() {
            "cargo_path" => "cargo_path",
            "cargo_git" => "cargo_git",
            "npm_path" => "npm_path",
            "npm_git" => "npm_git",
            "py_path" => "py_path",
            "py_git" => "py_git",
            "submodule" => "submodule",
            "symlink" => "symlink",
            "doc_url" | "doc_path" => "doc_link",
            "dep_name" => "name_match",
            _ => continue,
        };
        let mut push = |dst: String, note: String| evs.push(Ev { src: repo.clone(), dst, kind: k, source_path: sp.clone(), line: *line, note });
        if !resolved.is_empty() {
            if let Some((i, _)) = matcher.locate(Path::new(resolved)) {
                let dst = matcher.checkouts[i].repo_id.clone();
                if &dst != repo {
                    push(dst, format!("`{}`", clip(rawv, 120)));
                }
            }
            continue;
        }
        match kind.as_str() {
            "dep_name" => {
                for (other, osp, oline) in pkg.get(&rawv.to_lowercase()).into_iter().flatten() {
                    if other != repo {
                        push(other.clone(), format!("dependency `{}` = package declared at {}:{}", clip(rawv, 60), osp, oline));
                    }
                }
            }
            "submodule" if rawv.starts_with("../") || rawv.starts_with("./") => {
                for base in ids_of.get(repo).into_iter().flatten() {
                    if let Some(id) = relative_url_identity(base, rawv) {
                        for dst in target_of(&id, repo) {
                            push(dst, format!("`{}` relative to {base}", clip(rawv, 120)));
                        }
                    }
                }
            }
            "doc_url" => {
                // Longest identity prefix of the URL that names a known repository.
                if let Some(id) = identity_candidates(rawv).into_iter().find(|id| !target_of(id, repo).is_empty() || ids_of.get(repo).map(|v| v.contains(id)).unwrap_or(false)) {
                    for dst in target_of(&id, repo) {
                        push(dst, format!("`{}`", clip(rawv, 120)));
                    }
                }
            }
            _ => {
                if let Some(id) = remote_identity(rawv) {
                    for dst in target_of(&id, repo) {
                        push(dst, format!("`{}`", clip(rawv, 120)));
                    }
                }
            }
        }
    }
    // Separate clones of one remote.
    for (id, rs) in &by_identity {
        for (i, a) in rs.iter().enumerate() {
            for b in &rs[i + 1..] {
                if a != b {
                    evs.push(Ev { src: a.clone(), dst: b.clone(), kind: "same_upstream", source_path: String::new(), line: 0, note: format!("both have a remote for {id}") });
                }
            }
        }
    }
    // Identical substantive files (bounded by the per-checkout hash cap).
    {
        let mut st = tx.prepare(
            "WITH shared AS (SELECT blob FROM file_hashes GROUP BY blob HAVING count(DISTINCT repo_id) BETWEEN 2 AND ?1)
             SELECT a.repo_id, b.repo_id, count(DISTINCT a.blob), min(ca.path || '/' || a.rel), min(b.rel), max(a.size)
             FROM file_hashes a JOIN file_hashes b ON a.blob=b.blob AND a.repo_id < b.repo_id
             JOIN nodes ca ON ca.id=a.checkout_id
             WHERE a.blob IN (SELECT blob FROM shared) GROUP BY a.repo_id, b.repo_id ORDER BY a.repo_id, b.repo_id",
        )?;
        let rows: Vec<(String, String, i64, String, String, i64)> =
            st.query_map([HASH_MAX_REPOS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<Result<_, _>>()?;
        for (a, b, n, ex_a, ex_b, _) in rows {
            evs.push(Ev { src: a, dst: b, kind: "identical_files", source_path: ex_a, line: 0, note: format!("{n} identical file(s), e.g. same content as {ex_b}") });
        }
    }
    // Observed cross-project file access by conversations (up to 3 examples per pair).
    {
        let mut st = tx.prepare(
            "SELECT s.repo_id, f.repo_id, e.source_path, e.source_line, e.kind, f.path, s.label FROM edges e
             JOIN nodes s ON s.id=e.src AND s.kind='session' JOIN nodes f ON f.id=e.dst
             WHERE e.kind IN ('read','write') AND e.strength='observed' AND s.repo_id IS NOT NULL AND f.repo_id IS NOT NULL AND s.repo_id<>f.repo_id
             ORDER BY s.repo_id, f.repo_id, s.ts DESC, e.source_path, e.source_line",
        )?;
        let rows: Vec<(String, String, String, i64, String, String, String)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<Result<_, _>>()?;
        let mut per: HashMap<(String, String), usize> = HashMap::new();
        for (a, b, sp, line, kind, fpath, title) in rows {
            let n = per.entry((a.clone(), b.clone())).or_insert(0);
            *n += 1;
            if *n <= 3 {
                evs.push(Ev { src: a, dst: b, kind: "conversation_access", source_path: sp, line, note: format!("{kind} {fpath} in conversation \"{}\"", clip(&title, 60)) });
            }
        }
    }
    let mut seen = HashSet::new();
    let mut n = 0;
    for e in &evs {
        if !seen.insert((e.src.clone(), e.dst.clone(), e.kind, e.source_path.clone(), e.line)) {
            continue;
        }
        db::add_edge(
            tx,
            &EdgeRow { src: &e.src, dst: &e.dst, kind: &format!("rel:{}", e.kind), strength: class(e.kind), source_path: &e.source_path, source_line: e.line, ts: None, note: &e.note, origin: ORIGIN },
        )?;
        n += 1;
    }
    Ok(n)
}

// ------------------------------------------------------------------ queries

/// One piece of relation evidence seen from `from`.
#[derive(Clone, Debug)]
pub struct Rel {
    pub from: String,
    pub to: String,
    /// true when the stored edge points from `from` to `to`.
    pub forward: bool,
    pub kind: String,
    pub class: String,
    pub source_path: String,
    pub line: i64,
    pub note: String,
}

impl Rel {
    pub fn is_candidate(&self) -> bool {
        self.class == "candidate"
    }
}

pub fn neighbors(c: &Connection, node: &str) -> rusqlite::Result<Vec<Rel>> {
    let mut st = c.prepare(
        "SELECT src, dst, substr(kind,5), strength, source_path, source_line, note FROM edges WHERE kind LIKE 'rel:%' AND (src=?1 OR dst=?1)
         ORDER BY CASE strength WHEN 'declared' THEN 0 WHEN 'structural' THEN 1 WHEN 'documented' THEN 2 WHEN 'identity' THEN 3 WHEN 'observed' THEN 4 WHEN 'hash' THEN 5 ELSE 6 END, kind, src, dst, source_path, source_line",
    )?;
    let rows = st.query_map([node], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
    let mut v = Vec::new();
    for row in rows {
        let (s, d, kind, class, sp, line, note) = row?;
        let forward = s == node;
        v.push(Rel { from: node.to_string(), to: if forward { d } else { s }, forward, kind, class, source_path: sp, line, note });
    }
    Ok(v)
}

/// Breadth-first walk over non-candidate relations, bounded by depth and node
/// count; visited nodes are never re-entered, so cycles terminate.
/// Returns `node -> (hops, path of relations from start)` in discovery order.
pub fn bfs(c: &Connection, start: &str, depth: usize, max_nodes: usize) -> rusqlite::Result<Vec<(String, Vec<Rel>)>> {
    let mut out: Vec<(String, Vec<Rel>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([start.to_string()]);
    let mut q: VecDeque<(String, Vec<Rel>)> = VecDeque::from([(start.to_string(), vec![])]);
    while let Some((n, path)) = q.pop_front() {
        if path.len() >= depth {
            continue;
        }
        let mut next: BTreeMap<String, Rel> = BTreeMap::new();
        for r in neighbors(c, &n)? {
            if r.is_candidate() || seen.contains(&r.to) {
                continue;
            }
            next.entry(r.to.clone()).or_insert(r);
        }
        for (to, r) in next {
            if out.len() >= max_nodes {
                return Ok(out);
            }
            seen.insert(to.clone());
            let mut p = path.clone();
            p.push(r);
            out.push((to.clone(), p.clone()));
            q.push_back((to, p));
        }
    }
    Ok(out)
}

/// Shortest explanation chain from `a` to `b` (non-candidate relations only).
pub fn chain(c: &Connection, a: &str, b: &str, depth: usize) -> rusqlite::Result<Option<Vec<Rel>>> {
    Ok(bfs(c, a, depth, 5000)?.into_iter().find(|(n, _)| n == b).map(|x| x.1))
}

/// All evidence between two nodes, non-candidates first.
pub fn between(c: &Connection, a: &str, b: &str) -> rusqlite::Result<Vec<Rel>> {
    let mut v: Vec<Rel> = neighbors(c, a)?.into_iter().filter(|r| r.to == b).collect();
    v.sort_by_key(|r| r.is_candidate());
    Ok(v)
}

/// Printable name of a relation endpoint: project handle or `remote:<identity>`.
pub fn node_name(c: &Connection, id: &str) -> String {
    match get_node(c, id).ok().flatten() {
        Some(n) if n.kind == "repo" => crate::view::project_handle(c, &n).unwrap_or(n.label),
        Some(n) if n.kind == "remote" => format!("remote:{}", n.label),
        Some(n) => n.label,
        None => id.to_string(),
    }
}

/// One evidence line: what, where (path:line) and, for manifest/doc/symlink
/// evidence, the exact source line read now (bounded).
pub fn evidence_line(c: &Connection, r: &Rel) -> String {
    let arrow = if r.forward { "→" } else { "←" };
    let mut s = format!("{arrow} {} [{}]", label(&r.kind), r.class);
    if !r.source_path.is_empty() {
        s.push_str(&format!(" at {}{}", r.source_path, if r.line > 0 { format!(":{}", r.line) } else { String::new() }));
    }
    let quote = matches!(r.class.as_str(), "declared" | "documented" | "candidate") && r.line > 0;
    match quote.then(|| crate::view::line_text(&r.source_path, r.line as usize, 140)).flatten() {
        Some(t) => s.push_str(&format!(": \"{t}\"")),
        None if !r.note.is_empty() => s.push_str(&format!(" — {}", clip(&r.note, 160))),
        None => {}
    }
    let _ = c;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_declare_paths_gits_and_names() {
        let cargo = "[package]\nname = \"app\"\n\n[dependencies]\nserde = \"1\"\nlib = { path = \"../lib\" }\nremote = { git = \"https://github.com/acme/remote\" }\n\n[dependencies.tbl]\npath = \"../tbl\"\n\n[dev-dependencies.only_name]\nversion = \"2\"\n";
        let v = parse_cargo(cargo);
        assert!(v.contains(&("pkg_name", "app".into(), 2)));
        assert!(v.contains(&("dep_name", "serde".into(), 5)));
        assert!(v.contains(&("cargo_path", "../lib".into(), 6)));
        assert!(v.contains(&("cargo_git", "https://github.com/acme/remote".into(), 7)));
        assert!(v.contains(&("cargo_path", "../tbl".into(), 10)));
        assert!(v.contains(&("dep_name", "only_name".into(), 12)), "{v:?}");
        let pj = "{\n  \"name\": \"web\",\n  \"dependencies\": {\n    \"lib\": \"file:../lib\",\n    \"x\": \"github:acme/x#main\",\n    \"left-pad\": \"^1\",\n    \"ws\": \"workspace:*\"\n  }\n}";
        let v = parse_package_json(pj);
        assert!(v.contains(&("npm_path", "../lib".into(), 4)) && v.contains(&("npm_git", "https://github.com/acme/x".into(), 5)) && v.contains(&("dep_name", "left-pad".into(), 6)));
        assert!(!v.iter().any(|x| x.1 == "ws"));
        let py = "[project]\nname = \"tool\"\ndependencies = [\n  \"requests>=2\",\n  \"lib @ file:///opt/lib\",\n  \"x @ git+https://gitlab.com/g/x.git\",\n]\n[tool.uv.sources]\nother = { path = \"../other\" }\n";
        let v = parse_pyproject(py);
        assert!(v.contains(&("pkg_name", "tool".into(), 2)) && v.contains(&("dep_name", "requests".into(), 4)));
        assert!(v.contains(&("py_path", "/opt/lib".into(), 5)) && v.contains(&("py_git", "git+https://gitlab.com/g/x.git".into(), 6)) && v.contains(&("py_path", "../other".into(), 9)), "{v:?}");
        assert_eq!(relative_url_identity("github.com/acme/app", "../lib.git").as_deref(), Some("github.com/acme/lib"));
        assert!(hash_excluded("LICENSE") && hash_excluded("web/node_modules/x/a.js") && hash_excluded("Cargo.lock") && !hash_excluded("src/util.rs"));
    }
}
