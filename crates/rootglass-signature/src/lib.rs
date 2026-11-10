use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use thiserror::Error;
use zip::ZipArchive;

mod signing;
pub mod trust_config;
pub use signing::{
    load_signing_key, package_glass_directory, package_glass_directory_at, sign_glass_directory,
    sign_glass_directory_at, SigningReport,
};

pub const DSSE_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
pub const PREDICATE_TYPE: &str = "https://rootglass.dev/predicate/glass/v1";
pub const MAX_SIGNATURE_ENVELOPE_BYTES: u64 = 16 * 1024 * 1024;

/// Resource limits applied while verifying a production archive or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationLimits {
    pub max_archive_bytes: u64,
    pub max_entries: usize,
    pub max_member_bytes: u64,
    pub max_total_uncompressed_bytes: u64,
    pub max_compression_ratio: u64,
    pub max_signature_bytes: u64,
}

impl Default for VerificationLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 256 * 1024 * 1024,
            max_entries: 4096,
            max_member_bytes: 256 * 1024 * 1024,
            max_total_uncompressed_bytes: 512 * 1024 * 1024,
            max_compression_ratio: 200,
            max_signature_bytes: MAX_SIGNATURE_ENVELOPE_BYTES,
        }
    }
}

impl VerificationLimits {
    fn validate(&self) -> Result<(), SignatureError> {
        if self.max_archive_bytes == 0
            || self.max_entries == 0
            || self.max_member_bytes == 0
            || self.max_total_uncompressed_bytes == 0
            || self.max_compression_ratio == 0
            || self.max_signature_bytes == 0
            || self.max_member_bytes > self.max_total_uncompressed_bytes
            || self.max_signature_bytes > self.max_total_uncompressed_bytes
        {
            return Err(SignatureError::InvalidVerificationLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SignatureError {
    #[error("failed to read signature envelope at {path}: {source}")]
    ReadEnvelope {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read archive at {path}: {source}")]
    ReadArchive {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read ZIP archive: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("unsafe archive path: {0}")]
    UnsafeArchivePath(String),

    #[error("duplicate archive entry: {0}")]
    DuplicateArchiveEntry(String),

    #[error("archive exceeds the configured {limit}-byte limit")]
    ArchiveTooLarge { limit: u64 },

    #[error("archive has more than the configured {limit} entries")]
    TooManyArchiveEntries { limit: usize },

    #[error("signed file '{path}' exceeds the configured {limit}-byte limit")]
    SignedFileTooLarge { path: String, limit: u64 },

    #[error("archive exceeds the configured {limit}-byte uncompressed limit")]
    ArchiveUncompressedTooLarge { limit: u64 },

    #[error("archive member '{path}' exceeds the configured {limit}:1 compression ratio")]
    CompressionRatioExceeded { path: String, limit: u64 },

    #[error("archive contains a directory/file collision or unsupported entry type: {0}")]
    UnsupportedArchiveEntry(String),

    #[error("signature envelope exceeds the configured {limit}-byte limit")]
    SignatureEnvelopeTooLarge { limit: u64 },

    #[error("verification limits are invalid")]
    InvalidVerificationLimits,

    #[error("signed statement is invalid: {0}")]
    InvalidStatement(String),

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

    #[error("failed to read signing key at {path}: {source}")]
    ReadSigningKey {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("signing key at {0} must contain a 32-byte Ed25519 seed or base64url-encoded seed")]
    InvalidSigningKey(PathBuf),

    #[error("symbolic link cannot be included in a signed glass: {0}")]
    SymlinkInGlass(String),

    #[error("glass path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),

    #[error("failed to write signature envelope at {path}: {source}")]
    WriteEnvelope {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("package output already exists: {0}")]
    PackageOutputExists(PathBuf),

    #[error("failed to write package at {path}: {source}")]
    WritePackage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid glass manifest: {0}")]
    InvalidManifest(String),
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DsseEnvelope {
    pub payload_type: String,
    pub payload: String,
    pub signatures: Vec<DsseSignature>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DsseSignature {
    pub keyid: String,
    pub sig: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    #[serde(rename = "_type")]
    pub statement_type: String,
    pub subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    pub predicate: Predicate,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub name: String,
    pub digest: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Predicate {
    pub schema_version: u32,
    pub glass_identifier: String,
    pub glass_version: String,
    pub publisher: Publisher,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
        // Keep the legacy ID used by the checked-in directory envelope and
        // the canonical ID generated for packaged archives. Both IDs point
        // to this public development-only fixture key.
        const LEGACY_KEY_ID: &str = "ed25519:rootglass-example-fixture";
        const CANONICAL_KEY_ID: &str =
            "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0";
        const PUBLIC_KEY: &str = "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw";
        Self::new([
            TrustedPublisher {
                name: "Rootglass Examples".to_string(),
                identity: "rootglass.dev/examples".to_string(),
                key_id: LEGACY_KEY_ID.to_string(),
                public_key_base64url: PUBLIC_KEY.to_string(),
            },
            TrustedPublisher {
                name: "Rootglass Examples".to_string(),
                identity: "rootglass.dev/examples".to_string(),
                key_id: CANONICAL_KEY_ID.to_string(),
                public_key_base64url: PUBLIC_KEY.to_string(),
            },
        ])
    }

    /// The public publisher metadata held by this verifier.
    pub fn publishers(&self) -> impl Iterator<Item = &TrustedPublisher> {
        self.publishers.values()
    }

    /// Whether this store has public key material for the supplied signing key ID.
    pub fn contains(&self, key_id: &str) -> bool {
        self.publishers.contains_key(key_id)
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
    verify_glass_dir_with_limits(
        glass_root,
        signature_path,
        trust_store,
        &VerificationLimits::default(),
    )
}

pub fn verify_glass_dir_with_limits(
    glass_root: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
    limits: &VerificationLimits,
) -> Result<VerificationReport, SignatureError> {
    limits.validate()?;
    let glass_root = glass_root.as_ref();
    let signature_path = normalize_relative_path(signature_path.as_ref())?;
    let root_metadata =
        fs::symlink_metadata(glass_root).map_err(|source| SignatureError::ReadSignedFile {
            path: glass_root.to_path_buf(),
            source,
        })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(SignatureError::UnsupportedArchiveEntry(
            glass_root.display().to_string(),
        ));
    }
    let envelope_path = glass_root.join(&signature_path);
    ensure_no_symlink_components(glass_root, &signature_path, true)?;
    let envelope_bytes = read_bounded_file(
        &envelope_path,
        limits.max_signature_bytes,
        true,
        SignatureError::SignatureEnvelopeTooLarge {
            limit: limits.max_signature_bytes,
        },
    )?;
    let envelope: DsseEnvelope = serde_json::from_slice(&envelope_bytes)?;
    let file_set = DirectoryFileSet::new(glass_root, &signature_path, limits.clone());
    verify_envelope_with_files(&file_set, &envelope, trust_store)
}

pub fn verify_glass_archive(
    archive_path: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
) -> Result<VerificationReport, SignatureError> {
    verify_glass_archive_with_limits(
        archive_path,
        signature_path,
        trust_store,
        &VerificationLimits::default(),
    )
}

pub fn verify_glass_archive_with_limits(
    archive_path: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
    limits: &VerificationLimits,
) -> Result<VerificationReport, SignatureError> {
    limits.validate()?;
    let archive_path = archive_path.as_ref();
    let file = fs::File::open(archive_path).map_err(|source| SignatureError::ReadArchive {
        path: archive_path.to_path_buf(),
        source,
    })?;
    let size = file
        .metadata()
        .map_err(|source| SignatureError::ReadArchive {
            path: archive_path.to_path_buf(),
            source,
        })?
        .len();
    if size > limits.max_archive_bytes {
        return Err(SignatureError::ArchiveTooLarge {
            limit: limits.max_archive_bytes,
        });
    }
    verify_glass_archive_reader(file, signature_path, trust_store, limits)
}

pub fn verify_glass_archive_bytes(
    archive_bytes: impl AsRef<[u8]>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
) -> Result<VerificationReport, SignatureError> {
    verify_glass_archive_bytes_with_limits(
        archive_bytes,
        signature_path,
        trust_store,
        &VerificationLimits::default(),
    )
}

pub fn verify_glass_archive_bytes_with_limits(
    archive_bytes: impl AsRef<[u8]>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
    limits: &VerificationLimits,
) -> Result<VerificationReport, SignatureError> {
    limits.validate()?;
    if archive_bytes.as_ref().len() as u64 > limits.max_archive_bytes {
        return Err(SignatureError::ArchiveTooLarge {
            limit: limits.max_archive_bytes,
        });
    }
    verify_glass_archive_reader(
        Cursor::new(archive_bytes.as_ref()),
        signature_path,
        trust_store,
        limits,
    )
}

fn verify_glass_archive_reader<R: Read + Seek>(
    mut reader: R,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
    limits: &VerificationLimits,
) -> Result<VerificationReport, SignatureError> {
    let signature_path = normalize_relative_path(signature_path.as_ref())?;
    preflight_zip_entry_count(&mut reader, limits)?;
    let mut archive = ZipArchive::new(reader)?;
    let signature_name = path_to_string(&signature_path);
    let files = read_zip_files(&mut archive, limits, &signature_name)?;
    let envelope_bytes = files
        .get(&path_to_string(&signature_path))
        .ok_or_else(|| SignatureError::MissingSubject(path_to_string(&signature_path)))?;
    let envelope: DsseEnvelope = serde_json::from_slice(envelope_bytes)?;
    let file_set = ArchiveFileSet {
        files,
        signature_path: signature_name,
        limits: limits.clone(),
    };
    verify_envelope_with_files(&file_set, &envelope, trust_store)
}

/// Bound central-directory allocation before handing an untrusted ZIP to `zip`.
fn preflight_zip_entry_count<R: Read + Seek>(
    reader: &mut R,
    limits: &VerificationLimits,
) -> Result<(), SignatureError> {
    const EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
    const ZIP64_LOCATOR_SIGNATURE: &[u8; 4] = b"PK\x06\x07";
    const ZIP64_EOCD_SIGNATURE: &[u8; 4] = b"PK\x06\x06";
    const EOCD_MIN_BYTES: usize = 22;

    let file_len = reader.seek(SeekFrom::End(0)).map_err(zip_io_error)?;
    if file_len > limits.max_archive_bytes {
        return Err(SignatureError::ArchiveTooLarge {
            limit: limits.max_archive_bytes,
        });
    }
    let tail_len = file_len.min((EOCD_MIN_BYTES + u16::MAX as usize) as u64) as usize;
    if tail_len < EOCD_MIN_BYTES {
        return Err(invalid_zip_archive(
            "archive is too short to contain an end record",
        ));
    }
    let mut tail = vec![0; tail_len];
    reader
        .seek(SeekFrom::Start(file_len - tail_len as u64))
        .map_err(zip_io_error)?;
    reader.read_exact(&mut tail).map_err(zip_io_error)?;
    let eocd_offset = (0..=tail.len() - EOCD_MIN_BYTES)
        .rev()
        .find(|offset| {
            tail.get(*offset..*offset + 4) == Some(EOCD_SIGNATURE.as_slice())
                && offset.checked_add(EOCD_MIN_BYTES).and_then(|end| {
                    end.checked_add(
                        u16::from_le_bytes([tail[*offset + 20], tail[*offset + 21]]) as usize
                    )
                }) == Some(tail.len())
        })
        .ok_or_else(|| invalid_zip_archive("missing ZIP end record"))?;
    let entries = u16::from_le_bytes([tail[eocd_offset + 10], tail[eocd_offset + 11]]);
    let eocd_absolute = file_len - tail_len as u64 + eocd_offset as u64;
    let zip64_locator = eocd_absolute.checked_sub(20).and_then(|locator_absolute| {
        let mut locator = [0; 20];
        reader.seek(SeekFrom::Start(locator_absolute)).ok()?;
        reader.read_exact(&mut locator).ok()?;
        (&locator[..4] == ZIP64_LOCATOR_SIGNATURE).then_some(locator)
    });
    let count = if let Some(locator) = zip64_locator {
        let zip64_absolute = u64::from_le_bytes(locator[8..16].try_into().unwrap());
        let mut zip64 = [0; 56];
        reader
            .seek(SeekFrom::Start(zip64_absolute))
            .map_err(zip_io_error)?;
        reader.read_exact(&mut zip64).map_err(zip_io_error)?;
        if &zip64[..4] != ZIP64_EOCD_SIGNATURE {
            return Err(invalid_zip_archive("missing ZIP64 end record"));
        }
        u64::from_le_bytes(zip64[32..40].try_into().unwrap())
    } else if entries != u16::MAX {
        u64::from(entries)
    } else {
        return Err(invalid_zip_archive("missing ZIP64 locator"));
    };
    if count > limits.max_entries as u64 {
        return Err(SignatureError::TooManyArchiveEntries {
            limit: limits.max_entries,
        });
    }
    reader.seek(SeekFrom::Start(0)).map_err(zip_io_error)?;
    Ok(())
}

fn zip_io_error(error: std::io::Error) -> SignatureError {
    SignatureError::Zip(zip::result::ZipError::Io(error))
}

fn invalid_zip_archive(message: &'static str) -> SignatureError {
    zip_io_error(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

pub fn verify_envelope(
    glass_root: &Path,
    signature_path: &Path,
    envelope: &DsseEnvelope,
    trust_store: &TrustStore,
) -> Result<VerificationReport, SignatureError> {
    let signature_path = normalize_relative_path(signature_path)?;
    let file_set =
        DirectoryFileSet::new(glass_root, &signature_path, VerificationLimits::default());
    verify_envelope_with_files(&file_set, envelope, trust_store)
}

fn verify_envelope_with_files(
    file_set: &dyn FileSet,
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

    let max_encoded_payload = file_set.payload_limit().saturating_mul(4).saturating_add(2) / 3;
    if envelope.payload.len() as u64 > max_encoded_payload {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: file_set.payload_limit(),
        });
    }

    let payload = URL_SAFE_NO_PAD
        .decode(envelope.payload.as_bytes())
        .map_err(|error| SignatureError::InvalidPayloadEncoding(error.to_string()))?;
    if payload.len() as u64 > file_set.payload_limit() {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: file_set.payload_limit(),
        });
    }

    let dsse_signature = &envelope.signatures[0];
    if dsse_signature.keyid.len() > 256 || dsse_signature.sig.len() > 128 {
        return Err(SignatureError::InvalidSignatureEncoding(
            "signature metadata exceeds its size limit".to_string(),
        ));
    }
    let trusted = trust_store
        .get(&dsse_signature.keyid)
        .ok_or_else(|| SignatureError::UnknownKey(dsse_signature.keyid.clone()))?;

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
    verify_statement(file_set, &statement, trusted)?;

    Ok(VerificationReport {
        publisher_name: statement.predicate.publisher.name,
        publisher_identity: statement.predicate.publisher.identity,
        key_id: dsse_signature.keyid.clone(),
        subject_count: statement.subject.len(),
    })
}

fn verify_statement(
    file_set: &dyn FileSet,
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

    if statement.predicate.schema_version != rootglass_manifest::SUPPORTED_SCHEMA_VERSION {
        return Err(SignatureError::InvalidStatement(format!(
            "unsupported predicate schema version {}",
            statement.predicate.schema_version
        )));
    }

    if statement.predicate.publisher.identity != trusted.identity {
        return Err(SignatureError::PublisherIdentityMismatch {
            expected: trusted.identity.clone(),
            actual: statement.predicate.publisher.identity.clone(),
        });
    }
    if statement.predicate.publisher.key_id != trusted.key_id {
        return Err(SignatureError::InvalidStatement(format!(
            "publisher key id '{}' does not match trusted key '{}'",
            statement.predicate.publisher.key_id, trusted.key_id
        )));
    }
    let mut expected_subjects = BTreeSet::new();
    let mut signed_subjects = Vec::with_capacity(statement.subject.len());

    if statement.subject.len() > file_set.max_entries() {
        return Err(SignatureError::TooManyArchiveEntries {
            limit: file_set.max_entries(),
        });
    }

    for subject in &statement.subject {
        let normalized = path_to_string(&normalize_relative_path(Path::new(&subject.name))?);
        if !expected_subjects.insert(normalized.clone()) {
            return Err(SignatureError::InvalidStatement(format!(
                "duplicate subject path '{normalized}'"
            )));
        }
        let expected = subject.digest.get("sha256").ok_or_else(|| {
            SignatureError::InvalidStatement(format!(
                "subject '{}' has no sha256 digest",
                subject.name
            ))
        })?;
        if subject.digest.len() != 1
            || expected.len() != 64
            || !expected
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(SignatureError::InvalidStatement(format!(
                "subject '{}' must have one lowercase sha256 digest",
                subject.name
            )));
        }
        signed_subjects.push((normalized, expected.clone()));
    }

    let candidates = file_set.candidate_files()?;
    for file in &candidates {
        if !expected_subjects.contains(file) {
            return Err(SignatureError::UnsignedFile(file.clone()));
        }
    }

    for (subject, (normalized, expected)) in statement.subject.iter().zip(signed_subjects) {
        let bytes = file_set.read(&normalized)?;
        let actual = hex_sha256(&bytes);
        if actual != expected {
            return Err(SignatureError::DigestMismatch {
                path: subject.name.clone(),
                expected,
                actual,
            });
        }
    }

    Ok(())
}

trait FileSet {
    fn read(&self, path: &str) -> Result<Vec<u8>, SignatureError>;
    fn candidate_files(&self) -> Result<Vec<String>, SignatureError>;
    fn payload_limit(&self) -> u64;
    fn max_entries(&self) -> usize;
}

struct DirectoryFileSet<'a> {
    root: &'a Path,
    signature_path: String,
    limits: VerificationLimits,
    read_total: Cell<u64>,
}

impl<'a> DirectoryFileSet<'a> {
    fn new(root: &'a Path, signature_path: &Path, limits: VerificationLimits) -> Self {
        Self {
            root,
            signature_path: path_to_string(signature_path),
            limits,
            read_total: Cell::new(0),
        }
    }
}

impl FileSet for DirectoryFileSet<'_> {
    fn read(&self, path: &str) -> Result<Vec<u8>, SignatureError> {
        let normalized = normalize_relative_path(Path::new(path))?;
        ensure_no_symlink_components(self.root, &normalized, false)?;
        let path = self.root.join(&normalized);
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| SignatureError::ReadSignedFile {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SignatureError::MissingSubject(path_to_string(&normalized)));
        }
        let read_total = self.read_total.get().checked_add(metadata.len()).ok_or(
            SignatureError::ArchiveUncompressedTooLarge {
                limit: self.limits.max_total_uncompressed_bytes,
            },
        )?;
        if read_total > self.limits.max_total_uncompressed_bytes {
            return Err(SignatureError::ArchiveUncompressedTooLarge {
                limit: self.limits.max_total_uncompressed_bytes,
            });
        }
        self.read_total.set(read_total);
        read_bounded_file(
            &path,
            self.limits.max_member_bytes,
            false,
            SignatureError::SignedFileTooLarge {
                path: path_to_string(&normalized),
                limit: self.limits.max_member_bytes,
            },
        )
    }

    fn candidate_files(&self) -> Result<Vec<String>, SignatureError> {
        let root_metadata =
            fs::symlink_metadata(self.root).map_err(|source| SignatureError::ReadSignedFile {
                path: self.root.to_path_buf(),
                source,
            })?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(SignatureError::UnsupportedArchiveEntry(
                self.root.display().to_string(),
            ));
        }
        let mut files = Vec::new();
        let mut total = 0u64;
        let mut entry_count = 0usize;
        collect_directory_files(
            self.root,
            self.root,
            &self.signature_path,
            &self.limits,
            &mut total,
            &mut entry_count,
            &mut files,
        )?;
        files.sort();
        Ok(files)
    }

