use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use zip::{CompressionMethod, HasZipMetadata, ZipArchive};

mod recipient;
mod signing;
pub mod trust_config;
pub use recipient::{
    encrypted_artifact_set_digest, personalize_glass_archive_at, recipient_id_for_public_key,
    sign_recipient_envelope, verify_recipient_envelope, verify_recipient_envelope_bytes,
    RecipientEnvelopePayload, RecipientProvider, RecipientRecord, RecipientWrappingSuite,
    VerifiedRecipientEnvelope, MAX_RECIPIENTS, RECIPIENT_ENVELOPE_PATH,
    RECIPIENT_ENVELOPE_PAYLOAD_TYPE,
};
pub use signing::{
    load_signing_key, package_glass_directory, package_glass_directory_at, sign_glass_directory,
    sign_glass_directory_at, SigningReport,
};

pub const DSSE_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
pub const PREDICATE_TYPE: &str = "https://rootglass.dev/predicate/glass/v1";
pub const PREDICATE_TYPE_V2: &str = "https://rootglass.dev/predicate/glass/v2";
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

    #[error("artifact inventory mismatch for '{artifact_id}': {reason}")]
    ArtifactInventoryMismatch { artifact_id: String, reason: String },

    #[error("encrypted artifact '{artifact_id}' has an invalid ZIP layout: {reason}")]
    InvalidEncryptedArtifactLayout { artifact_id: String, reason: String },

    #[error("encrypted artifact '{0}' is not present in the verified catalog")]
    EncryptedArtifactNotFound(String),

    #[error("artifact '{0}' is not declared as encrypted")]
    ArtifactNotEncrypted(String),

    #[error("encrypted bundle is missing its recipient envelope")]
    MissingRecipientEnvelope,

    #[error("invalid recipient envelope: {0}")]
    InvalidRecipientEnvelope(String),
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
    pub glass_identifier: String,
    pub glass_version: String,
    /// SHA-256 of the exact publisher-signed manifest bytes, when present.
    pub manifest_sha256: Option<String>,
    pub subject_count: usize,
    pub schema_version: u32,
    /// Present only after schema-2 storage paths, sizes and digests match signed subjects.
    pub artifact_catalog: Option<rootglass_manifest::ArtifactCatalog>,
    /// Normalized member paths from the verified statement, in statement order.
    pub signed_paths: Vec<String>,
    verified_context: VerifiedContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedContext {
    schema_version: u32,
    glass_identifier: String,
    glass_version: String,
    manifest_sha256: Option<String>,
    artifact_catalog: Option<rootglass_manifest::ArtifactCatalog>,
}

/// An archive that has passed publisher verification and encrypted-entry layout checks.
/// Archive offsets and physical ZIP names are kept internal to the container abstraction.
pub struct VerifiedArchive {
    report: VerificationReport,
    source: Arc<Mutex<File>>,
    encrypted_artifacts: BTreeMap<String, VerifiedArtifactRange>,
    limits: VerificationLimits,
}

impl VerifiedArchive {
    pub fn report(&self) -> &VerificationReport {
        &self.report
    }

    /// Verify the separately signed recipient sidecar against this archive's publisher report.
    pub fn verify_recipient_envelope(&self) -> Result<VerifiedRecipientEnvelope, SignatureError> {
        let source = self.source.lock().map_err(|_| {
            SignatureError::InvalidRecipientEnvelope("archive reader lock was poisoned".to_string())
        })?;
        let mut archive = ZipArchive::new(source.try_clone().map_err(zip_io_error)?)?;
        let entry = match archive.by_name(RECIPIENT_ENVELOPE_PATH) {
            Ok(entry) => entry,
            Err(zip::result::ZipError::FileNotFound) => {
                return Err(SignatureError::MissingRecipientEnvelope)
            }
            Err(error) => return Err(error.into()),
        };
        let envelope_limit = self
            .limits
            .max_signature_bytes
            .min(self.limits.max_member_bytes)
            .min(MAX_SIGNATURE_ENVELOPE_BYTES);
        if entry.size() > envelope_limit {
            return Err(SignatureError::SignatureEnvelopeTooLarge {
                limit: envelope_limit,
            });
        }
        let mut bytes = Vec::new();
        entry
            .take(envelope_limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(zip_io_error)?;
        verify_recipient_envelope_bytes(&bytes, &self.report, &self.limits)
    }

    /// Open one signed encrypted disk as a bounded read-only stream.
    pub fn open_encrypted_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<VerifiedArtifactReader, SignatureError> {
        let catalog =
            self.report.artifact_catalog.as_ref().ok_or_else(|| {
                SignatureError::EncryptedArtifactNotFound(artifact_id.to_string())
            })?;
        let artifact = catalog
            .artifacts
            .get(artifact_id)
            .ok_or_else(|| SignatureError::EncryptedArtifactNotFound(artifact_id.to_string()))?;
        if artifact.encryption.is_none() {
            return Err(SignatureError::ArtifactNotEncrypted(
                artifact_id.to_string(),
            ));
        }
        let range = self
            .encrypted_artifacts
            .get(artifact_id)
            .ok_or_else(|| SignatureError::EncryptedArtifactNotFound(artifact_id.to_string()))?;
        Ok(VerifiedArtifactReader {
            source: Arc::clone(&self.source),
            offset: range.offset,
            length: range.length,
            position: 0,
        })
    }
}

/// A signed development directory. Its encrypted assets are ordinary verified files,
/// not ZIP ranges.
pub struct VerifiedDirectory {
    report: VerificationReport,
    root: PathBuf,
    limits: VerificationLimits,
}

impl VerifiedDirectory {
    pub fn report(&self) -> &VerificationReport {
        &self.report
    }

    /// Verify the separately signed recipient sidecar from a development directory.
    pub fn verify_recipient_envelope(&self) -> Result<VerifiedRecipientEnvelope, SignatureError> {
        let relative = PathBuf::from(RECIPIENT_ENVELOPE_PATH);
        ensure_no_symlink_components(&self.root, &relative, true)?;
        let path = self.root.join(relative);
        let envelope_limit = self
            .limits
            .max_signature_bytes
            .min(self.limits.max_member_bytes)
            .min(MAX_SIGNATURE_ENVELOPE_BYTES);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(SignatureError::MissingRecipientEnvelope)
            }
            Err(source) => return Err(SignatureError::ReadEnvelope { path, source }),
        }
        let bytes = read_bounded_file(
            &path,
            envelope_limit,
            true,
            SignatureError::SignatureEnvelopeTooLarge {
                limit: envelope_limit,
            },
        )?;
        verify_recipient_envelope_bytes(&bytes, &self.report, &self.limits)
    }

    /// Open one signed encrypted disk after checking its path, size, and digest again.
    pub fn open_encrypted_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<VerifiedArtifactReader, SignatureError> {
        let artifact = catalog_encrypted_artifact(&self.report, artifact_id)?;
        let relative = normalize_relative_path(Path::new(&artifact.storage.path))?;
        let root_metadata =
            fs::symlink_metadata(&self.root).map_err(|source| SignatureError::ReadSignedFile {
                path: self.root.clone(),
                source,
            })?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(SignatureError::UnsupportedArchiveEntry(
                self.root.display().to_string(),
            ));
        }
        ensure_no_symlink_components(&self.root, &relative, false)?;
        let path = self.root.join(&relative);
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| SignatureError::ReadSignedFile {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SignatureError::UnsupportedArchiveEntry(
                path.display().to_string(),
            ));
        }
        if metadata.len() != artifact.size_bytes {
            return Err(SignatureError::ArtifactInventoryMismatch {
                artifact_id: artifact_id.to_string(),
                reason: format!(
                    "development artifact size {} does not match signed size {}",
                    metadata.len(),
                    artifact.size_bytes
                ),
            });
        }
        let canonical_root =
            fs::canonicalize(&self.root).map_err(|source| SignatureError::ReadSignedFile {
                path: self.root.clone(),
                source,
            })?;
        let canonical_path =
            fs::canonicalize(&path).map_err(|source| SignatureError::ReadSignedFile {
                path: path.clone(),
                source,
            })?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(SignatureError::UnsupportedArchiveEntry(
                path.display().to_string(),
            ));
        }
        let mut file = File::open(&path).map_err(|source| SignatureError::ReadSignedFile {
            path: path.clone(),
            source,
        })?;
        let digest = hash_open_file(&mut file, artifact.size_bytes).map_err(|source| {
            SignatureError::ReadSignedFile {
                path: path.clone(),
                source,
            }
        })?;
        if digest != artifact.sha256 {
            return Err(SignatureError::DigestMismatch {
                path: artifact.storage.path.clone(),
                expected: artifact.sha256.clone(),
                actual: digest,
            });
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|source| SignatureError::ReadSignedFile {
                path: path.clone(),
                source,
            })?;
        Ok(VerifiedArtifactReader {
            source: Arc::new(Mutex::new(file)),
            offset: 0,
            length: artifact.size_bytes,
            position: 0,
        })
    }
}

fn catalog_encrypted_artifact<'a>(
    report: &'a VerificationReport,
    artifact_id: &str,
) -> Result<&'a rootglass_manifest::Artifact, SignatureError> {
    let catalog = report
        .artifact_catalog
        .as_ref()
        .ok_or_else(|| SignatureError::EncryptedArtifactNotFound(artifact_id.to_string()))?;
    let artifact = catalog
        .artifacts
        .get(artifact_id)
        .ok_or_else(|| SignatureError::EncryptedArtifactNotFound(artifact_id.to_string()))?;
    if artifact.encryption.is_none() {
        return Err(SignatureError::ArtifactNotEncrypted(
            artifact_id.to_string(),
        ));
    }
    Ok(artifact)
}

