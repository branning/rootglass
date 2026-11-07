# Architecture

Rootglass is a desktop shell around a contained embedded Linux world. The application should feel like a normal desktop app while hiding the mechanics of QEMU, kernel selection, device-tree selection, root filesystem images, writable overlays, networking, ports, signatures, and lifecycle management.

## System overview

```text
rootglass CLI
  validate  ----------------------> rootglass-manifest (manifest only)
  plan/boot ----------------------> rootglass-app
  sign/package -------------------> rootglass-signature
  tui (planned) ------------------> rootglass-tui (Ratatui session)
                                           |
                                           | app workflows
                                           v
GPUI desktop (current) ----------> rootglass-app (shared workflows)
                                           |
                                           v
                         +-------------------------+
                         | rootglass-core          |
                         | policy / launch types   |
                         +------------+------------+
                                      |
                  +-------------------+------------------+
                  |                   |                  |
                  v                   v                  v
           rootglass-qemu   rootglass-signature  rootglass-manifest
                  |
                  | QEMU process / guest serial I/O
                  v
             Booted guest

GPUI terminal bridge (planned): GPUI shell
       <--> libghostty terminal host <--> TUI session
            PTY/session model gated by S16.6 prototype
```

The current GPUI frontend uses [GPUI Kit](https://github.com/longbridge/gpui-kit), Longbridge's curated GPUI distribution and component library built on Zed's native Rust GPUI framework. The desktop crate directly calls `rootglass-app`; it does not shell out to the CLI. Use the exact Cargo requirement `gpui-kit = "=0.9.0"` (the Kit's default `gpui-pre` snapshot backend, corresponding to `gpui-pre` 0.3.11), **not** a direct dependency on Zed's moving `main`, another GPUI fork, or the optional `gpui-fast` backend. GPUI Kit reexports its compatible GPUI types; import from `gpui_kit::*` and `gpui_kit::component` to avoid mixing versions. Upgrade the Kit and its transitive GPUI snapshot together deliberately, with `Cargo.lock` committed. Keep QEMU supervision, signature decisions, and filesystem operations out of GPUI view code. Use asynchronous tasks and event subscriptions so boot and log streaming never block rendering.

The standalone frontend provides `rootglass tui` using Ratatui. Its controller and renderer are display-server independent and exercised with Ratatui `TestBackend`; domain actions go through `rootglass-app`, while signing and packaging use `rootglass-signature`. The planned desktop terminal bridge will host this same TUI session in a custom GPUI view backed by public libghostty-vt. The current desktop remains a direct `rootglass-app` frontend until that bridge is implemented and accepted. See [tui-architecture.md](tui-architecture.md) for workflow coverage, service gaps, async rules and libghostty gates.

The terminal host owns the outer terminal's keyboard, output, resize, clipboard and lifecycle. The TUI session owns Rootglass interaction; its guest-console pane uses `rootglass-app` serial I/O and is separate from the outer terminal's PTY. Filesystem, signature, import, package and process operations must run off the Ratatui draw/input loop. Keep one active-run owner, apply lifecycle events by `RunId`, bound output queues and scrollback, and discard stale completion events. Whether the GPUI bridge is in-process or uses a private PTY child remains open until the libghostty prototype validates the available interface and package model.

Target desktop hosts: macOS, Windows 11, Ubuntu 24.04+. GPUI platform backends differ, so validate builds on all three before treating the shell as cross-platform. GPUI is pre-1.0; treat GPUI Kit 0.9.0 as a tested dependency baseline, not a guarantee of platform compatibility. See [GPUI Kit getting started](https://gpui-kit.com/docs/getting-started/) and [installation](https://gpui-kit.com/docs/installation/).

## Major components

### GPUI desktop shell

The shell owns the user experience. It should avoid exposing QEMU or Linux details unless the user asks for diagnostics.

Expected responsibilities:

- Present the installed library, selected bundle details, trust result and preflight status.
- Start, stop and restart the current run through the shared app service.
- Render lifecycle state, errors, logs and guest serial I/O.
- Keep async service work and process supervision out of view code.
- Continue to use `rootglass-app` directly in the current desktop. Hosting the Ratatui session through libghostty is planned work, not current behavior.

### rootglass-core

The core crate provides UI-independent policy, validated launch requests, runtime-profile and asset resolution, and host preflight. It is used by the app service and is testable without a UI.

Expected responsibilities:

- Resolve a validated manifest into a typed launch request.
- Verify bundle signatures and publisher trust under the supplied trust store.
- Resolve runtime assets and host acceleration requirements.
- Expose lifecycle and user-facing policy types consumed by `rootglass-app`.

### rootglass-app

The application-service crate owns application workflows and process lifecycle. It has no GPUI dependency and is shared by the existing desktop and CLI paths.

Expected responsibilities:

- Inspect and preflight bundles; import verified archives and manage installed-release registry records.
- Own at most one active QEMU run, including start, polling, serial input, stop and cleanup.
- Select and lock per-instance state for a run.
- Publish typed lifecycle, serial and failure events for a frontend.

The standalone TUI must use this service for domain and lifecycle behavior. State record CRUD is not yet exposed as an app-service workflow; S12.2 owns the safe public contract before the TUI adds state controls.

### rootglass-tui (planned)

The planned Ratatui frontend provides `rootglass tui` and reuses the app-service and signature-library behavior rather than invoking individual CLI commands. Keep its controller/rendering boundary headless-testable with Ratatui `TestBackend`. Run filesystem, verification, signing, packaging and process operations asynchronously, bound serial buffering, and filter late results by operation/run ID. Do not expose an arbitrary shell through the GPUI terminal host.

### rootglass-manifest

The manifest crate should define the stable package contract for runnable worlds.

Expected responsibilities:

- Schema definition.
- Versioning and compatibility checks.
- Friendly validation errors.
- Host platform constraint modeling.
- Publisher/signature metadata modeling.
- Future migration hooks.

### rootglass-signature

The signature crate owns DSSE verification, trust configuration/reporting, signing and canonical package creation. `rootglass sign` and `rootglass package` currently call these library APIs from the CLI; the TUI should use the same APIs and obtain key material only from a secret file accepted by the signing library.

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

### rootglass CLI

The existing CLI keeps `validate`, `plan`, `boot`, `sign` and `package`. Its `validate` command checks the manifest only; trusted-publisher verification is provided by `rootglass-app` inspection/launch paths. The planned `tui` command starts the Ratatui session without changing the behavior of these existing commands.

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
