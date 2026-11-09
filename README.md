# Rootglass

Tiny worlds under glass.

Rootglass is a cross-platform shell for running self-contained embedded Linux application environments on a regular computer. One Rust executable provides the GPUI desktop, a Ratatui TUI, and CLI commands. The implementation can validate, verify, import, plan, and boot signed `.glass` environments through QEMU, with cross-platform package builds.

The `rootglass` executable opens GPUI by default. Passing a `.glass` file opens it in GPUI too, including when the OS launches Rootglass through the file association. Use `rootglass gui [path-to.glass]` to request that explicitly, `rootglass tui` to open the terminal UI, or commands such as `rootglass validate` and `rootglass boot` for CLI workflows. The embedded Ghostty terminal starts the TUI session through an internal mode of the same executable.

## Complete application build specification

[**App spec and coding-agent swarm build contract**](docs/app-spec.md) describes the existing Rust implementation, GPUI Kit desktop UI, signed bundle install/release path, QEMU lifecycle, production trust, OS packaging, acceptance tests, and dependency-ordered work packages. Coding agents should start with [AGENTS.md](AGENTS.md).

## Implementation backlog

Use [Ergo v6](https://github.com/sandover/ergo) to browse the tracked backlog: `ergo list --ready` and `ergo show <id>`. See [setup and the atomic agent workflow](docs/agent-workflow.md). The backlog preserves merged S00–S11 history, separates open S12–S15 acceptance/release gates, and keeps post-v1 work in draft.

## Current status

The CLI remains the reference workflow, and the GPUI Kit desktop shell now uses the shared application service. The current implementation supports:

- directory-form `.glass/` fixtures for development,
- ZIP-based serialized `.glass` archive verification,
- manifest parsing and validation,
- DSSE/Ed25519 signature verification,
- builtin aarch64 runtime profile resolution,
- read-only rootfs plus optional writable overlay modeling,
- QEMU launch-plan generation,
- an interactive CLI boot command once local assets exist,
- CI for formatting, clippy, tests, and CLI smoke checks.

The native desktop shell uses [GPUI Kit](https://github.com/longbridge/gpui-kit) `=0.9.0`, a curated distribution of Zed's GPUI with components. The desktop shell and bundled runtime packaging are implemented; clean-host installer validation, release signing, and GUI acceptance coverage remain open. See [the delivery roadmap](docs/roadmap.md) and [desktop package status](docs/desktop-packages.md).

### macOS preview first-launch caveat

The macOS preview DMGs, including v0.0.2 and v0.0.3, are unsigned and not notarized. After mounting the DMG, drag `Rootglass.app` to the Applications folder. If macOS reports that the app is damaged, recursively remove quarantine from the installed app bundle in Terminal:

```sh
xattr -dr com.apple.quarantine /Applications/Rootglass.app
```

The recursive form is needed because downloaded app bundles can retain quarantine on nested files even after the app's top-level attribute is removed. It clears quarantine attributes from this app bundle; it does not sign or notarize the app. Only use it for a Rootglass app downloaded from the official release. The v0.0.2 macOS runtime has a separate invalid QEMU signature that causes macOS to kill QEMU at startup; clearing quarantine does not fix that release. The fix is in the v0.0.3 preview and passed an installed-app boot smoke on a macOS CI runner. See the [preview release notes](CHANGELOG.md) and [desktop package limits](docs/desktop-packages.md).

## Quickstart

### Prerequisites

For normal development checks:

- Rust stable toolchain
- Cargo
- [`just`](https://github.com/casey/just), optional but recommended

For local example asset generation and boot:

- Linux host or VM
- `qemu-system-aarch64`
- `curl`, `tar`, `dd`, `mkfs.ext4`, `e2label`, `sudo`, `mount`, and `umount`.
- `cpio`, `gzip`, and `unsquashfs` (provided by `squashfs-tools`) to rebuild the initramfs.
- `python3` to run the bounded serial smoke test.

The asset materialization script currently mounts an ext4 image, so macOS and Windows users should run it inside a Linux VM or container with loop-device support.

### Run CI-equivalent checks

```bash
just check
```

This runs formatting checks, clippy, tests, and CLI smoke tests.

Equivalent manual commands:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo run --locked --manifest-path crates/rootglass-desktop/Cargo.toml --no-default-features -- validate examples/hello-world.glass
cargo run --locked --manifest-path crates/rootglass-desktop/Cargo.toml --no-default-features -- plan examples/hello-world.glass
```

### Validate the example glass

```bash
just validate
```

Equivalent:

```bash
cargo run --locked --manifest-path crates/rootglass-desktop/Cargo.toml --no-default-features -- validate examples/hello-world.glass
```

### Print the QEMU launch plan

```bash
just plan
```

Equivalent:

```bash
cargo run --locked --manifest-path crates/rootglass-desktop/Cargo.toml --no-default-features -- plan examples/hello-world.glass
```

This command does not require generated rootfs images. It verifies the manifest/signature path and prints the QEMU command that would be used after assets exist.

### Materialize local example assets

```bash
just materialize-assets
```

Equivalent:

```bash
bash scripts/materialize-example-assets.sh
```

This downloads canned Alpine Linux aarch64 assets and generates local files ignored by Git:

```text
examples/hello-world.glass/rootfs.img
examples/hello-world.glass/state.img
runtime-assets/alpine-aarch64/vmlinuz-virt
runtime-assets/alpine-aarch64/initramfs-virt
```

### Boot the example glass

```bash
just boot
```

Equivalent:

```bash
cargo run --locked --manifest-path crates/rootglass-desktop/Cargo.toml --no-default-features -- boot examples/hello-world.glass
```

The boot command validates the manifest, verifies the DSSE/Ed25519 signature, checks that generated assets exist, and then starts QEMU interactively.

With QEMU `-nographic -serial mon:stdio`, exit with:

```text
Ctrl-a x
```

If assets are missing, boot fails before QEMU starts, for example:

```text
error: missing kernel asset: runtime-assets/alpine-aarch64/vmlinuz-virt
```

### Try the release example bundle

Upcoming desktop releases include a materialized, signed Hello World `.glass`
and a SHA-256 checksum. After installing Rootglass and downloading both assets:

```sh
sha256sum --check SHA256SUMS-example.txt
rootglass plan rootglass-hello-world-0.1.0.glass
rootglass boot rootglass-hello-world-0.1.0.glass
```

The plan command verifies the bundle signature and reports preflight details.
When `plan` or `boot` receives a ZIP `.glass` archive, it verifies and imports
the package into the local content-addressed library first. A directory-form
`.glass` is planned or booted in place.
To log in to the disposable sample guest, use `root` and password
`rootglass-ci`. The fixture key and password are public demo credentials; do
not use the guest for sensitive data. The installed desktop app does not trust
the sample signing key by default; explicitly add the demo publisher to the
local trust file before opening the sample in the GUI. See [the trust setup and
release asset details](docs/example-assets.md#trusting-the-release-example-in-the-desktop-app)
and [desktop package notes](docs/desktop-packages.md). The published `v0.0.1`
release remains immutable and unchanged. The materialized example is included
with the `v0.0.2` preview release.

## What is a `.glass`?

A **glass** is a signed bundle describing a small runnable world.

Rootglass supports two forms:

- **Development directory form:** `examples/hello-world.glass/`
- **Serialized archive form:** a ZIP archive with the `.glass` extension

The archive form uses strict path rules and a canonical signed file set. Every file in a production archive must be covered by the DSSE statement except `signatures/glass.dsse.json` itself.

See:

- [Bundle format](docs/bundle-format.md)
- [Signature format](docs/signatures.md)
- [Writable overlays](docs/overlays.md)

## Repository layout

```text
rootglass/
  crates/
    rootglass-cli/          # CLI command library used by the rootglass binary
    rootglass-desktop/      # GPUI library and the single rootglass executable
    rootglass-core/         # Runtime orchestration and preflight
    rootglass-manifest/     # Manifest parsing and validation
    rootglass-qemu/         # QEMU launch-plan and spawn adapter
    rootglass-signature/    # DSSE/Ed25519 verification
  docs/
    architecture.md
    bundle-format.md
    example-assets.md
    milestone-1.md
    overlays.md
    signatures.md
  examples/
    hello-world.glass/      # Signed development-form example glass
  scripts/
    materialize-example-assets.sh
```

## Design goals

- One-click desktop workflow for nontechnical users eventually.
- Cross-platform host support: macOS, Windows, and Linux.
- Swappable root filesystems for different embedded applications.
- A stable host application that can run many app bundles over time.
- Prefer off-the-shelf kernels and device trees at first.
- Keep QEMU details behind a small Rust runtime interface.
- Make logs and failure states visible enough for support/debugging.
- Avoid turning the first version into a full board simulator or hardware-in-the-loop framework.

## Non-goals for Milestone 1

- GPUI desktop shell (delivered after Milestone 1).
- Nontechnical double-click workflow.
- Bundled QEMU installers.
- Production trust-store management.
- Full signing release automation.
- Cycle-accurate hardware simulation.
- Arbitrary peripheral modeling.
- Multi-node distributed simulation.
- Real-time guarantees.

## Working vocabulary

- **World** — the running embedded environment visible through Rootglass.
- **Glass** — the signed bundle containing a manifest, rootfs metadata, and launch metadata.
- **Manifest** — the structured file describing how to boot the glass.
- **Runtime** — the host-side Rust process supervision and QEMU orchestration layer.
- **Shell** — the future Rust/GPUI desktop app around the running world.

## Milestone 1

See [docs/milestone-1.md](docs/milestone-1.md) for current completion criteria and scope.

## License and notices

Rootglass is licensed under the [MIT License](LICENSE).

See [NOTICE.md](NOTICE.md) for notes about generated runtime assets, QEMU, Alpine Linux example assets, and Rust dependencies.