/// A read-only reader constrained to one verified artifact. Seeking cannot escape its extent.
pub struct VerifiedArtifactReader {
    source: Arc<Mutex<File>>,
    offset: u64,
    length: u64,
    position: u64,
}

impl VerifiedArtifactReader {
    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
}

impl Read for VerifiedArtifactReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.length.saturating_sub(self.position);
        let count = buffer.len().min(remaining.min(usize::MAX as u64) as usize);
        if count == 0 {
            return Ok(0);
        }
        let absolute = self.offset.checked_add(self.position).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "artifact range overflow")
        })?;
        let mut source = self
            .source
            .lock()
            .map_err(|_| io::Error::other("verified archive reader lock was poisoned"))?;
        source.seek(SeekFrom::Start(absolute))?;
        let read = source.read(&mut buffer[..count])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for VerifiedArtifactReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.length.checked_add_signed(offset),
        }
        .filter(|offset| *offset <= self.length)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek is outside the verified artifact extent",
            )
        })?;
        self.position = next;
        Ok(next)
    }
}

#[derive(Debug, Clone, Copy)]
struct VerifiedArtifactRange {
    offset: u64,
    length: u64,
}

/// Verify a ZIP archive and retain a read-only handle for its encrypted disk assets.
pub fn open_verified_glass_archive(
    archive_path: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
) -> Result<VerifiedArchive, SignatureError> {
    open_verified_glass_archive_with_limits(
        archive_path,
        signature_path,
        trust_store,
        &VerificationLimits::default(),
    )
}

pub fn open_verified_glass_archive_with_limits(
    archive_path: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
    limits: &VerificationLimits,
) -> Result<VerifiedArchive, SignatureError> {
    limits.validate()?;
    let archive_path = archive_path.as_ref();
    let metadata =
        fs::symlink_metadata(archive_path).map_err(|source| SignatureError::ReadArchive {
            path: archive_path.to_path_buf(),
            source,
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SignatureError::UnsupportedArchiveEntry(
            archive_path.display().to_string(),
        ));
    }
    if metadata.len() > limits.max_archive_bytes {
        return Err(SignatureError::ArchiveTooLarge {
            limit: limits.max_archive_bytes,
        });
    }
    let source = File::open(archive_path).map_err(|source| SignatureError::ReadArchive {
        path: archive_path.to_path_buf(),
        source,
    })?;
    let verification_source = source
        .try_clone()
        .map_err(|source| SignatureError::ReadArchive {
            path: archive_path.to_path_buf(),
            source,
        })?;
    let (report, encrypted_artifacts) = verify_glass_archive_reader_with_artifacts(
        verification_source,
        signature_path,
        trust_store,
        limits,
    )?;
    Ok(VerifiedArchive {
        report,
        source: Arc::new(Mutex::new(source)),
        encrypted_artifacts,
        limits: limits.clone(),
    })
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

pub fn open_verified_glass_directory(
    glass_root: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
) -> Result<VerifiedDirectory, SignatureError> {
    open_verified_glass_directory_with_limits(
        glass_root,
        signature_path,
        trust_store,
        &VerificationLimits::default(),
    )
}

pub fn open_verified_glass_directory_with_limits(
    glass_root: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
    limits: &VerificationLimits,
) -> Result<VerifiedDirectory, SignatureError> {
    let glass_root = glass_root.as_ref();
    let report = verify_glass_dir_with_limits(glass_root, signature_path, trust_store, limits)?;
    Ok(VerifiedDirectory {
        report,
        root: glass_root.to_path_buf(),
        limits: limits.clone(),
    })
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
    reader: R,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
    limits: &VerificationLimits,
) -> Result<VerificationReport, SignatureError> {
    verify_glass_archive_reader_with_artifacts(reader, signature_path, trust_store, limits)
        .map(|(report, _)| report)
}

fn verify_glass_archive_reader_with_artifacts<R: Read + Seek>(
    mut reader: R,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
    limits: &VerificationLimits,
) -> Result<(VerificationReport, BTreeMap<String, VerifiedArtifactRange>), SignatureError> {
    let signature_path = normalize_relative_path(signature_path.as_ref())?;
    preflight_zip_entry_count(&mut reader, limits)?;
    let archive_len = reader.seek(SeekFrom::End(0)).map_err(zip_io_error)?;
    reader.seek(SeekFrom::Start(0)).map_err(zip_io_error)?;
    let mut archive = ZipArchive::new(reader)?;
    let signature_name = path_to_string(&signature_path);
    let ReadZipFiles { files, layouts } = read_zip_files(&mut archive, limits, &signature_name)?;
    let envelope_bytes = files
        .get(&path_to_string(&signature_path))
        .ok_or_else(|| SignatureError::MissingSubject(path_to_string(&signature_path)))?;
    let envelope: DsseEnvelope = serde_json::from_slice(envelope_bytes)?;
    let file_set = ArchiveFileSet {
        files,
        signature_path: signature_name,
        limits: limits.clone(),
    };
    let report = verify_envelope_with_files(&file_set, &envelope, trust_store)?;
    let recipient_envelope_limit = limits
        .max_signature_bytes
        .min(limits.max_member_bytes)
        .min(MAX_SIGNATURE_ENVELOPE_BYTES);
    if report.schema_version == rootglass_manifest::MULTI_ARTIFACT_SCHEMA_VERSION
        && report
            .artifact_catalog
            .as_ref()
            .is_some_and(|catalog| catalog.encryption.is_some())
        && layouts.iter().any(|layout| {
            layout.path == RECIPIENT_ENVELOPE_PATH
                && !layout.is_dir
                && layout.size > recipient_envelope_limit
        })
    {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: recipient_envelope_limit,
        });
    }
    let mut reader = archive.into_inner();
    let ranges = verified_encrypted_artifact_ranges(
        &mut reader,
        archive_len,
        report.artifact_catalog.as_ref(),
        &layouts,
    )?;
    Ok((report, ranges))
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
    let (signed_paths, artifact_catalog) = verify_statement(file_set, &statement, trusted)?;
    let manifest_sha256 = statement.subject.iter().find_map(|subject| {
        let normalized = normalize_relative_path(Path::new(&subject.name)).ok()?;
        (path_to_string(&normalized) == rootglass_manifest::MANIFEST_FILE_NAME)
            .then(|| subject.digest.get("sha256").cloned())
            .flatten()
    });

    let glass_identifier = statement.predicate.glass_identifier;
    let glass_version = statement.predicate.glass_version;
    let schema_version = statement.predicate.schema_version;
    let verified_context = VerifiedContext {
        schema_version,
        glass_identifier: glass_identifier.clone(),
        glass_version: glass_version.clone(),
        manifest_sha256: manifest_sha256.clone(),
        artifact_catalog: artifact_catalog.clone(),
    };
    Ok(VerificationReport {
        publisher_name: statement.predicate.publisher.name,
        publisher_identity: statement.predicate.publisher.identity,
        key_id: dsse_signature.keyid.clone(),
        glass_identifier,
        glass_version,
        manifest_sha256,
        subject_count: statement.subject.len(),
        schema_version,
        artifact_catalog,
        signed_paths,
        verified_context,
    })
}

