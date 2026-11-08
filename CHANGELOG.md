# Rootglass release notes

## Unreleased

### Release example

The draft prerelease workflow now materializes, signs, verifies, and attaches a
Hello World `.glass` together with its SHA-256 checksum. The sample uses a
public development key and the disposable `rootglass-ci` guest password; it is
for verification only. Published `v0.0.1` is immutable and cannot accept these
assets, so they will be available with a later draft prerelease.

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
