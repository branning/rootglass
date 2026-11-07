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

## Trust policy

MVP policy:

- A `.glass` bundle must be signed.
- The signing key must be trusted by Rootglass/Riparium.
- Unknown publisher or unknown key is a hard launch failure.
- Signature verification must work offline.

Future policy options:

- User-installed trusted publishers.
- Organization-managed trust bundle.
- JFrog/AppTrust-backed publisher trust sync.
- Key rotation and revocation metadata.
- Transparency-log verification as an online enhancement.

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
