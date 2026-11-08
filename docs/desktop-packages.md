# Desktop package builds

The desktop package workflow builds on hosted Linux x64, macOS arm64, and Windows x64 runners. Published GitHub releases (including prereleases) build all three platforms from the release tag, bundle the matching QEMU runtime and Alpine guest boot assets, and attach installers, runtime archives, and checksums to the release. Pull requests and pushes do not run package builds. Manual runs upload artifacts to Actions for 14 days.

Each installer declares the `.glass` association and includes the repository license, notices, and a versioned offline runtime. Runtime manifests contain SHA-256 hashes for the QEMU executable, its bundled shared libraries and data files, and the Alpine kernel and initramfs. The app verifies those files before launching a guest and resolves them independently of the current working directory. Standalone runtime archives are also attached so reviewers can inspect the exact per-platform inventory.

The Linux runtime uses Ubuntu 24.04's `qemu-system-arm` package, macOS uses the Homebrew `qemu` formula, and Windows uses the Chocolatey QEMU package. The exact QEMU version, distribution source, and executable hash are recorded in each platform's `runtime-info.json`. Alpine Linux 3.23.8 aarch64 netboot assets are shared across targets. Runtime copyright files, QEMU's GPL-2.0 license text, and the Alpine source notice ship in the runtime bundle.

These are preview review artifacts. They are unsigned, have not passed complete native clean-host qualification, and make no claim that HVF, KVM, or WHPX acceleration is available on every host.

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

## Remaining release limits

- App and installer signing, macOS notarization, and release provenance are not configured. No signing credentials are stored in the repository.
- CI builds packages but does not install them on a clean OS image or verify the `.glass` association through the OS shell. Manual package builds provide review artifacts but do not count as clean-host qualification.
- Linux packaging emits a Debian package. macOS emits a DMG. Windows emits an NSIS installer.

Treat A-09 and A-10 as incomplete until clean-install, file-open, runtime inventory, and signing checks are recorded for each shipping platform. Runtime archives and manifests make the bundle inspectable; they do not substitute for those host checks.
