# Bundle Format

A Rootglass bundle is a portable description of a small runnable world. It should contain enough information for the desktop shell to boot the embedded application without requiring the user to understand the emulator, kernel, device tree, or root filesystem details.

The user-facing bundle name is a **glass**. The serialized bundle extension is `.glass`.

## Goals

- Make embedded application delivery feel like opening a desktop app.
- Keep root filesystems swappable.
- Keep the base root filesystem read-only and reproducible.
- Allow optional writable state through an explicit overlay partition.
- Allow the host app/runtime to evolve independently from individual guest images.
- Provide clear validation errors before trying to boot.
- Avoid baking host-specific paths into distributed bundles.
- Require publisher identity/signature metadata before running a distributed bundle.

## Non-goals

- A complete package manager.
- A full OCI/Docker image replacement.
- A general-purpose virtual machine definition format.
- A hardware simulation description language.
- Multiple launch profiles in the MVP.
- Kernel or device-tree override support in the MVP.

## Initial bundle shape

During development, a glass may be represented as a directory:

```text
hello-world.glass/
  rootglass.toml
  rootfs.img
  state.img                 # optional writable overlay partition
  metadata/
    icon.png
    README.md
  signatures/
    bundle.sig              # exact signature format TBD
```

For distribution, the same internal structure should be serialized as a single `.glass` archive.

## Storage model

A glass has one required immutable base and may have one serialized writable overlay.

Required:

- **Read-only rootfs** — the base guest filesystem. This is the reproducible application world.

Optional:

- **Writable overlay partition** — mutable guest state included with the bundle or created locally.

MVP constraints:

- Exactly one required read-only rootfs.
- At most one writable overlay partition inside a serialized `.glass` bundle.
- The local Rootglass application may create and track multiple writable overlay partitions for a glass.
- Only one writable overlay may be loaded at a time with the rootfs.
- Multiple read-only overlay packs may be useful later, but are not part of the MVP.

## Draft manifest

```toml
schema_version = 1
name = "Hello World"
identifier = "com.example.hello-world"
version = "0.1.0"
description = "Minimal Rootglass bundle used for runtime smoke testing."

[publisher]
name = "Example Publisher"
identity = "example.com"
signature = "signatures/bundle.sig"

[runtime]
architecture = "aarch64"
profile = "virt-linux"
requires_hardware_acceleration = true

[runtime.host_constraints]
macos = { hypervisor = true }
windows = { whpx = true }
linux = { kvm = true }

[boot]
kernel = "builtin:aarch64-linux"
device_tree = "builtin:aarch64-virt"
rootfs = "rootfs.img"
writable_overlay = "state.img"
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

### `publisher`

Publisher identity and signature metadata. Rootglass should require a widely usable signature format that proves the bundle was created by a named entity.

The exact signature mechanism is not chosen yet. The design should preserve:

- Human-readable publisher name.
- Stable publisher identity.
- Signature over the manifest and bundle assets.
- A verification result that can be displayed to the user.
- A policy hook for future trust decisions.

### `runtime.architecture`

Target guest architecture. The first useful value is `aarch64`.

### `runtime.profile`

Named runtime profile understood by Rootglass. The first likely profile is `virt-linux`, mapping to QEMU's generic aarch64 virt machine.

### `runtime.requires_hardware_acceleration`

Whether the bundle requires host virtualization acceleration. The MVP should allow bundles to declare that acceleration is required rather than silently falling back to an unusably slow path.

### `runtime.host_constraints`

Host platform constraints. These may include platform-specific acceleration requirements such as Apple's Hypervisor framework on macOS, WHPX on Windows, or KVM on Linux.

### `boot.kernel`

Kernel reference. For the MVP, only builtin runtime-profile kernels are allowed.

Future versions may allow bundle-provided kernel overrides or relative bundle paths, but the MVP should not.

### `boot.device_tree`

Device tree reference. For the MVP, only builtin/default device trees implied by the runtime profile are allowed.

Future versions may allow bundle-provided device-tree overrides or relative bundle paths, but the MVP should not.

### `boot.rootfs`

Relative path to the required read-only root filesystem image inside the bundle.

### `boot.writable_overlay`

Optional relative path to the writable overlay partition included in the bundle. A serialized `.glass` bundle may include at most one.

Local Rootglass state may track multiple writable overlays for the same glass, but only one may be selected for a launch.

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
- More than one writable overlay in a serialized `.glass` bundle.
- Unsupported architecture.
- Unsupported runtime profile.
- Invalid resource request.
- Required host acceleration is unavailable.
- Kernel/device tree reference is not a supported builtin profile reference.
- Bundle signature is missing or fails verification.

## Archive extension

The serialized bundle extension is `.glass`.

Directory bundles may also use a `.glass/` suffix during development so the filesystem shape mirrors the packaged artifact.

## Decisions captured

- Bundle name: **glass**.
- Bundle extension: `.glass`.
- Kernel/device-tree assets: builtin runtime profile only for MVP; optional overrides reserved for future use.
- Mutable rootfs state: optional writable overlay partition.
- Serialized bundle overlays: at most one writable overlay.
- Local overlays: multiple may be created and tracked by the app; one selected at launch.
- Signatures: required before running distributed bundles; exact widely usable format TBD.
- Host platform constraints: included in the manifest.
- Multiple launch profiles: left open, not MVP.
- Future read-only overlay packs: left open, not MVP.

## Open questions

- Which signature format should Rootglass require?
- What rootfs image formats should be accepted first?
- What exact `.glass` archive/container format should be used?
- How should local writable overlays be named, stored, cloned, and deleted?
- How should Rootglass explain publisher identity and trust to a nontechnical user?
