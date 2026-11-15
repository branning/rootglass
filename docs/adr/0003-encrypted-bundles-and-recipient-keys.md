# ADR-0003: Encrypted bundles and recipient keys

- Status: Accepted in PR #146; implementation proceeds through Ergo tasks
- Date: 2026-11-15
- Source proposal: [Rootglass encryption specification](../Rootglass-encryption.md)
- Related format decision: [ADR-0004: Multi-artifact bundles and container layout](0004-multi-artifact-bundles-and-container.md)
- Scope: Encrypted `.glass` bundles, recipient authorization, local key providers,
  encrypted QEMU launch, and per-run state isolation

The maintainer accepted this protocol in merged PR #146. It does not change the
current schema-1 bundle contract. Implementation proceeds through the staged
Ergo tasks; this ADR does not authorize arbitrary host code or untyped QEMU
configuration.

## Context

The existing format is intentionally strict. Schema 1 rejects unknown fields;
production ZIP bundles require every file other than
`signatures/glass.dsse.json` to be covered by the publisher DSSE statement.
The current importer verifies and extracts ZIP members into an immutable,
content-addressed store. QEMU currently opens an extracted raw rootfs as a
read-only drive, and the desktop supports one guest at a time. Encryption must
preserve those schema-1 behaviors while defining any schema-2 behavior
explicitly.

Maintainer direction now calls for a `.glass` to hold multiple Linux images,
MCU firmware, typed device declarations, and possibly approved backend assets.
ADR-0004 selects ZIP as the first physical container with a signed logical
artifact catalog. This ADR does not make `rootfs.img`, ZIP paths, or a single
payload the permanent schema-2 encryption model. The manifest-version and
catalog integration follow the accepted shared design in both ADRs.

The encryption proposal also needs to distinguish three separate signing
questions: `.glass` publisher signatures, code signing for desktop
distribution, and stable app identity or entitlements used by some macOS
keychain features. Desktop distribution signing is deferred while certificate
setup is pending. This is a schedule and credential constraint, not a decision
to ship unsigned, forgo signing entitlements, or remove Secure Enclave support.
Development can proceed with providers available to current local builds;
signing-dependent production behavior remains open for qualification.
The final distribution-signing and macOS entitlement/keychain architecture are
not decided by this ADR. Secure Enclave remains a target provider, not an
optional path to remove because certificate setup is pending.

## Proposed decisions

### 1. Versioning and compatibility

1. Encrypted bundles use manifest `schema_version = 2` and a version-2 Rootglass
   publisher predicate. Schema 1 remains unchanged: the v1 parser continues to
   reject unknown fields, all existing unencrypted signed fixtures retain their
   behavior, and no automatic manifest migration occurs.
2. A schema-1 manifest containing encryption fields is invalid. Schema 2 is the
   single strict schema for the coordinated artifact catalog, profiles, and
   optional encryption extension. It may describe unencrypted multi-artifact
   bundles; each encrypted artifact requires its approved encryption
   declaration and recipient binding. Unsupported schema versions and unknown
   v2 fields fail closed.
3. Schema 2 keeps the existing signed bundle identity but encryption refers to
   logical artifact IDs rather than making `boot.rootfs` or a ZIP entry path a
   permanent API. The initial encrypted Linux profile may contain one LUKS
   disk. Its manifest declaration includes a fixed format ID, hardware-policy
   minimum, and signed concurrency ceiling. The schema/version allocation and
   catalog shape follow ADR-0004. The
   manifest accepts no keys, passwords, arbitrary QEMU arguments, or host
   paths.
4. The publisher statement uses a distinct v2 predicate URI and continues to
   sign the SHA-256 digest of the manifest and every immutable file. The exact
   v2 Rust structs and JSON predicate are specified in E01 after this review.

### 2. Rootfs encryption

