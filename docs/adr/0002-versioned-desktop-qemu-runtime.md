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
