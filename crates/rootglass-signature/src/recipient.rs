use crate::{
    dsse_pae, hex_sha256, DsseEnvelope, DsseSignature, SignatureError, VerificationLimits,
    VerificationReport, VerifiedArchive, MAX_SIGNATURE_ENVELOPE_BYTES,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};
use rootglass_manifest::{ArtifactCatalog, EncryptionPolicy, MULTI_ARTIFACT_SCHEMA_VERSION};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub const RECIPIENT_ENVELOPE_PATH: &str = "signatures/recipients.dsse.json";
pub const RECIPIENT_ENVELOPE_PAYLOAD_TYPE: &str =
    "application/vnd.rootglass.recipients+json;version=1";
pub const MAX_RECIPIENTS: usize = 1024;
const MAX_WRAPPED_IMK_BYTES: usize = 16 * 1024;
const ENCRYPTED_ARTIFACT_SET_DOMAIN: &[u8] = b"rootglass-encrypted-artifact-set-v1\0";
static TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecipientProvider {
    #[serde(rename = "age-x25519")]
    AgeX25519,
    #[serde(rename = "apple-secure-enclave-p256")]
    AppleSecureEnclaveP256,
    #[serde(rename = "windows-tpm-rsa3072")]
    WindowsTpmRsa3072,
    #[serde(rename = "linux-tpm-rsa3072")]
    LinuxTpmRsa3072,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecipientWrappingSuite {
    #[serde(rename = "age-v1-x25519")]
    AgeV1X25519,
    #[serde(rename = "hpke-v1-p256-hkdf-sha256-aes128gcm")]
    HpkeV1P256HkdfSha256Aes128Gcm,
    #[serde(rename = "rsa-oaep-v1-sha256")]
    RsaOaepV1Sha256,
}

impl RecipientWrappingSuite {
    pub const fn id(self) -> &'static str {
        match self {
            Self::AgeV1X25519 => "age-v1-x25519",
            Self::HpkeV1P256HkdfSha256Aes128Gcm => "hpke-v1-p256-hkdf-sha256-aes128gcm",
            Self::RsaOaepV1Sha256 => "rsa-oaep-v1-sha256",
        }
    }
}

/// One recipient's wrapped image master key. `wrapped_imk` is base64url encoded
/// in the sidecar; its contents are opaque to the signature layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipientRecord {
    pub recipient_id: String,
    pub provider_id: RecipientProvider,
    pub wrapping_suite_id: RecipientWrappingSuite,
    #[serde(with = "base64url_bytes")]
    pub wrapped_imk: Vec<u8>,
}

/// Canonical payload authorized by one recipient authority from the signed manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipientEnvelopePayload {
    pub format_version: u32,
    pub bundle_identifier: String,
    pub bundle_version: String,
    pub manifest_sha256: String,
    pub encrypted_artifact_set_sha256: String,
    pub artifact_encryption_suites: Vec<String>,
    pub wrapping_suites: Vec<RecipientWrappingSuite>,
    pub recipients: Vec<RecipientRecord>,
}

/// A recipient envelope whose canonical payload, binding, and delegated signature verified.
pub struct VerifiedRecipientEnvelope {
    payload: RecipientEnvelopePayload,
    authority_key_id: String,
}

impl VerifiedRecipientEnvelope {
    pub fn payload(&self) -> &RecipientEnvelopePayload {
        &self.payload
    }

    pub fn authority_key_id(&self) -> &str {
        &self.authority_key_id
    }

    pub fn recipient(&self, recipient_id: &str) -> Option<&RecipientRecord> {
        self.payload
            .recipients
            .iter()
            .find(|recipient| recipient.recipient_id == recipient_id)
    }
}