fn verify_statement(
    file_set: &dyn FileSet,
    statement: &Statement,
    trusted: &TrustedPublisher,
) -> Result<(Vec<String>, Option<rootglass_manifest::ArtifactCatalog>), SignatureError> {
    if statement.statement_type != STATEMENT_TYPE {
        return Err(SignatureError::UnsupportedStatementType(
            statement.statement_type.clone(),
        ));
    }

    if !rootglass_manifest::is_supported_schema_version(statement.predicate.schema_version) {
        return Err(SignatureError::InvalidStatement(format!(
            "unsupported predicate schema version {}",
            statement.predicate.schema_version
        )));
    }
    let expected_predicate_type = predicate_type_for_schema(statement.predicate.schema_version)
        .expect("supported schema versions have predicate types");
    if statement.predicate_type != expected_predicate_type {
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
    if statement.predicate.publisher.key_id != trusted.key_id {
        return Err(SignatureError::InvalidStatement(format!(
            "publisher key id '{}' does not match trusted key '{}'",
            statement.predicate.publisher.key_id, trusted.key_id
        )));
    }
    let mut expected_subjects = BTreeSet::new();
    let mut signed_subjects = Vec::with_capacity(statement.subject.len());
    let mut signed_manifest = None;

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
        let is_schema2_recipient_sidecar = statement.predicate.schema_version
            == rootglass_manifest::MULTI_ARTIFACT_SCHEMA_VERSION
            && file == RECIPIENT_ENVELOPE_PATH;
        if !is_schema2_recipient_sidecar && !expected_subjects.contains(file) {
            return Err(SignatureError::UnsignedFile(file.clone()));
        }
    }

    let mut verified_subjects = BTreeMap::new();
    for (subject, (normalized, expected)) in statement.subject.iter().zip(&signed_subjects) {
        let bytes = file_set.read(normalized)?;
        let actual = hex_sha256(&bytes);
        if actual.as_str() != expected.as_str() {
            return Err(SignatureError::DigestMismatch {
                path: subject.name.clone(),
                expected: expected.clone(),
                actual,
            });
        }
        if normalized == rootglass_manifest::MANIFEST_FILE_NAME {
            signed_manifest = Some(bytes.clone());
        }
        verified_subjects.insert(normalized.as_str(), (expected.as_str(), bytes.len() as u64));
    }

    let artifact_catalog = if statement.predicate.schema_version
        == rootglass_manifest::MULTI_ARTIFACT_SCHEMA_VERSION
    {
        let manifest_bytes = signed_manifest.ok_or_else(|| {
            SignatureError::InvalidStatement(
                "schema-2 statement must sign rootglass.toml".to_string(),
            )
        })?;
        let manifest_text = std::str::from_utf8(&manifest_bytes)
            .map_err(|error| SignatureError::InvalidManifest(error.to_string()))?;
        let manifest = rootglass_manifest::Manifest::from_toml_str(manifest_text)
            .map_err(|error| SignatureError::InvalidManifest(error.to_string()))?;
        manifest
            .validate_manifest_only()
            .map_err(|error| SignatureError::InvalidManifest(error.to_string()))?;
        if manifest.schema_version != statement.predicate.schema_version
            || manifest.identifier != statement.predicate.glass_identifier
            || manifest.version != statement.predicate.glass_version
            || manifest.publisher.name != statement.predicate.publisher.name
            || manifest.publisher.identity != statement.predicate.publisher.identity
        {
            return Err(SignatureError::InvalidStatement(
                "schema-2 manifest identity does not match its signed predicate".to_string(),
            ));
        }
        let catalog = manifest.artifact_catalog.ok_or_else(|| {
            SignatureError::InvalidManifest(
                "schema-2 manifest is missing its artifact catalog".to_string(),
            )
        })?;
        if candidates
            .iter()
            .any(|path| path == RECIPIENT_ENVELOPE_PATH)
        {
            if catalog.encryption.is_none() {
                return Err(SignatureError::UnsignedFile(
                    RECIPIENT_ENVELOPE_PATH.to_string(),
                ));
            }
            if expected_subjects.contains(RECIPIENT_ENVELOPE_PATH) {
                return Err(SignatureError::InvalidStatement(
                    "recipient envelope must be excluded from publisher subjects".to_string(),
                ));
            }
        }
        for (artifact_id, artifact) in &catalog.artifacts {
            let normalized =
                path_to_string(&normalize_relative_path(Path::new(&artifact.storage.path))?);
            let Some((signed_digest, signed_size)) = verified_subjects.get(normalized.as_str())
            else {
                return Err(SignatureError::ArtifactInventoryMismatch {
                    artifact_id: artifact_id.clone(),
                    reason: format!(
                        "storage path '{}' is not a signed subject",
                        artifact.storage.path
                    ),
                });
            };
            if signed_digest != &artifact.sha256 {
                return Err(SignatureError::ArtifactInventoryMismatch {
                    artifact_id: artifact_id.clone(),
                    reason: format!(
                        "catalog digest {} does not match signed subject digest {signed_digest}",
                        artifact.sha256
                    ),
                });
            }
            if *signed_size != artifact.size_bytes {
                return Err(SignatureError::ArtifactInventoryMismatch {
                    artifact_id: artifact_id.clone(),
                    reason: format!(
                        "catalog size {} does not match signed subject size {signed_size}",
                        artifact.size_bytes
                    ),
                });
            }
        }
        Some(catalog)
    } else {
        None
    };

    Ok((
        signed_subjects.into_iter().map(|(path, _)| path).collect(),
        artifact_catalog,
    ))
}

fn predicate_type_for_schema(schema_version: u32) -> Option<&'static str> {
    match schema_version {
        rootglass_manifest::LEGACY_SCHEMA_VERSION => Some(PREDICATE_TYPE),
        rootglass_manifest::MULTI_ARTIFACT_SCHEMA_VERSION => Some(PREDICATE_TYPE_V2),
        _ => None,
    }
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

struct ReadZipFiles {
    files: BTreeMap<String, Vec<u8>>,
    layouts: Vec<ZipMemberLayout>,
}

fn read_zip_files<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    limits: &VerificationLimits,
    signature_path: &str,
) -> Result<ReadZipFiles, SignatureError> {
    if archive.len() > limits.max_entries {
        return Err(SignatureError::TooManyArchiveEntries {
            limit: limits.max_entries,
        });
    }
    let mut files = BTreeMap::new();
    let mut layouts = Vec::with_capacity(archive.len());
    let mut kinds = BTreeMap::<String, bool>::new();
    let mut total = 0u64;

    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        let normalized = normalize_archive_name(&name, entry.is_dir())?;
        let is_dir = entry.is_dir();
        let metadata = entry.get_metadata();
        layouts.push(ZipMemberLayout {
            path: normalized.clone(),
            is_dir,
            header_start: entry.header_start(),
            data_start: entry.data_start(),
            central_header_start: entry.central_header_start(),
            compressed_size: entry.compressed_size(),
            size: entry.size(),
            crc32: entry.crc32(),
            compression: entry.compression(),
            encrypted: entry.encrypted(),
            using_data_descriptor: metadata.using_data_descriptor,
            name_raw: entry.name_raw().to_vec(),
        });

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

    Ok(ReadZipFiles { files, layouts })
}

#[derive(Debug, Clone)]
struct ZipMemberLayout {
    path: String,
    is_dir: bool,
    header_start: u64,
    data_start: u64,
    central_header_start: u64,
    compressed_size: u64,
    size: u64,
    crc32: u32,
    compression: CompressionMethod,
    encrypted: bool,
    using_data_descriptor: bool,
    name_raw: Vec<u8>,
}

#[derive(Debug)]
struct ParsedZipMember {
    layout: ZipMemberLayout,
    data_end: u64,
    record_end: u64,
    central_record_end: u64,
    central_flags: u16,
    central_method: u16,
    central_crc32: u32,
    central_compressed_size: u32,
    central_size: u32,
    central_local_offset: u32,
    central_name: Vec<u8>,
    local_flags: u16,
    local_method: u16,
    local_crc32: u32,
    local_compressed_size: u32,
    local_size: u32,
    local_name: Vec<u8>,
    has_zip64: bool,
}

fn verified_encrypted_artifact_ranges<R: Read + Seek>(
    reader: &mut R,
    archive_len: u64,
    catalog: Option<&rootglass_manifest::ArtifactCatalog>,
    layouts: &[ZipMemberLayout],
) -> Result<BTreeMap<String, VerifiedArtifactRange>, SignatureError> {
    let Some(catalog) = catalog else {
        return Ok(BTreeMap::new());
    };
    let encrypted_artifacts = catalog
        .artifacts
        .iter()
        .filter(|(_, artifact)| artifact.encryption.is_some())
        .collect::<Vec<_>>();
    if encrypted_artifacts.is_empty() {
        return Ok(BTreeMap::new());
    }
    let diagnostic_id = encrypted_artifacts[0].0.clone();
    let fail = |reason: String| SignatureError::InvalidEncryptedArtifactLayout {
        artifact_id: diagnostic_id.clone(),
        reason,
    };

    let (uses_zip64, eocd_start) = archive_end_record_info(reader, archive_len).map_err(fail)?;
    if uses_zip64 {
        return Err(fail(
            "ZIP64 is not qualified for encrypted artifact ranges".to_string(),
        ));
    }

    let mut parsed = Vec::with_capacity(layouts.len());
    for layout in layouts {
        parsed.push(parse_zip_member_layout(reader, archive_len, layout).map_err(fail)?);
    }
    if parsed.iter().any(|member| member.has_zip64) {
        return Err(fail(
            "ZIP64 fields are not qualified in encrypted archives".to_string(),
        ));
    }
    validate_non_overlapping_zip_layouts(&parsed, archive_len, eocd_start).map_err(fail)?;

    let mut members_by_path = BTreeMap::new();
    for member in parsed {
        if members_by_path
            .insert(member.layout.path.clone(), member)
            .is_some()
        {
            return Err(fail(
                "multiple ZIP records map to one normalized path".to_string(),
            ));
        }
    }

    let mut ranges = BTreeMap::new();
    for (artifact_id, artifact) in encrypted_artifacts {
        if artifact.kind != rootglass_manifest::ArtifactKind::LinuxBootDisk {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "only Linux boot disks can use the encrypted range reader".to_string(),
            });
        }
        if artifact.storage.kind != rootglass_manifest::StorageKind::ZipEntry {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "encrypted disk storage must be a ZIP entry".to_string(),
            });
        }
        let expected_path =
            path_to_string(&normalize_relative_path(Path::new(&artifact.storage.path))?);
        let member = members_by_path.get(&expected_path).ok_or_else(|| {
            SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: format!("catalog storage path '{expected_path}' is absent from the ZIP"),
            }
        })?;
        if member.layout.is_dir {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "catalog storage path resolves to a ZIP directory".to_string(),
            });
        }
        if member.layout.compression != CompressionMethod::Stored
            || member.central_method != 0
            || member.local_method != 0
        {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "encrypted disk entry must use ZIP method STORED".to_string(),
            });
        }
        if member.layout.encrypted || member.central_flags & 1 != 0 || member.local_flags & 1 != 0 {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "ZIP-level encryption is not supported for encrypted disk entries"
                    .to_string(),
            });
        }
        if member.layout.using_data_descriptor
            || member.central_flags & (1 << 3) != 0
            || member.local_flags & (1 << 3) != 0
        {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "encrypted disk entry must not use a trailing data descriptor".to_string(),
            });
        }
        if member.has_zip64 {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "ZIP64 fields are not qualified for encrypted disk entries".to_string(),
            });
        }
        if member.central_name != artifact.storage.path.as_bytes()
            || member.local_name != artifact.storage.path.as_bytes()
        {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "local and central ZIP names must exactly match the signed storage path"
                    .to_string(),
            });
        }
        if member.local_flags != member.central_flags
            || member.local_method != member.central_method
            || member.local_crc32 != member.central_crc32
            || member.local_compressed_size != member.central_compressed_size
            || member.local_size != member.central_size
            || member.central_crc32 != member.layout.crc32
            || u64::from(member.central_compressed_size) != member.layout.compressed_size
            || u64::from(member.central_size) != member.layout.size
            || member.layout.size != artifact.size_bytes
            || member.layout.compressed_size != artifact.size_bytes
        {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "local header, central directory, and signed catalog metadata disagree"
                    .to_string(),
            });
        }
        if member.central_flags & !((1 << 11) as u16) != 0 {
            return Err(SignatureError::InvalidEncryptedArtifactLayout {
                artifact_id: artifact_id.clone(),
                reason: "encrypted disk entry uses unsupported ZIP general-purpose flags"
                    .to_string(),
            });
        }
        let length = member.data_end.saturating_sub(member.layout.data_start);
        ranges.insert(
            artifact_id.clone(),
            VerifiedArtifactRange {
                offset: member.layout.data_start,
                length,
            },
        );
    }
    Ok(ranges)
}

