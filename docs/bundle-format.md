# Bundle Format

A Rootglass bundle is a portable description of a small runnable world. It should contain enough information for the desktop shell to boot the embedded application without requiring the user to understand the emulator, kernel, device tree, or root filesystem details.

## Goals

- Make embedded application delivery feel like opening a desktop app.
- Keep root filesystems swappable.
- Allow the host app/runtime to evolve independently from individual guest images.
- Provide clear validation errors before trying to boot.
- Avoid baking host-specific paths into distributed bundles.

## Non-goals

- A complete package manager.
- A full OCI/Docker image replacement.
- A general-purpose virtual machine definition format.
- A hardware simulation description language.

## Initial bundle shape

A bundle may start as a directory during development:

```text
hello-world.rootglass/
  rootglass.toml
  rootfs.img
  metadata/
    icon.png
    README.md
```

Later it may become a single archive with the same internal structure.

## Draft manifest

```toml
schema_version = 1
name = "Hello World"
identifier = "com.example.hello-world"
version = "0.1.0"
description = "Minimal Rootglass bundle used for runtime smoke testing."

[runtime]
architecture = "aarch64"
profile = "virt-linux"

[boot]
kernel = "builtin:aarch64-linux"
device_tree = "builtin:aarch64-virt"
rootfs = "rootfs.img"
kernel_args = [
  "console=ttyAMA0",
  "root=/dev/vda",
  "rw"
]

[resources]
memory_mb = 1024
cpus = 2

[display]
mode = "serial-console"

[network]
mode = "user"
```

## Field notes

### `schema_version`

Integer manifest schema version. The first implementation should reject unsupported versions with a clear message.

### `name`

Human-readable display name.

### `identifier`

Stable reverse-DNS-style identifier for the bundle.

### `version`

Bundle version. SemVer is preferred but not required until enforcement is useful.

### `runtime.architecture`

Target guest architecture. The first useful value is `aarch64`.

### `runtime.profile`

Named runtime profile understood by Rootglass. The first likely profile is `virt-linux`, mapping to QEMU's generic aarch64 virt machine.

### `boot.kernel`

Kernel reference. The initial version should support builtin references and relative bundle paths.

Examples:

- `builtin:aarch64-linux`
- `vmlinuz`
- `assets/Image`

### `boot.device_tree`

Device tree reference. The first version may use a builtin/default device tree implied by the runtime profile.

### `boot.rootfs`

Relative path to the root filesystem image inside the bundle.

### `boot.kernel_args`

Extra kernel arguments. The runtime may append required arguments.

### `resources`

Requested host resources. The runtime should clamp or reject unsupported values.

### `display.mode`

Initial modes:

- `serial-console` — show guest serial console in the Rootglass UI.
- `none` — run without display.

Possible later modes:

- `framebuffer`
- `web-forward`
- `guest-agent`

### `network.mode`

Initial modes:

- `none`
- `user`

Later modes may include explicit port forwarding, tap networking, or isolated host-only networking.

## Validation principles

Validation should happen before QEMU launch and produce actionable messages.

Examples:

- Missing manifest.
- Unsupported schema version.
- Missing rootfs file.
- Unsupported architecture.
- Unsupported runtime profile.
- Invalid resource request.
- Kernel/device tree reference cannot be resolved.

## Archive extension

Potential extensions:

- `.rootglass`
- `.rglass`
- `.world`

For now, prefer a directory during development and defer the archive extension until the first distribution path is clearer.

## Open questions

- Should the bundle include kernel/device-tree assets or rely on Rootglass builtin runtime profiles?
- Should mutable rootfs state be copied to a per-run overlay by default?
- Should a bundle be signed before Rootglass runs it?
- Should the manifest include host platform constraints?
- Should the manifest support multiple launch profiles in one bundle?
