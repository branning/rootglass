# Notices

Rootglass is licensed under the MIT License. See [LICENSE](LICENSE).

## Generated runtime assets

The repository does not commit guest operating system images, QEMU binaries, or generated runtime assets. Preview desktop packages include generated per-platform QEMU and Alpine Linux runtime assets.

The example asset materialization script downloads Alpine Linux aarch64 release artifacts and creates local root filesystem and state images. Those generated files are ignored by Git and are not part of the Rootglass source distribution.

Generated local paths include:

```text
examples/hello-world.glass/rootfs.img
examples/hello-world.glass/state.img
runtime-assets/alpine-aarch64/vmlinuz-virt
runtime-assets/alpine-aarch64/initramfs-virt
```

## Third-party tools and assets

Preview desktop packages bundle QEMU from Ubuntu 24.04's `qemu-system-arm` package, Homebrew's `qemu` formula, or the Chocolatey QEMU package for their respective host platforms. Each runtime's `runtime-info.json` and `assets.json` record the source, version, executable hash, and hashes of bundled support files. The runtime package includes QEMU's GPL-2.0 license text and available platform package copyright files; each upstream component retains its own license terms.

The example fixture and packaged boot assets use Alpine Linux 3.23.8 aarch64 release artifacts. They are distributed under their respective upstream terms. The runtime bundle includes the netboot source URL and an Alpine runtime notice.

## Rust dependencies

Rust crate dependencies are resolved through Cargo and pinned in `Cargo.lock`. Each dependency is governed by its own license terms as published by its upstream project.
