# libghostty-vt and Ratatui host spike

This is a disposable S16.6 prototype, not the production Rootglass TUI or GPUI bridge. It pins the public `libghostty-vt` API to Ghostty tag `v1.0.0`, commit `0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec`.

The fixture runs a small Ratatui program in the host's native PTY (ConPTY on Windows). Its parent parses PTY output with `GhosttyTerminal` and forwards public libghostty-vt terminal replies back through the PTY. It reads render state, encodes a key, resizes the PTY and terminal grid, copies a selected range with the public formatter, then renders the parsed screen through Ratatui's `TestBackend`. Linux and macOS send bracketed paste and exercise Crossterm's `Event::Paste`; Crossterm 0.28's Windows event backend reports ConPTY text as key events, so Windows exercises that text-input path instead. The test also waits for the child to exit before dropping its PTY and terminal handles.

With the optional `gpui` feature, the same package builds a GPUI Kit 0.9 host. Its custom view reads libghostty-vt's parsed screen, sends left-arrow input through Ghostty's key encoder, relays text and bracketed clipboard paste through the PTY, copies the screen to the clipboard, and keeps the PTY and terminal grid sized to the GPUI window. Dropping the GPUI view terminates and reaps the fixture.

## Result and boundary

The native PTY matrix passes on Linux x86_64, macOS arm64, and Windows x86_64 for colored VT output, row/cell render state, a left-arrow key event, resize, selected-text copy, Ratatui `TestBackend`, text input, and PTY child cleanup. Linux and macOS exercise bracketed paste through Crossterm's `Event::Paste`. Windows sends text through ConPTY and checks the key-event path because Crossterm 0.28's Windows event backend does not emit `Event::Paste`. The C shim calls only public `libghostty-vt` headers. Linux desktop CI also compiles the optional GPUI host against GPUI Kit 0.9. The prototype renderer currently draws the formatted screen as plain text and copies the full screen rather than a GPUI selection; it is compatibility evidence, not the production terminal surface.

Observed fixture values, in order:

```text
initial screen: RATATUI-GHOSTTTY-FIXTURE / LEFT:0 / SIZE:80x24
render-state first cell: styled
encoded left key: LEFT:1
selected-text copy: LEFT:1
PTY resize: SIZE:90x30
input: INPUT:headless paste
shutdown: child exits with status 0
```

`libghostty-vt` does not create a PTY or draw. It provides terminal state, render rows/cells, selection/formatting, key and paste encoding; the application owns the PTY and renderer. The GPUI view here is a small custom host around the parser, not the production Rootglass terminal surface. It does not render cell styling, maintain a mouse selection, connect to a booted guest, or prove focus/accessibility on a real graphical host. Those checks remain for S16.7 and S13.2 before claiming a supported desktop platform.

The full `libghostty` C header at [`include/ghostty.h`](https://github.com/ghostty-org/ghostty/blob/8265e74cd3945c56d59aba3917ff01af33ba3e78/include/ghostty.h) says it is an internal embedder API tailored to the macOS app. This spike does not depend on it. The maintainer approved Rootglass building its own GPUI renderer and PTY/session adapter around `libghostty-vt`; the supported platform set still requires native qualification.

## Reproduce

The upstream v1.0.0 packaging guide requires Zig 0.17.0, which the smoke workflow pins. Obtain the v1.0.0 source and dependency archives from the Ghostty release, verify the signed `SHA256SUMS` manifest with Ghostty's published minisign key, and follow [the v1.0.0 packaging guide](https://github.com/ghostty-org/ghostty/blob/vt-1.0.0/PACKAGING-LIBGHOSTTY.md). From the extracted library source directory, build the shared library; then run the fixture from the Rootglass checkout:

```sh
zig build --prefix "$PWD/out" --system "/path/to/libghostty-vt-deps" \
  -Doptimize=ReleaseFast -Dcpu=baseline
GHOSTTY_VT_PREFIX="/path/to/libghostty-vt-source/out" cargo test --manifest-path \
  prototypes/libghostty-vt-ratatui/Cargo.toml -- --nocapture
GHOSTTY_VT_PREFIX="/path/to/libghostty-vt-source/out" cargo run \
  --manifest-path prototypes/libghostty-vt-ratatui/Cargo.toml --features gpui
```

The prototype build needs a C compiler and a host-native `libghostty-vt` build in the supplied prefix. The fixture uses `portable-pty` to select the native PTY implementation, including ConPTY on Windows; it does not call OS-specific PTY or process APIs directly.

## ABI, platforms, and licenses

- The public library version is `1.0.0`; the C ABI is stable within major version 1 and the Linux shared library uses SONAME `libghostty-vt.so.1`. The build and CI should pin a specific upstream tag and consume the signed source/dependency artifacts.
- The v1.0.0 packaging guide lists Tier 1 targets including Linux x86_64, macOS 13+, and Windows x86_64 GNU. The maintainer approved Linux x86_64, macOS arm64, and Windows x86_64 as Rootglass qualification targets. The CI matrix exercises those targets, but upstream library support and a headless PTY pass do not qualify Rootglass's GPUI host.
- Ghostty is MIT. The packaged dependencies and notices listed by the upstream guide include Highway (`Apache-2.0 OR BSD-3-Clause`), simdutf (`Apache-2.0 OR MIT`), translate_c (`MIT`), uucode (`MIT AND Unicode-3.0`), and Wuffs (`Apache-2.0 OR MIT`). ARO is build-only (`MIT AND Unicode-3.0`). Preserve the upstream license files and notices in any shipped package.
- The focused [`libghostty-vt smoke` workflow](../../.github/workflows/libghostty-vt-smoke.yml) builds the library from signed v1.0.0 source and dependency tarballs, verifies the Zig 0.17.0 archive checksum, then runs the PTY fixture on native Linux x86_64, macOS arm64, and Windows x86_64 runners. It caches the installed native prefix per OS, architecture, upstream version, Zig version and build recipe, and uses `rust-cache` for the standalone Cargo target. Linux and macOS also cache the C compiler output used by the Rust build script; the Zig-built library is reused through the native-prefix cache. It still verifies the signed source archives on each run. It runs on PR open/reopen/ready-for-review, on `main`, or by manual dispatch, but not on each PR `synchronize`. Manual dispatch can select all hosts or one OS. The standard Linux desktop CI job restores the native prefix and checks the optional GPUI host without rebuilding Zig; it does not run the graphical host.
- These native PTY builds do not qualify the GPUI surface, native clipboard, focus/accessibility, or any graphical host. Ghostty's packaging guide notes that its Windows library uses the GNU ABI and provides an import library for MSVC applications.

The native PTY matrix passes on the approved target set, and the GPUI host compiles in desktop Linux CI. S16.6 remains open until the GPUI prototype is exercised on a graphical host and its compatibility result is reviewed.
