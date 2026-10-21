//! Cross-project / cross-worktree file views and task graph expansion.
//! Synthetic fixtures only: temp homes, fake CLIs, no network, no real home.

#[allow(unused_imports)]
use super::*;

#[test]
fn ledger_fixture_scans_and_qualified_refs_resolve_without_scope() {
    let fx = crate::testutil::ledger();
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let p = view::resolve_project(&c, &fx.api_fix.to_string_lossy()).unwrap();
    assert!(p.handle.starts_with("ledger-api~"), "two clones of one remote get compact handles: {}", p.handle);
    let r = view::qualified_ref(&c, &p.focus, "ledger/webhooks.py").unwrap();
    assert!(r.ends_with("@fix/webhook-retry:ledger/webhooks.py"), "{r}");
    let out = view::expand(&v(&c), &r, &view::Req::default()).unwrap();
    assert!(out.starts_with("# :ledger/webhooks.py — file in ") && out.contains("Double charge on webhook retry"), "{out}");
    assert!(fx.conv_cross.exists() && fx.sdk.exists() && fx.api.exists() && fx.api_clone.exists() && fx.conv_fix.exists());
}
