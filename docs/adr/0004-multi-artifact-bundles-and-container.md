# ADR-0004: Multi-artifact `.glass` bundles and container layout

- Status: Proposed; maintainer review required before implementation
- Date: 2026-11-15
- Scope: Logical artifacts, launch profiles, and the physical `.glass` container
- Related: [bundle format](../bundle-format.md), [encryption proposal](../Rootglass-encryption.md), [ADR-0003](0003-encrypted-bundles-and-recipient-keys.md)

This proposal extends the v1 single-rootfs bundle without changing its
verification or runtime behavior. It recommends retaining ZIP as the first
multi-artifact transport while modeling Android-style named logical partitions
in a signed catalog. It does not adopt Android's `super.img` format, A/B
partition slots, or OTA update protocol.

## Context

One `.glass` should be able to carry multiple bootable Linux images, embedded
microcontroller firmware, device descriptions, and, where explicitly approved,
host-side emulator backends. The design target is at least five Linux images
and five MCU firmware assets in one bundle. A launch profile may select one or
compose several of those assets; holding several images in the bundle does not
mean every image must boot at once.

The current v1 contract is a ZIP archive with a single Linux rootfs, optional
state image, and one desktop guest at a time. ZIP is convenient to author,
inspect, sign, copy, and distribute, and it already has a bounded verifier and
an immutable content-addressed import path. Its central directory can locate
individually stored payload entries without inventing a second container
format. It does not by itself provide a stable logical partition model,
container-level sparse allocation, or efficient in-place updates.

Android dynamic partitions provide a useful design reference: logical names
and metadata map to physical storage extents. Rootglass has different goals.
It does not need Android's boot-slot or OTA machinery, and its guest images,
firmware, and emulator devices are not interchangeable with Android system
partitions. Any partition map must be Rootglass-owned, signed, bounded, and
independent of Android tooling.

## Options considered

| Layout | Authoring and compatibility | Access and update behavior | Cost and risk |
|---|---|---|---|
| ZIP entries for each artifact | Keeps the familiar `.glass` workflow and existing tooling; smallest change to import and signing | Central directory locates each entry; STORED entries permit validated byte ranges. Replacing any entry requires repacking the ZIP | ZIP paths and extents remain transport details; no container-level sparse extents or in-place updates |
| ZIP containing one partitioned payload | Retains a ZIP envelope but adds a Rootglass partition table and one large payload object | A logical partition can map to bounded extents; sparse allocation and range access can be defined independently of ZIP paths. ZIP and inner table both need validation | Two indexes, new parser and signing rules, and little immediate benefit over one ZIP entry per artifact |
| Dedicated versioned `.glass` partition container | Makes logical partitions and extents native to the file format | Can support aligned extents, sparse allocation, per-artifact verification, and future update strategies | New authoring tools, parser, recovery behavior, signing format, and migration path; highest implementation and audit cost |

## Proposed decision

1. Keep the serialized `.glass` as ZIP for the first multi-artifact format.
   Store each large Linux image and firmware blob as its own ZIP entry using
   STORED mode when direct range access is needed. Small metadata may use ZIP
   compression. The verifier returns a checked artifact reader; runtime code
   never consumes archive paths or unvalidated offsets directly.
2. Put stable logical artifact IDs and launch profiles in the signed manifest.
   Runtime and profile references use IDs, never ZIP paths, entry ordering, or
   raw offsets. A signed storage map connects each ID to the ZIP entry. This
   gives the bundle an Android-inspired logical partition catalog while
   preserving ZIP's single-file authoring and distribution convenience.
3. Require capacity for at least five Linux boot assets and five MCU firmware
   assets in one manifest. Apply explicit per-artifact and aggregate count,
   byte-size, and runtime-resource limits; five is a qualification floor, not a
   hard format maximum. The 5+5 count does not imply an unlimited payload size:
   implementation must state and qualify aggregate byte caps before use. A
   profile selects named nodes and may describe a composed run.
4. Keep the container API independent of ZIP. A future container can map the
   same IDs to partition extents if measured needs for sparse storage,
   incremental updates, deduplication, or ZIP limits justify its parser and
   tooling cost. A nested `super.img`-like payload is not part of the first
   format.
5. Use schema 2 as one coordinated strict manifest for the artifact catalog,
   launch profiles, versioned required-feature IDs, and optional encryption.
   An unencrypted multi-artifact schema-2 bundle is allowed; an encrypted
   artifact must carry its approved encryption declaration and recipient
   binding. Do not publish separate incompatible definitions of schema 2.
   ADR-0003 and this ADR use the same version boundary.

This is a format proposal, not approval to execute backend binaries or launch
arbitrary QEMU configurations. Implementation children remain staged until a
maintainer reviews the decision and its encryption/signature bindings.

## Logical model

The wire syntax is left to implementation planning, but the signed model has
three distinct parts:

