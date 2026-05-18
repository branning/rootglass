# Notices

Rootglass is licensed under the MIT License. See [LICENSE](LICENSE).

## Generated runtime assets

The repository does not commit guest operating system images, QEMU binaries, or generated runtime assets.

The example asset materialization script downloads Alpine Linux aarch64 release artifacts and creates local root filesystem and state images. Those generated files are ignored by Git and are not part of the Rootglass source distribution.

Generated local paths include:

```text
examples/hello-world.glass/rootfs.img
examples/hello-world.glass/state.img
runtime-assets/alpine-aarch64/vmlinuz-virt
runtime-assets/alpine-aarch64/initramfs-virt
```

## Third-party tools and assets

Rootglass can use QEMU to boot guest environments. QEMU is not bundled in Milestone 1. Developers should install QEMU through their operating system or development environment.

The example fixture uses Alpine Linux release artifacts as convenient canned test material. Alpine Linux artifacts are distributed under their own upstream terms.

## Rust dependencies

Rust crate dependencies are resolved through Cargo and pinned in `Cargo.lock`. Each dependency is governed by its own license terms as published by its upstream project.
