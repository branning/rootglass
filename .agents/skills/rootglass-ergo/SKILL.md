---
name: rootglass-ergo
description: Use when planning, choosing, implementing, resuming, reviewing, or handing off Rootglass work. Explains the repository's Ergo v6 backlog, task claims, dependencies, atomic commits, and evidence-based completion.
---

# Work one Rootglass task with Ergo

Read this skill before changing code or the backlog. It is linked from the root
[AGENTS.md](../../../AGENTS.md), so it does not depend on automatic skill loading
or a particular model/harness. Run commands from the repository root. Replace
`TASK_ID`, `DEPENDENCY_ID`, and `YOUR_IDENTITY` with actual values; they are not
literal task IDs. Use a stable identity such as `haiku@worker-1` or `luna@worker-1`.

## 1. Find the real backlog

```sh
ergo version
ergo --help
ergo quickstart
ergo where
ergo list --ready
```

Require Ergo v6 and confirm `where` points to this checkout's `.ergo` (or an
explicitly assigned coordinator checkout). If the CLI is missing, follow
[installation](../../../docs/agent-workflow.md#install-and-find-work). If it
cannot be installed, report that concrete blocker; do not invent commands,
hand-edit JSONL, run `ergo init`, or substitute a Markdown task queue.

`.ergo/backlog.jsonl` owns live tasks/dependencies. `.ergo/journal.jsonl` owns
work history. [app-spec.md](../../../docs/app-spec.md) owns product requirements.
Sxx IDs identify spec work packages; A-xx IDs identify acceptance tests. Neither
is an Ergo task ID. Copy the actual ID returned by `ergo list` or `ergo new task`.

## 2. Select and claim exactly one task

```sh
ergo show TASK_ID
ergo show DEPENDENCY_ID
```

Read each prerequisite's outcome, including dependencies inherited from its
epic. `list --ready` means schedulable, not proven successful: Ergo counts failed
and canceled dependencies as finished. If their required result is absent, block
the dependent task with a concrete explanation. A specific-ID `claim` can bypass
readiness; never use that to skip prerequisites.

| Situation | Action |
|---|---|
| User assigned a ready task | Inspect its body/dependencies, then claim it. |
| User authorized next backlog work | Choose one relevant ready task within that scope. |
| User requested new work absent from backlog | Create one focused task with goal, owned paths, source context, definition of done and validation; then claim it. |
| Task is already claimed by another identity | Coordinate ownership; do not steal or reopen it. |
| Task is waiting or draft | Do not claim it. Resolve prerequisites or obtain the scope decision needed to open a draft. |
| No ready work fits the request | Report the specific missing prerequisite or scope choice; do not start unrelated work. |

```sh
ergo claim TASK_ID --agent YOUR_IDENTITY
```

For planning, use `ergo new task`, `ergo new epic --file`, and `ergo sequence`
according to their `--help`. Use drafts while assembling a plan; open approved
leaves once dependencies are correct. Add edges only for needed results, not
merely because tasks are listed in an order. R3 drafts are not v1 work.

## 3. Load enough context to implement safely

Read spec sections 0, 2, 6, 10 and 12 as required by AGENTS.md, then the specific
requirements referenced by your task. Inspect the actual owned source files and
tests with `rg` before editing. Do not assume proposed spec APIs exist.

Keep a short working note: task ID, goal, owned paths, prerequisite results,
acceptance IDs and next check. Implement one useful outcome; if scope grows,
split or update the Ergo task rather than trying the whole epic. Coordinate
root Cargo manifests/lockfiles and generic CI with the integration owner.
Preserve signature verification, immutable assets, CLI behavior and the pinned
GPUI framework. Read [AGENTS.md](../../../AGENTS.md) for all product guardrails.

## 4. Validate and deliver one coherent change

Use a focused branch/PR. Prepare related sources, manifests, lockfiles and signed
fixtures together; stage explicit paths and publish one coherent commit. Never
use per-file GitHub Contents writes for an interdependent change. See the
[atomic workflow](../../../docs/agent-workflow.md#atomic-multi-file-changes-issue-12)
for the one-tree/one-commit API alternative and shared-file ownership.

Run the task's cheapest sufficient checks. Code changes need the relevant
fmt/test/clippy and affected guest/platform scenarios from the spec. Docs/backlog
changes need CLI parsing, readiness/dependency review, local links and
`git diff --check`. Keep required CI enabled. A missing GUI host or failed CI is
a limitation to record, never a passing test. Reuse evidence that still applies.

## 5. Record evidence, then hand off or finish

```sh
ergo result TASK_ID "Checks: <short command and observed result>"
ergo result TASK_ID "PR: <url>; remaining: <gate and next owner>"
```

Each result summary is one line, at most 120 characters. Use separate results
for checks and handoff, or attach longer evidence with `--file`.
Use `--file <project-relative-path>` only for an existing evidence file outside
`.ergo`. Record results on leaf tasks, not epics. Keep backlog and journal changes
in the same coherent commit. Do not commit `.ergo/lock`, prune history, or edit
JSONL directly. With worktrees, let the assigned coordinator own Ergo writes;
report task results to that coordinator instead of creating conflicting copies.

| Outcome | Command / state |
|---|---|
| PR opened, merge or acceptance still pending | `ergo result TASK_ID "..."`; retain doing and record the next owner/action. |
| Required host, credential, interface or other prerequisite is missing | `ergo block TASK_ID -m "Missing result; required next action"`. |
| Your blocked task can resume | Inspect results, then `ergo open TASK_ID` and claim again. |
| Implementation merged, required checks passed, all task acceptance evidence exists | `ergo done TASK_ID -m "Merged <PR/revision>; evidence <references>"`. |
| Attempt ended unsuccessfully | `ergo fail TASK_ID -m "Observed failure; missing result"`; tell the coordinator, since failed dependencies become schedulable. |

Pass lifecycle notes with `-m`, not piped stdin. In scripts, give lifecycle
commands an empty stdin (for example `</dev/null`) rather than inheriting a
script/body pipe; `new task` and `body` are the commands that accept task text.

Never mark done just because code compiles, a PR exists, or a package builds.
Historical S00–S11 done records are imported implementation history, not release
qualification. Human decisions and clean-host/GUI evidence remain required
where the task says so. A coordinator can record post-merge completion in a
follow-up backlog commit; do not prematurely mark done to fit it into the PR.

Before handing off, leave the task ID, branch/PR, changed paths, exact checks and
results, limitations, and next action in the Ergo result and final report. A new
agent should be able to resume with `ergo show TASK_ID` without this chat.