- **Artifacts** are immutable byte assets with stable bundle-local IDs. The
  initial ID syntax is lower-case ASCII (`[a-z0-9][a-z0-9._-]{0,63}`) to avoid
  Unicode normalization and case-folding ambiguity. Each ID has a
  declared kind and media/format, exact byte length, SHA-256 digest, target
  architecture or board where applicable, and a storage locator. Initial
  kinds include Linux boot disks, MCU firmware, and explicitly approved
  backend assets.
- **Profiles** declare launchable nodes and select artifact IDs. A Linux node
  names its QEMU machine/profile and boot asset references. An MCU node names a
  concrete M-profile board/machine and firmware asset; a CPU label such as
  `arm-m3` alone is not a boot contract. A profile may launch independent
  QEMU processes where machines cannot share one supported QEMU topology.
- **Devices** are typed declarations associated with a supported machine and
  node. Built-in QOM and virtio devices use an allowlisted type and validated
  property schema. Manifests never contain arbitrary QEMU command strings,
  executable paths, or shell fragments.

The new strict manifest also declares a bounded set of versioned required
feature IDs for capabilities such as the artifact catalog, composed launch
profiles, MCU nodes, typed devices, and external backends. These are defined
feature identifiers with typed payloads, not a free-form flags bag. An
unsupported required feature fails during verification/preflight; Rootglass
never silently ignores a field that changes launch or trust semantics.

Every artifact must be covered by the publisher's authenticated inventory.
The inventory binds logical ID, kind, declared length, digest, and storage
mapping; import verifies every referenced artifact before exposing it to a
profile. Duplicate IDs, dangling references, unreferenced executable assets,
overlapping extents, unsupported kinds, and undeclared device properties fail
closed. Schema-1 verification and its single-rootfs profile remain unchanged.

### Encryption binding

An encrypted bundle's recipient envelope must authorize the complete encrypted
artifact set, not only a field named `rootfs.img`. Define the set as the
encrypted artifacts sorted by UTF-8 artifact ID, with each item containing the
ID, ciphertext digest, ciphertext length, and encryption-suite ID. Each array
item has the fields `id`, `ciphertext_sha256`, `ciphertext_size`, and
`suite_id`; IDs are sorted by their UTF-8 bytes before canonicalization. Its
digest is SHA-256 over the byte prefix
`rootglass-encrypted-artifact-set-v1\0` followed by an RFC 8785 canonical JSON
encoding of that array. Duplicate IDs are rejected before digest calculation.
The recipient envelope binds this digest together with the bundle
identity/version and signed manifest digest. ADR-0003 retains the initial LUKS
Linux-disk profile but must use this set-oriented binding. Future encrypted
firmware requires an explicit shared-key or per-artifact key policy and an
envelope capable of authorizing all required keys before that capability is
implemented.

### QEMU and host-code boundary

An MCU firmware artifact is data supplied to an identified QEMU machine; it
does not itself select arbitrary devices or host processes. QEMU's Arm system
emulator requires a concrete board/machine model, and support for a Cortex-M3
must be qualified against that model and the packaged QEMU build. A system
profile declares which Linux and MCU nodes run together, their resource limits,
startup ordering, process boundaries, and failure cleanup.

External emulator or virtio backend executables are host code. A future profile
may reference only a separately approved backend ID whose binary digest,
publisher, target OS/architecture, capabilities, IPC endpoints, and lifecycle
are signed and policy-checked. Import never executes them. Unknown backends
fail closed. Launch requires an explicit trust/capability policy and the
strongest supported process isolation; platform gaps must be explicit rather
than silently running arbitrary bundled executables.

## Compatibility and migration

- Schema 1 remains the current production format: one Linux guest, existing
  signed ZIP members, and the established verifier/import behavior.
- The multi-artifact schema is a new strict schema. It rejects unknown fields,
  kinds, devices, storage mappings, and schema versions. It does not reinterpret
  a schema-1 `boot.rootfs` as a catalog or silently migrate signatures.
- Existing schema-1 bundles remain bootable by new releases. Older Rootglass
  releases reject a future schema they do not support.
- A migration/authoring tool can create a new schema while preserving original
  artifacts; it never rewrites a signed schema-1 bundle in place.

## Review and implementation gates

Before implementation opens, review the ZIP-first recommendation, schema
version coordination with ADR-0003, artifact and profile limits, canonical
encrypted-set digest, MCU machine contract, and host-code policy. The
implementation must qualify actual five-plus-five catalog capacity and a
composed profile using representative Linux and MCU assets, while preserving
the current schema-1 boot evidence.

References:

- [Android dynamic partitions](https://source.android.com/docs/core/ota/dynamic_partitions/implement) — logical partition metadata and extent mapping; Android-specific A/B and OTA behavior is not adopted.
- [QEMU Arm system emulator](https://www.qemu.org/docs/master/system/target-arm.html) — machine-specific Arm system emulation and MCU profiles.
- [QEMU multi-process](https://www.qemu.org/docs/master/devel/multi-process.html) — process boundaries for multi-process device models.
- [QEMU virtio backends](https://www.qemu.org/docs/master/devel/virtio-backends.html) — external backend interface and lifecycle considerations.
