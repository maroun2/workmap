//! Markdown docs: headings, short excerpts with line refs, and local path references.
//! Manifests: declared entry points. Nothing here interprets meaning.

use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const DOC_NAMES: &[&str] = &["README.md", "README", "README.rst", "CLAUDE.md", "AGENTS.md", "GEMINI.md", "CONTRIBUTING.md", "ARCHITECTURE.md", "DESIGN.md", "TODO.md", "MEMORY.md", "CHANGELOG.md"];

#[derive(Debug, Clone)]
pub struct Excerpt {
    pub line: usize,
    pub heading: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocRef {
    pub line: usize,
    pub heading: String,
    pub raw: String,
    /// link | code | bare
    pub how: &'static str,
}

#[derive(Debug, Default)]
pub struct DocInfo {
    pub title: Option<String>,
    pub excerpts: Vec<Excerpt>,
    pub refs: Vec<DocRef>,
    pub headings: usize,
}

const EXTS: &[&str] = &[
    "md", "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "json", "toml", "yaml", "yml", "nix", "sh", "go", "c", "h", "cpp", "java", "kt", "swift", "rb", "php", "sql", "html", "css", "scss",
    "txt", "lock", "cfg", "ini", "conf", "svg", "png", "vue", "svelte",
];

fn looks_like_path(tok: &str) -> bool {
    if tok.len() < 3 || tok.len() > 200 || tok.contains("://") || tok.starts_with('@') || tok.contains(' ') {
        return false;
    }
    if tok.chars().any(|c| matches!(c, '=' | '{' | '}' | '$' | '|' | '<' | '>' | '*' | '?' | '\\' | ',')) {
        return false;
    }
    let last = tok.rsplit('/').next().unwrap_or(tok);
    let has_ext = last.rsplit_once('.').map(|(stem, e)| !e.is_empty() && EXTS.contains(&e.to_ascii_lowercase().as_str()) && !stem.is_empty()).unwrap_or(false);
    let dirish = tok.contains('/') && tok.chars().next().map(|c| c.is_ascii_alphanumeric() || c == '.' || c == '/' || c == '~').unwrap_or(false);
    has_ext || (dirish && !tok.starts_with("//"))
}

fn trim_tok(t: &str) -> &str {
    t.trim_matches(|c: char| matches!(c, '*' | '`' | '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';' | ':' | '!' | '.' | '<' | '>'))
}

/// Parse a Markdown document (already bounded by the caller).
pub fn analyze_markdown(text: &str, max_excerpts: usize, max_refs: usize) -> DocInfo {
    let mut info = DocInfo::default();
    let mut heading = String::new();
    let mut need_excerpt = true;
    let mut in_fence = false;
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    for (i, raw_line) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw_line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && line.starts_with('#') {
            let h = line.trim_start_matches('#').trim();
            if !h.is_empty() {
                info.headings += 1;
                heading = h.to_string();
                if info.title.is_none() {
                    info.title = Some(crate::util::clip(h, 100));
                }
                need_excerpt = true;
                continue;
            }
        }
        if !in_fence && need_excerpt && !line.is_empty() && !line.starts_with("<!--") && !line.starts_with('|') && !line.starts_with("---") {
            if info.excerpts.len() < max_excerpts {
                info.excerpts.push(Excerpt { line: line_no, heading: heading.clone(), text: crate::util::redact(&crate::util::clip(line, 220)) });
            }
            need_excerpt = false;
        }
        if info.refs.len() >= max_refs {
            continue;
        }
        let mut push = |raw: &str, how: &'static str| {
            let raw = raw.split('#').next().unwrap_or("").trim();
            if raw.is_empty() || raw.starts_with("http") || raw.starts_with("mailto:") || raw.starts_with('#') {
                return;
            }
            if seen.insert((raw.to_string(), line_no)) {
                info.refs.push(DocRef { line: line_no, heading: heading.clone(), raw: raw.to_string(), how });
            }
        };
        // Markdown links: ](target)
        let mut rest = raw_line;
        while let Some(i) = rest.find("](") {
            let after = &rest[i + 2..];
            if let Some(j) = after.find(')') {
                let target = after[..j].split_whitespace().next().unwrap_or("");
                push(target, "link");
                rest = &after[j..];
            } else {
                break;
            }
        }
        // Inline code spans.
        let parts: Vec<&str> = raw_line.split('`').collect();
        for (k, p) in parts.iter().enumerate() {
            if k % 2 == 1 && looks_like_path(p.trim()) {
                push(p.trim(), "code");
            }
        }
        // Bare tokens (bold names, prose, fenced commands).
        for (k, p) in parts.iter().enumerate() {
            if k % 2 == 1 {
                continue;
            }
            for tok in p.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '[' || c == ']') {
                let t = trim_tok(tok);
                if looks_like_path(t) && !t.contains("](") {
                    push(t, "bare");
                }
            }
        }
    }
    info
}

