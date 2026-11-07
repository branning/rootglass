# Agent workflow and Ergo backlog

Use [sandover/ergo](https://github.com/sandover/ergo) v6 to manage implementation
work. The tracked `.ergo/backlog.jsonl` owns task status, dependencies and scope;
`.ergo/journal.jsonl` retains claims, outcomes and evidence. The
[app spec](app-spec.md) remains the product/technical contract and acceptance
criteria. Its Sxx table is a historical index, not a second live task queue.

The root [AGENTS.md](../AGENTS.md) directs every harness to the local
[Rootglass Ergo skill](../.agents/skills/rootglass-ergo/SKILL.md). That skill is
the step-by-step checklist for task discovery, execution, blockers and handoff;
this document supplies installation, atomic delivery and migration details.

## Install and find work

Install Ergo 6.0.6 (the version used for this migration), or a compatible v6
release, using an [official release](https://github.com/sandover/ergo/releases/tag/v6.0.6).
Verify the archive against the release checksums before adding `ergo` to PATH.
With Go 1.24+ an alternative is:

```sh
go install github.com/sandover/ergo/v6/cmd/ergo@v6.0.6
```

From the repository root:

```sh
ergo --help
ergo quickstart
ergo where                 # must identify this checkout's .ergo
ergo list --ready
ergo show <id>
ergo claim <id> --agent <identity>
```

This repository is already initialized; do not create a second backlog.
Read the task, its prerequisites, assigned paths and referenced spec sections.
Choose one authorized ready task and coordinate shared-file ownership before
claiming it. Claim only work you will implement. Readiness is scheduling, not
proof: Ergo also considers failed/canceled dependencies finished. Inspect their
outcomes and stop or block dependent work if required results are missing.
A specific-ID claim can bypass dependency readiness; do not use it to skip gates.

## Atomic multi-file changes (issue #12)

1. Start from current `main` on a focused branch, for example
   `git switch -c codex/<task-id>-<short-name>`. Use a PR for every multi-file,
   dependency, fixture/signature, workspace or CI change. Do not write those
   changes directly to `main`.
2. Prepare the complete usable change locally, including all new source files,
   manifests, lockfiles, signed fixtures and corresponding Ergo updates. Review
   `git diff` and `git diff --check`; stage explicit paths together. Never push a
   workspace member declaration without its sources, or signed bytes without
   their matching DSSE envelope.
3. Run the task's targeted checks against the complete staged result. For code,
   follow the spec's fmt/test/clippy and affected guest/platform checks. For a
   backlog/docs-only change, parse the backlog with `ergo list --all --json`,
   inspect readiness/dependencies and check links/whitespace. Do not describe
   unrun checks as passing.
4. Commit the coherent file set in **one Git commit**, then push the branch.
   If later fixes are needed, each pushed commit must again be coherent. Local
   preparation and Ergo commands need not each become separate pushed commits.
5. If using only the GitHub API, create all blobs, one tree containing every
   related addition/edit/deletion, then one commit referencing that tree and
   the observed branch head. Update the PR branch ref once, without force.
   Re-read/rebase if the head moved. Individual Contents API writes create
   separate commits and are unsuitable for interdependent file changes.
6. Open a PR referencing the Ergo ID, Sxx/A-xx IDs, objective, changed paths,
   checks/results and limitations. Include `Fixes #<issue>` when the PR fully
   resolves that issue. Wait for required CI and review before merging; an
   opened PR alone does not satisfy implementation or acceptance gates.

Keep workflow concurrency and `cancel-in-progress: true` enabled in both
`.github/workflows/ci.yml` and `desktop-packages.yml`. These cancel superseded
runs; atomic branch commits prevent invalid intermediate trees in the first
place. Do not suppress necessary CI to reduce noise. A standalone typo/doc fix
may use a single direct commit only when explicitly authorized; multi-file work
still uses a PR.

## Record progress and integrate

```sh
ergo result <id> "Targeted checks passed at <revision>" --file docs/<evidence>.md
ergo block <id> -m "Concrete impediment and required next action"
ergo open <id>                 # resume after resolving an impediment
ergo done <id> -m "Merged <PR/revision>; checks and acceptance evidence"
```

Use `result` only with an existing project-relative evidence file (or omit
`--file`). Use `done` after the task's actual definition of done is met. For
implementation this includes merge and required CI; for acceptance it includes
scenario evidence and any named human decision. Record a pending PR as a result,
not as completed work. Track backlog and journal updates together. Ignore only
transient locks/temp files; do not hand-edit JSONL records or prune history as
part of routine work.

One integration owner coordinates root `Cargo.toml`, `Cargo.lock`, generic
workflows and backlog integration. Local Ergo locking coordinates a single
checkout, not separate clones. With worktrees, keep a coordinator-owned backlog
and let workers report results; reconcile divergent task/journal changes before
merging. No force-pushing shared branches and no simultaneous edits on `main`.

## Migration and acceptance map

The initial backlog imports app-spec version 1.1 at `0d40ac1`. S00–S11 are marked
done **only as historical merged implementation**, with that limitation recorded
in each journal result. This migration did not rerun historical Rust/guest CI.
No open acceptance gate was closed by importing it.

| Epic | Scope and acceptance |
|---|---|
| S00–S11 | Historical implementation, preserving original prerequisites |
| S12 | Archive/threat tests (A-02/A-06), state (A-05), runtime regression (A-01/A-03/A-04/A-07), production trust, final A-01–A-10 review |
| S13 | UI models, asynchronous behavior and real GUI smoke (A-07/A-08) |
| S14 | Offline runtime inventory and separate Linux/macOS/Windows clean installs (A-03/A-09), after S13 |
| S15 | Signed release inventory/provenance and final offline artifact journey (A-02/A-06/A-10), after S14 |
| S16 | Headless Ratatui frontend, Rootglass workflow parity and a libghostty-hosted GPUI terminal surface |
| R3 | Draft product/design decisions; excluded from v1 gates |

S12's final review depends on S15; S15 depends only on the specific independent
S12 security/state/runtime/trust results it needs, avoiding an epic-level cycle.
The ready queue starts with those four S12 tasks and the S13 UI-model task.
S14 platform checks run independently once their shared prerequisites finish.

Every future task includes goal, owned scope, source sections, definition of
done, validation and review expectations. The final S12 review maps F-01–F-26,
A-01–A-10, UI/policy requirements and ADRs to evidence. Preserve the spec's
qualification distinction: successful package builds are not clean installs,
and merged UI code is not A-08 acceptance. Use `ergo list --all` to include
historical work and deferred drafts; update task bodies/dependencies through
Ergo when scope changes rather than adding another Markdown checklist.