1. The initial encrypted Linux disk is a standalone QEMU LUKS v1 volume
   (`qemu-luks-v1`). Its logical artifact ID is stable even if the selected
   container maps it to `rootfs.img` or another physical entry. Do not accept
   QEMU's legacy `aes` qcow2 encryption. QEMU documents LUKS v1 as compatible with
   `dm-crypt`/`cryptsetup` and accepts a secret object for the LUKS passphrase.
2. Generate one uniformly random 32-byte Image Master Key (IMK) per encrypted
   bundle. The IMK is the high-entropy LUKS passphrase; the LUKS header's
   PBKDF2 parameters derive the volume key. Human passwords are not accepted as
   IMKs, and no password KDF or fallback suite is part of this protocol.
3. Use the QEMU LUKS v1 profile with AES-256-XTS, `plain64` IV generation,
   SHA-256 for the LUKS keyslot KDF, and an explicit keyslot iteration time of
   2 seconds. The LUKS header must identify the profile; readers reject
   unsupported cipher/KDF/header parameters rather than guessing. E02/E09 must
   verify that every packaged QEMU version supports this profile before it is
   advertised.
4. QEMU opens the decrypted rootfs through its LUKS block driver and exposes
   it read-only to the guest. The IMK is never written into the manifest,
   archive metadata, process arguments, environment, persistent state, or
   diagnostics.

### 3. Publisher and recipient authorization

1. The publisher DSSE signature remains the trust root and covers the v2
   manifest and every encrypted payload artifact. The signed v2 manifest
   contains one or more Ed25519 recipient-authority public keys. Each authority
   is permitted only to issue recipient envelopes for that bundle version.
2. Encrypted schema-2 archives contain exactly one mutable sidecar at
   `signatures/recipients.dsse.json`. The only publisher-subject exclusions in
   schema 2 are the publisher envelope and this recipient envelope. Every
   other file is signed by the publisher; every other unsigned file is
   rejected.
3. `recipients.dsse.json` is a DSSE envelope with payload type
   `application/vnd.rootglass.recipients+json;version=1` and exactly one Ed25519
   signature from an authority listed in the publisher-signed manifest. Its
   payload uses canonical JSON (RFC 8785), rejects duplicate and unknown
   fields, and identifies the format version, bundle identifier and version,
   manifest SHA-256, encrypted-artifact-set SHA-256, encryption suites, and
   recipient records. The artifact-set digest follows ADR-0004: a
   domain-separated canonical list sorted by logical artifact ID and binding
   each encrypted artifact's ID, ciphertext digest, ciphertext length, and
   suite. The signature is over the exact canonical payload bytes using DSSE
   pre-authentication encoding; the DSSE `keyid` must match the delegated key.
4. A recipient record has a unique recipient key ID, an explicit provider and
   wrapping-suite ID, and opaque wrapped IMK bytes. Limit the sidecar to the
   existing 16 MiB signature-envelope bound and at most 1,024 recipients.
   Duplicate recipient IDs, unknown suites/providers, wrong bundle/digest
   bindings, larger envelopes, and signatures from non-delegated authorities
   fail before any key unwrap.
5. Recipient personalization can replace only the recipient sidecar. It does
   not modify encrypted payloads or the publisher signature, and the
   personalizer must possess the IMK or use a separately approved KMS wrapping
   operation. An untrusted CDN cannot add recipients. Repacking changes the ZIP
   bytes and its whole-archive digest; “zero-rehash” means no re-encryption,
   encrypted-payload rehash, or new publisher signature is needed by the
   personalizer when recipients change. Rootglass still hashes every signed
   subject before launch.

### 4. Key wrapping and platform providers

The envelope uses explicit, versioned suites. There is no algorithm fallback.