fn archive_end_record_info<R: Read + Seek>(
    reader: &mut R,
    archive_len: u64,
) -> Result<(bool, u64), String> {
    const EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
    const ZIP64_LOCATOR_SIGNATURE: &[u8; 4] = b"PK\x06\x07";
    const EOCD_MIN_BYTES: usize = 22;
    let tail_len = archive_len.min((EOCD_MIN_BYTES + u16::MAX as usize) as u64) as usize;
    if tail_len < EOCD_MIN_BYTES {
        return Err("archive is too short to contain a ZIP end record".to_string());
    }
    let tail_start = archive_len - tail_len as u64;
    let tail = read_zip_bytes(reader, archive_len, tail_start, tail_len)?;
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
        .ok_or_else(|| "archive is missing a valid ZIP end record".to_string())?;
    let eocd = &tail[eocd_offset..eocd_offset + EOCD_MIN_BYTES];
    let has_sentinel = u16::from_le_bytes([eocd[8], eocd[9]]) == u16::MAX
        || u16::from_le_bytes([eocd[10], eocd[11]]) == u16::MAX
        || u32::from_le_bytes(eocd[12..16].try_into().expect("fixed ZIP end record")) == u32::MAX
        || u32::from_le_bytes(eocd[16..20].try_into().expect("fixed ZIP end record")) == u32::MAX;
    let eocd_absolute = tail_start + eocd_offset as u64;
    let has_locator = eocd_absolute >= 20
        && read_zip_bytes(reader, archive_len, eocd_absolute - 20, 4)?.as_slice()
            == ZIP64_LOCATOR_SIGNATURE;
    if le_u16(eocd, 4) != 0 || le_u16(eocd, 6) != 0 {
        return Err("multi-disk ZIP archives are not supported for encrypted assets".to_string());
    }
    Ok((has_sentinel || has_locator, eocd_absolute))
}

fn parse_zip_member_layout<R: Read + Seek>(
    reader: &mut R,
    archive_len: u64,
    layout: &ZipMemberLayout,
) -> Result<ParsedZipMember, String> {
    const CENTRAL_SIGNATURE: &[u8; 4] = b"PK\x01\x02";
    const LOCAL_SIGNATURE: &[u8; 4] = b"PK\x03\x04";

    let central_fixed = read_zip_bytes(reader, archive_len, layout.central_header_start, 46)?;
    if central_fixed.get(..4) != Some(CENTRAL_SIGNATURE.as_slice()) {
        return Err(format!(
            "entry '{}' has an invalid central-directory header",
            layout.path
        ));
    }
    let central_flags = le_u16(&central_fixed, 8);
    let central_method = le_u16(&central_fixed, 10);
    let central_crc32 = le_u32(&central_fixed, 16);
    let central_compressed_size = le_u32(&central_fixed, 20);
    let central_size = le_u32(&central_fixed, 24);
    let central_name_len = le_u16(&central_fixed, 28) as usize;
    let central_extra_len = le_u16(&central_fixed, 30) as usize;
    let central_comment_len = le_u16(&central_fixed, 32) as usize;
    let central_disk = le_u16(&central_fixed, 34);
    let central_local_offset = le_u32(&central_fixed, 42);
    let central_variable_len = central_name_len
        .checked_add(central_extra_len)
        .and_then(|length| length.checked_add(central_comment_len))
        .ok_or_else(|| format!("entry '{}' central header length overflows", layout.path))?;
    let central_variable_start = layout
        .central_header_start
        .checked_add(46)
        .ok_or_else(|| format!("entry '{}' central header offset overflows", layout.path))?;
    let central_variable = read_zip_bytes(
        reader,
        archive_len,
        central_variable_start,
        central_variable_len,
    )?;
    let central_name = central_variable[..central_name_len].to_vec();
    let central_extra = &central_variable[central_name_len..central_name_len + central_extra_len];
    let central_extra_zip64 = has_zip64_extra(central_extra)?;
    let central_record_end = central_variable_start
        .checked_add(central_variable_len as u64)
        .ok_or_else(|| format!("entry '{}' central record end overflows", layout.path))?;

    let local_fixed = read_zip_bytes(reader, archive_len, layout.header_start, 30)?;
    if local_fixed.get(..4) != Some(LOCAL_SIGNATURE.as_slice()) {
        return Err(format!(
            "entry '{}' has an invalid local-file header",
            layout.path
        ));
    }
    let local_flags = le_u16(&local_fixed, 6);
    let local_method = le_u16(&local_fixed, 8);
    let local_crc32 = le_u32(&local_fixed, 14);
    let local_compressed_size = le_u32(&local_fixed, 18);
    let local_size = le_u32(&local_fixed, 22);
    let local_name_len = le_u16(&local_fixed, 26) as usize;
    let local_extra_len = le_u16(&local_fixed, 28) as usize;
    let local_variable_len = local_name_len
        .checked_add(local_extra_len)
        .ok_or_else(|| format!("entry '{}' local header length overflows", layout.path))?;
    let local_variable_start = layout
        .header_start
        .checked_add(30)
        .ok_or_else(|| format!("entry '{}' local header offset overflows", layout.path))?;
    let local_variable = read_zip_bytes(
        reader,
        archive_len,
        local_variable_start,
        local_variable_len,
    )?;
    let local_name = local_variable[..local_name_len].to_vec();
    let local_extra = &local_variable[local_name_len..];
    let local_extra_zip64 = has_zip64_extra(local_extra)?;
    let calculated_data_start = local_variable_start
        .checked_add(local_variable_len as u64)
        .ok_or_else(|| format!("entry '{}' data offset overflows", layout.path))?;
    if calculated_data_start != layout.data_start {
        return Err(format!(
            "entry '{}' data offset disagrees with its local header",
            layout.path
        ));
    }
    if central_disk != 0 {
        return Err(format!(
            "entry '{}' uses a multi-disk ZIP layout",
            layout.path
        ));
    }
    if central_name != local_name || central_name != layout.name_raw {
        return Err(format!(
            "entry '{}' local and central names disagree",
            layout.path
        ));
    }
    if central_flags != local_flags || central_method != local_method {
        return Err(format!(
            "entry '{}' local and central flags or methods disagree",
            layout.path
        ));
    }
    let has_descriptor = central_flags & (1 << 3) != 0;
    if has_descriptor != layout.using_data_descriptor
        || (central_flags & 1 != 0) != layout.encrypted
    {
        return Err(format!(
            "entry '{}' ZIP flags disagree with the parsed entry",
            layout.path
        ));
    }
    if central_compressed_size != u32::MAX
        && u64::from(central_compressed_size) != layout.compressed_size
        || central_size != u32::MAX && u64::from(central_size) != layout.size
        || central_crc32 != layout.crc32
    {
        return Err(format!(
            "entry '{}' central metadata disagrees with the parsed entry",
            layout.path
        ));
    }
    if !has_descriptor
        && (local_crc32 != central_crc32
            || local_compressed_size != central_compressed_size
            || local_size != central_size)
    {
        return Err(format!(
            "entry '{}' local sizes or CRC disagree with the central directory",
            layout.path
        ));
    }
    if has_descriptor
        && ((local_crc32 != 0 && local_crc32 != central_crc32)
            || (local_compressed_size != 0 && local_compressed_size != central_compressed_size)
            || (local_size != 0 && local_size != central_size))
    {
        return Err(format!(
            "entry '{}' local descriptor placeholders disagree with central metadata",
            layout.path
        ));
    }

    let data_end = layout
        .data_start
        .checked_add(layout.compressed_size)
        .filter(|end| *end <= archive_len)
        .ok_or_else(|| {
            format!(
                "entry '{}' payload is truncated or out of bounds",
                layout.path
            )
        })?;
    let descriptor_len = if has_descriptor {
        verify_data_descriptor(
            reader,
            archive_len,
            data_end,
            central_crc32,
            central_compressed_size,
            central_size,
            &layout.path,
        )?
    } else {
        0
    };
    let record_end = data_end
        .checked_add(descriptor_len)
        .filter(|end| *end <= archive_len)
        .ok_or_else(|| {
            format!(
                "entry '{}' record is truncated or out of bounds",
                layout.path
            )
        })?;

    let has_zip64 = central_compressed_size == u32::MAX
        || central_size == u32::MAX
        || central_local_offset == u32::MAX
        || local_compressed_size == u32::MAX
        || local_size == u32::MAX
        || central_extra_zip64
        || local_extra_zip64;
    Ok(ParsedZipMember {
        layout: layout.clone(),
        data_end,
        record_end,
        central_record_end,
        central_flags,
        central_method,
        central_crc32,
        central_compressed_size,
        central_size,
        central_local_offset,
        central_name,
        local_flags,
        local_method,
        local_crc32,
        local_compressed_size,
        local_size,
        local_name,
        has_zip64,
    })
}

