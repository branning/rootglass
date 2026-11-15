# Desktop package builds

The package workflow builds Linux x86_64, Linux aarch64, macOS arm64, Windows
x64, and Windows ARM64 on hosted runners. Linux ARM64 passed the Ubuntu 24.04.5
installed smoke. The Windows ARM64 package now has a native ARM64 build/runtime
cell. Its Windows 11 ARM64 clean-install smoke passed; the recorded result is
below. Both ARM64 packages are included in `platform=all` and will be attached to
new releases built after this change; existing published releases remain
unchanged. Manual builds upload review artifacts to Actions for 14 days. To
create a release,
build from a new draft prerelease's tag with `platform=all` and `release_tag`
set to that same tag; the workflow materializes and verifies the example
`.glass`, builds all five installers and matching QEMU runtimes, then attaches
the full asset set while the release is still mutable. Publish the draft only
after the attachment job succeeds. Pull requests and pushes do not run package
builds.

GitHub does not trigger `release.created` for draft releases, and publishing an
immutable release is too late to add assets. The published `v0.0.1` release
remains unchanged. Prepare a version-matched draft prerelease before
publication; the `v0.0.2` preview used this flow. The published `v0.0.4`
preview has the complete asset set and passed the installed macOS TUI smoke
against its exact tag. Its Linux and complete Windows 11 clean-host
qualification remains incomplete.

Each installer declares the `.glass` association and includes the repository license, notices, and a versioned offline runtime. Runtime manifests contain SHA-256 hashes for the QEMU executable, its bundled shared libraries and data files, and the Alpine kernel and initramfs. The app verifies those files before launching a guest and resolves them independently of the current working directory. Standalone runtime archives are also attached so reviewers can inspect the exact per-platform inventory.

The Linux runtime uses Ubuntu 24.04's `qemu-system-arm` package, macOS uses
the Homebrew `qemu` formula, Windows x64 uses the Chocolatey QEMU package, and
Windows ARM64 uses native QEMU from MSYS2's CLANGARM64 repository. The exact
QEMU version, distribution source, and executable hash are recorded in each
platform's `runtime-info.json`. Alpine Linux 3.23.8 aarch64 netboot assets are
shared across targets. Runtime copyright files, QEMU's GPL-2.0 license text,
and the Alpine source notice ship in the runtime bundle.