/// A Markdown heading: line, level, text, and its GitHub-style anchor
/// (repeated headings get `-1`, `-2`, …).
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub line: usize,
    pub level: usize,
    pub heading: String,
    pub slug: String,
}

/// Headings outside fenced code, with GitHub duplicate suffixes.
pub fn sections(text: &str) -> Vec<Section> {
    let mut out = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut fence = false;
    for (i, l) in text.lines().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence || !t.starts_with('#') {
            continue;
        }
        let level = t.chars().take_while(|c| *c == '#').count();
        let h = t[level..].trim();
        if level > 6 || h.is_empty() || !t[level..].starts_with(' ') {
            continue;
        }
        let base = crate::util::slug(h);
        let n = seen.entry(base.clone()).or_insert(0);
        let slug = if *n == 0 { base.clone() } else { format!("{base}-{n}") };
        *n += 1;
        out.push(Section { line: i + 1, level, heading: h.to_string(), slug });
    }
    out
}

/// Link candidates pointing outside the document's repository: web/Git URLs
/// (`doc_url`) and absolute, home-relative or parent-relative paths (`doc_path`).
/// Returns `(line, raw token, kind)`; resolution happens in the relation pass.
pub fn outside_links(text: &str, cap: usize) -> Vec<(usize, String, &'static str)> {
    let mut v: Vec<(usize, String, &'static str)> = Vec::new();
    let mut seen = HashSet::new();
    for (i, line) in text.lines().enumerate() {
        for tok in line.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '<' | '>' | '`' | '"' | '\'' | '|')) {
            let t = tok.trim_end_matches(['.', ',', ';', ':', '!', '*']).trim_start_matches('*');
            let kind = if t.contains("://") || (t.starts_with("git@") && t.contains(':')) {
                if crate::util::url_parts(t).is_none() {
                    continue;
                }
                "doc_url"
            } else if (t.starts_with('/') && !t.starts_with("//") && t[1..].contains('/')) || t.starts_with("~/") || t.starts_with("../") {
                if t.len() > 300 || t.contains(['$', '*', '{', '=']) {
                    continue;
                }
                "doc_path"
            } else {
                continue;
            };
            if seen.insert(t.to_string()) {
                v.push((i + 1, t.to_string(), kind));
            }
            if v.len() >= cap {
                return v;
            }
        }
    }
    v
}

/// Index of a checkout's tracked files for resolving doc tokens.
pub struct FileIndex {
    pub files: HashSet<String>,
    pub dirs: HashSet<String>,
    pub by_name: HashMap<String, Vec<String>>,
}

impl FileIndex {
    pub fn new(files: Vec<String>) -> FileIndex {
        let mut dirs = HashSet::new();
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for f in &files {
            let mut p = Path::new(f).parent();
            while let Some(d) = p {
                let s = d.to_string_lossy().into_owned();
                if s.is_empty() || !dirs.insert(s) {
                    break;
                }
                p = d.parent();
            }
            if let Some(n) = Path::new(f).file_name() {
                by_name.entry(n.to_string_lossy().into_owned()).or_default().push(f.clone());
            }
        }
        FileIndex { files: files.into_iter().collect(), dirs, by_name }
    }