| Suite ID | Key operation | Intended provider |
|---|---|---|
| `age-v1-x25519` | age v1 X25519 recipient encryption of the 32-byte IMK | Portable software identity |
| `hpke-v1-p256-hkdf-sha256-aes128gcm` | RFC 9180 DHKEM(P-256, HKDF-SHA256), HKDF-SHA256, AES-128-GCM | Apple Secure Enclave P-256 key agreement |
| `rsa-oaep-v1-sha256` | RSA-OAEP with SHA-256 and MGF1-SHA256; minimum 3072-bit key and empty OAEP label | Windows CNG TPM 2.0 and Linux TPM 2.0 |

Recipient IDs are algorithm-prefixed SHA-256 fingerprints of canonical public
key encodings. The delegated DSSE signature binds the wrap suite, recipient ID,
and wrapped bytes to the bundle digests. Software keys never satisfy a signed
hardware-required policy. Local user policy may be stricter than the manifest,
but may not weaken it. Missing devices, unsupported algorithms, unavailable
key stores, key denial, and hardware-policy mismatch fail closed with actionable
errors.

#### macOS keychain development path and signing qualification

For the current portable-provider development slice, store the Age identity as
a generic password/secret in the current user's **file-based login Keychain**.
The Rust `keyring` crate's macOS `keychain` backend allows this local work to
proceed without waiting for a Developer ID certificate. This is a development
path, not a decision that the eventual signed app must avoid entitlements,
keychain access groups, the Protected/Data Protection store, or a different
provider backend.

Ordinary login-Keychain use does not require a Developer ID distribution
certificate. The file-based Keychain uses per-item access control lists. An
unsigned build has no stable designated code requirement, so macOS may ask the
user to authorize access, and rebuilt executables may prompt again or fail an
ACL created for a different identity. Rootglass must surface cancellation or
denial, preserve the key, and never silently replace it. Test the selected
`keyring` version with unsigned local builds now, then qualify ACL stability
and prompts again with the signed distribution app after certificate setup.
CI tests use a mock provider rather than an interactive login Keychain.

Secure Enclave remains an in-scope hardware provider and is not replaced by the
portable Age provider. Its P-256 key-agreement API, persistent key access, and
any required code-signing entitlements/provisioning must be qualified on
supported Macs. Continue implementation and local experiments where current
development signing permits them. If a required entitlement or certificate is
not yet available, keep that qualification pending and resume it after setup;
do not remove the provider or make permanent product claims from the current
unsigned build.

On Windows the portable identity uses the current user's Windows Credential
Manager. On Linux it uses the session's Secret Service. Neither provider is
reported as hardware-backed. TPM private keys remain non-exportable and
provider-specific qualification must prove that the key operation actually
uses the TPM; software-backed CNG or software TPM implementations do not satisfy
the hardware policy.

### 5. ZIP authentication and QEMU byte ranges

1. The first proposed encrypted `.glass` uses the ZIP container recommended by
   ADR-0004. Each encrypted disk asset that QEMU reads by byte range must use
   method 0 (STORED), have the encrypted-entry flag clear, and have
   general-purpose bit 3 clear (no trailing data descriptor). The initial
   logical rootfs may map to `rootfs.img`; the signed catalog owns that map.
   Local-header and central-directory names, CRC, sizes, method, and flags must
   agree. Reject duplicate paths, overlap, truncation, ZIP64 until separately
   qualified, and any archive layout whose encrypted asset extents cannot be
   proven unambiguously.
2. Before unwrapping the IMK, validate archive bounds and paths, verify the
   trusted publisher DSSE signature, hash the manifest and every publisher
   subject (including every complete encrypted asset extent), verify the
   recipient-envelope DSSE signature against a delegated authority, and match
   all envelope bindings to the signed manifest and encrypted-artifact-set
   digest. CRC is an additional ZIP integrity check, not a substitute for
   SHA-256 or signatures.
3. The intended optimized path stores a fully verified encrypted archive as an
   immutable content-addressed object and records validated byte extents for
   encrypted disk assets. QEMU opens the archive read-only through a file node,
   a raw node with the validated `offset` and `size`, and then the LUKS block
   driver.
   E02/E09 must prove this chain on every packaged QEMU version before use. If a
   platform cannot safely open the bounded extent, it must use a separately
   verified immutable extracted image or fail closed; it must not pass the
   whole ZIP as a raw disk.
