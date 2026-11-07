# TUI architecture and feature parity

**Status:** Planned work in Ergo epic [S16](agent-workflow.md). No Ratatui or libghostty TUI implementation is shipped yet. The existing GPUI shell and `rootglass-app` service remain the observed baseline.

## Product contract

Rootglass will have a standalone Ratatui frontend available through `rootglass tui`. It must cover the existing Rootglass command and app workflows, including bundle verification/import, the installed library, trust/preflight reporting, run lifecycle, persistent state after its app-service contract is complete, the guest serial console, and developer signing/packaging. A supplied bundle path should be accepted by the TUI entry point so a platform file association can pass a `.glass` path through the existing package workflow.

The TUI must be testable without a display server or a real terminal. Keep its controller and renderer independent of GPUI, and exercise rendering with Ratatui's `TestBackend` and a deterministic fake service. Keep filesystem, signature, import, and process work off the input/render loop.

The GPUI application will host the TUI through an embedded terminal surface backed by libghostty. The terminal host handles terminal input, output, resize, clipboard, and lifecycle; the TUI session owns Rootglass interaction and calls shared Rust APIs. The session adapter must not expose an arbitrary shell. The terminal host is the *outer* terminal that displays the Ratatui app; the TUI's guest-console pane separately reads and writes the guest's serial stream through `rootglass-app`.

The TUI and GPUI host must not reimplement signature/trust policy, registry transactions, state locking, launch checks, or lifecycle rules. The TUI session calls `rootglass-app` APIs; signing/package flows call `rootglass-signature` APIs. Do not shell out to the CLI to perform an individual Rootglass operation. Whether the embedded session runs in-process or behind a private PTY process is gated by the libghostty prototype.

## Frontend map

| Rootglass workflow | Existing source of behavior | TUI surface | Acceptance / dependency |
|---|---|---|---|
| `validate` / open | `Glass::load`; `AppService::inspect` verifies publisher trust and signed files | Inspect and verification screen; do not call a manifest-only result “trusted” | F-01/F-03; S16.3 |
| `plan` / preflight | `AppService::launch_plan` and `preflight` | Launch summary, readiness and actionable blockers | F-07–F-11; S16.3 |
| `boot` / stop / restart | `AppService::start`, `poll_event`, `write_serial`, `stop`; controller composes restart while preserving `RunOptions` | Lifecycle controls and guest serial console with input, output, errors and cleanup | F-12–F-17; S16.3–S16.4, A-07/A-08 |
| Open and import `.glass` | `AppService::import_archive`, `PackageLibrary`; bounded verifier and registry | Open directory or ZIP, verify, import, and return to the installed release | F-01–F-06/F-26; S12.1 and S16.3 |
| Installed releases | `installed_glasses`, `remove_installed_glass`, import by digest | List/filter, select, reimport/update and remove | F-05/F-06; S16.3 |
| Persistent state | `InstanceStateStore` and `RunOptions::state_id`; public CRUD/confirmation flow is not yet exposed through `AppService` | List/create/clone/select/reset/delete with confirmations and active-run locks | F-18–F-22; wait for S12.2, then S16.5 |
| Publisher trust | Local trust config and fail-closed verifier; settings/rotation policy remains with S12.4 | Read-only trust status and diagnostics; do not ask for key material | F-23/F-24; S12.4 and S16.5 |
| `sign` / `package` | `rootglass_signature::{sign_glass_directory, package_glass_directory}` and CLI wrappers | Separate developer workflow for sign/package and verification report | F-23/F-25/F-26; S16.9 |
| OS file-open association | Desktop packaging declares `.glass`; app argument handoff and clean-host behavior remain S14/A-09 evidence | `rootglass tui <bundle-path>` accepts a bundle path | F-02; S14 and S16.3 |

The current `rootglass validate` command only validates the manifest; it does not verify the DSSE signature or publisher trust. The TUI's inspect/verify flow must call `AppService::inspect` and make the stronger result explicit. The current service exposes installed-library list/remove and state selection at run time, but not state CRUD through `AppService`; S12.2 must establish that contract before TUI state controls ship. The current desktop controller composes restart as stop/start; there is no separate public restart method.

Production TUI startup must follow the desktop's trust configuration behavior: load the local publisher trust config and fall back to `AppService::deny_all()` with a visible blocked state if the config is missing or invalid. Do not use `AppService::new()` in production mode because its default trust store contains the example fixture key. The proposed offline policy is in [ADR-0001](adr/0001-offline-publisher-trust.md); the desktop's config-path resolver is currently private to its UI model, so S16.3 should share or safely reuse that resolution rule instead of introducing a second platform-specific path.

The TUI must preserve signed release immutability and the existing development-only `.img` exception boundary. In particular, production signing/package operations cover generated `.img` assets as signed subjects, and the current signing API reads keys from an external secret file. Never paste, store in TUI state, or render private key bytes. Agent-backed signing requires an explicit library API before it is offered.

## GPUI and libghostty contract

Keep GPUI Kit `=0.9.0` as the application-window framework. Ratatui is the TUI renderer/controller; libghostty is only the embedded terminal host. The TUI and host communicate over a full-duplex terminal session, including key input, terminal output, resize and clipboard operations. The adapter owns the terminal-session/PTY lifecycle and shuts it down with the GPUI window.

The exact libghostty surface is not yet approved or proven cross-platform. Upstream [`include/ghostty.h`](https://github.com/ghostty-org/ghostty/blob/main/include/ghostty.h) labels the full embedding C API internal, says the macOS app is its only consumer, and directs external embedders to [`libghostty-vt`](https://github.com/ghostty-org/ghostty/tree/main/include/ghostty/vt). Upstream [`src/apprt/embedded.zig`](https://github.com/ghostty-org/ghostty/blob/main/src/apprt/embedded.zig) describes the embedded runtime used by the macOS Swift/Xcode app. The repository is MIT licensed, but its transitive libraries and platform packaging still need review.

S16.6 must pin an upstream source version/commit and prove that the documented external API supplies the render, input, resize, clipboard and lifecycle needs on each proposed host. It must record the C ABI/build/link route, dependency notices, and a maintainer decision on supported platforms. Do not claim Windows/Linux embedding or depend on the internal API without an explicit maintainer decision. If `libghostty-vt` requires a GPUI renderer adapter, demonstrate that before implementing S16.7; do not silently replace libghostty with the standalone Ghostty app or another emulator.

## Headless and host validation

Headless checks cover the same TUI controller and rendering code used by the standalone frontend: Ratatui `TestBackend`, deterministic service fakes, temporary library/state/trust directories, stale-operation and `RunId` handling, serial input/output, and fake-child cleanup. These checks do not need X11, a native window, or QEMU.

Separate host smoke is still required for the embedded libghostty surface: keyboard mapping, focus/shortcuts, resize, clipboard, accessibility, terminal cleanup, and a real guest serial login/input/output flow. A missing desktop host is a blocked acceptance result, never a headless pass. Reuse existing A-02/A-05/A-07 evidence only when its revision and scenario still apply. Keep the R3 deferred features outside this workstream.