    fn payload_limit(&self) -> u64 {
        self.limits.max_signature_bytes
    }

    fn max_entries(&self) -> usize {
        self.limits.max_entries
    }
}

struct ArchiveFileSet {
    files: BTreeMap<String, Vec<u8>>,
    signature_path: String,
    limits: VerificationLimits,
}

impl FileSet for ArchiveFileSet {
    fn read(&self, path: &str) -> Result<Vec<u8>, SignatureError> {
        let normalized = path_to_string(&normalize_relative_path(Path::new(path))?);
        self.files
            .get(&normalized)
            .cloned()
            .ok_or(SignatureError::MissingSubject(normalized))
    }

    fn candidate_files(&self) -> Result<Vec<String>, SignatureError> {
        Ok(self
            .files
            .keys()
            .filter(|path| *path != &self.signature_path)
            .cloned()
            .collect())
    }

    fn payload_limit(&self) -> u64 {
        self.limits.max_signature_bytes
    }

    fn max_entries(&self) -> usize {
        self.limits.max_entries
    }
}

fn read_zip_files<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    limits: &VerificationLimits,
    signature_path: &str,
) -> Result<BTreeMap<String, Vec<u8>>, SignatureError> {
    if archive.len() > limits.max_entries {
        return Err(SignatureError::TooManyArchiveEntries {
            limit: limits.max_entries,
        });
    }
    let mut files = BTreeMap::new();
    let mut kinds = BTreeMap::<String, bool>::new();
    let mut total = 0u64;

    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        let normalized = normalize_archive_name(&name, entry.is_dir())?;
        let is_dir = entry.is_dir();

        let collision_key = normalized.to_lowercase();
        if kinds.insert(collision_key.clone(), is_dir).is_some() {
            return Err(SignatureError::DuplicateArchiveEntry(normalized));
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind == 0o120000
                || (kind != 0 && kind != 0o100000 && kind != 0o040000)
                || (kind == 0o040000 && !is_dir)
                || (kind == 0o100000 && is_dir)
            {
                return Err(SignatureError::UnsupportedArchiveEntry(normalized));
            }
        }

        // Reject file-as-parent collisions regardless of archive ordering.
        let mut parent = collision_key.rsplit_once('/').map(|(parent, _)| parent);
        while let Some(parent_name) = parent {
            if kinds.get(parent_name) == Some(&false) {
                return Err(SignatureError::UnsupportedArchiveEntry(
                    parent_name.to_string(),
                ));
            }
            parent = parent_name.rsplit_once('/').map(|(parent, _)| parent);
        }
        if !is_dir
            && kinds
                .keys()
                .any(|existing| existing.starts_with(&(collision_key.clone() + "/")))
        {
            return Err(SignatureError::UnsupportedArchiveEntry(normalized));
        }

        if is_dir {
            if entry.size() != 0 {
                return Err(SignatureError::UnsupportedArchiveEntry(normalized));
            }
            continue;
        }

        let member_size = entry.size();
        let compressed_size = entry.compressed_size();
        let is_signature = normalized == signature_path;
        let member_limit = if is_signature {
            limits.max_signature_bytes
        } else {
            limits.max_member_bytes
        };
        if member_size > member_limit {
            if is_signature {
                return Err(SignatureError::SignatureEnvelopeTooLarge {
                    limit: member_limit,
                });
            }
            return Err(SignatureError::SignedFileTooLarge {
                path: normalized,
                limit: member_limit,
            });
        }
        total =
            total
                .checked_add(member_size)
                .ok_or(SignatureError::ArchiveUncompressedTooLarge {
                    limit: limits.max_total_uncompressed_bytes,
                })?;
        if total > limits.max_total_uncompressed_bytes {
            return Err(SignatureError::ArchiveUncompressedTooLarge {
                limit: limits.max_total_uncompressed_bytes,
            });
        }
        if member_size > 0
            && (compressed_size == 0
                || member_size > compressed_size.saturating_mul(limits.max_compression_ratio))
        {
            return Err(SignatureError::CompressionRatioExceeded {
                path: normalized,
                limit: limits.max_compression_ratio,
            });
        }

        let mut bytes = Vec::new();
        entry
            .take(member_limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| SignatureError::ReadSignedFile {
                path: PathBuf::from(&normalized),
                source,
            })?;
        if bytes.len() as u64 > member_limit {
            if is_signature {
                return Err(SignatureError::SignatureEnvelopeTooLarge {
                    limit: member_limit,
                });
            }
            return Err(SignatureError::SignedFileTooLarge {
                path: normalized,
                limit: member_limit,
            });
        }
        if bytes.len() as u64 != member_size {
            return Err(SignatureError::InvalidStatement(format!(
                "archive member '{normalized}' size changed while reading"
            )));
        }

        if files.insert(normalized.clone(), bytes).is_some() {
            return Err(SignatureError::DuplicateArchiveEntry(normalized));
        }
    }

    Ok(files)
}

