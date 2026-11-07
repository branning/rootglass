# Offline publisher trust validation

**Backlog:** UFT256 · S12.4  
**Acceptance:** A-02, A-06

## Checks

Run from the repository root:

```sh
cargo fmt --all -- --check
cargo test --locked -p rootglass-signature -p rootglass-app
cargo clippy --locked -p rootglass-signature -p rootglass-app --all-targets -- -D warnings
git diff --check
```

Observed results: formatting and whitespace checks passed; all 35 tests passed (23 app, 12 signature); Clippy passed with warnings denied.

The app integration tests use deterministic test-only Ed25519 keys and synthetic signed bundles/runtime assets. They verify trusted keys reach preview and preflight, an unknown key is rejected, the compiled sample fixture is not merged into production trust, and a rotated/revoked key is reported distinctly and rejected by inspect, preflight, and start before QEMU spawn. A new key in the rotated config reaches preview and preflight. A tampered bundle reports a digest mismatch; missing and invalid production config fail instead of falling back to fixture keys.

## Limits and next action

This validates service behavior on the current host; it does not test a graphical session, real guest boot, packaged clean install, or external operator/key custody. Trust policy is loaded once at startup, so updates require restart. The ADR remains proposed until maintainers approve trust-root ownership, provisioning, rotation, revocation, and recovery.
