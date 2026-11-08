# Rootglass agent instructions

**Start here for every model and harness:** read the local
[Rootglass Ergo skill](.agents/skills/rootglass-ergo/SKILL.md) before planning,
choosing, implementing, resuming or handing off work. If your harness does not
auto-load skills, open that file explicitly. `AGENTS.md` is the shared entry
point; no model-specific instruction file is needed.

**First actions, from the repository root:**

```sh
ergo --help
ergo quickstart
ergo where
ergo list --ready
ergo show TASK_ID
```

Replace `TASK_ID` with an actual Ergo ID. Confirm the backlog belongs to this
checkout, inspect prerequisite outcomes, then claim one authorized ready task
with `ergo claim TASK_ID --agent YOUR_IDENTITY`. If Ergo is missing, follow the
[install instructions](docs/agent-workflow.md#install-and-find-work); do not
create another backlog or edit JSONL by hand. Failed/canceled prerequisites can
appear ready in Ergo: verify their required results before starting.

The [app spec](docs/app-spec.md) is the product/technical contract. Read sections
0, 2, 6, 10 and 12 before editing code, plus your task's referenced requirements.
Ergo owns current task status and dependencies; the spec's Sxx table is not a
queue of new work.

## Rules for every coding agent

- Use [Ergo](https://github.com/sandover/ergo) v6 for the implementation backlog. Run `ergo --help`, `ergo quickstart`, and `ergo where` before writing tasks. Follow [the agent workflow](docs/agent-workflow.md).
- Claim **one authorized ready Ergo task** and its Sxx/A-xx IDs. Inspect prerequisite outcomes and coordinate owned paths; never implement the entire backlog in a single PR.
- Read *existing implementations and tests* for the exact module first. Do not invent API names or claim unimplemented behavior already works.
- Use a focused branch/PR and one coherent multi-file commit; never publish interdependent file-by-file commits to `main`. With GitHub APIs, batch blobs into one tree/commit and update the branch ref once. Edit only owned paths. Shared `Cargo.toml`, `Cargo.lock` and central workflows belong to the integration owner.
- Preserve the CLI and strict signed-bundle verification. Never weaken signatures or mark a guest boot successful by grepping for generic Alpine output.
- **Framework decision:** native Rust GUI is Longbridge GPUI Kit `=0.9.0` with its default `gpui-pre` backend, not Tauri, direct moving Zed GPUI, or a JavaScript shell. Consult GPUI Kit tagged docs/examples for method signatures.
- **Dependency direction:** `rootglass-qemu` already uses `rootglass-core`. The new `rootglass-app` service coordinates them, so no dependency cycle is introduced. Desktop and CLI call shared Rust service APIs, never each other as subprocesses.
- A pass means actual evidence: compile, targeted tests, negative-path tests, cleanup tests and linked acceptance IDs. Where GUI or host OS testing is unavailable, state the limitation and assign validation to the platform agent.
- For PRs that change user-visible UI, include screenshots in the PR description
  or link a CI screenshot artifact. Headless captures using Xvfb or VirtualGL
  are acceptable; identify the OS/display method and any interaction or
  accessibility checks the image does not cover. Screenshots supplement test
  and platform evidence; they do not replace it.
- Re-sign any changed signed fixture in the same PR; keep private keys out of Git.
- User/data security: production imports must be authenticated and bounded; run no untrusted host code; immutable package/rootfs must never be mutated by a guest or state reset.
- Do not change policies by assumption. Put tradeoffs into a small ADR/issue and notify the integration owner.

## GitHub Actions budget

- Keep automated work triggered by one pull-request `synchronize` event to **15 aggregate runner-minutes or less** across workflows. Count each job's runner time and add parallel jobs together; wall-clock duration is not the budget. Keep routine format, lint, unit, and targeted smoke checks within this cap.
- Do not run full native-platform or packaging matrices on every `synchronize`. Trigger those on PR open/reopen/ready-for-review, after applicable changes settle with `workflow_dispatch`, and/or on `main` after merge. For platform evidence required before merge, dispatch the matrix against the latest PR head after the relevant code changes.
- Before adding or widening a matrix, estimate its aggregate runner-minutes per event and adjust the trigger/cadence to stay within budget. Prefer caches and path filters where they reduce repeated work without weakening required evidence.

## Fast path

1. Follow the local Ergo skill: inspect and claim one authorized ready task. For new user-requested work, create a focused task if none already covers it; retain its Sxx/A-xx references where applicable.
2. Verify dependencies and path ownership; begin with source/tests.
3. Implement smallest usable slice with tests. Use section 10 of `docs/app-spec.md` for exact acceptance criteria.
4. Run `cargo fmt --all -- --check`, relevant `cargo test --locked` and `cargo clippy --locked`, plus any guest/platform scenario affected.
5. Review/stage all related files together, commit atomically, then open a focused PR with changes, tests, limits, follow-ups and Ergo/Sxx/A-xx IDs. Keep CI concurrency enabled.
6. Record results in Ergo; mark done only after merge/checks and the task’s acceptance evidence exist. Commit backlog and journal updates together.

**Do not mark R2/v1 done** until a clean user machine installs the app, opens a real signed ZIP, boots the guest, and stops it offline. See the release gate in the spec.
