# Rootglass delivery roadmap

**Status:** Updated 2026-11-09 against the merged `main` implementation and observed v0.0.3 package smoke. This roadmap tracks delivery and acceptance; the detailed product and technical contract remains in [app-spec.md](app-spec.md).

Rootglass is a native Rust desktop app built with GPUI Kit `=0.9.0`, backed by a shared Rust application service and CLI. CI is the functioning test environment for the reference guest: hosted x64 and arm64 TCG jobs materialize and boot it through the CLI. A developer machine run is useful, but not a merge gate.

## Delivered implementation

The merged work packages S00–S11 establish the CLI reference path and the first GPUI desktop slice:

- Signed `.glass` fixtures, manifest validation, signature verification and CLI workflows.
- QEMU supervision and shared application orchestration used by the CLI and desktop.
- ZIP import, installed-glass library, trust reporting, persistent instance-state handling, and runtime asset resolution.
- GPUI Kit desktop window with library/selection, bundle details, lifecycle controls, and live guest console integration.
- Cross-platform package builds for Linux, macOS, and Windows, with checksums and `.glass` file association metadata.

These are implementation milestones, not a claim that every v1 acceptance test has passed. In particular, see the open gates below and [desktop-packages.md](desktop-packages.md).

## Completed milestone — GPUI workflow smoke (A-08)

**Result:** the A-08 desktop workflow smoke is recorded in [s13-desktop-smoke-evidence.md](s13-desktop-smoke-evidence.md).

Linux 24.04 x86_64 Xvfb exercised the GPUI application through open/import, trust and verification states, preflight, guest start, serial input/output, restart, stop, resize, window exit, and child cleanup. The maintainer also installed the exact v0.0.3 macOS arm64 DMG, booted Hello World in the GUI, and ran `uname -a` over serial, confirming Alpine aarch64.

Native focus, dialogs, accessibility, and usability review are deferred to subsequent Figma-led work. This evidence does not qualify GPUI on every OS or establish the final support matrix.

**Evidence:** [Linux Xvfb screenshots and logs](https://github.com/branning/rootglass/actions/runs/42482916674/artifacts/13618535851); [exact-tag macOS installed TUI smoke](https://github.com/branning/rootglass/actions/runs/42560261893/artifacts/13648873558); maintainer's manual macOS GUI and serial confirmation.

## Current milestone — GPUI terminal bridge

**Goal:** host the merged Ratatui session in the existing GPUI desktop through public libghostty-vt and a custom GPUI view.

S16.1–S16.6, S16.9 and S16.10 are merged: the TUI has a headless controller and renderer, CLI terminal lifecycle, verified library workflows, guest serial interaction, state/trust controls and signing/package flows. The Linux Xvfb smoke exercises PTY keyboard input, ANSI output, selection, clipboard paste, resize and orderly cleanup. The smoke builds the optional `embedded-terminal` feature; current release package builds do not enable or bundle it. The feature smoke does not boot a guest through the embedded session, and native macOS/Windows GPUI hosts have not been qualified. The maintainer approved public libghostty-vt with a custom renderer and PTY/session adapter. The Ghostty GUI apps use native SwiftUI (macOS) and GTK (Linux), not GPUI Kit; libghostty-vt is the terminal core, not a ready-made view.

1. Keep the signed Linux x86_64 PTY smoke reproducible in focused CI and record upstream ABI, license and packaging boundaries in [tui-architecture.md](tui-architecture.md).
2. Exercise real guest boot, serial login/input/output, stop, and cleanup through the embedded Rootglass TUI session.
3. Decide which package targets should include and ship the embedded-terminal feature; qualify each target before claiming it.
4. Publish the cross-surface acceptance record with exact host/surface evidence and remaining limitations.

**Exit gate:** headless tests exercise the same TUI controller and screens used by the standalone command; every claimed GPUI host has evidence for the embedded terminal and real guest-console interaction. The A-08 desktop smoke is complete, but the embedded-host objective and app release gates remain open. Native UI polish and accessibility review are subsequent work.

## Next milestone — installed app works on a clean host

**Goal:** close A-09 for each shipping operating system.

1. Use the completed v0.0.3 QEMU/runtime inventory in [ADR-0002](adr/0002-versioned-desktop-qemu-runtime.md); verify clean installation and runtime behavior per platform.
2. Install each package on a clean OS image without developer tools, open a `.glass` through the app and through the OS file association, then boot and stop the guest.
3. Verify missing prerequisites and unsupported acceleration produce clear, truthful errors.
4. Record clean-install and file-open evidence by OS in [desktop-packages.md](desktop-packages.md).

**Exit gate:** A-09 passes for every advertised shipping OS; otherwise narrow the support matrix explicitly.

## Release readiness — integrity and support

**Goal:** close A-10 and the v1 release definition of done.

- Add application/installer signing and macOS notarization where applicable.
- Produce a release inventory containing exact bundled binary/runtime versions and hashes, SBOM/notices, signing provenance, and platform test results.
- Complete offline install → verify → import → boot → stop coverage with a production-structured signed archive.
- Review trust-store provisioning/rotation, recovery messaging, and uninstall/state-retention behavior against the app spec.

**Exit gate:** A-01–A-10 pass, and the v1 release definition in [app-spec.md](app-spec.md#103-release-definition-of-done) is met.

## Later, after v1

Only schedule these after release gates or with a separate product decision:

- Guest framebuffer or embedded guest web application.
- Guest agent/RPC and automatic service discovery.
- Multiple concurrent worlds, snapshots, and host-to-guest file transfer.
- Enterprise trust deployment, cloud catalogs, bundle auto-updates, or non-generic hardware/HIL.

Live task status and dependencies are tracked in [Ergo](agent-workflow.md); this roadmap remains a delivery summary. Run `ergo list --ready` to choose work.

## Working rules

- Keep the CLI and shared Rust service usable as the reference path.
- Use CI for the Linux x64 and arm64 guest boot environment; don't make a local QEMU setup a merge prerequisite.
- Use the acceptance IDs and work-package dependencies in [app-spec.md](app-spec.md) for implementation and review.
- Keep planned milestones aligned with user-approved Ergo epics; mark work delivered only when merged code or observed CI results support the change.
