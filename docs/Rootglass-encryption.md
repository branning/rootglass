# Rootglass encrypted bundles and multi-instance runtime

**Status: proposal for maintainer security review.** No encrypted-bundle format
or runtime behavior is approved or implemented yet. The current schema-1 bundle
contract remains unchanged. See [ADR-0003](adr/0003-encrypted-bundles-and-recipient-keys.md)
for the security decisions requiring review; if this overview and the ADR differ,
the ADR is authoritative during review.

## Goal

Add encrypted `.glass` distributions with recipient-specific key wrapping and
isolated concurrent guest runs while preserving existing unencrypted bundles,
offline verification, publisher trust, and immutable root filesystems. The
portable software provider must work independently of desktop application
distribution signing. Secure Enclave and TPM providers have their own hardware
and platform qualification gates.

## Proposed bundle and trust model

- Encrypted bundles use a new strict manifest and publisher-predicate schema
  version. Schema-1 bundles and their signatures retain their current behavior;
  schema-1 encryption fields and unsupported versions fail closed.
- The publisher signs the encrypted manifest and complete LUKS rootfs image.
  The signed manifest delegates recipient management to explicit Ed25519
  recipient-authority keys.
- An encrypted archive contains one separately signed
  `signatures/recipients.dsse.json` sidecar. The recipient-authority signature
  binds the bundle identity/version, manifest digest, encrypted-rootfs digest,
  suite identifiers, recipient identities, and wrapped keys. This sidecar is
  the only additional file excluded from the publisher's signed subject list.
- Replacing the recipient sidecar does not modify or require re-encryption of
  the rootfs or a new publisher signature. The personalizer must have the image
  key or an independently approved wrapping service; the CDN is not trusted to
  add recipients. Rootglass still hashes every publisher-signed subject before
  any key unwrap or launch.

## Proposed rootfs and ZIP profile

The rootfs is a standalone QEMU LUKS v1 volume identified by the suite
`qemu-luks-v1`. It uses AES-256-XTS, `plain64` IV generation, SHA-256 for the
LUKS keyslot PBKDF2 KDF, and a 2-second keyslot iteration time. A uniformly
random 32-byte Image Master Key (IMK) is the LUKS passphrase. Human passwords,
legacy QEMU `aes` encryption, and implicit algorithm fallback are not allowed.
Packaged QEMU versions must be qualified against the exact LUKS profile.

In production ZIP archives, `rootfs.img` is STORED without ZIP encryption or a
trailing data descriptor. The local header and central-directory name, CRC,
sizes, method, and flags must agree. Duplicate, overlapping, truncated, or
ambiguous extents fail validation. The verifier validates archive paths and
bounds, publisher trust and signature, every signed subject digest, and the
delegated recipient signature and its bindings before unwrapping the IMK.
CRC-32 is an additional archive check and does not replace cryptographic
verification.

The intended efficient path retains a verified archive as an immutable,
content-addressed object and opens only the bounded `rootfs.img` byte extent
through a QEMU file node, a raw node with the validated `offset` and `size`,
and a LUKS block driver. Every packaged QEMU version must prove that chain
works with the rootfs opened read-only. If a host cannot safely open a bounded
extent, it must use a separately verified immutable extracted image or fail
closed; it must never pass the complete ZIP as a raw disk.

Until the archive verifier and resource policy are updated and qualified, the
current 256 MiB per-member and 512 MiB expanded-size limits remain in effect.
Gigabyte-scale bundles require explicit larger limits, streaming verification,
disk-space preflight, and ZIP64 support. Directory-form `.glass` bundles remain
development/test inputs and do not use ZIP extents.

## Proposed key-wrapping providers

Each recipient record names a versioned algorithm suite. Unknown suites and
providers fail closed; there is no downgrade fallback.

| Suite | Intended provider | Hardware-backed? |
|---|---|---|
| `age-v1-x25519` | Portable Age identity | No |
| `hpke-v1-p256-hkdf-sha256-aes128gcm` | Apple Secure Enclave P-256 key agreement | Yes, only after device qualification |
| `rsa-oaep-v1-sha256` | Windows CNG TPM 2.0 or Linux TPM 2.0, RSA key at least 3072 bits | Yes, only after device qualification |

