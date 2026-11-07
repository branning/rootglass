# Milestone 1

Milestone 1 is the CLI-first developer milestone for Rootglass. It proves that Rootglass can describe, verify, plan, and boot a small embedded Linux world without requiring the GPUI desktop shell yet.

## Goal

A developer can use the repository to:

1. validate a `.glass` manifest,
2. verify the `.glass` signature path,
3. resolve the builtin aarch64 runtime profile,
4. generate example rootfs/runtime assets,
5. print a QEMU launch plan, and
6. boot the sample world locally through the CLI.

## In scope

Milestone 1 includes:

- CLI-first workflow.
- Directory-form `.glass/` fixtures for development.
- ZIP-based `.glass` archive verification.
- `rootglass validate`.
- `rootglass plan`.
- `rootglass boot`.
- A signed `hello-world.glass` fixture.
- DSSE-wrapped in-toto-style signature verification using Ed25519.
- A test trust store for the sample fixture.
- Read-only rootfs plus optional writable overlay model.
- Local overlay selection model.
- Host acceleration preflight for macOS Hypervisor framework, Windows WHPX, and Linux KVM.
- Runtime-profile resolution for the builtin aarch64 Linux profile.
- A materialization script for canned Alpine Linux aarch64 assets.
- CI checks for formatting, clippy, tests, and CLI smoke.

## Out of scope

Milestone 1 does not include:

- GPUI Kit (`=0.9.0`) desktop shell.
- Nontechnical double-click workflow.
- Packaged app installers.
- Bundled QEMU.
- Production trust-store management.
- Key rotation policy enforcement.
- Full `.glass` signing tool.
- Hardware-in-the-loop support.
- Guest framebuffer or webview integration.
- Multiple concurrent worlds.

## Required local tools

The repository CI path requires:

- Rust stable toolchain.
- Cargo.

The full local boot path additionally requires:

- Linux host or VM for `scripts/materialize-example-assets.sh`.
- `just` for convenience commands.
- `curl`, `tar`, `dd`, `mkfs.ext4`, `e2label`, `sudo`, `mount`, and `umount` for asset materialization.
- `cpio`, `gzip`, and `unsquashfs` (provided by `squashfs-tools`) to rebuild the initramfs.
- `python3` for the bounded serial smoke test and `qemu-system-aarch64` for boot.
- KVM if hardware acceleration is desired or required by a glass.

## Milestone 1 commands

Run CI-equivalent checks:

```bash
just check
```

Validate the example glass manifest:

```bash
just validate
```

Print a launch plan:

```bash
just plan
```

Generate local example assets:

```bash
just materialize-assets
```

Boot the example glass:

```bash
just boot
```

Exit the QEMU console with:

```text
Ctrl-a x
```

## Completion criteria

Milestone 1 is complete when:

- [x] The repository has a signed `examples/hello-world.glass` fixture.
- [x] Manifest parsing and validation are implemented.
- [x] Runtime lifecycle and preflight states are represented.
- [x] QEMU launch plans are generated from typed runtime data.
- [x] Host acceleration checks are represented.
- [x] Read-only rootfs plus writable overlay model is represented.
- [x] DSSE/Ed25519 verification is implemented and tested.
- [x] ZIP `.glass` archive verification is implemented and tested.
- [x] `rootglass validate` exists.
- [x] `rootglass plan` exists.
- [x] `rootglass boot` exists and preflights missing assets before QEMU launch.
- [x] CI is green for formatting, clippy, tests, and CLI smoke.
- [ ] A developer has run `just materialize-assets` and `just boot` successfully on a suitable local host.
- [x] README quickstart explains the current CLI workflow.
- [x] License and notice files are present.

## Post-Milestone 1 backlog

Important follow-on work that should not block Milestone 1:

- GPUI Kit (`=0.9.0`) desktop shell.
- QEMU bundling/distribution strategy.
- Production trusted publisher and key-rotation policy.
- Bundle signing tool.
- Archive creation tooling.
- Nontechnical installer and double-click UX.