4. Until E02 changes and qualifies storage/resource limits, current archive,
   member, expanded-size, entry-count, and signature bounds remain in force.
   The source proposal's gigabyte-scale promise is not supported by today's
   256 MiB per-member and 512 MiB expanded-size limits. A larger encrypted
   archive profile requires a separately stated maximum, streaming verification,
   disk-space preflight, and ZIP64 qualification before implementation.
5. Development directory bundles remain developer/test inputs, not production
   encrypted distributions. If encrypted-directory test fixtures are needed,
   verify the signed files and pass the standalone LUKS image path to QEMU; do
   not pretend a directory has a ZIP byte extent.

### 6. Instance and state policy

1. Schema 1 keeps its current one-guest behavior. Encrypted schema 2 defaults
   to one concurrent instance and may declare a signed maximum from 1 through
   5. A local preference may lower, but not raise, that ceiling. Host resource
   preflight may still block a launch.
2. Each instance receives a unique run ID, an exclusive slot lock, its own
   ephemeral writable QCOW2 overlay, and an independent serial/QMP lifecycle.
   The immutable encrypted rootfs is shared read-only. Overlay encryption uses
   QEMU's LUKS format with an independent random per-run key.
3. The first encrypted implementation supports ephemeral state only. Stop or
   process exit reaps QEMU, zeroizes the host key buffer, and removes the
   overlay. After a crash, startup identifies orphaned session files and
   removes them after confirming no owned QEMU process still holds the run.
   The app makes the non-persistent state behavior visible before boot.
   Persistent encrypted state and recovery/export are a separate reviewed
   feature; there is no silent fallback to plaintext or persistent state.
4. File deletion is best-effort cleanup, not secure erasure. SSD wear leveling,
   snapshots, swap, hibernation, crash dumps, and QEMU memory lifetime limit
   guarantees about physical secret removal.

### 7. QEMU secret and control-channel handling

1. Rootglass launches QEMU paused, creates a private per-session control
   endpoint, injects named QEMU secret objects through QMP, opens the LUKS
   block nodes, and resumes only after successful setup. Secret values are
   absent from process arguments, environment variables, persistent files,
   debug output, telemetry, and user-facing errors.
2. QMP is not exposed on unauthenticated loopback TCP. Use a Unix-domain socket
   with a private session directory and owner-only permissions, and a Windows
   named pipe with a current-user-only ACL. If a qualified platform cannot
   enforce a private endpoint, encrypted boot fails closed.
3. Rust-side IMK and overlay-key buffers use zeroizing containers and are
   cleared on success, failure, cancellation, and process exit. QMP payload
   construction and diagnostics must redact the secret value. QEMU necessarily
   holds decryption material in its own memory while running; Rootglass cannot
   promise zeroization of QEMU, kernel, swap, or crash-dump memory.

### 8. Threat boundary

Encryption protects rootfs contents at rest and restricts decryption to an
authorized recipient/key provider. It does not protect plaintext while the VM
is running from the Rootglass host process, QEMU, a compromised host kernel,
the local administrator/root user, or guest code after the disk is attached.
Bundle name, publisher, version, file paths, encrypted size, recipient IDs and
count, and other non-secret manifest data remain visible. Rootglass does not
claim secure physical deletion or prevent a same-user attacker with host
control from observing a running guest.

### 9. Proposed headless CLI contract

Keep the existing `validate`, `plan`, and foreground `boot` commands. Add these
encrypted-runtime operations and a public recipient selector on `boot`:

