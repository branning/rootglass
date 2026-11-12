# Rootglass release notes

## Unreleased

## v0.0.6 preview

### Windows import and guest startup

- Open `.glass` archives in the Windows native picker.
- Pass QEMU ordinary Windows paths for runtime resources, kernels, and disks so
  the installed runtime can find its bundled firmware files.
- Use TCG software emulation on Windows because the bundled QEMU does not
  include WHPX support. Windows guests that require hardware acceleration fail
  preflight with a clear unavailable-accelerator error.

### Release assets

Includes Linux x86_64, macOS arm64, and Windows x86_64 installers with matching
versioned offline QEMU runtimes, runtime checksums, and the signed Hello World
`.glass` example with its SHA-256 checksum. The example uses the public
development key and disposable `rootglass-ci` guest password; it is for
verification only. The desktop app does not trust the example key by default.

### Verification status

The [Windows Server 2025 x64 installed smoke](https://github.com/branning/rootglass/actions/runs/43203983228)
verified picker and file-association import, runtime checksums, Alpine boot and
login, `uname -a`, restart, `id`, stop with QEMU cleanup, uninstall, and
retention of user trust, library, and guest-state data. The command probes are
short; longer pasted console lines have not been qualified. This does not
qualify Windows 11 x64 or native Windows 11 ARM64. Linux clean-install
qualification remains open. No general WHPX, HVF, or KVM acceleration claim is
made.

### macOS first launch

The DMG is unsigned and not notarized. After mounting it, drag `Rootglass.app`
to Applications. If Gatekeeper reports that the app is damaged, run
`xattr -dr com.apple.quarantine /Applications/Rootglass.app` in Terminal. This
workaround does not sign or notarize the app.

## v0.0.5 preview

This refreshes the v0.0.4 preview as a new, versioned package set. It contains
no user-facing application behavior changes from v0.0.4.

### Release assets

Includes Linux x86_64, macOS arm64, and Windows x86_64 installers with matching
versioned offline QEMU runtimes, runtime checksums, and the signed Hello World
`.glass` example with its SHA-256 checksum. The example uses the public
development key and disposable `rootglass-ci` guest password; it is for
verification only. The desktop app does not trust the example key by default.

The v0.0.4 macOS arm64 package passed the installed-app offline guest smoke.
Linux and complete Windows 11 clean-host qualification remain open.

### macOS first launch

The DMG is unsigned and not notarized. After mounting it, drag `Rootglass.app`
to Applications. If Gatekeeper reports that the app is damaged, run
`xattr -dr com.apple.quarantine /Applications/Rootglass.app` in Terminal. This
workaround does not sign or notarize the app.

## v0.0.4 preview

### GPUI desktop experience

- Add a native GPUI library for inspecting installed guests and their publisher,
  launch checks, and saved-data status.
- Add publisher review before importing a bundle from an untrusted signer.
- Show guest console, run status, and launch-recovery actions in the desktop UI.
- Open `.glass` files through the installed app's operating-system association.

### Release assets

Includes Linux x86_64, macOS arm64, and Windows x86_64 desktop packages with
matching versioned QEMU runtimes, Alpine guest boot assets, runtime checksums,
and a signed Hello World `.glass` with its SHA-256 checksum. The example uses a
public development key and the disposable `rootglass-ci` guest password; it is
for verification only. The desktop app does not trust the example key by
default; follow the README trust setup before importing the example.

### macOS first launch

The macOS DMG is unsigned and not notarized. After mounting it, drag
`Rootglass.app` to Applications. If Gatekeeper reports that the app is damaged,
run `xattr -dr com.apple.quarantine /Applications/Rootglass.app` in Terminal.
This workaround does not sign or notarize the app. The v0.0.2 QEMU signature
defect is fixed in v0.0.3 and later previews.

## v0.0.3 preview

### macOS runtime and startup diagnostics

- Re-sign the relocated bundled QEMU executable and dynamic libraries after
  changing their Mach-O rpaths, and verify their signatures before packaging.
- Report the QEMU child state and startup diagnostics log path when QMP startup
  fails.
- Detect unavailable Hypervisor.framework acceleration on Apple Silicon, use
  TCG as the fallback, and show the selected accelerator in the TUI launch plan.
- Fix macOS package-store publication for imported `.glass` bundles.
- Add an opt-in macOS installed-app smoke that boots the signed example through
  the TUI, runs a guest command, checks clean shutdown, and uploads a screenshot
  and diagnostic logs.
- Clarify the recursive quarantine workaround. The v0.0.2 macOS runtime issue
  is corrected in this preview.

### Release assets

Includes Linux x86_64, macOS arm64, and Windows x86_64 desktop packages with
matching versioned QEMU runtimes, Alpine guest boot assets, runtime checksums,
and a signed Hello World `.glass` with its SHA-256 checksum. The example uses a
public development key and the disposable `rootglass-ci` guest password; it is
for verification only.

### macOS first launch

The macOS DMG remains unsigned and not notarized. After mounting it, drag
`Rootglass.app` to Applications. If Gatekeeper reports that the app is damaged,
use the recursive quarantine workaround in the README. This workaround does
not sign or notarize the app. Clean-host qualification remains open.

## v0.0.2 preview

### Release assets

Includes Linux x86_64, macOS arm64, and Windows x86_64 desktop packages with
matching versioned QEMU runtimes, Alpine guest boot assets, runtime checksums,
and a signed Hello World `.glass` with its SHA-256 checksum. The example uses a
public development key and the disposable `rootglass-ci` guest password; it is
for verification only.

### macOS first launch

The macOS DMG is unsigned and not notarized. After mounting the DMG, drag
`Rootglass.app` to the Applications folder. If Gatekeeper reports that the app
is damaged and cannot be opened, recursively remove quarantine from the installed
app in Terminal:

```sh
xattr -dr com.apple.quarantine /Applications/Rootglass.app
```

The recursive form also clears quarantine from nested files, but does not sign
or notarize the app; use it only for the official Rootglass release. The v0.0.2
macOS QEMU binary has an invalid code signature and macOS kills it at startup
(`Killed: 9`). Clearing quarantine does not fix that old package. The signature
issue is corrected in the v0.0.3 preview. Clean-host qualification remains open.

## v0.0.1 preview

### macOS first launch

The macOS DMG is unsigned and not notarized. After mounting the DMG, drag
`Rootglass.app` to the Applications folder. If Gatekeeper reports that the app
is damaged and cannot be opened, recursively remove quarantine from the installed
app in Terminal:

```sh
xattr -dr com.apple.quarantine /Applications/Rootglass.app
```

This does not sign or notarize the app; use it only for the official Rootglass
release. Clean-host qualification remains open.
