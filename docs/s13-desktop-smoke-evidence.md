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
xattr -dr com.apple.quarantine /Applications/Rootglass.app
```

Quarantine must be removed recursively because clearing only the top-level app attribute can leave nested files quarantined. The maintainer then confirmed the v0.0.2 bundled arm64 QEMU is killed with signal 9 and `codesign --verify --strict --verbose=4` reports `invalid signature (code or signature have been modified)`. The packaging script had modified QEMU's Mach-O rpaths without re-signing it. PR [#102](https://github.com/branning/rootglass/pull/102) re-signs and verifies the relocated binary and libraries. macOS package run [42553442626](https://github.com/branning/rootglass/actions/runs/42553442626) and installed TUI smoke [42553738230](https://github.com/branning/rootglass/actions/runs/42553738230) then passed: the installed app verified the QEMU signature, booted the signed example under TCG, accepted guest login and `uname -a`, stopped, and left no QEMU process. The smoke [artifact](https://github.com/branning/rootglass/actions/runs/42553738230/artifacts/13646302229) includes the TUI screenshot and diagnostic logs. This is macOS hosted-runner evidence for the TUI flow; native GPUI interaction, focus, accessibility, and clean-host qualification remain open.

## v0.0.3 tagged preview

PR [#104](https://github.com/branning/rootglass/pull/104) bumped the desktop
package to v0.0.3 and documented the v0.0.2 signature failure. The all-platform
package run [42556684965](https://github.com/branning/rootglass/actions/runs/42556684965)
passed from tag `v0.0.3` at `6cf2239f49cabf1689e19ada1aa76b3ee7faacc1`; the
draft prerelease now has all 14 expected release assets: three installers,
three runtime archives, checksums, and the signed example.

The exact tagged macOS package then passed the installed TUI smoke in run
[42560261893](https://github.com/branning/rootglass/actions/runs/42560261893).
It verified the bundled QEMU executable and libraries, installed the DMG app
into `/Applications`, checked the sample checksum, selected the expected TCG
fallback, booted and logged in to the signed Hello World guest, confirmed
`uname -a` reported Alpine aarch64, and verified Stop left no QEMU process. The
[smoke evidence artifact](https://github.com/branning/rootglass/actions/runs/42560261893/artifacts/13648873558)
contains the TUI screenshot, serial output, QEMU logs, and guest `uname` output.
The maintainer also manually opened the downloaded v0.0.3 DMG, booted Hello
World in the GUI, and confirmed the guest kernel from the serial console. The
draft remains unpublished pending maintainer approval; the app is unsigned and
not notarized.

## Scope decision and follow-on

On 2026-11-09, the maintainer accepted the evidence above as sufficient to close
GOMUP3: the Linux Xvfb run exercises the full GPUI workflow, the exact v0.0.3
macOS arm64 package passes the installed TUI guest boot, and the maintainer
manually confirmed that the v0.0.3 GUI boots Hello World and accepts serial
input (`uname -a` reported Alpine aarch64). The Xvfb screenshots and macOS TUI,
serial, QEMU, and guest evidence are linked above.

This closes the desktop workflow smoke objective. Native interaction and
usability work—including focus, native dialogs, accessibility, resizing, and
additional host review—moves to subsequent Figma-led work. This evidence does
not qualify native GUI behavior on every supported operating system or decide
the final platform support matrix. Screenshot artifacts have a 14-day
retention period; the package section lists artifact-specific expiry dates.
