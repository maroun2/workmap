//! Test helpers: self-cleaning temp dirs and git fixtures.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("workmap-test-{}-{}-{}", tag, std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(std::fs::canonicalize(&p).unwrap())
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=t@example.invalid", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&ok.stderr));
}

pub fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// Commit everything in `dir` with a fixed date.
pub fn commit_all(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
}

/// One JSON value per line.
pub fn jsonl(lines: &[serde_json::Value]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// Synthetic multi-project workspace mirroring the e2e review fixture:
///
/// - `api`: ledger-api main checkout (origin github.com/acme/ledger-api)
/// - `api_fix`: sibling worktree `fix/webhook-retry` whose `ledger/webhooks.py`
///   differs from main and was fixed by a Claude conversation + commit there
/// - `sdk`: ledger-sdk (origin github.com/acme/ledger-sdk) with `src/retry.ts`
/// - `api_clone`: a second clone of ledger-api (same remote, different checkout)
/// - Claude transcripts: `conv_fix` (cwd api_fix, edits webhooks.py) and
///   `conv_cross` (cwd api main, reads the sdk's src/retry.ts: cross-project)
pub struct Ledger {
    pub _t: TempDir,
    pub home: PathBuf,
    pub api: PathBuf,
    pub api_fix: PathBuf,
    pub sdk: PathBuf,
    pub api_clone: PathBuf,
    pub conv_fix: PathBuf,
    pub conv_cross: PathBuf,
}

pub fn ledger() -> Ledger {
    use serde_json::json;
    let t = TempDir::new("ledger");
    let home = t.path().join("home");
    let api = home.join("work/ledger-api");
    write(&api.join("README.md"), "# ledger-api\n\nPayments ledger HTTP service.\n\n## Layout\n- `ledger/webhooks.py` receives provider webhooks.\n- `ledger/charges.py` posts charges.\n");
    write(&api.join("docs/adr/0003-charge-dedup.md"), "# ADR 3: Charge deduplication\n\n## Decision\nCharges carry a dedup token stored in Redis for 24 hours.\n");
    write(&api.join("ledger/webhooks.py"), "from .charges import post_charge\ndef handle(event):\n    token = new_token()\n    post_charge(event, token)\n");
    write(&api.join("ledger/charges.py"), "def post_charge(event, token):\n    pass\n");
    git(&api, &["init", "-q"]);
    git(&api, &["remote", "add", "origin", "https://github.com/acme/ledger-api.git"]);
    commit_all(&api, "Initial ledger service");
    let api_fix = home.join("work/ledger-api-fix");
    git(&api, &["worktree", "add", "-q", "-b", "fix/webhook-retry", api_fix.to_str().unwrap()]);
    write(&api_fix.join("ledger/webhooks.py"), "from .charges import post_charge\ndef handle(event):\n    token = event.idempotency_key\n    post_charge(event, token)\n");
    commit_all(&api_fix, "Reuse provider idempotency key on webhook retry");

    let sdk = home.join("work/ledger-sdk");
    write(&sdk.join("README.md"), "# ledger-sdk\n\nTypeScript client for ledger-api.\n");
    write(&sdk.join("src/retry.ts"), "export function backoff(n: number) {\n  return 2 ** n * 100;\n}\n");
    write(&sdk.join("package.json"), "{\n  \"name\": \"ledger-sdk\",\n  \"version\": \"1.0.0\"\n}\n");
    git(&sdk, &["init", "-q"]);
    git(&sdk, &["remote", "add", "origin", "git@github.com:acme/ledger-sdk.git"]);
    commit_all(&sdk, "SDK skeleton with retry backoff");

    let api_clone = home.join("mirror/ledger-api");
    write(&api_clone.join("README.md"), "# ledger-api\n\nSecond clone.\n");
    git(&api_clone, &["init", "-q"]);
    git(&api_clone, &["remote", "add", "origin", "https://github.com/acme/ledger-api"]);
    commit_all(&api_clone, "Second clone");

    let claude = |cwd: &Path, id: &str| home.join(".claude/projects").join(crate::util::claude_project_dir_name(&cwd.display().to_string())).join(format!("{id}.jsonl"));
    let f = api_fix.display().to_string();
    let conv_fix = claude(&api_fix, "aaaa1111-fix");
    write(
        &conv_fix,
        &jsonl(&[
            json!({"type":"summary","summary":"Double charge on webhook retry"}),
            json!({"type":"user","cwd":f,"sessionId":"aaaa1111-fix","gitBranch":"fix/webhook-retry","timestamp":"2026-09-02T10:00:00Z","message":{"role":"user","content":"Customers were charged twice when the provider retried a webhook. Find the root cause."}}),
            json!({"type":"assistant","cwd":f,"sessionId":"aaaa1111-fix","timestamp":"2026-09-02T10:00:05Z","message":{"content":[
                {"type":"tool_use","id":"r1","name":"Read","input":{"file_path": format!("{f}/ledger/webhooks.py")}},
                {"type":"tool_use","id":"e1","name":"Edit","input":{"file_path": format!("{f}/ledger/webhooks.py")}}]}}),
            json!({"type":"user","cwd":f,"sessionId":"aaaa1111-fix","timestamp":"2026-09-02T10:00:06Z","toolUseResult":{},"message":{"content":[
                {"type":"tool_result","tool_use_id":"r1","content":"..."},
                {"type":"tool_result","tool_use_id":"e1","content":"ok"}]}}),
            json!({"type":"assistant","cwd":f,"sessionId":"aaaa1111-fix","timestamp":"2026-09-02T10:01:00Z","message":{"content":[{"type":"text","text":"Root cause: handle() generated a fresh dedup token on every delivery, so a retried webhook deduplicated against nothing. Reusing the provider idempotency key fixes it."}]}}),
        ]),
    );
    let m = api.display().to_string();
    let s = sdk.display().to_string();
    let conv_cross = claude(&api, "bbbb2222-cross");
    write(
        &conv_cross,
        &jsonl(&[
            json!({"type":"summary","summary":"Port retry backoff from the sdk"}),
            json!({"type":"user","cwd":m,"sessionId":"bbbb2222-cross","gitBranch":"main","timestamp":"2026-09-03T09:00:00Z","message":{"role":"user","content":"Use the same exponential backoff as the sdk client in the API worker."}}),
            json!({"type":"assistant","cwd":m,"sessionId":"bbbb2222-cross","timestamp":"2026-09-03T09:00:05Z","message":{"content":[
                {"type":"tool_use","id":"r2","name":"Read","input":{"file_path": format!("{s}/src/retry.ts")}},
                {"type":"tool_use","id":"r3","name":"Read","input":{"file_path": format!("{m}/ledger/charges.py")}}]}}),
            json!({"type":"user","cwd":m,"sessionId":"bbbb2222-cross","timestamp":"2026-09-03T09:00:06Z","toolUseResult":{},"message":{"content":[
                {"type":"tool_result","tool_use_id":"r2","content":"..."},
                {"type":"tool_result","tool_use_id":"r3","content":"..."}]}}),
        ]),
    );
    Ledger { _t: t, home, api, api_fix, sdk, api_clone, conv_fix, conv_cross }
}
