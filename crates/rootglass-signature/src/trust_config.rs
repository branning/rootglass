//! Strict, local trust-store configuration for production publisher keys.
//!
//! The compiled-in example fixture remains available through [`TrustStore::example_fixture`]
//! and is deliberately not merged into a loaded configuration. Trust files are offline policy:
//! a key must be explicitly listed, and an ID in `revokedKeyIds` is rejected even if a signature
//! names it. Rotation is performed by adding the new key and revoking the old key in one config
//! update; revocation takes effect on the next load and does not require network access.
//!
//! Version 1 JSON shape:
//!
//! ```json
//! {
//!   "schemaVersion": 1,
//!   "trustedPublishers": [{
//!     "name": "Publisher name",
//!     "identity": "publisher.example/project",
//!     "keyId": "ed25519:<64 lowercase hex characters>",
//!     "publicKeyBase64url": "<unpadded base64url Ed25519 public key>"
//!   }],
//!   "revokedKeyIds": []
//! }
//! ```

use crate::{TrustStore, TrustedPublisher};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use thiserror::Error;

const SUPPORTED_SCHEMA_VERSION: u32 = 1;
const MAX_TRUST_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum TrustConfigError {
    #[error("failed to read trust configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("trust configuration at {path} exceeds the {limit}-byte limit")]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("invalid trust configuration JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported trust configuration schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("trusted publisher at index {index} has an empty {field}")]
    EmptyField { index: usize, field: &'static str },
    #[error(
        "invalid key ID '{0}': expected ed25519 followed by a colon and 64 lowercase hexadecimal characters"
    )]
    InvalidKeyId(String),
    #[error(
        "trusted publisher key ID '{key_id}' does not match its public key (expected '{expected}')"
    )]
    KeyIdMismatch { key_id: String, expected: String },
    #[error("trusted publisher '{key_id}' has invalid base64url public key: {reason}")]
    InvalidPublicKeyEncoding { key_id: String, reason: String },
    #[error("trusted publisher '{key_id}' public key must decode to exactly 32 bytes")]
    InvalidPublicKeyLength { key_id: String },
    #[error("trusted publisher '{key_id}' has an invalid Ed25519 public key")]
    InvalidEd25519PublicKey { key_id: String },
    #[error("duplicate trusted publisher key ID '{0}'")]
    DuplicateTrustedKeyId(String),
    #[error("duplicate revoked key ID '{0}'")]
    DuplicateRevokedKeyId(String),
    #[error("key ID '{0}' cannot be both trusted and revoked")]
    TrustedAndRevoked(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TrustLookupError {
    #[error("publisher key '{0}' has been revoked")]
    RevokedKey(String),
    #[error("publisher key '{0}' is not trusted")]
    UnknownKey(String),
}

/// A validated, versioned trust configuration, separate from the example fixture trust.
#[derive(Debug, Clone)]
pub struct LoadedTrustStore {
    trust_store: TrustStore,
    publishers: BTreeMap<String, TrustedPublisher>,
    revoked_key_ids: BTreeSet<String>,
}

impl LoadedTrustStore {
    /// The verifier-compatible trust store. Revoked keys are absent from this store, so the
    /// existing signature verifier fails closed for them as unknown keys.
    pub fn trust_store(&self) -> &TrustStore {
        &self.trust_store
    }

    /// Resolve a signing key ID for app-facing policy checks. Unknown and revoked IDs are errors.
    pub fn lookup(&self, key_id: &str) -> Result<&TrustedPublisher, TrustLookupError> {
        if self.revoked_key_ids.contains(key_id) {
            return Err(TrustLookupError::RevokedKey(key_id.to_owned()));
        }
        self.publishers
            .get(key_id)
            .ok_or_else(|| TrustLookupError::UnknownKey(key_id.to_owned()))
    }

    /// Returns whether the configuration explicitly revokes this key ID.
    pub fn is_revoked(&self, key_id: &str) -> bool {
        self.revoked_key_ids.contains(key_id)
    }

    /// The explicitly revoked key IDs, in deterministic order.
    pub fn revoked_key_ids(&self) -> impl Iterator<Item = &str> {
        self.revoked_key_ids.iter().map(String::as_str)
    }

    /// The trusted publishers from the loaded configuration, in deterministic key-ID order.
    pub fn trusted_publishers(&self) -> impl Iterator<Item = &TrustedPublisher> {
        self.publishers.values()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawTrustConfig {
    schema_version: u32,
    trusted_publishers: Vec<RawTrustedPublisher>,
    revoked_key_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawTrustedPublisher {
    name: String,
    identity: String,
    key_id: String,
    public_key_base64url: String,
}

/// Parse a version 1 JSON trust configuration from bytes.
pub fn parse_trust_store(bytes: &[u8]) -> Result<LoadedTrustStore, TrustConfigError> {
    if bytes.len() as u64 > MAX_TRUST_CONFIG_BYTES {
        return Err(TrustConfigError::TooLarge {
            path: PathBuf::from("<memory>"),
            limit: MAX_TRUST_CONFIG_BYTES,
        });
    }
    let raw: RawTrustConfig = serde_json::from_slice(bytes)?;
    if raw.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(TrustConfigError::UnsupportedSchemaVersion(
            raw.schema_version,
        ));
    }

    let mut publishers = BTreeMap::new();
    for (index, raw_publisher) in raw.trusted_publishers.into_iter().enumerate() {
        let name = nonempty(index, "name", raw_publisher.name)?;
        let identity = nonempty(index, "identity", raw_publisher.identity)?;
        let key_id = nonempty(index, "keyId", raw_publisher.key_id)?;
        validate_key_id(&key_id)?;
        let public_key_base64url = nonempty(
            index,
            "publicKeyBase64url",
            raw_publisher.public_key_base64url,
        )?;
        let key_bytes = URL_SAFE_NO_PAD
            .decode(public_key_base64url.as_bytes())
            .map_err(|error| TrustConfigError::InvalidPublicKeyEncoding {
                key_id: key_id.clone(),
                reason: error.to_string(),
            })?;
        let key_bytes: [u8; 32] =
            key_bytes
                .try_into()
                .map_err(|_| TrustConfigError::InvalidPublicKeyLength {
                    key_id: key_id.clone(),
                })?;
        VerifyingKey::from_bytes(&key_bytes).map_err(|_| {
            TrustConfigError::InvalidEd25519PublicKey {
                key_id: key_id.clone(),
            }
        })?;
        let expected_key_id = format!("ed25519:{}", hex_sha256(&key_bytes));
        if key_id != expected_key_id {
            return Err(TrustConfigError::KeyIdMismatch {
                key_id,
                expected: expected_key_id,
            });
        }
        let publisher = TrustedPublisher {
            name,
            identity,
            key_id: key_id.clone(),
            public_key_base64url,
        };
        if publishers.insert(key_id.clone(), publisher).is_some() {
            return Err(TrustConfigError::DuplicateTrustedKeyId(key_id));
        }
    }

    let mut revoked_key_ids = BTreeSet::new();
    for key_id in raw.revoked_key_ids {
        validate_key_id(&key_id)?;
        if !revoked_key_ids.insert(key_id.clone()) {
            return Err(TrustConfigError::DuplicateRevokedKeyId(key_id));
        }
        if publishers.contains_key(&key_id) {
            return Err(TrustConfigError::TrustedAndRevoked(key_id));
        }
    }

    let trust_store = TrustStore::new(publishers.values().cloned());
    Ok(LoadedTrustStore {
        trust_store,
        publishers,
        revoked_key_ids,
    })
}

/// Load a local JSON trust configuration, rejecting files larger than 1 MiB.
pub fn load_trust_store(path: impl AsRef<Path>) -> Result<LoadedTrustStore, TrustConfigError> {
    let path = path.as_ref();
    let mut file = File::open(path).map_err(|source| TrustConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_TRUST_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| TrustConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_TRUST_CONFIG_BYTES {
        return Err(TrustConfigError::TooLarge {
            path: path.to_path_buf(),
            limit: MAX_TRUST_CONFIG_BYTES,
        });
    }
    parse_trust_store(&bytes)
}

fn nonempty(index: usize, field: &'static str, value: String) -> Result<String, TrustConfigError> {
    if value.trim().is_empty() {
        Err(TrustConfigError::EmptyField { index, field })
    } else {
        Ok(value)
    }
}

fn validate_key_id(key_id: &str) -> Result<(), TrustConfigError> {
    let Some(fingerprint) = key_id.strip_prefix("ed25519:") else {
        return Err(TrustConfigError::InvalidKeyId(key_id.to_owned()));
    };
    if fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(TrustConfigError::InvalidKeyId(key_id.to_owned()));
    }
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixture_public_key() -> (String, String) {
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let public_key = signing_key.verifying_key().to_bytes();
        let key_id = format!("ed25519:{}", hex_sha256(&public_key));
        let encoded_key = URL_SAFE_NO_PAD.encode(public_key);
        (key_id, encoded_key)
    }

    fn config(publishers: &str, revoked: &str) -> String {
        format!(
            r#"{{"schemaVersion":1,"trustedPublishers":{publishers},"revokedKeyIds":{revoked}}}"#
        )
    }

    fn publisher(key_id: &str, public_key: &str) -> String {
        format!(
            r#"{{
                "name":"Example Publisher",
                "identity":"example.test/project",
                "keyId":"{key_id}",
                "publicKeyBase64url":"{public_key}"
            }}"#
        )
    }

    #[test]
    fn parses_valid_publisher_and_resolves_it() {
        let (key_id, public_key) = fixture_public_key();
        let bytes = config(&format!("[{}]", publisher(&key_id, &public_key)), "[]");
        let loaded = parse_trust_store(bytes.as_bytes()).expect("valid trust config");
        let trusted = loaded.lookup(&key_id).expect("explicitly trusted key");
        assert_eq!(trusted.name, "Example Publisher");
        assert_eq!(trusted.identity, "example.test/project");
        assert_eq!(trusted.key_id, key_id);
        assert!(loaded.trust_store().get(&key_id).is_some());
    }

    #[test]
    fn rejects_unknown_fields_and_schema_versions() {
        let unknown_field =
            br#"{"schemaVersion":1,"trustedPublishers":[],"revokedKeyIds":[],"extra":true}"#;
        assert!(matches!(
            parse_trust_store(unknown_field),
            Err(TrustConfigError::Parse(_))
        ));

        let unsupported = br#"{"schemaVersion":2,"trustedPublishers":[],"revokedKeyIds":[]}"#;
        assert!(matches!(
            parse_trust_store(unsupported),
            Err(TrustConfigError::UnsupportedSchemaVersion(2))
        ));
    }

    #[test]
    fn rejects_key_id_mismatch() {
        let (actual_key_id, public_key) = fixture_public_key();
        let wrong_key_id = format!("ed25519:{}", "0".repeat(64));
        assert_ne!(wrong_key_id, actual_key_id);
        let bytes = config(
            &format!("[{}]", publisher(&wrong_key_id, &public_key)),
            "[]",
        );
        assert!(matches!(
            parse_trust_store(bytes.as_bytes()),
            Err(TrustConfigError::KeyIdMismatch { .. })
        ));
    }

    #[test]
    fn lookup_fails_closed_for_revoked_and_unknown_keys() {
        let (trusted_key_id, public_key) = fixture_public_key();
        // Use a distinct valid identifier to represent a previously trusted key whose public
        // material is no longer in this trust configuration.
        let revoked_key_id = format!("ed25519:{}", "a".repeat(64));
        let bytes = config(
            &format!("[{}]", publisher(&trusted_key_id, &public_key)),
            &format!("[\"{revoked_key_id}\"]"),
        );
        let loaded = parse_trust_store(bytes.as_bytes()).expect("valid revocation list");
        assert!(matches!(
            loaded.lookup(&revoked_key_id),
            Err(TrustLookupError::RevokedKey(_))
        ));
        assert!(matches!(
            loaded.lookup("ed25519:unknown"),
            Err(TrustLookupError::UnknownKey(_))
        ));
        assert!(loaded.is_revoked(&revoked_key_id));
        assert!(loaded.trust_store().get(&revoked_key_id).is_none());
    }

    #[test]
    fn rejects_oversized_configuration_before_parsing() {
        let oversized = vec![b' '; MAX_TRUST_CONFIG_BYTES as usize + 1];
        assert!(matches!(
            parse_trust_store(&oversized),
            Err(TrustConfigError::TooLarge { .. })
        ));
    }
}
