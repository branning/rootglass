# Example RootFS Assets

The repository does not commit large generated guest images. Instead, the `hello-world.glass` fixture can be materialized locally from canned Alpine Linux aarch64 assets.

## Source material

The materialization script uses a matched, pinned Alpine Linux aarch64 release set:

- `v3.23/releases/aarch64/alpine-minirootfs-3.23.8-aarch64.tar.gz`
- `v3.23/releases/aarch64/netboot-3.23.8/vmlinuz-virt`
- `v3.23/releases/aarch64/netboot-3.23.8/initramfs-virt`
- `v3.23/releases/aarch64/netboot-3.23.8/modloop-virt`

The versioned `netboot-3.23.8` directory keeps the kernel and initramfs aligned
with the rootfs. The generated initramfs includes the matching kernel modules
from `modloop-virt`; Alpine's stock netboot initramfs does not include ext4,
which it otherwise loads only after mounting the root filesystem. Override
`ALPINE_RELEASE` and `ALPINE_VERSION` together when testing another release.

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
ALPINE_RELEASE=v3.23 \
ALPINE_VERSION=3.23.8 \
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

## End-to-end smoke boot

`bash scripts/smoke-boot-example.sh` uses a PTY to exercise the real serial
console. It waits for the fixture login, logs in as `root` with the
developer/CI-only password `rootglass-ci`, runs `uname -a`, checks for the
guest hostname and `aarch64`, then requests guest shutdown and requires QEMU
to exit. The raw console transcript is saved to
`.rootglass/build/hello-world-boot.log`. This disposable credential is only
for the offline example fixture and must not be shipped in a production glass.

When changing any signed example file, regenerate its development DSSE envelope:

```bash
cargo run --locked -p rootglass-signature --example sign_example_fixture
```

That example uses a deterministic, public fixture key. It is for the sample only
and is not a production signing tool.

## Why Alpine

Alpine is small, boring, widely mirrored, and publishes canned aarch64 minirootfs/netboot artifacts. That makes it a good first fixture source without building a custom embedded distribution yet.

