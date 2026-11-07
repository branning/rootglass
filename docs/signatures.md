# Glass Signature Format

## Decision

Rootglass `.glass` bundles will use a **DSSE-wrapped in-toto-style statement signed with Ed25519**.

Runtime verification must be implemented inside Rootglass/Riparium. End users should not need to install JFrog CLI, cosign, minisign, GPG, OpenSSL, or platform-specific verification tools to open a `.glass` bundle.

## Why this path

This choice is a middle path between the earlier options:

- Simpler than full Sigstore/keyless verification at runtime.
- More supply-chain/provenance friendly than raw minisign-style detached signatures.
- Compatible with JFrog-centric downstream signing/provenance workflows.
- Works offline on macOS, Windows 11, and Ubuntu 24.04+ because verification is just local hashing plus public-key signature verification.
- Allows future bridge to JFrog Evidence/AppTrust, Sigstore/Rekor, or release-bundle policies without forcing those systems into the desktop runtime.

## Non-goals

- Rootglass will not depend on network access to verify a bundle.
- Rootglass will not require a local JFrog login to open a bundle.
- Rootglass will not use OS-native installer signing as the bundle trust mechanism.
- Rootglass will not use raw unsigned checksums as a substitute for publisher signatures.

## Bundle layout

Development directory form:

```text
example.glass/
  rootglass.toml
  rootfs.img
  state.img
  metadata/
    icon.png
  signatures/
    glass.dsse.json
```

Serialized archive form:

```text
example.glass     # ZIP archive with .glass extension
  rootglass.toml
  rootfs.img
  state.img
  metadata/icon.png
  signatures/glass.dsse.json
```

The DSSE signature file is excluded from the signed file set. In production archive verification, all other files in the ZIP archive must be listed as signed subjects. Directory-form fixture verification may temporarily exclude generated `.img` files so the repository can test manifests and metadata without committing large images.

## Archive path rules

Serialized `.glass` files are ZIP archives with strict path handling:

- entry names must use `/`, not `\\`;
- entry names must be relative;
- absolute paths are invalid;
- `.` and `..` components are invalid;
- duplicate normalized entry names are invalid;
- directory entries are ignored for signature subjects;
- every file except `signatures/glass.dsse.json` must be covered by the signed statement.

## Signed payload

The DSSE payload is an in-toto-style statement.

Draft payload shape:

```json
{
  "_type": "https://in-toto.io/Statement/v1",
  "subject": [
    {
      "name": "rootglass.toml",
      "digest": { "sha256": "..." }
    },
    {
      "name": "rootfs.img",
      "digest": { "sha256": "..." }
    },
    {
      "name": "state.img",
      "digest": { "sha256": "..." }
    }
  ],
  "predicateType": "https://rootglass.dev/predicate/glass/v1",
  "predicate": {
    "schemaVersion": 1,
    "glassIdentifier": "io.rootglass.examples.hello-world",
    "glassVersion": "0.1.0",
    "publisher": {
      "name": "Rootglass Examples",
      "identity": "rootglass.dev/examples",
      "keyId": "ed25519:<fingerprint>"
    },
    "createdAt": "2026-05-17T00:00:00Z"
  }
}
```

## DSSE envelope

Draft envelope shape:

```json
{
  "payloadType": "application/vnd.in-toto+json",
  "payload": "<base64url encoded statement>",
  "signatures": [
    {
      "keyid": "ed25519:<fingerprint>",
      "sig": "<base64url encoded Ed25519 signature>"
    }
  ]
}
```

## Verification flow

1. Open the `.glass` directory or ZIP archive.
2. Read `signatures/glass.dsse.json`.
3. Decode the DSSE envelope.
4. Verify the DSSE signature with a trusted Ed25519 public key.
5. Decode the in-toto-style statement payload.
6. Verify statement type and Rootglass predicate type.
7. Verify publisher identity and key ID against the local Rootglass trust policy.
8. Hash every signed bundle file and compare it against the statement subjects.
9. For production archives, reject any file not covered by the signed statement, except the DSSE envelope itself.
10. Reject missing files, unsafe archive paths, duplicate archive entries, digest mismatches, unsupported signature versions, unknown publishers, or untrusted keys.

## Production trust configuration

The production GPUI app loads a local, per-user trust file at startup. Its location is `$XDG_CONFIG_HOME/rootglass/trust.json` when `XDG_CONFIG_HOME` is absolute, otherwise `$HOME/.config/rootglass/trust.json` on Linux/Unix; `$HOME/Library/Application Support/Rootglass/trust.json` on macOS; and `%APPDATA%\Rootglass\trust.json` on Windows.

Version 1 has this shape:

```json
{
  "schemaVersion": 1,
  "trustedPublishers": [
    {
      "name": "Example Publisher",
      "identity": "publisher.example/project",
      "keyId": "ed25519:<sha256-of-raw-public-key-as-64-lowercase-hex>",
      "publicKeyBase64url": "<32-byte-ed25519-public-key-as-unpadded-base64url>"
    }
  ],
  "revokedKeyIds": []
}
```

