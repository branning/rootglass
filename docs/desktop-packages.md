# Desktop package builds

The desktop package workflow builds a native package on hosted Linux x64, macOS arm64, and Windows x64 runners. Pull requests and manual runs upload each package and a SHA-256 checksum for 14 days.

Each package declares the `.glass` association and includes the repository license and notices. The package workflow is a build check and review artifact; its output is unsigned and is not a production release.

## Current release limits

- The package builds do not include a versioned QEMU runtime set. Runtime asset resolution must be configured by the release packaging step before an installed app can boot without developer-provisioned assets.
- App and installer signing, macOS notarization, and release provenance are not configured. No signing credentials are stored in the repository.
- CI builds packages but does not install them on a clean OS image or verify the `.glass` association through the OS shell.
- Linux packaging currently emits a Debian package. macOS emits a DMG. Windows emits an NSIS installer.

Treat A-09 and A-10 as incomplete until clean-install, file-open, runtime inventory, and signing checks are added and pass on each shipping platform.
