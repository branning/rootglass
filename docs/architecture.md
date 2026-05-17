# Architecture

Rootglass is a desktop shell around a contained embedded Linux world. The application should feel like a normal desktop app while hiding the mechanics of QEMU, kernel selection, device-tree selection, root filesystem images, networking, ports, and lifecycle management.

## System overview

```text
+-----------------------------+
| Tauri desktop shell          |
| - launcher UI                |
| - status and lifecycle       |
| - console/log view           |
| - controls                   |
+--------------+--------------+
               |
               | Tauri commands / events
               v
+-----------------------------+
| rootglass-core               |
| - bundle validation          |
| - runtime state directory    |
| - process supervision        |
| - lifecycle state machine    |
| - event stream               |
+--------------+--------------+
               |
               | launch profile
               v
+-----------------------------+
| rootglass-qemu               |
| - QEMU binary resolution     |
| - command construction       |
| - stdio/log capture          |
| - shutdown/restart handling  |
+--------------+--------------+
               |
               | boots
               v
+-----------------------------+
| Guest world                  |
| - aarch64 Linux kernel       |
| - device tree                |
| - swappable rootfs           |
| - embedded app/service       |
+-----------------------------+
```

## Major components

### Tauri desktop shell

The shell owns the user experience. It should avoid exposing QEMU or Linux details unless the user asks for diagnostics.

Expected responsibilities:

- Bundle selection and recent bundle list.
- Start, stop, restart, and reset controls.
- Current world status: stopped, validating, starting, booting, running, stopping, failed.
- Console and log display.
- Error rendering with suggested support actions.
- Optional embedded webview or console view into the guest application.

### rootglass-core

The core crate is the host-side application runtime. It should be UI-agnostic and testable outside Tauri.

Expected responsibilities:

- Parse and validate bundle manifests.
- Resolve kernel, device tree, rootfs, and runtime profile paths.
- Prepare per-run state directories.
- Allocate host resources such as ports and temporary files.
- Start and supervise the QEMU adapter.
- Translate low-level process events into lifecycle events for the UI.
- Handle shutdown, restart, and cleanup.

### rootglass-manifest

The manifest crate should define the stable package contract for runnable worlds.

Expected responsibilities:

- Schema definition.
- Versioning and compatibility checks.
- Friendly validation errors.
- Future migration hooks.

### rootglass-qemu

The QEMU adapter should keep emulator-specific behavior isolated.

Expected responsibilities:

- Locate bundled or system QEMU binaries.
- Build launch commands from a typed profile.
- Capture stdout/stderr/serial streams.
- Support graceful shutdown where possible.
- Preserve enough command details for diagnostics.

## Bundle model

A Rootglass bundle should be a directory or archive that contains a manifest and one or more assets.

Example shape:

```text
sample-world.rootglass/
  rootglass.toml
  rootfs.img
  metadata/
    icon.png
    screenshots/
```

The manifest should describe what the bundle needs, not hard-code host-specific paths.

## Lifecycle model

Proposed states:

```text
stopped
validating
ready
starting
booting
running
stopping
failed
```

Failures should include:

- Human-readable summary.
- Technical detail for logs.
- Stage where the failure occurred.
- Suggested next action when available.

## First implementation bias

Prefer boring implementation choices:

- One QEMU process per running world.
- One local bundle selected from disk.
- One serial console/log stream.
- One rootfs image per bundle.
- No custom guest agent in the first pass.
- No attempt to model target peripherals beyond what the stock kernel/device tree profile supports.

## Open design questions

- Should QEMU be bundled with the app per platform or discovered from the host?
- What rootfs image formats should be accepted first?
- How should a guest app expose a UI to the host shell: framebuffer, serial, HTTP forwarded from the guest, or a dedicated guest agent?
- What should reset mean: restart QEMU, restore rootfs from pristine copy, or both?
- How should large bundle distribution work for nontechnical users?
