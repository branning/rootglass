# Rootglass

Tiny worlds under glass.

Rootglass is a cross-platform desktop shell for running self-contained embedded Linux applications on a regular computer. It presents a friendly desktop window around an emulated aarch64 system, booted with QEMU using an off-the-shelf Linux kernel and device tree, while allowing the application root filesystem to be swapped independently.

The goal is a nontechnical user workflow: download an app bundle, double-click it, and get a window into a complete embedded environment without learning Docker, QEMU, command-line launch scripts, or board bring-up details.

## Product shape

Rootglass is composed of three layers:

1. **Desktop shell** — a Tauri application that provides the user-facing launcher, status UI, logs, controls, and app window.
2. **Runtime core** — Rust code that validates bundles, prepares runtime state, starts and supervises QEMU, exposes lifecycle events, and manages host resources.
3. **World bundle** — a packaged embedded application environment containing a manifest, root filesystem image, optional metadata, and references to the kernel/device tree/runtime profile required to boot it.

## Design goals

- One-click desktop workflow for nontechnical users.
- Cross-platform host support: macOS, Windows, and Linux.
- Swappable root filesystems for different embedded applications.
- A stable host application that can run many app bundles over time.
- Prefer off-the-shelf kernels and device trees at first.
- Keep QEMU details behind a small Rust runtime interface.
- Make logs and failure states visible enough for support/debugging.
- Avoid turning the first version into a full board simulator or hardware-in-the-loop framework.

## Non-goals for the first version

- Cycle-accurate hardware simulation.
- Arbitrary peripheral modeling.
- Full Docker replacement.
- Multi-node distributed simulation.
- Real-time guarantees.
- Recreating every target-board boot detail.

## Initial repository layout

```text
rootglass/
  apps/
    desktop/              # Tauri UI shell
  crates/
    rootglass-core/       # Runtime orchestration
    rootglass-manifest/   # Bundle manifest parsing and validation
    rootglass-qemu/       # QEMU launch/supervision adapter
  docs/
    architecture.md
    bundle-format.md
    roadmap.md
  examples/
    hello-world/          # Minimal sample bundle metadata/rootfs recipe later
```

This layout is intentionally docs-first until the first Tauri project and Rust workspace are generated.

## Working vocabulary

- **World** — the running embedded environment visible through Rootglass.
- **Bundle** — the portable package containing a rootfs and launch metadata.
- **Manifest** — the structured file describing how to boot the bundle.
- **Runtime** — the host-side Rust process supervision and QEMU orchestration layer.
- **Shell** — the desktop app around the running world.

## First milestone

The first useful milestone is a local developer prototype that can:

1. Load a bundle manifest from disk.
2. Validate that required assets exist.
3. Start QEMU for an aarch64 Linux image.
4. Attach the QEMU console/log stream to the desktop UI.
5. Stop/restart the world cleanly.
6. Swap to another rootfs by selecting a different bundle.

See [docs/roadmap.md](docs/roadmap.md) for the proposed implementation sequence.
