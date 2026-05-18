# Example RootFS Assets

The repository does not commit large generated guest images. Instead, the `hello-world.glass` fixture can be materialized locally from canned Alpine Linux aarch64 assets.

## Source material

The materialization script uses Alpine Linux aarch64 release artifacts:

- `alpine-minirootfs-*.tar.gz`
- `netboot/vmlinuz-virt`
- `netboot/initramfs-virt`

By default, the script uses Alpine's `latest-stable` aarch64 release path and a pinned minirootfs filename. Override these with environment variables when needed.

## Generate assets

Run from the repository root on Linux:

```bash
./scripts/materialize-example-assets.sh
```

Or with `just`:

```bash
just materialize-assets
```

Generated files:

```text
examples/hello-world.glass/rootfs.img
examples/hello-world.glass/state.img
runtime-assets/alpine-aarch64/vmlinuz-virt
runtime-assets/alpine-aarch64/initramfs-virt
```

These paths are ignored by Git.

## Environment variables

```bash
ALPINE_RELEASE=latest-stable \
ALPINE_MINIROOTFS=alpine-minirootfs-3.22.2-aarch64.tar.gz \
ROOTFS_SIZE_MB=256 \
STATE_SIZE_MB=64 \
./scripts/materialize-example-assets.sh
```

## Validate and plan

These commands do not require generated `.img` files:

```bash
just validate
just plan
```

`rootglass plan` resolves the builtin aarch64 profile into local runtime-asset paths:

```text
runtime-assets/alpine-aarch64/vmlinuz-virt
runtime-assets/alpine-aarch64/initramfs-virt
```

QEMU synthesizes the aarch64 `virt` device tree for the MVP profile, so the launch plan does not pass a `-dtb` argument.

## Boot

Once assets are materialized, boot the example glass interactively:

```bash
just boot
```

Equivalent command:

```bash
cargo run --locked -p rootglass-cli -- boot examples/hello-world.glass
```

The `boot` command:

1. validates the manifest,
2. verifies the DSSE/Ed25519 signature,
3. resolves runtime assets,
4. checks that kernel/initrd/rootfs/state files exist,
5. spawns QEMU with inherited terminal I/O.

With `-nographic -serial mon:stdio`, exit QEMU with:

```text
Ctrl-a x
```

## Expected preflight failure before materialization

If generated assets are missing, `boot` should fail before starting QEMU. For example:

```text
error: missing kernel asset: runtime-assets/alpine-aarch64/vmlinuz-virt
```

This behavior is intentional and is covered by tests.

## Why Alpine

Alpine is small, boring, widely mirrored, and publishes canned aarch64 minirootfs/netboot artifacts. That makes it a good first fixture source without building a custom embedded distribution yet.
