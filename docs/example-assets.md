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

## Current limitations

This gives Rootglass concrete local files to work with, but a real boot still needs runtime-profile asset resolution wiring:

- `builtin:aarch64-linux` should resolve to `runtime-assets/alpine-aarch64/vmlinuz-virt` or another chosen kernel path.
- `builtin:aarch64-virt` may either resolve to a device tree file or be represented as a QEMU-generated device tree for the `virt` machine.
- The QEMU command currently includes the manifest references rather than resolved host paths.

See the runtime-profile asset resolution backlog issue for the next step.

## Why Alpine

Alpine is small, boring, widely mirrored, and publishes canned aarch64 minirootfs/netboot artifacts. That makes it a good first fixture source without building a custom embedded distribution yet.