```text
rootglass identity list
rootglass identity create --provider <age-x25519|apple-secure-enclave-p256|windows-tpm-rsa3072|linux-tpm-rsa3072> [--label LABEL]
rootglass identity delete <recipient-id> --confirm
rootglass recipients list <bundle.glass>
rootglass boot <bundle.glass> [--recipient <recipient-id>]
rootglass instances list
rootglass instances stop <run-id>
```

`identity list` and `recipients list` display public identities, fingerprints,
provider/hardware capability, and verification state only. `identity create`
generates a key in the selected OS provider and returns the public recipient ID;
`identity delete` requires `--confirm` and never exports or prints private key
material. `boot` remains foreground; `--recipient` selects a public recipient
ID and does not accept key material. Without it, boot may select the only
eligible local recipient; multiple candidates require an explicit ID. Boot
returns a nonzero exit status on
verification, provider, hardware-policy, capacity, QEMU, or cleanup failure.
`instances list/stop` operate only on Rootglass-owned session records and
private control endpoints in the current user's data directory; stop applies
the same bounded graceful-shutdown/escalation contract as the app service.
There is no detached or system-wide daemon in this proposal. The CLI accepts no
IMK, private key, password, or raw QEMU argument on its command line or in
environment variables.

## Consequences and required follow-up

- Schema 1 and current unencrypted examples remain unchanged.
- Recipient rotation requires access to the IMK or a separately approved
  wrapping service; a CDN alone cannot create a valid recipient envelope.
- The current ZIP verifier's whole-archive buffering and extracted-package
  storage do not implement the proposed encrypted path. E02/E03/E09 must add
  bounded streaming verification, archive-backed immutable storage, recipient
  authorization, and the QEMU byte-range/LUKS path without weakening v1.
- QEMU package versions, LUKS profile, QMP object setup, and byte-range/LUKS
  composition require targeted host evidence before release qualification.
- Development of the portable software provider can proceed while application
  signing credentials are being arranged. The final signing/entitlement model
  remains open; Secure Enclave and TPM support remain in scope and require
  provider-specific qualification.

## Decisions accepted in PR #146

1. Schema-2 boundary, coordinated catalog/encryption model, and unchanged
   schema-1 behavior.
2. LUKS v1 profile, 32-byte random IMK, and prohibition on legacy QEMU AES.
3. Delegated Ed25519 authority and one separately signed mutable recipient
   envelope.
4. age v1 / HPKE P-256 / RSA-OAEP-SHA256 wrapping-suite set.
5. Use the file-based login Keychain for current portable-provider development;
   leave the final distribution-signing, entitlement, and Keychain backend
   decisions open for signed-app qualification. Keep Secure Enclave in scope.
6. ZIP-first STORED, no-data-descriptor encrypted disk assets and the
   archive-backed byte-range QEMU path, with current size limits retained
   until separately qualified.
7. Ephemeral-only state and default-one/max-five instance policy.
8. Private QMP endpoints and the stated host/guest threat boundary.
9. The public-only identity, recipient-inspection, and local instance CLI
   commands; no secret input or detached daemon.

## References

- [Apple TN3137: On Mac keychain APIs and implementations](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)
- [Apple: Access Control Lists](https://developer.apple.com/documentation/security/access-control-lists)
- [Apple TN3127: Code signing requirements](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements)
- [Rust `keyring` macOS backend](https://docs.rs/keyring/latest/keyring/v1/)
- [`apple-native-keyring-store` file-based Keychain](https://docs.rs/apple-native-keyring-store/latest/apple_native_keyring_store/keychain/)
- [`apple-native-keyring-store` Protected Data store](https://docs.rs/apple-native-keyring-store/latest/apple_native_keyring_store/protected/)
- [Apple: Protecting keys with the Secure Enclave](https://developer.apple.com/documentation/security/protecting-keys-with-the-secure-enclave)
- [QEMU Disk Images](https://www.qemu.org/docs/master/system/images.html)
- [QEMU QMP block-device reference](https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html)
- [QEMU: Providing secret data](https://www.qemu.org/docs/master/system/secrets.html)
