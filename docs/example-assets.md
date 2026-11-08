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

Run from the repository root on Linux; no sudo, loop device, or display server is required:

```bash
bash scripts/materialize-example-assets.sh
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
bash scripts/materialize-example-assets.sh
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
cargo run --locked --manifest-path crates/rootglass-desktop/Cargo.toml --no-default-features -- boot examples/hello-world.glass
```

The `boot` command:

1. validates the manifest,
2. verifies the DSSE/Ed25519 signature,
3. resolves runtime assets,
4. checks that kernel/initrd/rootfs/state files exist,
5. copies the bundled state template into the per-user state store under the `default` name and holds a cross-process lock for the guest run,
6. spawns QEMU with inherited terminal I/O. The guest mounts the state disk by label at `/state`; the rootfs remains read-only.

With `-nographic -serial mon:stdio`, exit QEMU with:

```text
Ctrl-a x
```

## Release example bundle

The desktop release workflow materializes and packages this fixture as
`rootglass-hello-world-0.1.0.glass`, then includes it and
`SHA256SUMS-example.txt` in the draft prerelease asset set. The example is
included with the `v0.0.2` preview prerelease; the published `v0.0.1` release
remains immutable and unchanged.

After installing Rootglass and downloading both files, verify and inspect the
bundle from the command line:

```bash
sha256sum --check SHA256SUMS-example.txt
rootglass plan rootglass-hello-world-0.1.0.glass
```

Planning or booting a ZIP archive verifies and imports it into Rootglass's local
content-addressed package library first. Directory-form `.glass` bundles are
planned or booted in place.

To boot it interactively, run `rootglass boot rootglass-hello-world-0.1.0.glass`
and log in as `root` with the disposable `rootglass-ci` password. The package's
public signing key and guest credential are demonstration-only. Do not use the
fixture for sensitive data or as a production trust example. See [desktop
package builds](desktop-packages.md) for the draft-release process and artifact
limits.

## Expected preflight failure before materialization

If generated assets are missing, `boot` should fail before starting QEMU. For example:

```text
error: missing kernel asset: runtime-assets/alpine-aarch64/vmlinuz-virt
```

This behavior is intentional and is covered by tests.

## End-to-end boot and persistence smoke

`bash scripts/smoke-boot-example.sh` uses a PTY to exercise the real serial
console twice. It logs in as `root` with the developer/CI-only password
`rootglass-ci`, checks `uname -a`, writes an exact marker under `/state`, powers
off, boots again, and reads the marker back. It hashes both the read-only
rootfs and bundled state template before and after to prove they stayed
unchanged. The raw console transcript is saved to
`.rootglass/build/hello-world-boot.log`. This disposable credential is only
for the offline example fixture and must not be shipped in a production glass.
The CI x64/arm64 boot matrix uploads this transcript on failure for maintainer
diagnosis; it does not send guest output to a telemetry service.

When changing any signed example file, regenerate its development DSSE envelope:

```bash
cargo run --locked -p rootglass-signature --example sign_example_fixture
```

That example uses a deterministic, public fixture key. It is for the sample only
and is not a production signing tool.

## Why Alpine

Alpine is small, boring, widely mirrored, and publishes canned aarch64 minirootfs/netboot artifacts. That makes it a good first fixture source without building a custom embedded distribution yet.

