# Desktop package builds

The desktop package workflow builds on hosted Linux x64, macOS arm64, and Windows x64 runners. Manual builds upload review artifacts to Actions for 14 days. To create a release, build from a new draft prerelease's tag with `platform=all` and `release_tag` set to that same tag; the workflow materializes and verifies the example `.glass`, builds all three installers and matching QEMU runtimes, then attaches the full asset set while the release is still mutable. Publish the draft only after the attachment job succeeds. Pull requests and pushes do not run package builds.

GitHub does not trigger `release.created` for draft releases, and publishing an
immutable release is too late to add assets. The published `v0.0.1` release
remains unchanged. Prepare a version-matched draft prerelease before
publication; the `v0.0.2` preview used this flow. The `v0.0.3` draft has the
complete asset set and passed the installed macOS TUI smoke against its exact
tag; it remains unpublished pending maintainer approval.

Each installer declares the `.glass` association and includes the repository license, notices, and a versioned offline runtime. Runtime manifests contain SHA-256 hashes for the QEMU executable, its bundled shared libraries and data files, and the Alpine kernel and initramfs. The app verifies those files before launching a guest and resolves them independently of the current working directory. Standalone runtime archives are also attached so reviewers can inspect the exact per-platform inventory.

The Linux runtime uses Ubuntu 24.04's `qemu-system-arm` package, macOS uses the Homebrew `qemu` formula, and Windows uses the Chocolatey QEMU package. The exact QEMU version, distribution source, and executable hash are recorded in each platform's `runtime-info.json`. Alpine Linux 3.23.8 aarch64 netboot assets are shared across targets. Runtime copyright files, QEMU's GPL-2.0 license text, and the Alpine source notice ship in the runtime bundle.

On macOS, runtime preparation relocates QEMU's library search paths with
`install_name_tool`. That invalidates upstream Mach-O signatures, so the build
ad-hoc signs the relocated QEMU executable and bundled dynamic libraries and
verifies those signatures before archiving the runtime. The Rootglass app itself
remains unsigned and not notarized in preview builds.

These are preview review artifacts. The desktop apps and installers are not
Developer ID signed or notarized and have not passed complete native clean-host
qualification. They make no claim that HVF, KVM, or WHPX acceleration is
available on every host.

## Manual package builds

In Actions, select **Desktop packages → Run workflow**, choose a branch or tag,
and select `all` (the default), `linux`, `macos`, or `windows`. A single-platform
selection starts only that platform's package job and cannot attach a release.

For example, build only the Windows app from `main` with GitHub CLI:

```sh
gh workflow run desktop-packages.yml --repo branning/rootglass --ref main -f platform=windows
```

To build and attach a new release, create a mutable draft prerelease, then run
the workflow against its tag with `platform=all` and `release_tag` set to the
same value. Replace `vX.Y.Z` below with the new tag matching the desktop
version:

```sh
release_tag=vX.Y.Z
gh workflow run desktop-packages.yml --repo branning/rootglass --ref "$release_tag" \
  -f platform=all -f "release_tag=$release_tag"
```

The job checks that the target is still a mutable prerelease draft, verifies
the runtime and example checksums, and attaches 14 assets: an installer,
manifest, QEMU runtime archive, and runtime checksum for each platform, plus
`rootglass-hello-world-0.1.0.glass` and `SHA256SUMS-example.txt`. On Windows it
selects the NSIS `*-setup.exe` installer and omits the unpackaged desktop
executable. Publish the release after the job completes.

To reuse a completed all-platform run from the same tag, set both
`artifact_run_id` and `release_tag`. The workflow checks that the run's source
commit matches the release tag and that all four source jobs succeeded before
attaching the assets.

The materialized example is signed with the public development fixture key and
contains the disposable `rootglass-ci` guest password. It is for verifying
Rootglass and the preview workflow only; it is not a production appliance.
The installed desktop app does not trust the sample key by default; add it
explicitly to the per-user trust file to open the bundle in the GUI. See the
[sample trust instructions](example-assets.md#trusting-the-release-example-in-the-desktop-app).
Download the `.glass` and checksum next to the installer, verify the checksum,
then use the installed `rootglass` CLI:

```sh
sha256sum --check SHA256SUMS-example.txt
rootglass plan rootglass-hello-world-0.1.0.glass
rootglass boot rootglass-hello-world-0.1.0.glass
```

On Windows, compare the SHA-256 value in `SHA256SUMS-example.txt` with:

```powershell
(Get-FileHash .\rootglass-hello-world-0.1.0.glass -Algorithm SHA256).Hash
```

The plan command verifies the sample signature and reports preflight details.
When `plan` or `boot` receives a ZIP `.glass` archive, it verifies and imports
it into the local content-addressed library first. Directory-form `.glass`
bundles are planned or booted in place. The Rootglass package supplies the
matching QEMU runtime and guest boot assets.

Different platform selections can run independently. A newer run for the same
ref, event, and selection cancels the superseded run. Routine CI still runs
formatting, Clippy, tests, the Linux desktop compile check, and CLI/boot smoke checks.

## Manual installed macOS TUI smoke

The **Installed macOS TUI smoke** workflow verifies a packaged app without
adding a macOS job to every push or pull request. First run **Desktop packages**
with `platform=macos`, then dispatch **Installed macOS TUI smoke** on the same
commit and pass the successful package workflow's run ID as `package_run_id`.
The smoke downloads that run's DMG and signed example, copies `Rootglass.app`
into `/Applications`, configures the public example trust key in a temporary
runner home, and drives the installed Ratatui TUI through planning, guest boot,
serial login, an `uname -a` check, and stop. It verifies the bundled QEMU
signature before boot and uploads a TUI screenshot, serial output, and QEMU
logs as a 14-day artifact.

For example, after the macOS package run succeeds on the current `main` commit,
dispatch the smoke with its run ID:

```sh
gh workflow run installed-macos-tui-smoke.yml --repo branning/rootglass \
  --ref main -f package_run_id=123456789
```

The smoke rejects artifacts built from a different commit. Its only trigger is
`workflow_dispatch`, so the additional macOS runner minutes are opt-in.

## Remaining release limits

- App and installer signing, macOS notarization, and release provenance are not configured. No signing credentials are stored in the repository.
- The published `v0.0.1` release is immutable and has no package or example assets; it cannot be updated in place.
- CI builds packages but does not install them on a clean OS image or verify the `.glass` association through the OS shell. Manual package builds provide review artifacts but do not count as clean-host qualification.
- Linux packaging emits a Debian package. macOS emits a DMG. Windows emits an NSIS installer.

Treat A-09 and A-10 as incomplete until clean-install, file-open, runtime inventory, and signing checks are recorded for each shipping platform. Runtime archives and manifests make the bundle inspectable; they do not substitute for those host checks.