The [ADR-0002 runtime decision](adr/0002-versioned-desktop-qemu-runtime.md)
records the exact QEMU executable versions and hashes, shared Alpine asset
hashes, and runtime archive checksums for the v0.0.3
[draft release](https://github.com/branning/rootglass/releases/tag/v0.0.3).

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
and select `all` (the default), `linux` (x64), `linux-arm64`, `macos`,
`windows` (x64), or `windows-arm64`. `all` builds every release target; a
single-platform selection starts only that platform's package job and cannot
attach a release.

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
the runtime and example checksums, and attaches 22 assets: an installer,
manifest, QEMU runtime archive, and runtime checksum for each of five targets,
plus `rootglass-hello-world-0.1.0.glass` and `SHA256SUMS-example.txt`. On
Windows it selects the NSIS `*-setup.exe` installer and omits the unpackaged
desktop executable. Publish the release after the job completes.

To reuse a completed all-platform run from the same tag, set both
`artifact_run_id` and `release_tag`. The workflow checks that the run's source
commit matches the release tag and that all five package jobs succeeded before
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
with `platform=macos`, then dispatch **Installed macOS TUI smoke** and pass the
successful package workflow's run ID as `package_run_id`. The package must come
from the smoke commit or an ancestor where changes are limited to this smoke
workflow, its Python driver, and this evidence document; that lets driver fixes
reuse the identical app binary without another package build. The smoke
downloads that run's DMG and signed example, copies `Rootglass.app` into
`/Applications`, configures the public example trust key under the runner's
account home for LaunchServices and under an isolated home for the TUI, and
opens the signed `.glass` file through LaunchServices. macOS LaunchServices
uses the account's normal HOME even when `launchctl setenv HOME` is set. The smoke checks
that the installed app is the registered handler and that the GUI accepts and
imports the archive. The smoke verifies the packaged runtime inventory and
signatures, then runs the installed Ratatui TUI outside the checkout while the
host network interface is down. Fail-fast `cargo`, `rustc`, and system-QEMU
shims ensure the installed app uses its bundled runtime. The TUI plans, boots,
logs into, restarts, and stops the guest, checks `uname -a`, and confirms QEMU
exits. Finally, the workflow removes the app bundle and verifies the temporary
trust policy and imported library remain. Package digests, runtime inventory,
LaunchServices events, a TUI screenshot, serial output, and QEMU logs are
uploaded as a 14-day artifact.

For example, after the macOS package run succeeds on the current `main` commit,
dispatch the smoke with its run ID:

```sh
gh workflow run installed-macos-tui-smoke.yml --repo branning/rootglass \
  --ref main -f package_run_id=123456789
```

The smoke rejects packages from a different commit lineage or with product
source changes. It permits an ancestor package only when intervening changes
are limited to the smoke workflow, its driver, and this evidence document. Its
only trigger is `workflow_dispatch`, so the additional macOS runner minutes are
opt-in.

### Recorded macOS 15 Apple Silicon result

Package run [42739442625](https://github.com/branning/rootglass/actions/runs/42739442625)
passed on source commit `65e4889fcb7b2a76c14ab3b299b6bcbb5992d0e0` and produced
Rootglass 0.0.3. Smoke run
[42740401624](https://github.com/branning/rootglass/actions/runs/42740401624)
passed on `50a74311ac5148e684b8596e347205849226de90`. The smoke verified that
the package commit is an ancestor and that intervening changes touched only
the smoke workflow, its TUI driver, and this evidence document.

The hosted runner was macOS 15.8.2 (build 24H232), arm64. The smoke checked the
DMG SHA-256 (`03feef85d6b991bef8d9750d32c513c699b43e5489bc4d3fcdfdfc30fb0c65d1`)
and signed example SHA-256
(`f9f55591171b1e47e9e77b2ecac0ea1c38e00aaa22a86050e4a67d2527ccaaab`), copied
the app to `/Applications`, confirmed LaunchServices selected it for the
`.glass` file, and verified/imported that bundle using the configured example
publisher key. LaunchServices used the runner account's normal HOME; the TUI
boot used a separate temporary home.

The installed runtime inventory matched its manifest, all bundled QEMU
signatures verified, and QEMU 11.1.3 launched with TCG software emulation while
the runner's network interface was down. The installed TUI ran outside the
checkout with `cargo`, `rustc`, and host-QEMU shims; none were invoked. It
planned and booted the signed Alpine example, accepted login and `uname -a`
input, restarted the guest, and stopped with no QEMU process left. Removing the
app left the isolated TUI home trust policy, imported package library, and guest
state intact.

The [macOS package and checksum artifact](https://github.com/branning/rootglass/actions/runs/42739442625/artifacts/13728338169)
and [signed example artifact](https://github.com/branning/rootglass/actions/runs/42739442625/artifacts/13729135120)
expire on 2026-11-24. The [smoke evidence artifact](https://github.com/branning/rootglass/actions/runs/42740401624/artifacts/13729635661)
expires on 2026-11-24 and contains the TUI screenshot, serial output, QEMU
logs, LaunchServices events, runtime inventory, package checksums, and
uninstall-retention record. Its uploaded ZIP SHA-256 is
`de6e028cf6bd6e481f3c1c6d1e5805f2325deab2aa6ef233b0aa63c89884da52`.

This verifies installation, OS association/import, and offline guest operation
on the hosted macOS runner. It does not test native picker interaction,
accessibility, notarization, or HVF performance; no macOS acceleration claim is
made.

### Recorded v0.0.4 installed macOS arm64 result

Package run [42905505796](https://github.com/branning/rootglass/actions/runs/42905505796)
built v0.0.4 from `6fab114df7de18b5e0874b9069f0b3fcd56d6578`. The installed
smoke [42909500277](https://github.com/branning/rootglass/actions/runs/42909500277)
passed on macOS 15.8.2 (24H232), arm64. It copied the app to
`/Applications`, opened the signed example through LaunchServices, and verified
the sample publisher trust. The DMG SHA-256 is
`034e313428c2a0de577dc29bc6affdfe66d4d94297274dbeb52efeb59ad1ef8b`; the
example SHA-256 is
`d65d9532903b8e1167f82a7273d29204c947b69cb00e250713cff8efd57ac522`.

The smoke verified the v0.0.4 runtime manifest and bundled QEMU 11.1.3,
including the QEMU and dynamic-library code signatures. With the runner's
network interface down, it ran the installed TUI from outside the checkout,
without invoking host Rust or QEMU tools. The signed Alpine guest booted,
accepted login and `uname -a`, restarted, and stopped with no QEMU process left.
After removing the app, the trust policy, imported library, and guest state were
unchanged. The [smoke evidence artifact](https://github.com/branning/rootglass/actions/runs/42909500277/artifacts/13807365619)
has SHA-256
`5683345feb6847f91b101e645be6b5e477685804262f7706cae4dd3954aab455`.

This qualifies the selected macOS arm64 preview target on the hosted OS. It does
not test native picker interaction, accessibility, notarization, or HVF
performance; no macOS acceleration claim is made.

## Manual installed Ubuntu package smoke

The **Installed Ubuntu clean-install smoke** workflow checks a Linux package on
a fresh Ubuntu 24.04 Actions runner. Choose `x86_64` or `aarch64` as the
`architecture` input. First run **Desktop packages** with `platform=linux` for
x86_64 or `platform=linux-arm64` for aarch64 on the commit under review, then
dispatch the install smoke on that same commit and provide the package run ID
as `package_run_id`.
The `.deb` declares its required X11/XKB runtime libraries, and the smoke checks
that dependency metadata before installing it through APT.
It installs the `.deb`, checks its runtime inventory and `.glass` MIME handler,
opens the signed example through `xdg-open`, and exercises the installed GUI and
guest lifecycle with outbound networking blocked. The application processes run
from temporary directories outside the checkout; failing shims catch any use of
`cargo`, `rustc`, or system QEMU. Screenshots, install inventory, plan output,
serial logs, and uninstall/data-retention evidence are kept for 14 days.

The x86_64 cell uses a hosted Ubuntu runner and the aarch64 cell uses a hosted
Ubuntu ARM64 runner. Both exercise the package's bundled QEMU with TCG while
offline. Neither cell qualifies KVM hardware acceleration or native interaction
on a user's desktop. Linux ARM64 is an approved release target and new releases
from the package matrix will include it.

### Recorded Ubuntu 24.04 ARM64 installed result

Package run [43673101880](https://github.com/branning/rootglass/actions/runs/43673101880)
built `rootglass_0.0.6_arm64.deb` at `3af9e86`. The installed smoke
[43674044104](https://github.com/branning/rootglass/actions/runs/43674044104)
passed on the `ubuntu-24.04-arm` runner (Ubuntu 24.04.5, aarch64; kernel
`7.0.0-1017-azure`). It verified the `.deb` architecture and SHA-256
`df8f00fd26f9dfe1e7ce758cf37cc2c722fd554bc74b38ddcbef562d521cc663`, then
installed the package and verified the `linux-aarch64` runtime inventory. The
bundled QEMU is 8.2.2 with SHA-256
`e19e11bd054ccf0f3cfcea8e0acdcc4288de6abd2ed6c30f743092c362eb2673`; the
runtime archive SHA-256 is
`cbb9398d21d4244a7a01f3251ffb34c50d76f0d5c7ea686255c3cf6635264fdf`.

With the runner offline and `/dev/kvm` unavailable, the installed GUI and CLI
booted the signed Alpine example using bundled QEMU TCG. The guest accepted
serial input, and uninstall preserved the user's trust policy and imported
library. The example SHA-256 is
`a0ebccb4efb220c366451def33983f58214682b29367fc9a16c60a4fb857359f`. The
[smoke evidence artifact](https://github.com/branning/rootglass/actions/runs/43674044104/artifacts/14144659823)
contains the install inventory, runtime verification, screenshots, guest logs,
and retention results. PR check run
[43674042165](https://github.com/branning/rootglass/actions/runs/43674042165)
also passed, including the hosted ARM64 boot smoke.

This qualifies installation and TCG guest operation on the hosted Ubuntu ARM64
image. Linux ARM64 is approved for the release matrix; new releases from it
will include this target. The smoke does not test Linux KVM acceleration or
native interaction on a user's desktop.

## Manual installed Windows package smoke

The **Installed Windows clean-install smoke** workflow is manual because it
installs an unsigned Windows review package and boots a guest. Build the x64
package with `platform=windows` and the ARM64 package with `platform=windows-arm64`.
To run both host cells, build with `platform=all`, then dispatch the install
smoke with that package run ID. Either host can be selected separately to
reduce runner minutes; pair `platform=windows` with `host=windows-x64`, or
`platform=windows-arm64` with `host=windows-arm64`.

The Windows Server 2025 x64 cell installs the x64 NSIS package. The Windows 11
ARM64 cell installs the native ARM64 package and uses native ARM64 QEMU from
MSYS2. Both verify the package checksum and runtime inventory, check that the
application and QEMU PE headers match the selected architecture, check the `.glass`
file association, import the signed example, and run the installed Ratatui
workflow through a Windows pseudoterminal with outbound Internet traffic
blocked for Rootglass and QEMU. The guest boots, accepts serial login, runs
`uname -a`, restarts, runs `id`, and stops. The workflow verifies that QEMU
exits, uninstalls the app, and confirms trust configuration, imported library
data, and guest state remain. A TUI screenshot, serial logs, runtime inventory,
association events, and uninstall record are retained for 14 days. The Windows
Server 2025 x64 result is recorded in [run
43203983228](https://github.com/branning/rootglass/actions/runs/43203983228).

### Recorded Windows 11 ARM64 installed result

The native ARM64 package and offline installed smoke passed on 2026-11-15.
Package build [run 43723384799](https://github.com/branning/rootglass/actions/runs/43723384799)
produced the installer and runtime used by installed smoke
[run 43724549765](https://github.com/branning/rootglass/actions/runs/43724549765).
The clean hosted image was Microsoft Windows 11 Enterprise 10.0.26200, build
26200, on the `windows-11-vs2026-arm64` image (`20261112.217.1`), with an ARM64
runner. The installed Rootglass app and QEMU PE headers were both `0xAA64`; the
runtime was version 0.0.6 with QEMU 11.1.3. The launch plan truthfully selected
TCG software emulation.

The installer SHA-256 is
`770e8cb93c4542c4c4f765d43ffbb6352cf0832fe7a8f934b8e669a8f688d175`; the
runtime archive SHA-256 is
`8499d3d222a6354b0b656516736c0a652034124c54d7f7315a60550e9d3c88e8`; and the
signed `rootglass-hello-world-0.1.0.glass` SHA-256 is
`c9b90909242eb198a2d729b5c70a9f5f347c51c1b85c634cb8fc7c1c180bd339`.
The smoke verified runtime asset hashes and the `.glass` signature, imported
the example through both the GPUI picker and Windows file association, then
blocked Internet egress for Rootglass and QEMU. The TUI booted the Alpine
aarch64 guest, accepted serial login and `uname -a`, restarted and accepted
`id`, stopped without an orphan QEMU process, and uninstalled while preserving
trust, imported package, registry, and guest state. The run artifact retains
the TUI screenshot, runtime inventory, serial logs, process monitoring, and
uninstall evidence for 14 days.

The installed smoke uses short guest commands; it does not qualify arbitrary
long pasted console lines.

Maintainer decision recorded with the merge of PR #141 on 2026-11-15: qualify
the native package on Windows 11 ARM64. The x64 installed smoke is exact only
for Windows Server 2025 x64; it is not Windows 11 x64 evidence. Until a clean
Windows 11 x64 install is recorded, advertise Windows 11 support for ARM64 only
and keep the Server 2025 x64 result as separate host evidence.

Rootglass currently selects TCG on Windows and discloses software emulation in
the launch plan. Do not enable WHPX selection until each packaged QEMU runtime
is verified to support it. A guest that requires WHPX fails preflight before
QEMU starts.

## Remaining release limits

- App and installer signing, macOS notarization, and release provenance are not configured. No signing credentials are stored in the repository.
- The published `v0.0.1` release is immutable and has no package or example assets; it cannot be updated in place.
- The standard package jobs only build artifacts; the separate installed smoke workflows install the exact package on clean hosted OS images and verify the `.glass` association. Qualification is limited to the hosts and artifact digests recorded above.
- Linux packaging emits a Debian package. macOS emits a DMG. Windows emits an NSIS installer.

Treat A-09 and A-10 as incomplete until clean-install, file-open, runtime inventory, and signing checks are recorded for each shipping platform. Runtime archives and manifests make the bundle inspectable; they do not substitute for those host checks.
