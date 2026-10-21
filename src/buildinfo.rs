//! Build identity shared by `build.rs` (which computes it) and the binary
//! (which prints it in `--version`). No dependencies: build.rs includes this file.

/// Revision recorded in the binary: the Git commit of the source tree when it
/// is a checkout of this crate (`-dirty` with uncommitted tracked changes),
/// else `WORKMAP_BUILD_REV` (set by default.nix / CI, where there is no
/// `.git`), else `unknown`. Values are sanitized; a build never fails here.
#[allow(dead_code)]
pub fn revision(git_rev: Option<&str>, dirty: bool, env_rev: Option<&str>) -> String {
    let clean = |s: &str| -> Option<String> {
        let s = s.trim();
        let ok = !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c));
        ok.then(|| s.to_string())
    };
    if let Some(r) = git_rev.and_then(clean).filter(|r| r.chars().all(|c| c.is_ascii_hexdigit())) {
        return if dirty { format!("{r}-dirty") } else { r };
    }
    env_rev.and_then(clean).filter(|r| r != "unknown").unwrap_or_else(|| "unknown".to_string())
}

/// `<semver> (rev <revision>; <profile>; <target>)`.
#[allow(dead_code)]
pub fn render(semver: &str, rev: &str, profile: &str, target: &str) -> String {
    let mut extra = vec![format!("rev {rev}")];
    extra.extend([profile, target].iter().filter(|s| !s.is_empty()).map(|s| s.to_string()));
    format!("{semver} ({})", extra.join("; "))
}
