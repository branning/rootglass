# Technical Specification: Rootglass (.glass) Encrypted Bundle Extension & Multi-Instance Runtime


1. Objective & Scope

Extend the Rootglass .glass bundle specification (schema_version = 1) and runtime (rootglass-core, rootglass-gui in GPUI, and rootglass-cli) to support per-user/per-node encryption, end-to-end DSSE/Ed25519 integrity, and concurrent multi-instance execution (2–5 simultaneous instances) across macOS, Windows, and Linux without altering Rootglass's core archive or storage principles.

Core Design Alignment with bundle-format.md

• Preserves .glass Archive Rules: Retains the standard ZIP archive structure and development .glass/ directory layout (rootglass.toml, rootfs.img, optional state.img, metadata/, signatures/).
• Zero-Rehash CDN Distribution: rootfs.img is encrypted once at build time as a LUKS-encrypted qcow2 (or raw LUKS) image and signed inside signatures/glass.dsse.json. Per-user/server recipient keys are added via an unsigned/separately-signed recipient envelope (signatures/recipients.json or signatures/recipients.dsse.json), matching how signatures/glass.dsse.json is excluded from base content hashing.
• Zero-Copy ZIP Mounting: Store rootfs.img inside the .glass ZIP archive using ZIP method 0 (STORED / uncompressed) so QEMU can mount the encrypted rootfs.img directly at its exact byte offset inside the .glass file via QEMU's raw block driver (offset + size) without extracting gigabytes to disk.
• Concurrent Multi-Instance Overlays: Leverages Rootglass's existing storage rule ("The local Rootglass application may create and track multiple writable overlay partitions for a glass; only one writable overlay may be loaded at a time with the rootfs") to spawn 2–5 isolated instances simultaneously, each paired with its own ephemeral LUKS-encrypted state-<instance>.qcow2 overlay.

2. .glass Bundle Extensions


A–C. Archive Structure, Manifest, and ZIP Rules

The .glass directory structure and ZIP layout incorporate an encrypted rootfs.img (stored uncompressed via ZIP method 0) alongside manifest extensions in rootglass.toml under the [encryption] table (supporting parameters like format = "luks-qcow2" and max_concurrent_instances = 5). Both signatures/glass.dsse.json and signatures/recipients.dsse.json are excluded from the base DSSE subject digest list to allow on-the-fly recipient injection. The full specification details are available in the source documentation.

3. Recipient Key Envelope (signatures/recipients.dsse.json)

To avoid sharing static passwords while keeping rootfs.img immutable on CDNs, envelope encryption is utilized via a DSSE/Ed25519 envelope. This includes provider types (AgeX25519, AppleSecureEnclaveP256, Tpm20RsaOaep), recipient entries containing wrapped Image Master Keys (IMKs), and a server/portal sealing flow that binds the rootfs_sha256 digest without requiring base re-hashing. For complete schema definitions, consult the reference material.

4. Device Identity & Key Providers (rootglass-core)

A unified DeviceIdentityProvider trait is implemented across targets to handle public keys, fingerprints, and IMK unwrapping. Supported providers include:
• Desktop Portable Provider (AgeKeyringProvider): Backed by macOS Keychain and Windows Credential Manager via the keyring crate.
• Server / Enterprise Node-Locked Providers: Utilizes hardware-backed mechanisms including Apple Secure Enclave (SecKeyCreateRandomKey), Windows CNG TPM 2.0 (NCryptCreatePersistedKey), and Linux tss-esapi, enforcing hardware constraints when require_hardware_backed = true.

5. Pre-Boot Validation & Integrity Pipeline

Pre-launch validation enforces archive paths, schema versions, hardware acceleration availability, ZIP STORED compliance for rootfs.img, and cryptographic verification of both publisher (glass.dsse.json) and recipient (recipients.dsse.json) DSSE signatures before unwrapping the IMK.

6. Multi-Instance QEMU Execution (2–5 Concurrent Instances)

To support 2–5 simultaneous instances of a single .glass bundle:
• Ephemeral Overlays: Each active instance slot (1..=max_concurrent_instances) generates a random overlay key and initializes an isolated LUKS-encrypted inst-<N>.qcow2 overlay file.
• Paused Spawn & QMP Secret Injection: QEMU is launched in paused mode (-S) across host platforms (macOS hvf, Windows whpx, Linux kvm), injecting the IMK and overlay secret via QMP objects (object-add, blockdev-add), attaching unique MAC addresses and serial consoles, and zeroizing secrets from memory.
• Cleanup: Child processes are managed via OS-level job objects or process groups with automatic overlay deletion upon termination.

7. GPUI Desktop Shell (rootglass-gui) Deliverables

• Identity & Device Key Bar: Displays local age1 public keys or TPM status with one-click copying and URI scheme integration.
• Bundle Verification Inspector: Renders publisher credentials, signature verification badges, and encryption diagnostics.
• Multi-Instance Workspace Manager: Provides tabbed/paned execution for up to max_concurrent_instances with live serial console streams and lifecycle controls.

8. Acceptance Criteria

1. Standard .glass Compatibility: Unencrypted bundles continue to validate and boot identically to the base specification.
2. Zero-Rehash Personalization: Injecting signatures/recipients.dsse.json succeeds without modifying rootfs.img or breaking publisher signatures.
3. ZIP Compression Guard: Compressed rootfs.img entries (DEFLATE) fail pre-boot validation with a STORED (method 0) requirement.
4. Concurrent Instance Isolation: Running up to 5 simultaneous instances isolates guest writes into distinct overlay files while leaving the archive checksum untouched.
5. Hardware Lock Enforcement: Enabling require_hardware_backed blocks software age keys across supported builds.
