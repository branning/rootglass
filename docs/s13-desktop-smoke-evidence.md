# S13.2 desktop smoke evidence

**Ergo:** `GOMUP3` · S13.2; **Acceptance:** A-08.
**Change:** PR [#76](https://github.com/branning/rootglass/pull/76), merged as [`819a0eb`](https://github.com/branning/rootglass/commit/819a0eb15b6d9d63d5a575d96733314e782412d9) from head `55da624ec023eff2aca4d21e64e6bbe3a71bcc1d`.

## Results

The required CI run [42482901592](https://github.com/branning/rootglass/actions/runs/42482901592) passed Format, Clippy, Tests, Linux desktop compilation, CLI smoke, and x64/arm64 hosted boot smoke.

The full smoke run [42482916674](https://github.com/branning/rootglass/actions/runs/42482916674) passed the GPUI desktop workflow on Ubuntu 24.04 x86_64 under Xvfb and the libghostty PTY smoke on Linux x86_64, macOS arm64, and Windows x86_64. The desktop scenario exercised picker cancellation, file drop, trusted/untrusted/tampered bundle handling, preflight, guest start, serial input/response, restart, stop, resize, screenshots, window exit, and child cleanup.

The desktop workflow built and ran the single `rootglass` executable for both the GPUI shell and CLI operations:

```sh
cargo build --locked --manifest-path crates/rootglass-desktop/Cargo.toml
bash scripts/materialize-example-assets.sh
xvfb-run -a -s "-screen 0 1280x800x24 +extension GLX +render -noreset" \
  dbus-run-session -- bash scripts/smoke-rootglass-desktop-xvfb.sh \
  crates/rootglass-desktop/target/debug/rootglass \
  artifacts/gpui-desktop-xvfb
```

Screenshot and log artifacts from that run:

- [GPUI desktop workflow screenshots and log](https://github.com/branning/rootglass/actions/runs/42482916674/artifacts/13618535851)
- [Embedded Rootglass TUI screenshot](https://github.com/branning/rootglass/actions/runs/42482916674/artifacts/13618675310)
- [GPUI libghostty host screenshot](https://github.com/branning/rootglass/actions/runs/42482916674/artifacts/13617379526)

## macOS review package

Manual package run [42520653157](https://github.com/branning/rootglass/actions/runs/42520653157) passed for `main` at `26fabafe9e4335065fb29bde7b11cef12fe53250`. It produced a macOS arm64 DMG named `Rootglass_0.0.1_aarch64.dmg`, along with its checksum and bundled QEMU runtime:

- [macOS arm64 DMG and checksum](https://github.com/branning/rootglass/actions/runs/42520653157/artifacts/13632713661) — available through 2026-11-22 20:38 UTC.
- [Signed example `.glass` bundle](https://github.com/branning/rootglass/actions/runs/42520653157/artifacts/13633312455) — available through 2026-11-22 20:34 UTC.
- [Offline Alpine guest boot assets](https://github.com/branning/rootglass/actions/runs/42520653157/artifacts/13633481163) — available through 2026-11-09 20:34 UTC.

The DMG is unsigned and not notarized. To install it, download and extract the DMG artifact ZIP, open the DMG, and drag `Rootglass.app` to Applications. If macOS reports that the installed app is damaged, use the documented [quarantine workaround](../README.md#macos-preview-first-launch-caveat):

```sh
xattr -d com.apple.quarantine /Applications/Rootglass.app
```

This is an installable review package, not native macOS GUI qualification: the job built and packaged the app but did not launch it or check native interaction, focus, or accessibility.

## Remaining acceptance

The GUI workflow ran on Linux under Xvfb; the macOS and Windows runs were PTY smoke, not native GUI qualification. A maintainer still needs to review native-host interaction, focus, accessibility, and overall usability, then approve or narrow the supported platform set. Keep GOMUP3 open until those decisions and any required host evidence are recorded. Screenshot artifacts have a 14-day retention period; the package section lists artifact-specific expiry dates.
