# Rootglass release notes

## Unreleased

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
is damaged and cannot be opened, remove the quarantine attribute from the
installed app in Terminal:

```sh
xattr -d com.apple.quarantine /Applications/Rootglass.app
```

This removes the quarantine attribute from the installed app bundle. It does
not sign or notarize the application; clean-host qualification remains open.

## v0.0.1 preview

### macOS first launch

The macOS DMG is unsigned and not notarized. After mounting the DMG, drag
`Rootglass.app` to the Applications folder. If Gatekeeper reports that the app
is damaged and cannot be opened, remove the quarantine attribute from the
installed app in Terminal:

```sh
xattr -d com.apple.quarantine /Applications/Rootglass.app
```

This removes the quarantine attribute from the installed app bundle. It does
not sign or notarize the application; clean-host qualification remains open.
