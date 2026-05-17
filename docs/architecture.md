# Architecture

Rootglass is a desktop shell around a contained embedded Linux world. The application should feel like a normal desktop app while hiding the mechanics of QEMU, kernel selection, device-tree selection, root filesystem images, writable overlays, networking, ports, signatures, and lifecycle management.

## System overview

```text
+-----------------------------+
| Tauri desktop shell          |
| - launcher UI                |
| - status and lifecycle       |
| - console/log view           |
| - controls                   |
| - publisher/trust display    |
+--------------+--------------+
               |
               | Tauri commands / events
               v
+-----------------------------+
| rootglass-core               |
| - bundle validation          |
| - signature verification     |
| - runtime state directory    |
| - overlay selection          |
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
| - accelerator selection      |
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
| - read-only rootfs           |
| - selected writable overlay  |
| - embedded app/service       |
+-----------------------------+
```

## Major components

### Tauri desktop shell

The shell owns the user experience. It should avoid exposing QEMU or Linux details unless the user asks for diagnostics.

Expected responsibilities:

- Glass selection and recent glass list.
- Start, stop, restart, and reset controls.
- Current world status: stopped, validating, starting, booting, running, stopping, failed.
- Console and log display.
- Error rendering with suggested support actions.
- Publisher identity and signature verification display.
- Local writable overlay selection when multiple overlays exist.
- Optional embedded webview or console view into the guest application.

### rootglass-core

The core crate is the host-side application runtime. It should be UI-agnostic and testable outside Tauri.

Expected responsibilities:

- Parse and validate glass manifests.
- Verify required bundle signatures.
- Resolve builtin kernel, device tree, rootfs, selected writable overlay, and runtime profile paths.
- Prepare per-run state directories.
- Track local writable overlays for each glass.
- Allocate host resources such as ports and temporary files.
- Check host platform constraints such as required hardware acceleration.
- Start and supervise the QEMU adapter.
- Translate low-level process events into lifecycle events for the UI.
- Handle shutdown, restart, and cleanup.

### rootglass-manifest

The manifest crate should define the stable package contract for runnable worlds.

Expected responsibilities:

- Schema definition.
- Versioning and compatibility checks.
- Friendly validation errors.
- Host platform constraint modeling.
- Publisher/signature metadata modeling.
- Future migration hooks.

### rootglass-qemu

The QEMU adapter should keep emulator-specific behavior isolated.

Expected responsibilities:

- Locate bundled or system QEMU binaries.
- Build launch commands from a typed profile.
- Select host acceleration when required and available.
- Attach the read-only rootfs and selected writable overlay.
- Capture stdout/stderr/serial streams.
- Support graceful shutdown where possible.
- Preserve enough command details for diagnostics.

## Bundle model

A Rootglass bundle is called a **glass**. The serialized extension is `.glass`.

A glass should be a directory or archive that contains a manifest and one or more assets.

Example development shape:

```text
sample-world.glass/
  rootglass.toml
  rootfs.img
  state.img
  metadata/
    icon.png
    screenshots/
  signatures/
    bundle.sig
```

The manifest should describe what the bundle needs, not hard-code host-specific paths.

## Storage model

Rootglass should model storage as:

- one required read-only rootfs, and
- zero or one selected writable overlay at launch time.

A serialized `.glass` bundle may include at most one writable overlay partition. The local Rootglass application may create and track multiple writable overlays for the same glass, but only one overlay may be loaded at a time.

Future versions may support multiple read-only overlay packs, but this is not part of the MVP.

## Runtime profile model

For the MVP, a glass should rely on a Rootglass builtin runtime profile for kernel and device-tree selection. Optional kernel/device-tree overrides are reserved for a future version and should not be accepted in the initial implementation.

The first likely profile is a generic aarch64 Linux profile using QEMU's `virt` machine.

## Lifecycle model

Proposed states:

```text
stopped
validating
verifying
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
- One local glass selected from disk.
- One serial console/log stream.
- One required read-only rootfs per glass.
- At most one writable overlay selected at launch.
- Builtin kernel and device-tree profile only.
- Required signature verification path, even if the first trust policy is simple.
- Explicit failure if required host acceleration is unavailable.
- No custom guest agent in the first pass.
- No attempt to model target peripherals beyond what the stock kernel/device tree profile supports.

## Open design questions

- Should QEMU be bundled with the app per platform or discovered from the host?
- What rootfs image formats should be accepted first?
- Which signature format should be required for `.glass` bundles?
- How should a guest app expose a UI to the host shell: framebuffer, serial, HTTP forwarded from the guest, or a dedicated guest agent?
- What should reset mean: restart QEMU, restore writable overlay from pristine copy, create a fresh overlay, or some combination?
- How should large bundle distribution work for nontechnical users?