/// Compute ADR-0004's domain-separated digest for the complete encrypted asset set.
pub fn encrypted_artifact_set_digest(catalog: &ArtifactCatalog) -> Result<String, SignatureError> {
    let members = catalog
        .encrypted_artifact_set()
        .into_iter()
        .map(|member| {
            serde_json::json!({
                "id": member.id,
                "ciphertext_sha256": member.ciphertext_sha256,
                "ciphertext_size": member.ciphertext_size,
                "suite_id": member.suite_id,
            })
        })
        .collect::<Vec<_>>();
    let canonical = canonical_json(&Value::Array(members))?;
    let mut digest = Sha256::new();
    digest.update(ENCRYPTED_ARTIFACT_SET_DOMAIN);
    digest.update(canonical);
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Build a delegated DSSE sidecar without touching any encrypted asset or publisher signature.
pub fn sign_recipient_envelope(
    report: &VerificationReport,
    mut recipients: Vec<RecipientRecord>,
    authority_key: &ed25519_dalek::SigningKey,
    limits: &VerificationLimits,
) -> Result<Vec<u8>, SignatureError> {
    limits.validate()?;
    if recipients.len() > MAX_RECIPIENTS {
        return Err(invalid_envelope("recipient count exceeds 1,024"));
    }
    for recipient in &recipients {
        validate_recipient_record(recipient)?;
    }
    let policy = encrypted_policy(report)?;
    let authority_public_key = authority_key.verifying_key().to_bytes();
    let authority_key_id = format!("ed25519:{}", hex_sha256(&authority_public_key));
    if !policy
        .recipient_authorities
        .iter()
        .any(|authority| authority.key_id == authority_key_id)
    {
        return Err(invalid_envelope(
            "signing key is not a recipient authority in the publisher-signed manifest",
        ));
    }

    recipients.sort_by(|left, right| {
        left.recipient_id
            .as_bytes()
            .cmp(right.recipient_id.as_bytes())
    });
    let artifact_catalog = report
        .verified_context
        .artifact_catalog
        .as_ref()
        .expect("encrypted policy requires a catalog");
    let artifact_encryption_suites = artifact_suites(artifact_catalog);
    let wrapping_suites = recipient_suites(&recipients);
    let payload = RecipientEnvelopePayload {
        format_version: 1,
        bundle_identifier: report.verified_context.glass_identifier.clone(),
        bundle_version: report.verified_context.glass_version.clone(),
        manifest_sha256: report
            .verified_context
            .manifest_sha256
            .clone()
            .ok_or_else(|| invalid_envelope("publisher statement does not sign rootglass.toml"))?,
        encrypted_artifact_set_sha256: encrypted_artifact_set_digest(artifact_catalog)?,
        artifact_encryption_suites,
        wrapping_suites,
        recipients,
    };
    validate_payload(report, &payload, limits)?;
    let payload_bytes = canonical_json(&payload)?;
    let signature = authority_key.sign(&dsse_pae(
        RECIPIENT_ENVELOPE_PAYLOAD_TYPE.as_bytes(),
        &payload_bytes,
    ));
    let envelope = DsseEnvelope {
        payload_type: RECIPIENT_ENVELOPE_PAYLOAD_TYPE.to_string(),
        payload: URL_SAFE_NO_PAD.encode(payload_bytes),
        signatures: vec![DsseSignature {
            keyid: authority_key_id,
            sig: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        }],
    };
    let encoded = serde_json::to_vec(&envelope)?;
    if encoded.len() as u64 > recipient_envelope_limit(limits) {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: recipient_envelope_limit(limits),
        });
    }
    verify_recipient_envelope(&envelope, report, limits)?;
    Ok(encoded)
}

