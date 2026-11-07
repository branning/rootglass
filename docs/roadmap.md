# Rootglass delivery roadmap

**Status:** Updated 2026-11-07 against the merged `main` implementation. This roadmap tracks delivery and acceptance; the detailed product and technical contract remains in [app-spec.md](app-spec.md).

Rootglass is a native Rust desktop app built with GPUI Kit `=0.9.0`, backed by a shared Rust application service and CLI. CI is the functioning test environment for the reference guest: hosted x64 and arm64 TCG jobs materialize and boot it through the CLI. A developer machine run is useful, but not a merge gate.

## Delivered implementation

The merged work packages S00–S11 establish the CLI reference path and the first GPUI desktop slice:

- Signed `.glass` fixtures, manifest validation, signature verification and CLI workflows.
- QEMU supervision and shared application orchestration used by the CLI and desktop.
- ZIP import, installed-glass library, trust reporting, persistent instance-state handling, and runtime asset resolution.
- GPUI Kit desktop window with library/selection, bundle details, lifecycle controls, and live guest console integration.
- Cross-platform package builds for Linux, macOS, and Windows, with checksums and `.glass` file association metadata.

These are implementation milestones, not a claim that every v1 acceptance test has passed. In particular, see the open gates below and [desktop-packages.md](desktop-packages.md).

## Current milestone — GPUI v1 qualification

**Goal:** turn the merged GPUI vertical slice into a user-verified desktop workflow and close acceptance A-08.

1. Exercise open/import, trust and manifest details, preflight, boot, serial input/output, stop, restart, and failure recovery using the real application service.
2. Add or complete headless UI-model tests for action availability and error states; add GUI smoke coverage for file-open, verification, Boot/Stop, and console interaction where supported.
3. Check that filesystem work, signature verification, archive extraction, and process waits stay off the render thread; exercise cancellation and stale-event handling.
4. Record per-platform results and any narrower supported-host claims in the app spec.

**Exit gate:** A-08 passes on supported desktop runners or has a documented, evidence-backed platform limitation. Core behavior remains covered by deterministic service/supervisor tests; GUI automation is reserved for user-visible integration.

## Next milestone — installed app works on a clean host

**Goal:** close A-09 for each shipping operating system.

1. Decide and implement the installed-app QEMU and versioned runtime asset inventory. The current packages do not include a versioned QEMU/runtime set.
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
- Update this roadmap only when merged code or observed CI results change the delivery state.