fn collect_directory_files(
    root: &Path,
    current: &Path,
    signature_path: &str,
    limits: &VerificationLimits,
    total: &mut u64,
    entry_count: &mut usize,
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
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| SignatureError::ReadSignedFile {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(SignatureError::UnsupportedArchiveEntry(
                path.display().to_string(),
            ));
        }
        if metadata.is_dir() {
            collect_directory_files(
                root,
                &path,
                signature_path,
                limits,
                total,
                entry_count,
                files,
            )?;
        } else if metadata.is_file() {
            *entry_count =
                entry_count
                    .checked_add(1)
                    .ok_or(SignatureError::TooManyArchiveEntries {
                        limit: limits.max_entries,
                    })?;
            if *entry_count > limits.max_entries {
                return Err(SignatureError::TooManyArchiveEntries {
                    limit: limits.max_entries,
                });
            }
            let relative = path.strip_prefix(root).expect("path is inside root");
            let relative = path_to_string(&normalize_relative_path(relative)?);
            if relative == signature_path {
                continue;
            }
            // Directory-form fixture verification allows generated images to be absent
            // or unsigned. Production archive verification does not apply this exclusion.
            if path.extension().is_some_and(|extension| extension == "img") {
                continue;
            }
            if metadata.len() > limits.max_member_bytes {
                return Err(SignatureError::SignedFileTooLarge {
                    path: relative,
                    limit: limits.max_member_bytes,
                });
            }
            *total = total.checked_add(metadata.len()).ok_or(
                SignatureError::ArchiveUncompressedTooLarge {
                    limit: limits.max_total_uncompressed_bytes,
                },
            )?;
            if *total > limits.max_total_uncompressed_bytes {
                return Err(SignatureError::ArchiveUncompressedTooLarge {
                    limit: limits.max_total_uncompressed_bytes,
                });
            }
            files.push(relative);
        } else {
            return Err(SignatureError::UnsupportedArchiveEntry(
                path.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn normalize_archive_name(name: &str, is_dir: bool) -> Result<String, SignatureError> {
    let candidate = if is_dir {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    normalize_path_text(candidate)
}

pub(crate) fn normalize_relative_path(path: &Path) -> Result<PathBuf, SignatureError> {
    let text = path
        .to_str()
        .ok_or_else(|| SignatureError::NonUtf8Path(path.to_path_buf()))?;
    let text = text.replace(std::path::MAIN_SEPARATOR, "/");
    let normalized = normalize_path_text(&text)?;
    Ok(normalized.split('/').collect())
}

fn normalize_path_text(name: &str) -> Result<String, SignatureError> {
    if name.is_empty()
        || name.len() > 4096
        || name.contains('\\')
        || name.starts_with('/')
        || name.contains(':')
        || name.contains('\0')
    {
        return Err(SignatureError::UnsafeArchivePath(name.to_string()));
    }
    let mut normalized = Vec::new();
    for segment in name.split('/') {
        let device_name = segment
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.len() > 255
            || segment.ends_with('.')
            || segment.ends_with(' ')
            || segment
                .chars()
                .any(|ch| ch.is_control() || "<>|?*".contains(ch))
            || matches!(
                device_name.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
                    | "COM¹"
                    | "COM²"
                    | "COM³"
                    | "LPT¹"
                    | "LPT²"
                    | "LPT³"
            )
        {
            return Err(SignatureError::UnsafeArchivePath(name.to_string()));
        }
        normalized.push(segment);
    }
    Ok(normalized.join("/"))
}

fn ensure_no_symlink_components(
    root: &Path,
    relative: &Path,
    envelope: bool,
) -> Result<(), SignatureError> {
    let mut current = root.to_path_buf();
    let components = relative
        .to_str()
        .ok_or_else(|| SignatureError::NonUtf8Path(relative.to_path_buf()))?
        .split('/')
        .collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|source| {
            if envelope {
                SignatureError::ReadEnvelope {
                    path: current.clone(),
                    source,
                }
            } else {
                SignatureError::ReadSignedFile {
                    path: current.clone(),
                    source,
                }
            }
        })?;
        if metadata.file_type().is_symlink() || (index + 1 < components.len() && !metadata.is_dir())
        {
            return Err(SignatureError::UnsupportedArchiveEntry(
                current.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn read_bounded_file(
    path: &Path,
    limit: u64,
    envelope: bool,
    too_large_error: SignatureError,
) -> Result<Vec<u8>, SignatureError> {
    let file = fs::File::open(path).map_err(|source| {
        if envelope {
            SignatureError::ReadEnvelope {
                path: path.to_path_buf(),
                source,
            }
        } else {
            SignatureError::ReadSignedFile {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let metadata = file.metadata().map_err(|source| {
        if envelope {
            SignatureError::ReadEnvelope {
                path: path.to_path_buf(),
                source,
            }
        } else {
            SignatureError::ReadSignedFile {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if metadata.len() > limit {
        return Err(too_large_error);
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| {
            if envelope {
                SignatureError::ReadEnvelope {
                    path: path.to_path_buf(),
                    source,
                }
            } else {
                SignatureError::ReadSignedFile {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
    if bytes.len() as u64 > limit {
        return Err(too_large_error);
    }
    Ok(bytes)
}

pub(crate) fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub(crate) fn dsse_pae(payload_type: &[u8], payload: &[u8]) -> Vec<u8> {
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

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

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
    fn verifies_sample_glass_zip_archive() {
        let archive = build_sample_archive([]);
        let report = verify_glass_archive_bytes(
            archive,
            "signatures/glass.dsse.json",
            &TrustStore::example_fixture(),
        )
        .expect("sample archive verifies");

        assert_eq!(report.publisher_identity, "rootglass.dev/examples");
        assert_eq!(report.subject_count, 3);
    }

    #[test]
    fn archive_rejects_unsigned_extra_file() {
        let archive = build_sample_archive([("extra.txt", b"extra".as_slice())]);
        let error = verify_glass_archive_bytes(
            archive,
            "signatures/glass.dsse.json",
            &TrustStore::example_fixture(),
        )
        .expect_err("archive with unsigned file should fail");

        assert!(matches!(error, SignatureError::UnsignedFile(path) if path == "extra.txt"));
    }

    #[test]
    fn archive_rejects_unsafe_path() {
        let archive = build_archive([("../escape.txt", b"nope".as_slice())]);
        let error = verify_glass_archive_bytes(
            archive,
            "signatures/glass.dsse.json",
            &TrustStore::example_fixture(),
        )
        .expect_err("archive with unsafe path should fail");

        assert!(matches!(error, SignatureError::UnsafeArchivePath(_)));
    }

    #[test]
    fn archive_enforces_size_entry_and_compression_limits() {
        let sample = build_sample_archive([]);
        let limits = VerificationLimits {
            max_archive_bytes: 1,
            ..VerificationLimits::default()
        };
        assert!(matches!(
            verify_glass_archive_bytes_with_limits(
                &sample,
                "signatures/glass.dsse.json",
                &TrustStore::example_fixture(),
                &limits
            ),
            Err(SignatureError::ArchiveTooLarge { .. })
        ));

        let limits = VerificationLimits {
            max_entries: 3,
            ..VerificationLimits::default()
        };
        assert!(matches!(
            verify_glass_archive_bytes_with_limits(
                &sample,
                "signatures/glass.dsse.json",
                &TrustStore::example_fixture(),
                &limits
            ),
            Err(SignatureError::TooManyArchiveEntries { .. })
        ));

        let limits = VerificationLimits {
            max_signature_bytes: 8,
            ..VerificationLimits::default()
        };
        assert!(matches!(
            verify_glass_archive_bytes_with_limits(
                &sample,
                "signatures/glass.dsse.json",
                &TrustStore::example_fixture(),
                &limits
            ),
            Err(SignatureError::SignatureEnvelopeTooLarge { .. })
        ));

        let large = vec![b'x'; 32 * 1024];
        let compressed = build_sample_archive_deflated([("extra.txt", large.as_slice())]);
        let limits = VerificationLimits {
            max_compression_ratio: 2,
            ..VerificationLimits::default()
        };
        assert!(matches!(
            verify_glass_archive_bytes_with_limits(
                compressed,
                "signatures/glass.dsse.json",
                &TrustStore::example_fixture(),
                &limits
            ),
            Err(SignatureError::CompressionRatioExceeded { .. })
        ));
    }

    #[test]
    fn archive_rejects_case_collisions_parent_conflicts_and_symlinks() {
        let collision = build_archive([
            ("Case.txt", b"first".as_slice()),
            ("case.txt", b"second".as_slice()),
        ]);
        assert!(matches!(
            verify_glass_archive_bytes(
                collision,
                "signatures/glass.dsse.json",
                &TrustStore::example_fixture()
            ),
            Err(SignatureError::DuplicateArchiveEntry(_))
        ));

        let conflict = build_archive([("a", b"file".as_slice()), ("a/b", b"child".as_slice())]);
        assert!(matches!(
            verify_glass_archive_bytes(
                conflict,
                "signatures/glass.dsse.json",
                &TrustStore::example_fixture()
            ),
            Err(SignatureError::UnsupportedArchiveEntry(_))
        ));

        let symlink = build_symlink_archive("link", "target");
        assert!(matches!(
            verify_glass_archive_bytes(
                symlink,
                "signatures/glass.dsse.json",
                &TrustStore::example_fixture()
            ),
            Err(SignatureError::UnsupportedArchiveEntry(_))
        ));
    }

    #[test]
    fn missing_sha256_digest_is_rejected_without_panicking() {
        let key_id = "ed25519:rootglass-example-fixture";
        let statement = serde_json::json!({
            "_type": STATEMENT_TYPE,
            "subject": [{"name": "README.md", "digest": {}}],
            "predicateType": PREDICATE_TYPE,
            "predicate": {
                "schemaVersion": 1,
                "glassIdentifier": "io.rootglass.examples.hello-world",
                "glassVersion": "0.1.0",
                "publisher": {
                    "name": "Rootglass Examples",
                    "identity": "rootglass.dev/examples",
                    "keyId": key_id
                },
                "createdAt": "2026-11-06T00:00:00Z"
            }
        });
        let envelope = sign_fixture_statement(statement);
        let error = verify_envelope(
            Path::new("../../examples/hello-world.glass"),
            Path::new("signatures/glass.dsse.json"),
            &envelope,
            &TrustStore::example_fixture(),
        )
        .expect_err("missing sha256 should fail closed");
        assert!(matches!(error, SignatureError::InvalidStatement(_)));
    }

    #[test]
    fn unsupported_predicate_schema_is_rejected() {
        let statement = serde_json::json!({
            "_type": STATEMENT_TYPE,
            "subject": [],
            "predicateType": PREDICATE_TYPE,
            "predicate": {
                "schemaVersion": 99,
                "glassIdentifier": "io.rootglass.examples.hello-world",
                "glassVersion": "0.1.0",
                "publisher": {
                    "name": "Rootglass Examples",
                    "identity": "rootglass.dev/examples",
                    "keyId": "ed25519:rootglass-example-fixture"
                },
                "createdAt": "2026-11-06T00:00:00Z"
            }
        });
        let envelope = sign_fixture_statement(statement);
        let error = verify_envelope(
            Path::new("../../examples/hello-world.glass"),
            Path::new("signatures/glass.dsse.json"),
            &envelope,
            &TrustStore::example_fixture(),
        )
        .expect_err("an unknown statement schema must fail closed");
        assert!(matches!(error, SignatureError::InvalidStatement(_)));
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

    #[test]
    fn rejects_missing_signature_file() {
        let error = verify_glass_dir(
            "../../examples/hello-world.glass",
            "signatures/missing.dsse.json",
            &TrustStore::example_fixture(),
        )
        .expect_err("missing signature should fail");

        assert!(matches!(error, SignatureError::ReadEnvelope { .. }));
    }

    #[test]
    fn directory_signature_envelope_obeys_configured_limit() {
        let limits = VerificationLimits {
            max_signature_bytes: 8,
            ..VerificationLimits::default()
        };
        let error = verify_glass_dir_with_limits(
            "../../examples/hello-world.glass",
            "signatures/glass.dsse.json",
            &TrustStore::example_fixture(),
            &limits,
        )
        .expect_err("oversized directory signature envelope should fail");
        assert!(matches!(
            error,
            SignatureError::SignatureEnvelopeTooLarge { .. }
        ));
    }

    #[test]
    fn rejects_tampered_signed_file() {
        let temp = copy_sample_glass_to_temp();
        fs::write(temp.join("README.md"), "tampered\n").expect("tamper with signed file");

        let error = verify_glass_dir(
            &temp,
            "signatures/glass.dsse.json",
            &TrustStore::example_fixture(),
        )
        .expect_err("tampered file should fail");

        assert!(matches!(error, SignatureError::DigestMismatch { .. }));
        fs::remove_dir_all(temp).expect("remove temp fixture");
    }

    #[cfg(unix)]
    #[test]
    fn directory_verification_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = copy_sample_glass_to_temp();
        symlink("/etc/passwd", temp.join("external.txt")).unwrap();
        let error = verify_glass_dir(
            &temp,
            "signatures/glass.dsse.json",
            &TrustStore::example_fixture(),
        )
        .expect_err("a directory symlink must not be followed");
        assert!(matches!(error, SignatureError::UnsupportedArchiveEntry(_)));
        fs::remove_dir_all(temp).expect("remove temp fixture");
    }

    #[test]
    fn signing_key_file_read_is_bounded() {
        let path = std::env::temp_dir().join(format!(
            "rootglass-signing-key-{}-{}.key",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, vec![b'x'; 257]).unwrap();
        assert!(matches!(
            load_signing_key(&path),
            Err(SignatureError::InvalidSigningKey(_))
        ));
        let _ = fs::remove_file(path);
    }

    fn build_sample_archive<const N: usize>(extras: [(&'static str, &'static [u8]); N]) -> Vec<u8> {
        let files = [
            (
                "README.md",
                include_bytes!("../../../examples/hello-world.glass/README.md").as_slice(),
            ),
            (
                "metadata/README.md",
                include_bytes!("../../../examples/hello-world.glass/metadata/README.md").as_slice(),
            ),
            (
                "rootglass.toml",
                include_bytes!("../../../examples/hello-world.glass/rootglass.toml").as_slice(),
            ),
            (
                "signatures/glass.dsse.json",
                include_bytes!("../../../examples/hello-world.glass/signatures/glass.dsse.json")
                    .as_slice(),
            ),
        ];

        let mut entries = Vec::new();
        entries.extend(files);
        entries.extend(extras);
        build_archive(entries)
    }

    fn build_sample_archive_deflated<const N: usize>(
        extras: [(&'static str, &[u8]); N],
    ) -> Vec<u8> {
        let files = [
            (
                "README.md",
                include_bytes!("../../../examples/hello-world.glass/README.md").as_slice(),
            ),
            (
                "metadata/README.md",
                include_bytes!("../../../examples/hello-world.glass/metadata/README.md").as_slice(),
            ),
            (
                "rootglass.toml",
                include_bytes!("../../../examples/hello-world.glass/rootglass.toml").as_slice(),
            ),
            (
                "signatures/glass.dsse.json",
                include_bytes!("../../../examples/hello-world.glass/signatures/glass.dsse.json")
                    .as_slice(),
            ),
        ];
        let mut entries = Vec::new();
        entries.extend(files);
        entries.extend(extras);
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn build_symlink_archive(name: &'static str, target: &'static str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink(name, target, SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap().into_inner()
    }

    fn sign_fixture_statement(statement: serde_json::Value) -> DsseEnvelope {
        let key_seed: [u8; 32] =
            Sha256::digest(b"Rootglass public development fixture signing key v1").into();
        let key = SigningKey::from_bytes(&key_seed);
        let payload = serde_json::to_vec(&statement).unwrap();
        let signature = key.sign(&dsse_pae(DSSE_PAYLOAD_TYPE.as_bytes(), &payload));
        serde_json::from_value(serde_json::json!({
            "payloadType": DSSE_PAYLOAD_TYPE,
            "payload": URL_SAFE_NO_PAD.encode(payload),
            "signatures": [{
                "keyid": "ed25519:rootglass-example-fixture",
                "sig": URL_SAFE_NO_PAD.encode(signature.to_bytes())
            }]
        }))
        .unwrap()
    }

    fn build_archive<I>(entries: I) -> Vec<u8>
    where
        I: IntoIterator<Item = (&'static str, &'static [u8])>,
    {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

        for (name, bytes) in entries {
            writer.start_file(name, options).expect("start file");
            writer.write_all(bytes).expect("write file");
        }

        writer.finish().expect("finish archive").into_inner()
    }

    fn copy_sample_glass_to_temp() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after unix epoch")
            .as_nanos();
        let temp = std::env::temp_dir().join(format!("rootglass-signature-test-{unique}.glass"));
        fs::create_dir_all(temp.join("metadata")).expect("create metadata dir");
        fs::create_dir_all(temp.join("signatures")).expect("create signatures dir");

        copy_fixture_file("README.md", &temp);
        copy_fixture_file("rootglass.toml", &temp);
        copy_fixture_file("metadata/README.md", &temp);
        copy_fixture_file("signatures/glass.dsse.json", &temp);
        temp
    }

    fn copy_fixture_file(relative: &str, temp: &Path) {
        let source = Path::new("../../examples/hello-world.glass").join(relative);
        let destination = temp.join(relative);
        fs::copy(source, destination).expect("copy fixture file");
    }
}