The parser rejects malformed or duplicate key IDs, key/fingerprint mismatches, unknown fields, unsupported schema versions, and files larger than 1 MiB. A key ID is `ed25519:` plus SHA-256 of the decoded 32-byte public key. Trust files contain no private material.

The application reads the file once at startup. Missing, invalid, or inaccessible configuration produces a visible trust-configuration failure and an empty `AppService::deny_all()` store; the example fixture key is never merged into this production store. Unknown keys fail as “unknown signing key”; revoked keys have a distinct “signing key has been revoked” message. The CLI and `AppService::default()` remain development/test entry points and must not be used to construct a production GUI service.

| Verification result | User-facing result |
| --- | --- |
| Trusted key and intact bundle | Preview includes the trusted publisher identity and verified status. |
| Unknown key | Verification fails with `unknown signing key`; there is no run override. |
| Revoked key | Verification fails with `This publisher's signing key has been revoked.` |
| Rotated key | The new key is trusted after the updated file is loaded; the old key reports revoked. There is no separate rotated status. |
| Expiry | Version 1 has no time-based expiry or expired-key status; offline clients do not rely on local clocks. |
| Tampered bundle | Verification fails with `This glass could not be verified.` and a digest mismatch detail. |

Provision the file outside Rootglass through an authenticated operator or release process. Replace it atomically and retain a known-good backup. To rotate, add the new key during an intentional overlap, then deploy a file that trusts the new key and lists the old key ID under `revokedKeyIds`. The parser rejects a key that appears in both arrays. Restart Rootglass after an update; the current process does not reload policy, and active guests are not reverified. Unknown and revoked keys both fail closed before preview/preflight/start can proceed. The current application has no trust-editing GUI/CLI and no enterprise-managed trust source.

See [ADR-0001](adr/0001-offline-publisher-trust.md) for the proposed ownership, recovery, and review policy and [trust validation](trust-validation.md) for test evidence. Expiry is not automatic because offline clients cannot rely on a shared clock; there is no expired-key status in version 1. JFrog/AppTrust and transparency evidence remain optional distribution-time inputs, never runtime dependencies.

## JFrog integration

Riparium can use JFrog as the upstream signing/provenance system while still embedding a self-contained DSSE envelope in the `.glass` bundle.

Expected downstream flow:

1. Build the `.glass` ZIP archive or development directory.
2. Generate the Rootglass file-digest statement.
3. Sign the statement using a key controlled by the release pipeline.
4. Embed `signatures/glass.dsse.json` in the `.glass` archive.
5. Store the signed `.glass` in JFrog.
6. Optionally attach JFrog Evidence/AppTrust provenance to the artifact in JFrog.
7. Distribute the `.glass` to users.
8. Rootglass/Riparium verifies the embedded DSSE signature offline at open time.

## Why not raw minisign/signify for MVP

Minisign-style signatures are attractive because they are simple and offline-friendly. The downside is that Rootglass still needs a structured statement for publisher identity, bundle file digests, archive layout, and future provenance. Using DSSE gives us a simple signed envelope while preserving room for richer supply-chain metadata.

## Why not full Sigstore/cosign runtime verification for MVP

Sigstore/cosign remains useful for CI, distribution, and optional provenance. It is not the right minimum runtime requirement because Rootglass must verify bundles offline and should not require users to install external tools or depend on online identity/transparency services before opening a local desktop bundle.

## Implementation notes

Initial Rust implementation should prefer:

- SHA-256 for file digests.
- Ed25519 for signatures.
- A small DSSE verifier implemented in `rootglass-signature` or similar.
- No shelling out to external verification tools.
- Deterministic lexicographic file-path ordering when generating statements.
- Clear user-facing errors for unsafe archive paths, unsigned files, tampering, unknown publisher, and unsupported signature cases.


## CLI signing and packaging

The CLI accepts a private Ed25519 seed from a file. The file may contain either the raw 32-byte
seed or its unpadded base64url encoding. Treat this file as a secret and keep it outside the
repository. Rootglass does not embed or generate a release key.

```sh
rootglass sign ./hello-world.glass --key /secure/path/publisher.seed
rootglass package ./hello-world.glass ./hello-world.glass.zip --key /secure/path/publisher.seed
```

Both commands use the publisher name, identity, and signature path in `rootglass.toml`. Signing
updates the DSSE envelope in the directory. Packaging writes a ZIP archive with stable path order,
fixed ZIP timestamps and permissions, signs every regular file except the envelope (including
generated image assets), and verifies the completed archive with Rootglass's archive verifier
before publishing it. The output path must not already exist.

The default `createdAt` value is the current UTC time. For repeatable output, supply an explicit
RFC 3339 timestamp:

```sh
rootglass package ./hello-world.glass ./hello-world.glass.zip \
  --key /secure/path/publisher.seed --created-at 2026-11-06T00:00:00Z
```

With identical source files, key, and timestamp, signing and packaging produce deterministic
statements and signatures. Directory-form development verification may have fixture-specific
generated-image exclusions; the signing and production archive paths include all regular files and
do not apply those exclusions.

The archive verifier currently reads the full archive and file contents into memory. The CLI
therefore provides integrity verification, but it does not yet satisfy the bounded-memory and ZIP
resource-limit requirements for untrusted large archives described in the application spec.
