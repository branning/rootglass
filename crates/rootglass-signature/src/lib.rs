use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const DSSE_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
pub const PREDICATE_TYPE: &str = "https://rootglass.dev/predicate/glass/v1";

#[derive(Debug, Error)]
pub enum SignatureError {
    #[error("failed to read signature envelope at {path}: {source}")]
    ReadEnvelope {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse signature envelope: {0}")]
    ParseEnvelope(#[from] serde_json::Error),

    #[error("unsupported DSSE payload type '{0}'")]
    UnsupportedPayloadType(String),

    #[error("DSSE envelope must contain exactly one signature")]
    InvalidSignatureCount,

    #[error("unknown signing key '{0}'")]
    UnknownKey(String),

    #[error("invalid trusted public key for '{key_id}': {reason}")]
    InvalidTrustedKey { key_id: String, reason: String },

    #[error("invalid signature encoding: {0}")]
    InvalidSignatureEncoding(String),

    #[error("invalid payload encoding: {0}")]
    InvalidPayloadEncoding(String),

    #[error("signature verification failed")]
    SignatureVerificationFailed,

    #[error("unsupported statement type '{0}'")]
    UnsupportedStatementType(String),

    #[error("unsupported predicate type '{0}'")]
    UnsupportedPredicateType(String),

    #[error("publisher identity mismatch: expected '{expected}', got '{actual}'")]
    PublisherIdentityMismatch { expected: String, actual: String },

    #[error("subject digest mismatch for {path}: expected {expected}, got {actual}")]
    DigestMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("signed subject is missing from glass: {0}")]
    MissingSubject(String),

    #[error("unsigned file found in glass: {0}")]
    UnsignedFile(String),

    #[error("failed to read signed file {path}: {source}")]
    ReadSignedFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DsseEnvelope {
    pub payload_type: String,
    pub payload: String,
    pub signatures: Vec<DsseSignature>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DsseSignature {
    pub keyid: String,
    pub sig: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Statement {
    #[serde(rename = "_type")]
    pub statement_type: String,
    pub subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    pub predicate: Predicate,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Subject {
    pub name: String,
    pub digest: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Predicate {
    pub schema_version: u32,
    pub glass_identifier: String,
    pub glass_version: String,
    pub publisher: Publisher,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Publisher {
    pub name: String,
    pub identity: String,
    pub key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPublisher {
    pub name: String,
    pub identity: String,
    pub key_id: String,
    pub public_key_base64url: String,
}

#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    publishers: BTreeMap<String, TrustedPublisher>,
}

impl TrustStore {
    pub fn new(publishers: impl IntoIterator<Item = TrustedPublisher>) -> Self {
        Self {
            publishers: publishers
                .into_iter()
                .map(|publisher| (publisher.key_id.clone(), publisher))
                .collect(),
        }
    }

    pub fn example_fixture() -> Self {
        Self::new([TrustedPublisher {
            name: "Rootglass Examples".to_string(),
            identity: "rootglass.dev/examples".to_string(),
            key_id: "ed25519:rootglass-example-fixture".to_string(),
            public_key_base64url: "FrtQPQofbex46km54joj4L1k_7Bp_iDDewj7h1cRz4s".to_string(),
        }])
    }

    fn get(&self, key_id: &str) -> Option<&TrustedPublisher> {
        self.publishers.get(key_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
    pub subject_count: usize,
}

pub fn verify_glass_dir(
    glass_root: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
) -> Result<VerificationReport, SignatureError> {
    let glass_root = glass_root.as_ref();
    let signature_path = signature_path.as_ref();
    let envelope_path = glass_root.join(signature_path);
    let envelope_bytes =
        fs::read(&envelope_path).map_err(|source| SignatureError::ReadEnvelope {
            path: envelope_path,
            source,
        })?;
    let envelope: DsseEnvelope = serde_json::from_slice(&envelope_bytes)?;
    verify_envelope(glass_root, signature_path, &envelope, trust_store)
}

pub fn verify_envelope(
    glass_root: &Path,
    signature_path: &Path,
    envelope: &DsseEnvelope,
    trust_store: &TrustStore,
) -> Result<VerificationReport, SignatureError> {
    if envelope.payload_type != DSSE_PAYLOAD_TYPE {
        return Err(SignatureError::UnsupportedPayloadType(
            envelope.payload_type.clone(),
        ));
    }

    if envelope.signatures.len() != 1 {
        return Err(SignatureError::InvalidSignatureCount);
    }

    let dsse_signature = &envelope.signatures[0];
    let trusted = trust_store
        .get(&dsse_signature.keyid)
        .ok_or_else(|| SignatureError::UnknownKey(dsse_signature.keyid.clone()))?;

    let payload = URL_SAFE_NO_PAD
        .decode(envelope.payload.as_bytes())
        .map_err(|error| SignatureError::InvalidPayloadEncoding(error.to_string()))?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(dsse_signature.sig.as_bytes())
        .map_err(|error| SignatureError::InvalidSignatureEncoding(error.to_string()))?;
    let public_key_bytes = URL_SAFE_NO_PAD
        .decode(trusted.public_key_base64url.as_bytes())
        .map_err(|error| SignatureError::InvalidTrustedKey {
            key_id: trusted.key_id.clone(),
            reason: error.to_string(),
        })?;

    let verifying_key =
        VerifyingKey::from_bytes(public_key_bytes.as_slice().try_into().map_err(|_| {
            SignatureError::InvalidTrustedKey {
                key_id: trusted.key_id.clone(),
                reason: "public key must be 32 bytes".to_string(),
            }
        })?)
        .map_err(|error| SignatureError::InvalidTrustedKey {
            key_id: trusted.key_id.clone(),
            reason: error.to_string(),
        })?;

    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|error| SignatureError::InvalidSignatureEncoding(error.to_string()))?;
    let pae = dsse_pae(envelope.payload_type.as_bytes(), &payload);
    verifying_key
        .verify(&pae, &signature)
        .map_err(|_| SignatureError::SignatureVerificationFailed)?;

    let statement: Statement = serde_json::from_slice(&payload)?;
    verify_statement(glass_root, signature_path, &statement, trusted)?;

    Ok(VerificationReport {
        publisher_name: statement.predicate.publisher.name,
        publisher_identity: statement.predicate.publisher.identity,
        key_id: dsse_signature.keyid.clone(),
        subject_count: statement.subject.len(),
    })
}

fn verify_statement(
    glass_root: &Path,
    signature_path: &Path,
    statement: &Statement,
    trusted: &TrustedPublisher,
) -> Result<(), SignatureError> {
    if statement.statement_type != STATEMENT_TYPE {
        return Err(SignatureError::UnsupportedStatementType(
            statement.statement_type.clone(),
        ));
    }

    if statement.predicate_type != PREDICATE_TYPE {
        return Err(SignatureError::UnsupportedPredicateType(
            statement.predicate_type.clone(),
        ));
    }

    if statement.predicate.publisher.identity != trusted.identity {
        return Err(SignatureError::PublisherIdentityMismatch {
            expected: trusted.identity.clone(),
            actual: statement.predicate.publisher.identity.clone(),
        });
    }

    let expected_subjects = statement
        .subject
        .iter()
        .map(|subject| subject.name.clone())
        .collect::<BTreeSet<_>>();

    for subject in &statement.subject {
        let expected = subject
            .digest
            .get("sha256")
            .expect("sha256 digest is required by schema");
        let path = glass_root.join(&subject.name);
        if !path.is_file() {
            return Err(SignatureError::MissingSubject(subject.name.clone()));
        }
        let bytes =
            fs::read(&path).map_err(|source| SignatureError::ReadSignedFile { path, source })?;
        let actual = hex_sha256(&bytes);
        if actual != *expected {
            return Err(SignatureError::DigestMismatch {
                path: subject.name.clone(),
                expected: expected.clone(),
                actual,
            });
        }
    }

    for file in list_signed_candidate_files(glass_root, signature_path)? {
        if !expected_subjects.contains(&file) {
            return Err(SignatureError::UnsignedFile(file));
        }
    }

    Ok(())
}

fn list_signed_candidate_files(
    root: &Path,
    signature_path: &Path,
) -> Result<Vec<String>, SignatureError> {
    let mut files = Vec::new();
    collect_files(root, root, signature_path, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files(
    root: &Path,
    current: &Path,
    signature_path: &Path,
    files: &mut Vec<String>,
) -> Result<(), SignatureError> {
    for entry in fs::read_dir(current).map_err(|source| SignatureError::ReadSignedFile {
        path: current.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| SignatureError::ReadSignedFile {
            path: current.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, signature_path, files)?;
        } else if path.is_file() {
            let relative = path.strip_prefix(root).expect("path is inside root");
            if relative == signature_path {
                continue;
            }
            if path.extension().is_some_and(|extension| extension == "img") {
                continue;
            }
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn dsse_pae(payload_type: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"DSSEv1 ");
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type);
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_sample_glass_fixture() {
        let report = verify_glass_dir(
            "../../examples/hello-world.glass",
            "signatures/glass.dsse.json",
            &TrustStore::example_fixture(),
        )
        .expect("sample fixture verifies");

        assert_eq!(report.publisher_identity, "rootglass.dev/examples");
        assert_eq!(report.subject_count, 3);
    }

    #[test]
    fn rejects_unknown_publisher_key() {
        let error = verify_glass_dir(
            "../../examples/hello-world.glass",
            "signatures/glass.dsse.json",
            &TrustStore::default(),
        )
        .expect_err("empty trust store should reject sample fixture");

        assert!(matches!(error, SignatureError::UnknownKey(_)));
    }
}
