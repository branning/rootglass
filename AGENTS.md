# Rootglass agent instructions

**Start here:** [docs/app-spec.md](docs/app-spec.md). It is the authoritative implementation contract. Read sections 0, 2, 6, 10 and 12 before editing code.

## Rules for every coding agent

- Use [Ergo](https://github.com/sandover/ergo) v6 for the implementation backlog. Run `ergo --help`, `ergo quickstart`, and `ergo where` before writing tasks. Follow [the agent workflow](docs/agent-workflow.md).
- Claim **one authorized ready Ergo task** and its Sxx/A-xx IDs. Inspect prerequisite outcomes and coordinate owned paths; never implement the entire backlog in a single PR.
- Read *existing implementations and tests* for the exact module first. Do not invent API names or claim unimplemented behavior already works.
- Use a focused branch/PR and one coherent multi-file commit; never publish interdependent file-by-file commits to `main`. With GitHub APIs, batch blobs into one tree/commit and update the branch ref once. Edit only owned paths. Shared `Cargo.toml`, `Cargo.lock` and central workflows belong to the integration owner.
- Preserve the CLI and strict signed-bundle verification. Never weaken signatures or mark a guest boot successful by grepping for generic Alpine output.
- **Framework decision:** native Rust GUI is Longbridge GPUI Kit `=0.9.0` with its default `gpui-pre` backend, not Tauri, direct moving Zed GPUI, or a JavaScript shell. Consult GPUI Kit tagged docs/examples for method signatures.
- **Dependency direction:** `rootglass-qemu` already uses `rootglass-core`. The new `rootglass-app` service coordinates them, so no dependency cycle is introduced. Desktop and CLI call shared Rust service APIs, never each other as subprocesses.
- A pass means actual evidence: compile, targeted tests, negative-path tests, cleanup tests and linked acceptance IDs. Where GUI or host OS testing is unavailable, state the limitation and assign validation to the platform agent.
- Re-sign any changed signed fixture in the same PR; keep private keys out of Git.
- User/data security: production imports must be authenticated and bounded; run no untrusted host code; immutable package/rootfs must never be mutated by a guest or state reset.
- Do not change policies by assumption. Put tradeoffs into a small ADR/issue and notify the integration owner.

## Fast path

1. Choose assigned `Sxx` package in [`12 swarm work packages](docs/app-spec.md#12-swarm-work-packages-assign-one-owner-per-package).
2. Verify dependencies and path ownership; begin with source/tests.
3. Implement smallest usable slice with tests. Use section 10 of `docs/app-spec.md` for exact acceptance criteria.
4. Run `cargo fmt --all -- --check`, relevant `cargo test --locked` and `cargo clippy --locked`, plus any guest/platform scenario affected.
5. Review/stage all related files together, commit atomically, then open a focused PR with changes, tests, limits, follow-ups and Ergo/Sxx/A-xx IDs. Keep CI concurrency enabled.
6. Record results in Ergo; mark done only after merge/checks and the task’s acceptance evidence exist. Commit backlog and journal updates together.

**Do not mark R2/v1 done** until a clean user machine installs the app, opens a real signed ZIP, boots the guest, and stops it offline. See the release gate in the spec.
