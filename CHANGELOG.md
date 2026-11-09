# Rootglass release notes

## Unreleased

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