Recipient IDs are algorithm-prefixed SHA-256 fingerprints of canonical public
keys. The software provider stores the Age private identity in the user's OS
credential store: the macOS file-based login Keychain, Windows Credential
Manager, or Linux Secret Service. It exposes only public identities and
fingerprints. It does not qualify as hardware-backed.

### macOS keychain and application signing

The portable provider uses the classic file-based login Keychain through the
Rust `keyring` crate's macOS `keychain` backend. This does not require a
Developer ID distribution certificate. Keychain ACLs can still prompt or deny
access; unsigned builds lack a stable code-signing designated requirement, so
rebuilt binaries may need authorization again. Rootglass surfaces those errors
and never silently replaces a stored identity.

The Protected/Data Protection Keychain and keychain access groups are not used
for this portable provider. Secure Enclave keys are a separate hardware
provider, and its access requirements and entitlements must be qualified
independently. Waiting for application distribution certificates does not
block the ordinary login-Keychain-backed software provider.

## Proposed instances, overlays, and QEMU secrets

- Schema 1 remains limited to one guest. Encrypted schema 2 defaults to one
  instance and may declare a signed maximum from 1 to 5. Local preferences may
  lower this limit but never raise it.
- Each run has its own run ID, exclusive slot lock, serial stream, QMP session,
  and ephemeral writable QCOW2 overlay. The encrypted rootfs is shared
  read-only; every overlay receives a distinct random key.
- The initial encrypted implementation supports ephemeral state only. On
  shutdown or exit, Rootglass reaps QEMU, clears its host key buffer, and removes
  the overlay. Startup handles orphaned session files after confirming that no
  corresponding QEMU process is alive. Persistent encrypted state, recovery,
  and export require separate design and review. File deletion is best effort,
  not secure erasure.
- QEMU starts paused. Rootglass provides secret objects through QMP over a
  private Unix socket with owner-only permissions or a Windows named pipe with
  a current-user-only ACL. QMP is not exposed on unauthenticated loopback TCP.
  Secrets never appear in arguments, environment variables, persistent files,
  logs, diagnostics, or telemetry. Rust buffers are zeroized best-effort;
  QEMU necessarily holds decryption material in memory while the VM runs.

## Proposed headless CLI contract

Keep `rootglass validate`, `plan`, and foreground `boot`. Add public identity
management, verified recipient inspection, and local instance management:

```text
rootglass identity list
rootglass identity create --provider <provider> [--label LABEL]
rootglass identity delete <recipient-id> --confirm
rootglass recipients list <bundle.glass>
rootglass boot <bundle.glass> [--recipient <recipient-id>]
rootglass instances list
rootglass instances stop <run-id>
```

Identity and recipient inspection show only public keys, fingerprints,
provider capability, and signature status. Key creation stores private
material directly in the OS provider; deletion requires confirmation. Instance
commands operate on Rootglass-owned local sessions. `boot --recipient` selects
a public recipient ID when multiple local keys can open the bundle. No command
accepts a private key, IMK, password, or raw QEMU option. CLI validation,
provider, hardware-policy, capacity, launch, and cleanup failures return a
nonzero exit status. The TUI and CLI call the shared application service rather
than each other.

## Threat boundary

Encryption protects the rootfs at rest and limits decryption to an authorized
recipient identity/provider. It does not protect plaintext during execution
from Rootglass, QEMU, the host kernel, a host administrator, or guest code after
the disk is attached. Bundle identity, publisher, version, paths, ciphertext
size, and recipient IDs/count remain visible. Rootglass does not promise secure
physical deletion or protection against a same-user attacker controlling the
host.

## Review gate

Before encrypted implementation work is opened, a maintainer must approve
[ADR-0003](adr/0003-encrypted-bundles-and-recipient-keys.md), including its
schema-version boundary, LUKS profile, delegated recipient authority and
wrapping suites, login-Keychain behavior, ZIP extent rules, instance/state
policy, QMP transport, and threat boundary. After approval, the implementation
tasks will update the current app and bundle contracts without weakening
schema-1 verification.