/// Verify a recipient sidecar against the verified publisher statement and its delegated key.
pub fn verify_recipient_envelope(
    envelope: &DsseEnvelope,
    report: &VerificationReport,
    limits: &VerificationLimits,
) -> Result<VerifiedRecipientEnvelope, SignatureError> {
    limits.validate()?;
    if envelope.payload_type != RECIPIENT_ENVELOPE_PAYLOAD_TYPE {
        return Err(SignatureError::UnsupportedPayloadType(
            envelope.payload_type.clone(),
        ));
    }
    if envelope.signatures.len() != 1 {
        return Err(SignatureError::InvalidSignatureCount);
    }
    let envelope_limit = recipient_envelope_limit(limits);
    let dsse_signature = &envelope.signatures[0];
    if dsse_signature.keyid.len() > 256 || dsse_signature.sig.len() > 128 {
        return Err(invalid_envelope(
            "signature metadata exceeds its size limit",
        ));
    }
    let encoded_payload_limit = envelope_limit.saturating_mul(4).saturating_add(2) / 3;
    if envelope.payload.len() as u64 > encoded_payload_limit {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: envelope_limit,
        });
    }
    if serde_json::to_vec(envelope)?.len() as u64 > envelope_limit {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: envelope_limit,
        });
    }
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(envelope.payload.as_bytes())
        .map_err(|error| SignatureError::InvalidPayloadEncoding(error.to_string()))?;
    if URL_SAFE_NO_PAD.encode(&payload_bytes) != envelope.payload {
        return Err(SignatureError::InvalidPayloadEncoding(
            "payload is not canonical unpadded base64url".to_string(),
        ));
    }
    if payload_bytes.len() as u64 > envelope_limit {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: envelope_limit,
        });
    }
    let policy = encrypted_policy(report)?;
    let authority = policy
        .recipient_authorities
        .iter()
        .find(|authority| authority.key_id == dsse_signature.keyid)
        .ok_or_else(|| invalid_envelope("signature key is not a delegated recipient authority"))?;
    let public_key_bytes = URL_SAFE_NO_PAD
        .decode(authority.public_key_base64url.as_bytes())
        .map_err(|error| invalid_envelope(&format!("invalid delegated key encoding: {error}")))?;
    if URL_SAFE_NO_PAD.encode(&public_key_bytes) != authority.public_key_base64url
        || public_key_bytes.len() != 32
        || format!("ed25519:{}", hex_sha256(&public_key_bytes)) != authority.key_id
    {
        return Err(invalid_envelope(
            "delegated key ID does not match its public key",
        ));
    }
    let public_key = VerifyingKey::from_bytes(
        public_key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| invalid_envelope("delegated Ed25519 public key must be 32 bytes"))?,
    )
    .map_err(|error| invalid_envelope(&format!("invalid delegated Ed25519 key: {error}")))?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(dsse_signature.sig.as_bytes())
        .map_err(|error| SignatureError::InvalidSignatureEncoding(error.to_string()))?;
    if URL_SAFE_NO_PAD.encode(&signature_bytes) != dsse_signature.sig {
        return Err(SignatureError::InvalidSignatureEncoding(
            "signature is not canonical unpadded base64url".to_string(),
        ));
    }
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|error| SignatureError::InvalidSignatureEncoding(error.to_string()))?;
    public_key
        .verify(
            &dsse_pae(envelope.payload_type.as_bytes(), &payload_bytes),
            &signature,
        )
        .map_err(|_| SignatureError::SignatureVerificationFailed)?;

    let payload: RecipientEnvelopePayload = serde_json::from_slice(&payload_bytes)
        .map_err(|error| invalid_envelope(&format!("invalid recipient payload: {error}")))?;
    if canonical_json(&payload)? != payload_bytes {
        return Err(invalid_envelope(
            "recipient payload is not RFC 8785 canonical JSON",
        ));
    }
    validate_payload(report, &payload, limits)?;
    Ok(VerifiedRecipientEnvelope {
        payload,
        authority_key_id: dsse_signature.keyid.clone(),
    })
}

/// Parse and verify a bounded recipient DSSE sidecar.
pub fn verify_recipient_envelope_bytes(
    bytes: &[u8],
    report: &VerificationReport,
    limits: &VerificationLimits,
) -> Result<VerifiedRecipientEnvelope, SignatureError> {
    limits.validate()?;
    if bytes.len() as u64 > recipient_envelope_limit(limits) {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: recipient_envelope_limit(limits),
        });
    }
    let envelope: DsseEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| invalid_envelope(&format!("invalid recipient DSSE envelope: {error}")))?;
    verify_recipient_envelope(&envelope, report, limits)
}

/// Generate the stable algorithm-prefixed fingerprint used as a recipient ID.
pub fn recipient_id_for_public_key(
    suite: RecipientWrappingSuite,
    canonical_public_key: &[u8],
) -> String {
    format!("{}:{}", suite.id(), hex_sha256(canonical_public_key))
}

