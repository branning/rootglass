# ADR-0002: Versioned desktop QEMU runtime

- Status: Accepted for preview desktop packages
- Date: 2026-11-08
- Issue: [#20](https://github.com/branning/rootglass/issues/20)

## Context

The installed desktop app must boot a guest without requiring users to install
Rust, Cargo, or QEMU separately. The existing runtime resolver verifies a
versioned QEMU executable, kernel, and initramfs, but the desktop package did
not include or configure them. QEMU also loads host libraries and data files
that must travel with the executable for offline use.

## Decision

Desktop packages bundle one host-native QEMU runtime and the pinned Alpine
Linux 3.23.8 aarch64 kernel/initramfs set under
`runtime-assets/<rootglass-version>/`. The host-specific QEMU package sources
are:

| Host | QEMU source |
|---|---|
| Ubuntu 24.04 x86_64 | Ubuntu `qemu-system-arm` package |
| macOS arm64 | Homebrew `qemu` formula |
| Windows x86_64 | Chocolatey QEMU package, which distributes the Windows QEMU installer |

The build copies QEMU's shared libraries and data files into the runtime,
records the exact QEMU version/source, and hashes the executable, libraries,
data files, kernel, and initramfs. The app verifies the complete runtime set
before it starts QEMU. It resolves resources from the package and then the
Rootglass per-user data directory, independent of the working directory.

Packaged builds use only the versioned runtime resolver. The developer CLI and
development builds may continue to use QEMU from `PATH`. The per-user runtime
directory is an offline recovery/upgrade location; it does not silently
override the runtime shipped in the package.

The required guest feature set is the current `QemuLaunchPlan`: aarch64
`virt` machine with `cortex-a57`, offline kernel/initramfs boot, a readonly
virtio block root filesystem, optional writable virtio block state, and
`-nographic` serial I/O for guest login and shutdown. TCG is available when the
signed bundle does not require hardware acceleration. A-04's x64/arm64 hosted
boot smoke and the v0.0.3 installed macOS smoke exercise this serial guest path.

## v0.0.3 preview inventory

The draft [Rootglass v0.0.3 release](https://github.com/branning/rootglass/releases/tag/v0.0.3)
contains one versioned offline runtime for each current package target. These
versions and executable hashes come from each archive's `runtime-info.json`;
`assets.json` records hashes for every bundled file. The release asset digests
match the accompanying `.sha256` sidecars.

| Target | QEMU source and reported version | QEMU executable SHA-256 | Runtime archive SHA-256 |
|---|---|---|---|
| Linux x86_64 | Ubuntu 24.04 `qemu-system-arm`; `8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.18)` | `b511b60cc31479b71dfdd589e6c981ae37b20ef0e75adc5a57bb0d6737e9b30c` | `cf01d0ae08491637c84ee376cd4fe60db3800f92d81720de38c4b44bc5d10c4b` |
| macOS arm64 | Homebrew `qemu`; `11.1.3` | `e8dbe9fa506f30ad8f06a0783aaaf2affd5b8b41919e1d8f8a79cdc80d099a44` | `bcacbd09e7f2733bbef4501334bd3396bd73772428ec00eb74e043568ada3021` |
| Windows x86_64 | Chocolatey QEMU; `11.1.0 (v11.1.0-12130-ge470268ff4)` | `952f897511177803722efed6fefe79bc3e7e5a51a026112dfc40f9a37fb0dad2` | `56a7f9dead3a9c46d2b991bab005bd351f74b080411fe9b7aad3034ac52574c` |

All three archives use the same Alpine Linux 3.23.8 aarch64 netboot assets:
`boot/vmlinuz-virt` SHA-256
`4ac63d06e6cf5820905ffa7741292cfbe90fb0cf1edba4c42e871f6daf699437` and
`boot/initramfs-virt` SHA-256
`b02d097b49d75c8f11c28dc77abffaf67266b8763a4b94edc16482200180f925`. The
archives also include QEMU's required libraries/data and their license files.

The build currently installs QEMU from each platform's package repository and
records the resolved version and hashes in the resulting release. That pins
the shipped artifact, not a future rebuild to the same upstream package
snapshot. Any later release can advance those inputs, but must publish the new
version, full manifest, and archive checksum together.

## Licensing, updates, and security

QEMU is distributed under GPL-2.0-or-later. Bundles include the GPL-2.0 text
and available host-package copyright/license files. The Alpine kernel and
initramfs remain under their upstream component licenses; the bundle records
the pinned source URL and hashes. Package maintainers must preserve those
notices and source references when updating or republishing a runtime.

Runtime updates ship with a new Rootglass package/release. There is no
background download or in-app runtime update in this preview. For each release,
record the host package version, source, full runtime manifest, and archive
checksum. Rebuild promptly when a security advisory affects the included
QEMU/runtime package; otherwise refresh the host package versions as part of a
reviewed Rootglass release.

## Consequences

- The installer is larger, but the guest can boot offline without a separately
  installed QEMU.
- Host distributions provide native binaries and acceleration support; the
  app still has to preflight host capabilities and cannot promise HVF, KVM, or
  WHPX on every machine.
- CI package artifacts and their manifests provide version/hash inventory, but
  do not count as clean-host install or native acceleration qualification.
- Preview packages remain unsigned and are not a production release.

## Revisit when

Revisit this choice if an upstream host package stops providing the required
aarch64 `virt` functionality, a security update cannot be shipped promptly, or
the platform qualification work requires a different source or supported host
set.
