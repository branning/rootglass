# libghostty-vt and Ratatui host spike

This is a disposable S16.6 prototype, not the production Rootglass TUI or GPUI bridge. It pins the public `libghostty-vt` API to Ghostty tag `v1.0.0`, commit `0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec`.

The fixture runs a small Ratatui program in a Linux PTY. Its parent parses the PTY output with `GhosttyTerminal`, reads render state, encodes a key and bracketed paste with libghostty-vt, resizes the PTY and terminal grid, copies a selected range with the public formatter, then renders the parsed screen through Ratatui's `TestBackend`. The test also waits for the child to exit before dropping its PTY and terminal handles.

## Result and boundary

The headless Linux x86_64 smoke passes for colored VT output, row/cell render state, a left-arrow key event, resize, selected-text copy, bracketed paste, Ratatui `TestBackend`, and PTY child cleanup. The C shim calls only public `libghostty-vt` headers.

Observed fixture values, in order:

```text
initial screen: RATATUI-GHOSTTTY-FIXTURE / LEFT:0 / SIZE:80x24
render-state first cell: styled
encoded left key: LEFT:1
selected-text copy: LEFT:1
PTY resize: SIZE:90x30
bracketed paste: PASTE:headless paste
shutdown: child exits with status 0
```

`libghostty-vt` does not create a PTY and does not draw. It provides terminal state, render rows/cells, selection/formatting, key and paste encoding; the application owns the PTY and renderer. The prototype confirms that a custom renderer can read the public render state, but it is not an actual GPUI surface, native clipboard integration, guest serial session, focus/accessibility check, or graphical host smoke. Those checks remain required before S16.7 and before claiming a supported desktop platform.

The full `libghostty` C header at [`include/ghostty.h`](https://github.com/ghostty-org/ghostty/blob/8265e74cd3945c56d59aba3917ff01af33ba3e78/include/ghostty.h) says it is an internal embedder API tailored to the macOS app. This spike does not depend on it. The maintainer approved Rootglass building its own GPUI renderer and PTY/session adapter around `libghostty-vt`; the supported platform set still requires native qualification.

## Reproduce

The upstream v1.0.0 packaging guide requires Zig 0.17.0. This run used Zig 0.17.1. Obtain the v1.0.0 source and dependency archives from the Ghostty release, verify the signed `SHA256SUMS` manifest with Ghostty's published minisign key, and follow [the v1.0.0 packaging guide](https://github.com/ghostty-org/ghostty/blob/vt-1.0.0/PACKAGING-LIBGHOSTTY.md). From the extracted library source directory, build the shared library; then run the fixture from the Rootglass checkout:

```sh
zig build --prefix "$PWD/out" --system "/path/to/libghostty-vt-deps" \
  -Doptimize=ReleaseFast -Dcpu=baseline
GHOSTTY_VT_PREFIX="/path/to/libghostty-vt-source/out" cargo test --manifest-path \
  prototypes/libghostty-vt-ratatui/Cargo.toml -- --nocapture
```

The prototype build also needs a C compiler and Linux `libutil` (for `forkpty`). It links the shared `libghostty-vt.so` from the supplied prefix. It is intentionally Linux-only; the checked fixture uses `forkpty`, `ioctl(TIOCSWINSZ)`, and POSIX process cleanup.

## ABI, platforms, and licenses

- The public library version is `1.0.0`; the C ABI is stable within major version 1 and the Linux shared library uses SONAME `libghostty-vt.so.1`. The build and CI should pin a specific upstream tag and consume the signed source/dependency artifacts.
- The v1.0.0 packaging guide lists Tier 1 targets as Linux x86_64 and aarch64 (glibc and musl), macOS 13+, and Windows x86_64 GNU. Those are upstream library targets, not Rootglass embedding claims. This PTY fixture only runs on Linux.
- Ghostty is MIT. The packaged dependencies and notices listed by the upstream guide include Highway (`Apache-2.0 OR BSD-3-Clause`), simdutf (`Apache-2.0 OR MIT`), translate_c (`MIT`), uucode (`MIT AND Unicode-3.0`), and Wuffs (`Apache-2.0 OR MIT`). ARO is build-only (`MIT AND Unicode-3.0`). Preserve the upstream license files and notices in any shipped package.
- The focused [`libghostty-vt smoke` workflow](../../.github/workflows/libghostty-vt-smoke.yml) builds the library from the signed v1.0.0 source and dependency tarballs, verifies the Zig 0.17.0 archive checksum, and runs this PTY test on Ubuntu 24.04 x86_64. It runs when the prototype, build script, or workflow changes and can also be dispatched manually. The standard workspace CI does not build Zig or this isolated prototype.
- macOS and Windows host checks need separate native runners and GPUI integration; passing this Linux test does not qualify those platforms or any graphical surface.

The spike leaves S16.6 open for graphical-host validation and an evidence-backed supported-platform set.
