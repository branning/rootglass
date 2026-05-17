# Roadmap

This roadmap is intentionally biased toward getting a small vertical slice working before turning Rootglass into a broad simulation platform.

## Milestone 0 — Repository seed

- README with project intent.
- Architecture sketch.
- Draft bundle format.
- Initial issue backlog.

## Milestone 1 — Developer boot spike

Goal: prove that a local Rust command can boot an aarch64 Linux world through QEMU from a manifest.

Deliverables:

- Rust workspace.
- Manifest parser for a minimal `rootglass.toml`.
- QEMU launch adapter.
- Local example bundle directory.
- Console log capture to host stdout.
- Manual stop/restart flow.

Success criteria:

- A developer can run one command and boot the sample rootfs.
- Missing files and invalid manifest fields fail before launch.
- The QEMU command is reproducible and visible for debugging.

## Milestone 2 — Tauri shell prototype

Goal: wrap the boot spike in a cross-platform desktop window.

Deliverables:

- Tauri app skeleton.
- Bundle picker.
- Start/stop/restart controls.
- Lifecycle status display.
- Console/log panel.
- Basic error rendering.

Success criteria:

- A user can select a bundle from the UI and boot it.
- The user can stop and restart the world without using a terminal.
- Errors appear in the UI rather than only in logs.

## Milestone 3 — Bundle ergonomics

Goal: make bundles feel like user-facing app artifacts rather than developer directories.

Deliverables:

- Bundle validation report.
- Bundle metadata display: name, version, description, icon.
- Runtime state directory management.
- Rootfs copy/overlay policy decision.
- Example bundle documentation.

Success criteria:

- A malformed bundle gives useful feedback.
- A valid bundle is recognizable in the UI.
- Restart/reset behavior is clearly defined.

## Milestone 4 — Distribution path

Goal: enable nontechnical users to install Rootglass and open a world bundle.

Deliverables:

- Platform-specific build packaging.
- Bundled or resolved QEMU strategy.
- Release artifact plan.
- First signed/notarized build investigation for macOS and Windows.
- Large bundle download/update strategy.

Success criteria:

- A nontechnical test user can install Rootglass and open a provided bundle.
- The app fails gracefully when host platform prerequisites are missing.

## Milestone 5 — Guest application integration

Goal: expose a useful guest application experience rather than only a serial console.

Possible paths:

- Forward a guest HTTP service into a Tauri webview.
- Display a guest framebuffer.
- Add a minimal guest agent for lifecycle/readiness signals.

Success criteria:

- The desktop app can show the embedded application in a way that feels intentional.
- The runtime can tell the difference between QEMU started and guest application ready.

## Deferred ideas

- Bundle signing and trust policy.
- Multiple concurrent worlds.
- Snapshot/restore.
- Guest-agent API.
- Host-to-guest file drop.
- Custom virtual devices.
- Hardware-in-the-loop bridges.
- Automated bundle builder.
- Delta updates for large rootfs images.