/// Personalize a previously verified archive with a new recipient sidecar.
/// Signed subjects are copied unchanged; the function does not hash them again or re-sign them.
pub fn personalize_glass_archive_at(
    verified: &VerifiedArchive,
    output_path: impl AsRef<Path>,
    recipients: Vec<RecipientRecord>,
    authority_key: &ed25519_dalek::SigningKey,
) -> Result<(), SignatureError> {
    let output_path = output_path.as_ref();
    let sidecar = sign_recipient_envelope(
        verified.report(),
        recipients,
        authority_key,
        &verified.limits,
    )?;
    let parent = output_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| SignatureError::WritePackage {
        path: parent.to_path_buf(),
        source,
    })?;
    if fs::symlink_metadata(output_path).is_ok() {
        return Err(SignatureError::PackageOutputExists(
            output_path.to_path_buf(),
        ));
    }
    let temporary = recipient_temp_path(output_path);
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|source| package_write_error(&temporary, source))?;
    let result = (|| {
        let source_guard = verified
            .source
            .lock()
            .map_err(|_| invalid_envelope("verified archive reader lock was poisoned"))?;
        let source = source_guard
            .try_clone()
            .map_err(|source| package_write_error(output_path, source))?;
        let mut archive = ZipArchive::new(source)?;
        let mut writer = ZipWriter::new(output);
        let has_existing_sidecar = archive
            .file_names()
            .any(|name| name == RECIPIENT_ENVELOPE_PATH);
        if !has_existing_sidecar && archive.len() >= verified.limits.max_entries {
            return Err(SignatureError::TooManyArchiveEntries {
                limit: verified.limits.max_entries,
            });
        }
        let mut total_uncompressed = sidecar.len() as u64;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let name = entry.name().to_string();
            if entry.is_dir() || name == RECIPIENT_ENVELOPE_PATH {
                continue;
            }
            total_uncompressed = total_uncompressed
                .checked_add(entry.size())
                .filter(|total| *total <= verified.limits.max_total_uncompressed_bytes)
                .ok_or(SignatureError::ArchiveUncompressedTooLarge {
                    limit: verified.limits.max_total_uncompressed_bytes,
                })?;
            let options = SimpleFileOptions::default().compression_method(entry.compression());
            writer
                .start_file(&name, options)
                .map_err(|error| package_write_error(&temporary, io::Error::other(error)))?;
            io::copy(&mut entry, &mut writer)
                .map_err(|source| package_write_error(&temporary, source))?;
        }
        writer
            .start_file(
                RECIPIENT_ENVELOPE_PATH,
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .map_err(|error| package_write_error(&temporary, io::Error::other(error)))?;
        writer
            .write_all(&sidecar)
            .map_err(|source| package_write_error(&temporary, source))?;
        let output = writer
            .finish()
            .map_err(|error| package_write_error(&temporary, io::Error::other(error)))?;
        let output_size = output
            .metadata()
            .map_err(|source| package_write_error(&temporary, source))?
            .len();
        if output_size > verified.limits.max_archive_bytes {
            return Err(SignatureError::ArchiveTooLarge {
                limit: verified.limits.max_archive_bytes,
            });
        }
        output
            .sync_all()
            .map_err(|source| package_write_error(&temporary, source))?;
        drop(source_guard);
        match fs::hard_link(&temporary, output_path) {
            Ok(()) => {}
            Err(source) => return Err(package_write_error(output_path, source)),
        }
        let _ = fs::remove_file(&temporary);
        if let Ok(parent) = File::open(parent) {
            let _ = parent.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn encrypted_policy(report: &VerificationReport) -> Result<&EncryptionPolicy, SignatureError> {
    if report.verified_context.schema_version != MULTI_ARTIFACT_SCHEMA_VERSION {
        return Err(invalid_envelope(
            "recipient envelopes require a verified schema-2 publisher statement",
        ));
    }
    let catalog = report
        .verified_context
        .artifact_catalog
        .as_ref()
        .ok_or_else(|| invalid_envelope("verified statement has no artifact catalog"))?;
    catalog
        .encryption
        .as_ref()
        .filter(|_| !catalog.encrypted_artifact_set().is_empty())
        .ok_or_else(|| invalid_envelope("verified bundle has no encrypted artifacts"))
}

fn validate_payload(
    report: &VerificationReport,
    payload: &RecipientEnvelopePayload,
    limits: &VerificationLimits,
) -> Result<(), SignatureError> {
    encrypted_policy(report)?;
    let catalog = report
        .verified_context
        .artifact_catalog
        .as_ref()
        .expect("encrypted policy requires a catalog");
    if payload.format_version != 1 {
        return Err(invalid_envelope("unsupported recipient envelope version"));
    }
    let manifest_sha256 = report
        .verified_context
        .manifest_sha256
        .as_deref()
        .ok_or_else(|| invalid_envelope("publisher statement does not sign rootglass.toml"))?;
    if payload.bundle_identifier != report.verified_context.glass_identifier
        || payload.bundle_version != report.verified_context.glass_version
        || payload.manifest_sha256 != manifest_sha256
        || payload.encrypted_artifact_set_sha256 != encrypted_artifact_set_digest(catalog)?
    {
        return Err(invalid_envelope(
            "recipient envelope binding does not match the verified bundle",
        ));
    }
    let expected_artifact_suites = artifact_suites(catalog);
    if payload.artifact_encryption_suites != expected_artifact_suites {
        return Err(invalid_envelope(
            "recipient envelope artifact encryption suites do not match the catalog",
        ));
    }
    if payload.recipients.len() > MAX_RECIPIENTS {
        return Err(invalid_envelope("recipient count exceeds 1,024"));
    }
    let expected_wrapping_suites = recipient_suites(&payload.recipients);
    if payload.wrapping_suites != expected_wrapping_suites {
        return Err(invalid_envelope(
            "recipient wrapping suite list does not match recipient records",
        ));
    }
    let mut previous_id: Option<&str> = None;
    for recipient in &payload.recipients {
        validate_recipient_record(recipient)?;
        if previous_id
            .is_some_and(|previous| previous.as_bytes() >= recipient.recipient_id.as_bytes())
        {
            return Err(invalid_envelope(
                "recipient records must be sorted by unique recipient ID",
            ));
        }
        previous_id = Some(&recipient.recipient_id);
    }
    let canonical_payload = canonical_json(payload)?;
    if canonical_payload.len() as u64 > recipient_envelope_limit(limits) {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: recipient_envelope_limit(limits),
        });
    }
    Ok(())
}

fn recipient_envelope_limit(limits: &VerificationLimits) -> u64 {
    limits
        .max_signature_bytes
        .min(limits.max_member_bytes)
        .min(MAX_SIGNATURE_ENVELOPE_BYTES)
}

fn validate_recipient_record(recipient: &RecipientRecord) -> Result<(), SignatureError> {
    let suite_id = recipient.wrapping_suite_id.id();
    let Some(fingerprint) = recipient.recipient_id.strip_prefix(&format!("{suite_id}:")) else {
        return Err(invalid_envelope(
            "recipient ID algorithm prefix does not match its wrapping suite",
        ));
    };
    if fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_envelope(
            "recipient ID must end in a lowercase SHA-256 fingerprint",
        ));
    }
    if recipient.wrapped_imk.is_empty() || recipient.wrapped_imk.len() > MAX_WRAPPED_IMK_BYTES {
        return Err(invalid_envelope(
            "wrapped IMK length is outside its allowed bound",
        ));
    }
    let compatible = matches!(
        (recipient.provider_id, recipient.wrapping_suite_id),
        (
            RecipientProvider::AgeX25519,
            RecipientWrappingSuite::AgeV1X25519
        ) | (
            RecipientProvider::AppleSecureEnclaveP256,
            RecipientWrappingSuite::HpkeV1P256HkdfSha256Aes128Gcm
        ) | (
            RecipientProvider::WindowsTpmRsa3072,
            RecipientWrappingSuite::RsaOaepV1Sha256
        ) | (
            RecipientProvider::LinuxTpmRsa3072,
            RecipientWrappingSuite::RsaOaepV1Sha256
        )
    );
    if !compatible {
        return Err(invalid_envelope(
            "recipient provider is incompatible with its wrapping suite",
        ));
    }
    Ok(())
}

