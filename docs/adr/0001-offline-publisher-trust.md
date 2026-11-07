# ADR-0001: Offline publisher trust configuration

**Status:** Proposed for maintainer approval  
**Date:** 2026-11-07

## Context

Rootglass verifies `.glass` bundles offline. A valid DSSE signature alone is not sufficient: the publisher key must be explicitly trusted. The compiled example key and the disposable guest login are test fixtures, not production trust roots or credentials. App signing and publisher trust are separate decisions: private signing seeds stay in an external secret file; trust configuration contains public keys only.

## Decision

The production GPUI application loads a versioned local trust file once at startup. Its location is per-user:

| Platform | Path |
| --- | --- |
| Linux and other Unix | `$XDG_CONFIG_HOME/rootglass/trust.json` when `XDG_CONFIG_HOME` is absolute; otherwise `$HOME/.config/rootglass/trust.json` |
| macOS | `$HOME/Library/Application Support/Rootglass/trust.json` |
| Windows | `%APPDATA%\Rootglass\trust.json` |

The file uses schema version 1 with explicit `trustedPublishers` and `revokedKeyIds`. Key IDs are `ed25519:` followed by lowercase SHA-256 hex of the 32-byte Ed25519 public key. Public keys use unpadded base64url. Duplicate entries, mismatched fingerprints, unknown fields, malformed keys, unsupported versions, and files over 1 MiB are rejected.

The production app does not merge this file with the example fixture key. If the path cannot be resolved, the file is missing, or validation fails, startup uses an empty `AppService::deny_all()` store and shows a trust-configuration failure. There is no “run anyway” or built-in fallback. `AppService::default()` remains fixture-oriented for development and tests; production desktop startup uses `AppService::from_trust_config()`.

Provisioning and updates happen outside Rootglass through an authenticated operator or release process. The operator installs the complete validated file atomically and keeps a known-good backup. The current release has no GUI or CLI for editing trust roots and no enterprise-managed trust source; the per-user config is the only production input.

Rotation uses two config updates. First add the new public key while keeping the old key trusted during the planned overlap. Then replace the file with the new key and add the old key ID to `revokedKeyIds`. A key cannot be trusted and revoked in the same file. Revocation is offline and takes effect when a new app service loads the updated file; restart Rootglass after replacement. The current app does not reload trust live, and already-running guests are not retroactively reverified. During an incident, stop affected Rootglass processes before deploying the revocation.

There is no time-based key expiry: an offline verifier cannot safely infer that a key has expired from a local clock. Unknown and revoked keys both fail closed in signature verification; `LoadedTrustStore::lookup` retains the distinct revoked status for policy reporting. Recovery from a bad update means restoring a known-good file or deploying a corrected file through the same trusted operator channel. Recovery never enables the example key.

The example publisher key and `rootglass-ci` guest password are limited to example/test assets. Release packaging must not add the fixture key to production trust or ship the disposable login in a production guest. JFrog/AppTrust and transparency services may provide authenticated distribution evidence, but runtime verification does not depend on them.

## Consequences and review

This policy gives offline users a deterministic, fail-closed trust source and keeps signing secrets out of the app. It also makes trust administration an external operator task and requires app restart for updates. The per-user file is not an enterprise policy channel; a future managed-trust requirement needs a separate design.

Maintainers should approve the trust-root ownership model, rotation window, and restart behavior before production distribution. This ADR remains proposed until that review occurs. Implementation checks and their limits are recorded in [trust validation](../trust-validation.md).