fn validate_non_overlapping_zip_layouts(
    members: &[ParsedZipMember],
    archive_len: u64,
    eocd_start: u64,
) -> Result<(), String> {
    let central_start = members
        .iter()
        .map(|member| member.layout.central_header_start)
        .min()
        .ok_or_else(|| "archive has no central-directory entries".to_string())?;
    if central_start > archive_len {
        return Err("central directory lies outside the archive".to_string());
    }

    let mut local_ranges = members
        .iter()
        .map(|member| {
            (
                member.layout.header_start,
                member.record_end,
                member.layout.path.as_str(),
            )
        })
        .collect::<Vec<_>>();
    local_ranges.sort_by_key(|range| range.0);
    for pair in local_ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(format!(
                "ZIP local records '{}' and '{}' overlap",
                pair[0].2, pair[1].2
            ));
        }
    }
    if local_ranges.iter().any(|(_, end, _)| *end > central_start) {
        return Err("ZIP local record overlaps the central directory".to_string());
    }

    let mut central_ranges = members
        .iter()
        .map(|member| {
            (
                member.layout.central_header_start,
                member.central_record_end,
                member.layout.path.as_str(),
            )
        })
        .collect::<Vec<_>>();
    central_ranges.sort_by_key(|range| range.0);
    if central_ranges.iter().any(|(_, end, _)| *end > eocd_start) {
        return Err("central directory overlaps the ZIP end record".to_string());
    }
    for pair in central_ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(format!(
                "ZIP central records '{}' and '{}' overlap",
                pair[0].2, pair[1].2
            ));
        }
    }

    let archive_base = members
        .iter()
        .map(|member| {
            member
                .layout
                .header_start
                .checked_sub(u64::from(member.central_local_offset))
                .ok_or_else(|| {
                    format!(
                        "entry '{}' has an invalid local-header offset",
                        member.layout.path
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if archive_base.iter().any(|base| *base != archive_base[0]) {
        return Err("central directory local-header offsets disagree".to_string());
    }
    Ok(())
}

fn verify_data_descriptor<R: Read + Seek>(
    reader: &mut R,
    archive_len: u64,
    offset: u64,
    crc32: u32,
    compressed_size: u32,
    size: u32,
    path: &str,
) -> Result<u64, String> {
    const DESCRIPTOR_SIGNATURE: u32 = 0x0807_4b50;
    let available = archive_len.saturating_sub(offset).min(16) as usize;
    if available < 12 {
        return Err(format!("entry '{path}' has a truncated data descriptor"));
    }
    let bytes = read_zip_bytes(reader, archive_len, offset, available)?;
    let direct_matches = le_u32(&bytes, 0) == crc32
        && le_u32(&bytes, 4) == compressed_size
        && le_u32(&bytes, 8) == size;
    let signed_matches = bytes.len() >= 16
        && le_u32(&bytes, 0) == DESCRIPTOR_SIGNATURE
        && le_u32(&bytes, 4) == crc32
        && le_u32(&bytes, 8) == compressed_size
        && le_u32(&bytes, 12) == size;
    match (direct_matches, signed_matches) {
        (true, false) => Ok(12),
        (false, true) => Ok(16),
        (true, true) => Err(format!("entry '{path}' has an ambiguous data descriptor")),
        (false, false) => Err(format!(
            "entry '{path}' data descriptor disagrees with central metadata"
        )),
    }
}

fn has_zip64_extra(extra: &[u8]) -> Result<bool, String> {
    let mut cursor = 0usize;
    let mut found = false;
    while cursor < extra.len() {
        let header_end = cursor
            .checked_add(4)
            .filter(|end| *end <= extra.len())
            .ok_or_else(|| "malformed ZIP extra field".to_string())?;
        let identifier = u16::from_le_bytes([extra[cursor], extra[cursor + 1]]);
        let length = u16::from_le_bytes([extra[cursor + 2], extra[cursor + 3]]) as usize;
        cursor = header_end
            .checked_add(length)
            .filter(|end| *end <= extra.len())
            .ok_or_else(|| "malformed ZIP extra field length".to_string())?;
        found |= identifier == 0x0001;
    }
    Ok(found)
}

fn read_zip_bytes<R: Read + Seek>(
    reader: &mut R,
    archive_len: u64,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>, String> {
    let end = offset
        .checked_add(length as u64)
        .filter(|end| *end <= archive_len)
        .ok_or_else(|| "ZIP structure is truncated or out of bounds".to_string())?;
    let mut bytes = vec![0; length];
    reader
        .seek(SeekFrom::Start(offset))
        .and_then(|_| reader.read_exact(&mut bytes))
        .map_err(|error| format!("could not read ZIP structure: {error}"))?;
    debug_assert!(end <= archive_len);
    Ok(bytes)
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
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

fn hash_open_file(file: &mut File, expected_size: u64) -> io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file size overflow"))?;
        if count > expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file grew after signature verification",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if count != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file size changed after signature verification",
        ));
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipient::canonical_json;
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
        assert_eq!(report.schema_version, 1);
        assert!(report.artifact_catalog.is_none());
        assert_eq!(
            report.signed_paths,
            ["README.md", "metadata/README.md", "rootglass.toml"]
        );
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
        assert_eq!(report.schema_version, 1);
        assert!(report.artifact_catalog.is_none());
        assert_eq!(
            report.signed_paths,
            ["README.md", "metadata/README.md", "rootglass.toml"]
        );
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
    fn signs_and_verifies_schema2_catalog_and_rejects_payload_tampering() {
        let (root, archive_path) = temp_schema2_bundle(false);
        let key = SigningKey::from_bytes(&[42; 32]);
        let trust = trust_store_for(&key);
        let timestamp = "2026-11-15T00:00:00Z";

        sign_glass_directory_at(&root, &key, timestamp).expect("schema 2 signs");
        let report = verify_glass_dir(&root, "signatures/glass.dsse.json", &trust)
            .expect("schema 2 verifies");
        assert_eq!(report.schema_version, 2);
        assert_eq!(report.subject_count, 3);
        assert_eq!(
            report
                .artifact_catalog
                .as_ref()
                .expect("verified catalog is returned")
                .artifacts
                .len(),
            2
        );

        package_glass_directory_at(&root, &archive_path, &key, timestamp)
            .expect("schema 2 ZIP package verifies before publishing");
        let archive_report =
            verify_glass_archive(&archive_path, "signatures/glass.dsse.json", &trust)
                .expect("schema 2 ZIP verifies");
        assert_eq!(archive_report.schema_version, 2);
        assert_eq!(archive_report.artifact_catalog.unwrap().artifacts.len(), 2);

        fs::write(root.join("images/root.raw"), b"altered").expect("tamper with signed artifact");
        let error = verify_glass_dir(&root, "signatures/glass.dsse.json", &trust)
            .expect_err("tampered schema-2 artifact is rejected");
        assert!(matches!(error, SignatureError::DigestMismatch { .. }));

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
    }

    #[test]
    fn encrypted_archive_packages_stored_ranges_and_returns_bounded_reader() {
        let (root, archive_path, key, trust, _authority_key) = temp_encrypted_schema2_bundle();
        package_glass_directory_at(&root, &archive_path, &key, "2026-11-15T00:00:00Z")
            .expect("encrypted disk is packaged as a stored ZIP member");

        let verified =
            open_verified_glass_archive(&archive_path, "signatures/glass.dsse.json", &trust)
                .expect("encrypted archive verifies and exposes its checked reader");
        let mut artifact = verified
            .open_encrypted_artifact("linux-root")
            .expect("logical artifact ID selects the encrypted disk");
        let mut bytes = Vec::new();
        artifact.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"linux-rootfs");
        assert_eq!(artifact.len(), bytes.len() as u64);
        assert_eq!(artifact.seek(SeekFrom::End(0)).unwrap(), bytes.len() as u64);
        assert!(artifact.seek(SeekFrom::Current(1)).is_err());
        assert!(matches!(
            verified.open_encrypted_artifact("controller-fw"),
            Err(SignatureError::ArtifactNotEncrypted(_))
        ));

        let directory = open_verified_glass_directory(&root, "signatures/glass.dsse.json", &trust)
            .expect("signed development directory verifies separately");
        let mut directory_artifact = directory
            .open_encrypted_artifact("linux-root")
            .expect("development directory provides a signed file reader");
        let mut directory_bytes = Vec::new();
        directory_artifact
            .read_to_end(&mut directory_bytes)
            .unwrap();
        assert_eq!(directory_bytes, bytes);

        fs::write(root.join("images/root.raw"), b"bad-rootfs!!")
            .expect("tamper with development asset");
        assert!(matches!(
            directory.open_encrypted_artifact("linux-root"),
            Err(SignatureError::DigestMismatch { .. })
        ));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
    }

    #[test]
    fn encrypted_archive_rejects_deflate_descriptors_and_inconsistent_headers() {
        let (root, archive_path, key, trust, _authority_key) = temp_encrypted_schema2_bundle();
        let deflated = build_encrypted_archive(&root, CompressionMethod::Deflated, false);
        assert!(matches!(
            verify_glass_archive_bytes(&deflated, "signatures/glass.dsse.json", &trust),
            Err(SignatureError::InvalidEncryptedArtifactLayout { .. })
        ));

        let descriptor = build_encrypted_archive(&root, CompressionMethod::Stored, true);
        assert!(matches!(
            verify_glass_archive_bytes(&descriptor, "signatures/glass.dsse.json", &trust),
            Err(SignatureError::InvalidEncryptedArtifactLayout { .. })
        ));

        let mut mismatched = build_encrypted_archive(&root, CompressionMethod::Stored, false);
        let header = local_header_for_path(&mismatched, "images/root.raw");
        mismatched[header + 14..header + 18].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        assert!(matches!(
            verify_glass_archive_bytes(&mismatched, "signatures/glass.dsse.json", &trust),
            Err(SignatureError::InvalidEncryptedArtifactLayout { .. })
        ));

        fs::write(root.join("firmware/controller.bin"), b"linux-rootfs")
            .expect("make two signed members share bytes");
        let manifest_path = root.join("rootglass.toml");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        let manifest = manifest.replace(&hex_sha256(b"mcu-firmware"), &hex_sha256(b"linux-rootfs"));
        fs::write(manifest_path, manifest).unwrap();
        sign_glass_directory_at(&root, &key, "2026-11-15T00:00:00Z")
            .expect("resign the overlapping-layout fixture");
        let mut overlapping = build_encrypted_archive(&root, CompressionMethod::Stored, false);
        let root_header = local_header_for_path(&overlapping, "images/root.raw") as u32;
        let firmware_header = central_header_for_path(&overlapping, "firmware/controller.bin");
        overlapping[firmware_header + 42..firmware_header + 46]
            .copy_from_slice(&root_header.to_le_bytes());
        assert!(matches!(
            verify_glass_archive_bytes(&overlapping, "signatures/glass.dsse.json", &trust),
            Err(SignatureError::InvalidEncryptedArtifactLayout { .. })
        ));

        let valid = build_encrypted_archive(&root, CompressionMethod::Stored, false);
        assert!(verify_glass_archive_bytes(
            &valid[..valid.len() - 10],
            "signatures/glass.dsse.json",
            &trust
        )
        .is_err());

        let mut out_of_range = valid;
        let central = central_header_for_path(&out_of_range, "images/root.raw");
        out_of_range[central + 42..central + 46].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(
            verify_glass_archive_bytes(out_of_range, "signatures/glass.dsse.json", &trust).is_err()
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
    }

    #[test]
    fn recipient_envelope_binds_the_verified_bundle_and_delegated_authority() {
        let (root, archive_path, publisher_key, trust, authority_key) =
            temp_encrypted_schema2_bundle();
        package_glass_directory_at(&root, &archive_path, &publisher_key, "2026-11-15T00:00:00Z")
            .expect("package encrypted bundle");
        let verified =
            open_verified_glass_archive(&archive_path, "signatures/glass.dsse.json", &trust)
                .expect("publisher and encrypted artifact set verify");
        let report = verified.report().clone();
        assert_eq!(report.glass_identifier, "io.rootglass.tests.schema2");
        assert_eq!(report.glass_version, "0.2.0");
        assert_eq!(report.manifest_sha256.as_deref().unwrap().len(), 64);
        assert_eq!(
            encrypted_artifact_set_digest(
                report.artifact_catalog.as_ref().expect("encrypted catalog")
            )
            .unwrap(),
            "fd3d2c8e008685180848bab6672f501ab950116bb456a46531da700de1c440df"
        );
        let mut multiple_encrypted = report.artifact_catalog.as_ref().unwrap().clone();
        multiple_encrypted
            .artifacts
            .get_mut("controller-fw")
            .unwrap()
            .encryption = Some(rootglass_manifest::ArtifactEncryption {
            suite_id: rootglass_manifest::EncryptionSuite::QemuLuksV1,
        });
        assert_eq!(
            encrypted_artifact_set_digest(&multiple_encrypted).unwrap(),
            "a944ba1ea57e9488b3d20c6283660d676feaf0791f07db368595f37a9ae0520b"
        );

        let recipient = test_age_recipient(b"canonical age public key", b"wrapped image key");
        let bytes = sign_recipient_envelope(
            &report,
            vec![recipient.clone()],
            &authority_key,
            &VerificationLimits::default(),
        )
        .expect("delegated authority can sign recipient envelope");
        let verified_recipients =
            verify_recipient_envelope_bytes(&bytes, &report, &VerificationLimits::default())
                .expect("recipient envelope verifies");
        assert_eq!(
            verified_recipients.minimum_recipient_hardware(),
            &rootglass_manifest::MinimumRecipientHardware::SoftwareAllowed
        );
        assert_eq!(
            verified_recipients.authority_key_id(),
            report
                .artifact_catalog
                .as_ref()
                .unwrap()
                .encryption
                .as_ref()
                .unwrap()
                .recipient_authorities[0]
                .key_id
        );
        assert_eq!(
            verified_recipients
                .recipient(&recipient.recipient_id)
                .unwrap()
                .wrapped_imk
                .as_slice(),
            b"wrapped image key".as_slice()
        );

        let mut display_mutation = report.clone();
        display_mutation.manifest_sha256 = Some("0".repeat(64));
        assert!(verify_recipient_envelope_bytes(
            &bytes,
            &display_mutation,
            &VerificationLimits::default()
        )
        .is_ok());
        let mut wrong_binding = report.clone();
        wrong_binding.verified_context.manifest_sha256 = Some("0".repeat(64));
        assert!(matches!(
            verify_recipient_envelope_bytes(&bytes, &wrong_binding, &VerificationLimits::default()),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        let decoded: DsseEnvelope = serde_json::from_slice(&bytes).unwrap();
        let payload_bytes = URL_SAFE_NO_PAD.decode(decoded.payload).unwrap();
        let mut wrong_set: RecipientEnvelopePayload =
            serde_json::from_slice(&payload_bytes).unwrap();
        wrong_set.encrypted_artifact_set_sha256 = "f".repeat(64);
        let wrong_set_bytes =
            signed_recipient_payload(&canonical_json(&wrong_set).unwrap(), &authority_key);
        assert!(matches!(
            verify_recipient_envelope_bytes(
                &wrong_set_bytes,
                &report,
                &VerificationLimits::default()
            ),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        let mut tampered = serde_json::from_slice::<DsseEnvelope>(&bytes).unwrap();
        tampered.payload = URL_SAFE_NO_PAD.encode(b"tampered");
        assert!(matches!(
            verify_recipient_envelope(&tampered, &report, &VerificationLimits::default()),
            Err(SignatureError::SignatureVerificationFailed)
        ));

        let unauthorized_key = SigningKey::from_bytes(&[77; 32]);
        let decoded: DsseEnvelope = serde_json::from_slice(&bytes).unwrap();
        let payload = URL_SAFE_NO_PAD.decode(decoded.payload).unwrap();
        let unauthorized = signed_recipient_payload(&payload, &unauthorized_key);
        assert!(matches!(
            verify_recipient_envelope_bytes(&unauthorized, &report, &VerificationLimits::default()),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        let duplicate_recipient = sign_recipient_envelope(
            &report,
            vec![recipient.clone(), recipient],
            &authority_key,
            &VerificationLimits::default(),
        );
        assert!(matches!(
            duplicate_recipient,
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));
        let excessive_recipients = sign_recipient_envelope(
            &report,
            vec![test_age_recipient(b"recipient key", b"wrapped key"); MAX_RECIPIENTS + 1],
            &authority_key,
            &VerificationLimits::default(),
        );
        assert!(matches!(
            excessive_recipients,
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));
        let unauthorized_signing_key = SigningKey::from_bytes(&[78; 32]);
        assert!(matches!(
            sign_recipient_envelope(
                &report,
                Vec::new(),
                &unauthorized_signing_key,
                &VerificationLimits::default()
            ),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        let small_limits = VerificationLimits {
            max_signature_bytes: 64,
            max_total_uncompressed_bytes: 64,
            max_member_bytes: 64,
            ..VerificationLimits::default()
        };
        assert!(matches!(
            verify_recipient_envelope_bytes(&bytes, &report, &small_limits),
            Err(SignatureError::SignatureEnvelopeTooLarge { .. })
        ));
        let mut oversized_object: DsseEnvelope = serde_json::from_slice(&bytes).unwrap();
        oversized_object.payload = "A".repeat(200);
        assert!(matches!(
            verify_recipient_envelope(&oversized_object, &report, &small_limits),
            Err(SignatureError::SignatureEnvelopeTooLarge { .. })
        ));
        assert!(matches!(
            verified.verify_recipient_envelope(),
            Err(SignatureError::MissingRecipientEnvelope)
        ));

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
    }

    #[test]
    fn verified_recipient_envelope_carries_publisher_hardware_policy() {
        let (root, archive_path, publisher_key, trust, authority_key) =
            temp_encrypted_schema2_bundle_with_policy("hardware-backed-required");
        package_glass_directory_at(&root, &archive_path, &publisher_key, "2026-11-15T00:00:00Z")
            .expect("package encrypted bundle");
        let verified =
            open_verified_glass_archive(&archive_path, "signatures/glass.dsse.json", &trust)
                .expect("verify signed encrypted bundle");
        let record = test_age_recipient(b"canonical age public key", b"wrapped image key");
        let envelope_bytes = sign_recipient_envelope(
            verified.report(),
            vec![record],
            &authority_key,
            &VerificationLimits::default(),
        )
        .expect("sign recipient envelope");

        let recipient_envelope = verify_recipient_envelope_bytes(
            &envelope_bytes,
            verified.report(),
            &VerificationLimits::default(),
        )
        .expect("verify recipient envelope");
        assert_eq!(
            recipient_envelope.minimum_recipient_hardware(),
            &rootglass_manifest::MinimumRecipientHardware::HardwareBackedRequired
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
    }

    #[test]
    fn recipient_envelope_rejects_noncanonical_json_unknown_fields_and_suites() {
        let (root, archive_path, publisher_key, trust, authority_key) =
            temp_encrypted_schema2_bundle();
        package_glass_directory_at(&root, &archive_path, &publisher_key, "2026-11-15T00:00:00Z")
            .expect("package encrypted bundle");
        let verified =
            open_verified_glass_archive(&archive_path, "signatures/glass.dsse.json", &trust)
                .expect("publisher signature verifies");
        let report = verified.report();
        let bytes = sign_recipient_envelope(
            report,
            vec![test_age_recipient(
                b"canonical age public key",
                b"wrapped image key",
            )],
            &authority_key,
            &VerificationLimits::default(),
        )
        .unwrap();
        let envelope: DsseEnvelope = serde_json::from_slice(&bytes).unwrap();
        let payload_bytes = URL_SAFE_NO_PAD.decode(envelope.payload).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&payload_bytes).unwrap();

        let mut unknown_provider = value.clone();
        unknown_provider["recipients"][0]["providerId"] = serde_json::json!("unknown-provider");
        let invalid = signed_recipient_payload(
            &serde_json::to_vec(&unknown_provider).unwrap(),
            &authority_key,
        );
        assert!(matches!(
            verify_recipient_envelope_bytes(&invalid, report, &VerificationLimits::default()),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        let mut unknown_suite = value.clone();
        unknown_suite["recipients"][0]["wrappingSuiteId"] = serde_json::json!("rsa-fallback");
        let invalid =
            signed_recipient_payload(&serde_json::to_vec(&unknown_suite).unwrap(), &authority_key);
        assert!(matches!(
            verify_recipient_envelope_bytes(&invalid, report, &VerificationLimits::default()),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        value["unrecognized"] = serde_json::json!(true);
        let invalid =
            signed_recipient_payload(&serde_json::to_vec(&value).unwrap(), &authority_key);
        assert!(matches!(
            verify_recipient_envelope_bytes(&invalid, report, &VerificationLimits::default()),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        let canonical = canonical_json(
            &serde_json::from_slice::<RecipientEnvelopePayload>(&payload_bytes).unwrap(),
        )
        .unwrap();
        let duplicate_field = [b"{\"formatVersion\":1,".as_slice(), &canonical[1..]].concat();
        let invalid = signed_recipient_payload(&duplicate_field, &authority_key);
        assert!(matches!(
            verify_recipient_envelope_bytes(&invalid, report, &VerificationLimits::default()),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        let mut whitespace_payload = canonical_json(
            &serde_json::from_slice::<RecipientEnvelopePayload>(&payload_bytes).unwrap(),
        )
        .unwrap();
        whitespace_payload.insert(0, b' ');
        let invalid = signed_recipient_payload(&whitespace_payload, &authority_key);
        assert!(matches!(
            verify_recipient_envelope_bytes(&invalid, report, &VerificationLimits::default()),
            Err(SignatureError::InvalidRecipientEnvelope(_))
        ));

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
    }

    #[test]
    fn recipient_personalization_preserves_ciphertext_and_publisher_signature() {
        let (root, archive_path, publisher_key, trust, authority_key) =
            temp_encrypted_schema2_bundle();
        package_glass_directory_at(&root, &archive_path, &publisher_key, "2026-11-15T00:00:00Z")
            .expect("package encrypted bundle");
        let verified =
            open_verified_glass_archive(&archive_path, "signatures/glass.dsse.json", &trust)
                .expect("publisher and payloads verify");
        let base_signature = read_archive_member(&archive_path, "signatures/glass.dsse.json");
        let base_disk = read_archive_member(&archive_path, "images/root.raw");

        let first_output = root.join("personalized-one.glass");
        let first_recipient =
            test_age_recipient(b"first canonical public key", b"first wrapped key");
        personalize_glass_archive_at(
            &verified,
            &first_output,
            vec![first_recipient.clone()],
            &authority_key,
        )
        .expect("add a recipient without changing the publisher signature");
        let first_verified =
            open_verified_glass_archive(&first_output, "signatures/glass.dsse.json", &trust)
                .expect("personalized archive retains publisher verification");
        assert_eq!(
            first_verified
                .verify_recipient_envelope()
                .unwrap()
                .recipient(&first_recipient.recipient_id)
                .unwrap()
                .wrapped_imk,
            first_recipient.wrapped_imk
        );

        let second_output = root.join("personalized-two.glass");
        let second_recipient =
            test_age_recipient(b"second canonical public key", b"second wrapped key");
        personalize_glass_archive_at(
            &first_verified,
            &second_output,
            vec![second_recipient.clone()],
            &authority_key,
        )
        .expect("replace an existing recipient set without republishing the bundle");
        let second_verified =
            open_verified_glass_archive(&second_output, "signatures/glass.dsse.json", &trust)
                .expect("replacement archive retains publisher verification");
        let verified_recipients = second_verified.verify_recipient_envelope().unwrap();
        assert!(verified_recipients
            .recipient(&first_recipient.recipient_id)
            .is_none());
        assert!(verified_recipients
            .recipient(&second_recipient.recipient_id)
            .is_some());

        for output in [&first_output, &second_output] {
            assert_eq!(
                read_archive_member(output, "signatures/glass.dsse.json"),
                base_signature
            );
            assert_eq!(read_archive_member(output, "images/root.raw"), base_disk);
            assert!(!read_archive_member(output, RECIPIENT_ENVELOPE_PATH).is_empty());
        }
        assert_ne!(
            read_archive_member(&first_output, RECIPIENT_ENVELOPE_PATH),
            read_archive_member(&second_output, RECIPIENT_ENVELOPE_PATH)
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
    }

    #[test]
    fn signing_rejects_catalog_metadata_that_does_not_match_payload() {
        let (root, archive_path) = temp_schema2_bundle(true);
        let key = SigningKey::from_bytes(&[43; 32]);
        let error = sign_glass_directory_at(&root, &key, "2026-11-15T00:00:00Z")
            .expect_err("wrong inventory digest cannot be signed");
        assert!(matches!(
            error,
            SignatureError::ArtifactInventoryMismatch { artifact_id, .. }
                if artifact_id == "linux-root"
        ));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
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
    fn encrypted_archive_rejects_oversized_recipient_sidecar() {
        let (root, archive_path, publisher_key, trust, _authority_key) =
            temp_encrypted_schema2_bundle();
        package_glass_directory_at(&root, &archive_path, &publisher_key, "2026-11-15T00:00:00Z")
            .expect("package encrypted bundle");
        let original = fs::read(&archive_path).unwrap();
        let mut source = ZipArchive::new(Cursor::new(original)).unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..source.len() {
            let mut entry = source.by_index(index).unwrap();
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            let options = SimpleFileOptions::default().compression_method(entry.compression());
            writer.start_file(name, options).unwrap();
            io::copy(&mut entry, &mut writer).unwrap();
        }
        writer
            .start_file(
                RECIPIENT_ENVELOPE_PATH,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer
            .write_all(&vec![b'x'; MAX_SIGNATURE_ENVELOPE_BYTES as usize + 1])
            .unwrap();
        let oversized_archive = writer.finish().unwrap().into_inner();
        assert!(matches!(
            verify_glass_archive_bytes(&oversized_archive, "signatures/glass.dsse.json", &trust),
            Err(SignatureError::SignatureEnvelopeTooLarge {
                limit: MAX_SIGNATURE_ENVELOPE_BYTES
            })
        ));

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(archive_path);
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

    fn temp_schema2_bundle(wrong_digest: bool) -> (PathBuf, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after unix epoch")
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("rootglass-schema2-{unique}"));
        let root = parent.join("bundle.glass");
        let archive = parent.join("bundle.zip");
        fs::create_dir_all(root.join("images")).expect("create image directory");
        fs::create_dir_all(root.join("firmware")).expect("create firmware directory");
        fs::create_dir_all(root.join("signatures")).expect("create signature directory");
        let rootfs = b"linux-rootfs";
        let firmware = b"mcu-firmware";
        fs::write(root.join("images/root.raw"), rootfs).expect("write rootfs");
        fs::write(root.join("firmware/controller.bin"), firmware).expect("write firmware");
        let root_digest = if wrong_digest {
            "0".repeat(64)
        } else {
            hex_sha256(rootfs)
        };
        let text = format!(
            r#"schema_version = 2
name = "Schema 2 Test"
identifier = "io.rootglass.tests.schema2"
version = "0.2.0"

[publisher]
name = "Schema 2 Test Publisher"
identity = "rootglass.test/schema2"
signature = "signatures/glass.dsse.json"

[artifact_catalog]
catalog_version = 1
default_profile = "default"

[artifact_catalog.artifacts.linux-root]
kind = "linux-boot-disk"
format = "raw"
size_bytes = {}
sha256 = "{}"
target_architecture = "aarch64"
target_machine = "qemu:virt"

[artifact_catalog.artifacts.linux-root.storage]
kind = "zip-entry"
path = "images/root.raw"

[artifact_catalog.artifacts.controller-fw]
kind = "mcu-firmware"
format = "binary"
size_bytes = {}
sha256 = "{}"
target_architecture = "arm"
target_machine = "qemu:mps2-an385"

[artifact_catalog.artifacts.controller-fw.storage]
kind = "zip-entry"
path = "firmware/controller.bin"

[artifact_catalog.profiles.default]
required_features = ["launch-profiles-v1", "linux-node-v1", "mcu-node-v1", "typed-devices-v1"]

[artifact_catalog.profiles.default.resources]
memory_mb = 1024
cpus = 2

[artifact_catalog.profiles.default.network]
mode = "none"

[[artifact_catalog.profiles.default.nodes]]
id = "linux"
startup_order = 0
process_group = "linux"
architecture = "aarch64"
machine = "qemu:virt"
boot = {{ kind = "linux", kernel = "builtin:aarch64-linux", device_tree = "builtin:aarch64-virt", rootfs_artifact = "linux-root", kernel_args = [] }}

[[artifact_catalog.profiles.default.nodes]]
id = "mcu"
startup_order = 1
process_group = "mcu"
architecture = "arm"
machine = "qemu:mps2-an385"
boot = {{ kind = "mcu", firmware_artifact = "controller-fw" }}

[[artifact_catalog.profiles.default.devices]]
kind = "virtio-blk-mmio"
id = "root-disk"
node_id = "linux"
artifact_id = "linux-root"
read_only = true

[[artifact_catalog.profiles.default.devices]]
kind = "pl011-uart"
id = "linux-console"
node_id = "linux"

[[artifact_catalog.profiles.default.devices]]
kind = "mps2-uart"
id = "mcu-console"
node_id = "mcu"
"#,
            rootfs.len(),
            root_digest,
            firmware.len(),
            hex_sha256(firmware)
        );
        fs::write(root.join("rootglass.toml"), text).expect("write schema 2 manifest");
        (root, archive)
    }

    fn temp_encrypted_schema2_bundle() -> (PathBuf, PathBuf, SigningKey, TrustStore, SigningKey) {
        temp_encrypted_schema2_bundle_with_policy("software-allowed")
    }

    fn temp_encrypted_schema2_bundle_with_policy(
        minimum_hardware: &str,
    ) -> (PathBuf, PathBuf, SigningKey, TrustStore, SigningKey) {
        let (root, archive) = temp_schema2_bundle(false);
        let authority_key = SigningKey::from_bytes(&[19; 32]);
        let recipient = authority_key.verifying_key().to_bytes();
        let recipient_id = format!("ed25519:{}", hex_sha256(&recipient));
        let manifest_path = root.join("rootglass.toml");
        let manifest = fs::read_to_string(&manifest_path).expect("read schema-2 fixture");
        let manifest = manifest
            .replace(
                "[artifact_catalog.artifacts.linux-root.storage]",
                "[artifact_catalog.artifacts.linux-root.encryption]\nsuite_id = \"qemu-luks-v1\"\n\n[artifact_catalog.artifacts.linux-root.storage]",
            )
            .replace(
                "[artifact_catalog.profiles.default]",
                &format!(
                    "[artifact_catalog.encryption]\nminimum_recipient_hardware = \"{minimum_hardware}\"\nmax_concurrent_instances = 1\n\n[[artifact_catalog.encryption.recipient_authorities]]\nkey_id = \"{recipient_id}\"\npublic_key_base64url = \"{}\"\n\n[artifact_catalog.profiles.default]",
                    URL_SAFE_NO_PAD.encode(recipient)
                ),
            )
            .replace(
                "\"launch-profiles-v1\", \"linux-node-v1\", \"mcu-node-v1\", \"typed-devices-v1\"",
                "\"launch-profiles-v1\", \"linux-node-v1\", \"mcu-node-v1\", \"typed-devices-v1\", \"encrypted-artifacts-v1\"",
            );
        fs::write(manifest_path, manifest).expect("write encrypted schema-2 fixture");
        let key = SigningKey::from_bytes(&[42; 32]);
        sign_glass_directory_at(&root, &key, "2026-11-15T00:00:00Z")
            .expect("sign encrypted schema-2 fixture");
        let trust = trust_store_for(&key);
        (root, archive, key, trust, authority_key)
    }

    fn build_encrypted_archive(
        root: &Path,
        root_compression: CompressionMethod,
        with_descriptors: bool,
    ) -> Vec<u8> {
        let regular_order = [
            "firmware/controller.bin",
            "images/root.raw",
            "rootglass.toml",
            "signatures/glass.dsse.json",
        ];
        let descriptor_order = [
            "firmware/controller.bin",
            "rootglass.toml",
            "signatures/glass.dsse.json",
            "images/root.raw",
        ];
        let names = if with_descriptors {
            &descriptor_order
        } else {
            &regular_order
        };
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in names {
            let method = if *name == "images/root.raw" {
                root_compression
            } else {
                CompressionMethod::Stored
            };
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default().compression_method(method),
                )
                .expect("start fixture entry");
            writer
                .write_all(&fs::read(root.join(name)).expect("read fixture member"))
                .expect("write fixture entry");
        }
        let mut bytes = writer.finish().expect("finish archive").into_inner();
        if with_descriptors {
            let local = local_header_for_path(&bytes, "images/root.raw");
            let central = central_header_for_path(&bytes, "images/root.raw");
            let name_len = u16::from_le_bytes([bytes[local + 26], bytes[local + 27]]) as usize;
            let extra_len = u16::from_le_bytes([bytes[local + 28], bytes[local + 29]]) as usize;
            let data_start = local + 30 + name_len + extra_len;
            let compressed_size = u32::from_le_bytes([
                bytes[local + 18],
                bytes[local + 19],
                bytes[local + 20],
                bytes[local + 21],
            ]) as usize;
            let data_end = data_start + compressed_size;
            let crc32 = u32::from_le_bytes([
                bytes[central + 16],
                bytes[central + 17],
                bytes[central + 18],
                bytes[central + 19],
            ]);
            let size = u32::from_le_bytes([
                bytes[central + 24],
                bytes[central + 25],
                bytes[central + 26],
                bytes[central + 27],
            ]);
            let descriptor = [
                0x0807_4b50u32.to_le_bytes(),
                crc32.to_le_bytes(),
                (compressed_size as u32).to_le_bytes(),
                size.to_le_bytes(),
            ]
            .concat();
            bytes.splice(data_end..data_end, descriptor);
            let local = local_header_for_path(&bytes, "images/root.raw");
            let central = central_header_for_path(&bytes, "images/root.raw");
            let local_flags = u16::from_le_bytes([bytes[local + 6], bytes[local + 7]]) | (1 << 3);
            bytes[local + 6..local + 8].copy_from_slice(&local_flags.to_le_bytes());
            bytes[local + 14..local + 26].fill(0);
            let central_flags =
                u16::from_le_bytes([bytes[central + 8], bytes[central + 9]]) | (1 << 3);
            bytes[central + 8..central + 10].copy_from_slice(&central_flags.to_le_bytes());
            let eocd = bytes
                .windows(4)
                .rposition(|signature| signature == b"PK\x05\x06")
                .expect("find ZIP end record");
            let central_offset = u32::from_le_bytes([
                bytes[eocd + 16],
                bytes[eocd + 17],
                bytes[eocd + 18],
                bytes[eocd + 19],
            ]) + 16;
            bytes[eocd + 16..eocd + 20].copy_from_slice(&central_offset.to_le_bytes());
        }
        bytes
    }

    fn local_header_for_path(bytes: &[u8], name: &str) -> usize {
        bytes
            .windows(4)
            .enumerate()
            .find_map(|(offset, signature)| {
                if signature != b"PK\x03\x04" || offset + 30 > bytes.len() {
                    return None;
                }
                let name_len =
                    u16::from_le_bytes([bytes[offset + 26], bytes[offset + 27]]) as usize;
                let name_start = offset + 30;
                let name_end = name_start.checked_add(name_len)?;
                (name_end <= bytes.len() && &bytes[name_start..name_end] == name.as_bytes())
                    .then_some(offset)
            })
            .expect("find ZIP local header")
    }

    fn central_header_for_path(bytes: &[u8], name: &str) -> usize {
        bytes
            .windows(4)
            .enumerate()
            .find_map(|(offset, signature)| {
                if signature != b"PK\x01\x02" || offset + 46 > bytes.len() {
                    return None;
                }
                let name_len =
                    u16::from_le_bytes([bytes[offset + 28], bytes[offset + 29]]) as usize;
                let name_start = offset + 46;
                let name_end = name_start.checked_add(name_len)?;
                (name_end <= bytes.len() && &bytes[name_start..name_end] == name.as_bytes())
                    .then_some(offset)
            })
            .expect("find ZIP central header")
    }

    fn test_age_recipient(public_key: &[u8], wrapped_imk: &[u8]) -> RecipientRecord {
        RecipientRecord {
            recipient_id: recipient_id_for_public_key(
                RecipientWrappingSuite::AgeV1X25519,
                public_key,
            ),
            provider_id: RecipientProvider::AgeX25519,
            wrapping_suite_id: RecipientWrappingSuite::AgeV1X25519,
            wrapped_imk: wrapped_imk.to_vec(),
        }
    }

    fn signed_recipient_payload(payload: &[u8], key: &SigningKey) -> Vec<u8> {
        let signature = key.sign(&dsse_pae(
            RECIPIENT_ENVELOPE_PAYLOAD_TYPE.as_bytes(),
            payload,
        ));
        serde_json::to_vec(&DsseEnvelope {
            payload_type: RECIPIENT_ENVELOPE_PAYLOAD_TYPE.to_string(),
            payload: URL_SAFE_NO_PAD.encode(payload),
            signatures: vec![DsseSignature {
                keyid: format!("ed25519:{}", hex_sha256(&key.verifying_key().to_bytes())),
                sig: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
            }],
        })
        .unwrap()
    }

    fn read_archive_member(path: &Path, name: &str) -> Vec<u8> {
        let file = File::open(path).expect("open archive");
        let mut archive = ZipArchive::new(file).expect("parse archive");
        let mut entry = archive.by_name(name).expect("find archive member");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("read archive member");
        bytes
    }

    fn trust_store_for(key: &SigningKey) -> TrustStore {
        let public_key = key.verifying_key().to_bytes();
        let key_id = format!("ed25519:{}", hex_sha256(&public_key));
        TrustStore::new([TrustedPublisher {
            name: "Schema 2 Test Publisher".to_string(),
            identity: "rootglass.test/schema2".to_string(),
            key_id,
            public_key_base64url: URL_SAFE_NO_PAD.encode(public_key),
        }])
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