fn artifact_suites(catalog: &ArtifactCatalog) -> Vec<String> {
    catalog
        .encrypted_artifact_set()
        .into_iter()
        .map(|member| member.suite_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn recipient_suites(recipients: &[RecipientRecord]) -> Vec<RecipientWrappingSuite> {
    let ids = recipients
        .iter()
        .map(|recipient| recipient.wrapping_suite_id.id())
        .collect::<BTreeSet<_>>();
    ids.into_iter()
        .filter_map(|id| {
            recipients
                .iter()
                .find(|recipient| recipient.wrapping_suite_id.id() == id)
                .map(|recipient| recipient.wrapping_suite_id)
        })
        .collect()
}

pub(crate) fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, SignatureError> {
    let value = serde_json::to_value(value)?;
    let mut output = Vec::new();
    write_canonical_value(&value, &mut output)?;
    Ok(output)
}

fn write_canonical_value(value: &Value, output: &mut Vec<u8>) -> Result<(), SignatureError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => {
            if !value.is_i64() && !value.is_u64() {
                return Err(invalid_envelope(
                    "canonical recipient JSON permits only integral numbers",
                ));
            }
            output.extend_from_slice(value.to_string().as_bytes());
        }
        Value::String(value) => output.extend(serde_json::to_vec(value)?),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_value(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            output.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend(serde_json::to_vec(key)?);
                output.push(b':');
                write_canonical_value(&values[key], output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn package_write_error(path: &Path, source: io::Error) -> SignatureError {
    SignatureError::WritePackage {
        path: path.to_path_buf(),
        source,
    }
}

fn invalid_envelope(message: &str) -> SignatureError {
    SignatureError::InvalidRecipientEnvelope(message.to_string())
}

fn recipient_temp_path(output: &Path) -> PathBuf {
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recipient-envelope.glass");
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    output.with_file_name(format!(".{name}.rootglass-{}-{id}.tmp", std::process::id()))
}

mod base64url_bytes {
    use super::*;

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)?;
        if URL_SAFE_NO_PAD.encode(&decoded) != encoded {
            return Err(serde::de::Error::custom(
                "wrapped IMK must use canonical unpadded base64url",
            ));
        }
        Ok(decoded)
    }
}
