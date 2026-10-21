//! Small helpers: stable ids, time formatting, bounded line reading, redaction.

use std::io::{BufRead, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// FNV-1a 64-bit. Deterministic across runs and platforms (unlike std's hasher).
pub fn fnv64(data: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Stable node id: `<prefix>-<12 hex>` derived only from kind + canonical key.
pub fn stable_id(prefix: &str, key: &str) -> String {
    format!("{}-{:012x}", prefix, fnv64(&format!("{prefix}\0{key}")) & 0xffff_ffff_ffff)
}

pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn mtime_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Days since 1970-01-01 -> (y, m, d). Howard Hinnant's civil_from_days.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix seconds -> `YYYY-MM-DD HH:MM` (UTC).
pub fn fmt_ts(ts: i64) -> String {
    if ts <= 0 {
        return "unknown-date".into();
    }
    let (y, m, d) = civil(ts.div_euclid(86400));
    let s = ts.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}Z", s / 3600, (s % 3600) / 60)
}

pub fn fmt_date(ts: i64) -> String {
    if ts <= 0 {
        return "unknown-date".into();
    }
    let (y, m, d) = civil(ts.div_euclid(86400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parse RFC3339-ish `2026-10-10T20:06:47.123Z` / `+02:00` into unix seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, l: usize| s.get(a..a + l)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 2)?, num(8, 2)?, num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let mut t = days_from_civil(y, mo as u32, d as u32) * 86400 + h * 3600 + mi * 60 + se;
    let rest = &s[19..];
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if rest.len() >= 6 && (rest.starts_with('+') || rest.starts_with('-')) {
        let oh: i64 = rest[1..3].parse().ok()?;
        let om: i64 = rest[4..6].parse().ok()?;
        let off = oh * 3600 + om * 60;
        t -= if rest.starts_with('+') { off } else { -off };
    }
    Some(t)
}

/// Lexically normalise `a/./b/../c` without touching the filesystem.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve `p` against `base` when relative, then normalise.
pub fn resolve_against(base: &Path, p: &str) -> PathBuf {
    let pp = Path::new(p);
    if pp.is_absolute() {
        normalize(pp)
    } else {
        normalize(&base.join(pp))
    }
}

/// Truncate on a char boundary, collapse whitespace, append an ellipsis.
pub fn clip(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Replace secret-looking tokens before anything is stored or printed.
pub fn redact(s: &str) -> String {
    let prefixes = ["sk-", "ghp_", "gho_", "ghs_", "github_pat_", "xoxb-", "xoxp-", "AKIA", "eyJ", "glpat-", "AIza"];
    let mut out = Vec::new();
    for word in s.split(' ') {
        let core = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        let lower = core.to_ascii_lowercase();
        let secretish = prefixes.iter().any(|p| core.starts_with(p) && core.len() > p.len() + 12)
            || (core.len() >= 32
                && core.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                && core.chars().any(|c| c.is_ascii_digit())
                && core.chars().any(|c| c.is_ascii_alphabetic()))
            || ((lower.contains("password=") || lower.contains("token=") || lower.contains("secret=")) && core.len() > 10);
        if secretish {
            out.push(word.replace(core, "[redacted]"));
        } else {
            out.push(word.to_string());
        }
    }
    out.join(" ")
}

/// Result of a bounded line read.
pub enum Line {
    Data(Vec<u8>),
    /// Line exceeded the cap; the remainder was consumed and discarded.
    TooLong(usize),
    Eof,
}

/// Read one `\n` terminated line, never buffering more than `cap` bytes.
pub fn read_line_bounded<R: BufRead>(r: &mut R, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<Line> {
    buf.clear();
    let mut total = 0usize;
    let mut overflow = false;
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            if total == 0 {
                return Ok(Line::Eof);
            }
            break;
        }
        let (chunk, done) = match avail.iter().position(|b| *b == b'\n') {
            Some(i) => (&avail[..i], Some(i + 1)),
            None => (avail, None),
        };
        total += chunk.len();
        if !overflow {
            if buf.len() + chunk.len() > cap {
                overflow = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let consumed = done.unwrap_or(avail.len());
        r.consume(consumed);
        if done.is_some() {
            break;
        }
    }
    Ok(if overflow { Line::TooLong(total) } else { Line::Data(std::mem::take(buf)) })
}

/// Read at most `cap` bytes from the start of a file.
pub fn read_head(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mut v = Vec::new();
    f.take(cap as u64).read_to_end(&mut v)?;
    Ok(v)
}

/// Run a command with a wall-clock timeout; returns stdout on success.
pub fn run_cmd(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = (&mut stdout).take(32 << 20).read_to_end(&mut v);
        v
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let out = reader.join().ok()?;
                return if st.success() { Some(String::from_utf8_lossy(&out).into_owned()) } else { None };
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return None,
        }
    }
}

/// POSIX shell quoting: bare when safe, otherwise single-quoted.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_=./:,+@#%".contains(c)) && !s.starts_with('#') {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// GitHub-style heading anchor: lowercase, spaces to '-', punctuation dropped.
pub fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                Some(c)
            } else if c == ' ' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// Claude Code's project directory name for a cwd (every non-alphanumeric -> '-').
pub fn claude_project_dir_name(path: &str) -> String {
    path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_prefixed() {
        assert_eq!(stable_id("file", "/a/b"), stable_id("file", "/a/b"));
        assert_ne!(stable_id("file", "/a/b"), stable_id("file", "/a/c"));
        assert!(stable_id("repo", "x").starts_with("repo-"));
    }

    #[test]
    fn iso_roundtrip() {
        let t = parse_iso("2026-10-10T20:06:47.155Z").unwrap();
        assert_eq!(fmt_ts(t), "2026-10-10 20:06Z");
        assert_eq!(parse_iso("2026-10-10T22:06:47+02:00"), Some(t));
    }

    #[test]
    fn bounded_lines() {
        let data = b"short\nthis-line-is-way-too-long\nok";
        let mut r = std::io::BufReader::with_capacity(4, &data[..]);
        let mut b = Vec::new();
        assert!(matches!(read_line_bounded(&mut r, &mut b, 10).unwrap(), Line::Data(d) if d == b"short"));
        assert!(matches!(read_line_bounded(&mut r, &mut b, 10).unwrap(), Line::TooLong(25)));
        assert!(matches!(read_line_bounded(&mut r, &mut b, 10).unwrap(), Line::Data(d) if d == b"ok"));
        assert!(matches!(read_line_bounded(&mut r, &mut b, 10).unwrap(), Line::Eof));
    }

    #[test]
    fn redacts_tokens() {
        let s = redact("use ghp_abcdefghijklmnopqrstuvwxyz0123 now");
        assert!(s.contains("[redacted]") && !s.contains("ghp_abc"));
        assert_eq!(redact("plain words only"), "plain words only");
    }
}
