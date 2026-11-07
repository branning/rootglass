# Desktop package builds

The desktop package workflow builds a native package on hosted Linux x64, macOS arm64, and Windows x64 runners. Published GitHub releases (including prereleases) build all three platforms from the release tag. Pull requests and pushes do not run package builds. Each run uploads the selected packages and SHA-256 checksums as Actions artifacts for 14 days; it does not attach them to the GitHub release.

Each package declares the `.glass` association and includes the repository license and notices. The package workflow is a build check and review artifact; its output is unsigned and is not a production release.

## Manual package builds

In Actions, select **Desktop packages → Run workflow**, choose the branch or tag,
and select `all` (the default), `linux`, `macos`, or `windows`. A single-platform
selection starts only that platform's package job.

For example, build only the Windows app from `main` with GitHub CLI:

```sh
gh workflow run desktop-packages.yml --repo branning/rootglass --ref main -f platform=windows
```

Different platform selections can run independently. A newer run for the same
ref, event, and selection cancels the superseded run. Routine CI still runs
formatting, Clippy, tests, the Linux desktop compile check, and CLI/boot smoke checks.

## Current release limits

- The package builds do not include a versioned QEMU runtime set. Runtime asset resolution must be configured by the release packaging step before an installed app can boot without developer-provisioned assets.
- App and installer signing, macOS notarization, and release provenance are not configured. No signing credentials are stored in the repository.
- CI builds packages but does not install them on a clean OS image or verify the `.glass` association through the OS shell.
- Linux packaging currently emits a Debian package. macOS emits a DMG. Windows emits an NSIS installer.

Treat A-09 and A-10 as incomplete until clean-install, file-open, runtime inventory, and signing checks are added and pass on each shipping platform.
