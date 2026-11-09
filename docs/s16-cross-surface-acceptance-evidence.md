# S16.8 cross-surface acceptance evidence

**Ergo:** `E6SUUH` · S16.8. **Acceptance:** A-02, A-05, A-07, A-08.
**Change:** PR [#111](https://github.com/branning/rootglass/pull/111).

## Embedded guest workflow

The Linux embedded-terminal scenario passed in [workflow run 42672997313](https://github.com/branning/rootglass/actions/runs/42672997313) on Ubuntu 24.04 x86_64. The run tested commit `7f9bf8732b7ff2c9172c8acec1b18dc36465da6b`; the evidence artifact [13699786813](https://github.com/branning/rootglass/actions/runs/42672997313/artifacts/13699786813) contains four screenshots, the TUI trace, the embedded application log, and two QEMU logs. It expires 2026-11-23.

The workflow materialized the signed Hello World guest, built Rootglass with `--features embedded-terminal`, and ran the GPUI/libghostty session under Xvfb. The smoke imported the archive, validated its manifest and verified launch plan, opened state and trust diagnostics, created a named state, signed the directory, created and verified a `.glass` package using an external key-file path, booted the guest, logged in, ran `uname -a`, restarted and stopped the guest, and verified terminal and QEMU cleanup. The guest reported Linux `6.18.60-0-virt` on `aarch64`.

The exact workflow commands were:

```sh
bash scripts/materialize-example-assets.sh
cargo build --locked --manifest-path crates/rootglass-desktop/Cargo.toml \
  --features embedded-terminal
xvfb-run -a -s "-screen 0 1280x800x24 +extension GLX +render -noreset" \
  dbus-run-session -- bash scripts/smoke-rootglass-embedded-guest-xvfb.sh \
  crates/rootglass-desktop/target/debug/rootglass \
  crates/rootglass-desktop/artifacts/embedded-guest-xvfb
```

The workflow supplies `GHOSTTY_VT_PREFIX` and `LD_LIBRARY_PATH` from its cached libghostty-vt 1.0.0 build. The PTY smoke and release-package jobs run separately; the embedded guest job is Linux-only and workflow-dispatch opt-in outside the qualifying PR event.

## Standalone TUI and shared workflow tests

The TUI controller and Ratatui renderer are covered by the 52 tests in `rootglass-tui`, including real service validation/preflight, bundle inspection, trust and named-state controls, serial interaction, lifecycle transitions, signing/package verification, and cleanup. The desktop CI also runs the CLI TUI terminal-lifecycle tests.

| Workflow | Standalone Ratatui session | GPUI/libghostty embedded session |
|---|---|---|
| Open/import, verify, validate and plan | Controller/service tests; the installed v0.0.3 TUI smoke opens and plans the signed example | Linux artifact run: import, manifest validation, verified plan and trust diagnostics |
| Named state and developer sign/package | State/controller tests and developer signing/package tests | Linux artifact run: create a named state, sign a directory, package and verify a `.glass` archive using an external key-file path |
| Guest boot, serial I/O, restart and stop | v0.0.3 macOS standalone TUI smoke: boot, login, run `uname -a`, stop, and QEMU cleanup | Linux artifact run: boot, login, run `uname -a`, restart, stop, and cleanup |

The focused headless suite passed on this change with:

```sh
cargo test --locked --manifest-path crates/rootglass-tui/Cargo.toml
```

The standalone TUI guest boot passed on the exact v0.0.3 macOS arm64 package in [run 42560261893](https://github.com/branning/rootglass/actions/runs/42560261893), with the [TUI screenshot and serial/QEMU evidence](https://github.com/branning/rootglass/actions/runs/42560261893/artifacts/13648873558). It verified the bundled QEMU signature, launched the signed example, logged in, confirmed `uname -a` reported Alpine aarch64, stopped the guest, and verified that no QEMU process remained. The maintainer also confirmed a GUI boot and serial `uname -a` on that package; see [S13.2 evidence](s13-desktop-smoke-evidence.md).

## Scope and remaining qualification

The embedded-terminal feature is opt-in and is not enabled or bundled by release package workflows. This evidence qualifies the feature's Linux x86_64 path. The v0.0.3 macOS result qualifies the standalone packaged TUI guest flow; it does not qualify an embedded GPUI terminal on macOS. Native macOS and Windows GPUI hosts, native focus/accessibility/usability review, and a platform-support decision remain outside these results. Native UX review is deferred to later Figma-led work.