    /// Resolve a token to a repo-relative path. Returns (path, how).
    pub fn resolve(&self, doc_dir_rel: &str, raw: &str) -> Option<(String, &'static str)> {
        let raw = raw.trim_start_matches("./").trim_end_matches('/');
        if raw.is_empty() || raw.starts_with('~') || raw.starts_with('/') {
            return None;
        }
        let mut candidates = Vec::new();
        if !doc_dir_rel.is_empty() {
            candidates.push(crate::util::normalize(&Path::new(doc_dir_rel).join(raw)).to_string_lossy().into_owned());
        }
        candidates.push(crate::util::normalize(Path::new(raw)).to_string_lossy().into_owned());
        for c in &candidates {
            if self.files.contains(c) {
                return Some((c.clone(), "path"));
            }
            if self.dirs.contains(c) {
                return Some((c.clone(), "dir"));
            }
        }
        // `Settings.tsx` or `components/Settings.tsx`: unique suffix match only.
        let name = Path::new(raw).file_name()?.to_string_lossy().into_owned();
        let hits: Vec<&String> = self.by_name.get(&name)?.iter().filter(|f| f.ends_with(raw)).collect();
        if hits.len() == 1 {
            return Some((hits[0].clone(), "unique-name"));
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct EntryPoint {
    pub rel: String,
    pub source: String,
    pub line: usize,
    pub note: String,
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map(|i| i + 1).unwrap_or(1)
}

/// Declared entry points from common manifests (only paths that exist are returned).
pub fn entry_points(root: &Path, idx: &FileIndex) -> Vec<EntryPoint> {
    let mut v = Vec::new();
    let mut add = |rel: &str, source: &str, line: usize, note: String| {
        let rel = rel.trim_start_matches("./");
        if idx.files.contains(rel) || idx.dirs.contains(rel) {
            v.push(EntryPoint { rel: rel.to_string(), source: source.to_string(), line, note });
        }
    };
    if let Ok(t) = std::fs::read_to_string(root.join("package.json")) {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&t) {
            for k in ["main", "module", "types", "source"] {
                if let Some(s) = j.get(k).and_then(|x| x.as_str()) {
                    add(s, "package.json", line_of(&t, &format!("\"{k}\"")), format!("package.json {k}"));
                }
            }
            match j.get("bin") {
                Some(serde_json::Value::String(s)) => add(s, "package.json", line_of(&t, "\"bin\""), "package.json bin".into()),
                Some(serde_json::Value::Object(m)) => {
                    for (name, p) in m {
                        if let Some(s) = p.as_str() {
                            add(s, "package.json", line_of(&t, s), format!("package.json bin {name}"));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(root.join("Cargo.toml")) {
        for l in t.lines() {
            if let Some(p) = l.trim().strip_prefix("path").map(|r| r.trim_start_matches([' ', '=']).trim().trim_matches('"')) {
                add(p, "Cargo.toml", line_of(&t, l), "Cargo.toml target path".into());
            }
        }
        add("src/main.rs", "Cargo.toml", 1, "cargo default binary".into());
        add("src/lib.rs", "Cargo.toml", 1, "cargo default library".into());
    }
    if let Ok(t) = std::fs::read_to_string(root.join("pyproject.toml")) {
        if t.contains("[project.scripts]") {
            add("pyproject.toml", "pyproject.toml", line_of(&t, "[project.scripts]"), "pyproject scripts table".into());
        }
    }
    for (f, note) in [
        ("main.py", "python main module"),
        ("__main__.py", "python __main__"),
        ("src/main.ts", "conventional ts entry"),
        ("src/index.ts", "conventional ts entry"),
        ("src/index.tsx", "conventional tsx entry"),
        ("main.go", "go main"),
        ("flake.nix", "nix flake"),
        ("Makefile", "make targets"),
        ("Dockerfile", "container build"),
        ("plugin.json", "plugin manifest"),
    ] {
        add(f, f, 1, note.into());
    }
    let mut seen = HashSet::new();
    v.retain(|e| seen.insert(e.rel.clone()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_refs_with_lines_and_headings() {
        let md = "# Proj\nIntro text.\n## Key Files\n- **src/components/Settings.tsx** - UI\n- see [guide](docs/guide.md#x) and `main.py`\n```\nnpm run build\n```\nhttps://x.y/z.md\n";
        let d = analyze_markdown(md, 10, 50);
        assert_eq!(d.title.as_deref(), Some("Proj"));
        let raws: Vec<_> = d.refs.iter().map(|r| (r.raw.as_str(), r.line, r.heading.as_str())).collect();
        assert!(raws.contains(&("src/components/Settings.tsx", 4, "Key Files")));
        assert!(raws.contains(&("docs/guide.md", 5, "Key Files")));
        assert!(raws.contains(&("main.py", 5, "Key Files")));
        assert!(!raws.iter().any(|r| r.0.contains("x.y")));
        assert_eq!(d.excerpts[0].line, 2);
    }

    #[test]
    fn sections_and_outside_links() {
        let md = "# A\n```\n# not a heading\n```\n## B\n## B\nsee https://github.com/acme/tool/tree/main and ../sibling/README.md, `/opt/shared/lib`\nlocal docs/x.md\n";
        let s = sections(md);
        assert_eq!(s.iter().map(|x| (x.line, x.slug.as_str())).collect::<Vec<_>>(), vec![(1, "a"), (5, "b"), (6, "b-1")]);
        let l = outside_links(md, 10);
        assert_eq!(l, vec![(7, "https://github.com/acme/tool/tree/main".into(), "doc_url"), (7, "../sibling/README.md".into(), "doc_path"), (7, "/opt/shared/lib".into(), "doc_path")]);
    }

    #[test]
    fn resolves_relative_and_unique_names() {
        let idx = FileIndex::new(vec!["src/components/Settings.tsx".into(), "docs/guide.md".into(), "docs/img.png".into(), "a/x.ts".into(), "b/x.ts".into()]);
        assert_eq!(idx.resolve("docs", "img.png").unwrap().0, "docs/img.png");
        assert_eq!(idx.resolve("", "Settings.tsx").unwrap(), ("src/components/Settings.tsx".into(), "unique-name"));
        assert!(idx.resolve("", "x.ts").is_none()); // ambiguous
        assert_eq!(idx.resolve("", "src/components/").unwrap().1, "dir");
    }
}
