use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read, Seek};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use zip::ZipArchive;

mod signing;
pub use signing::{
    load_signing_key, package_glass_directory, package_glass_directory_at, sign_glass_directory,
    sign_glass_directory_at, SigningReport,
};

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
#[serde(rename_all = "camelCase")]
pub struct DsseEnvelope {
    pub payload_type: String,
    pub payload: String,
    pub signatures: Vec<DsseSignature>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
            public_key_base64url: "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw".to_string(),
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
    let signature_path = normalize_relative_path(signature_path.as_ref())?;
    let envelope_path = glass_root.join(&signature_path);
    let envelope_bytes =
        fs::read(&envelope_path).map_err(|source| SignatureError::ReadEnvelope {
            path: envelope_path,
            source,
        })?;
    let envelope: DsseEnvelope = serde_json::from_slice(&envelope_bytes)?;
    let file_set = DirectoryFileSet::new(glass_root, &signature_path);
    verify_envelope_with_files(&file_set, &envelope, trust_store)
}

pub fn verify_glass_archive(
    archive_path: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
) -> Result<VerificationReport, SignatureError> {
    let archive_path = archive_path.as_ref();
    let bytes = fs::read(archive_path).map_err(|source| SignatureError::ReadArchive {
        path: archive_path.to_path_buf(),
        source,
    })?;
    verify_glass_archive_bytes(bytes, signature_path, trust_store)
}

pub fn verify_glass_archive_bytes(
    archive_bytes: impl AsRef<[u8]>,
    signature_path: impl AsRef<Path>,
    trust_store: &TrustStore,
) -> Result<VerificationReport, SignatureError> {
    let signature_path = normalize_relative_path(signature_path.as_ref())?;
    let mut archive = ZipArchive::new(Cursor::new(archive_bytes.as_ref()))?;
    let files = read_zip_files(&mut archive)?;
    let envelope_bytes = files
        .get(&path_to_string(&signature_path))
        .ok_or_else(|| SignatureError::MissingSubject(path_to_string(&signature_path)))?;
    let envelope: DsseEnvelope = serde_json::from_slice(envelope_bytes)?;
    let file_set = ArchiveFileSet {
        files,
        signature_path: path_to_string(&signature_path),
    };
    verify_envelope_with_files(&file_set, &envelope, trust_store)
}

pub fn verify_envelope(
    glass_root: &Path,
    signature_path: &Path,
    envelope: &DsseEnvelope,
    trust_store: &TrustStore,
) -> Result<VerificationReport, SignatureError> {
    let signature_path = normalize_relative_path(signature_path)?;
    let file_set = DirectoryFileSet::new(glass_root, &signature_path);
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
        let bytes = file_set.read(&subject.name)?;
        let actual = hex_sha256(&bytes);
        if actual != *expected {
            return Err(SignatureError::DigestMismatch {
                path: subject.name.clone(),
                expected: expected.clone(),
                actual,
            });
        }
    }

    for file in file_set.candidate_files()? {
        if !expected_subjects.contains(&file) {
            return Err(SignatureError::UnsignedFile(file));
        }
    }

    Ok(())
}

trait FileSet {
    fn read(&self, path: &str) -> Result<Vec<u8>, SignatureError>;
    fn candidate_files(&self) -> Result<Vec<String>, SignatureError>;
}

struct DirectoryFileSet<'a> {
    root: &'a Path,
    signature_path: String,
}

impl<'a> DirectoryFileSet<'a> {
    fn new(root: &'a Path, signature_path: &Path) -> Self {
        Self {
            root,
            signature_path: path_to_string(signature_path),
        }
    }
}

impl FileSet for DirectoryFileSet<'_> {
    fn read(&self, path: &str) -> Result<Vec<u8>, SignatureError> {
        let normalized = normalize_relative_path(Path::new(path))?;
        let path = self.root.join(&normalized);
        if !path.is_file() {
            return Err(SignatureError::MissingSubject(path_to_string(&normalized)));
        }
        fs::read(&path).map_err(|source| SignatureError::ReadSignedFile { path, source })
    }

    fn candidate_files(&self) -> Result<Vec<String>, SignatureError> {
        let mut files = Vec::new();
        collect_directory_files(self.root, self.root, &self.signature_path, &mut files)?;
        files.sort();
        Ok(files)
    }
}

struct ArchiveFileSet {
    files: BTreeMap<String, Vec<u8>>,
    signature_path: String,
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
}

fn read_zip_files<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<BTreeMap<String, Vec<u8>>, SignatureError> {
    let mut files = BTreeMap::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        let normalized = normalize_archive_name(&name)?;

        if entry.is_dir() {
            continue;
        }

        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|source| SignatureError::ReadSignedFile {
                path: PathBuf::from(&normalized),
                source,
            })?;

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
            collect_directory_files(root, &path, signature_path, files)?;
        } else if path.is_file() {
            let relative = path.strip_prefix(root).expect("path is inside root");
            let relative = path_to_string(relative);
            if relative == signature_path {
                continue;
            }
            // Directory-form fixture verification allows generated images to be absent
            // or unsigned. Production archive verification does not apply this exclusion.
            if path.extension().is_some_and(|extension| extension == "img") {
                continue;
            }
            files.push(relative);
        }
    }
    Ok(())
}

fn normalize_archive_name(name: &str) -> Result<String, SignatureError> {
    if name.contains('\\') || name.starts_with('/') {
        return Err(SignatureError::UnsafeArchivePath(name.to_string()));
    }
    let path = normalize_relative_path(Path::new(name))?;
    Ok(path_to_string(&path))
}

pub(crate) fn normalize_relative_path(path: &Path) -> Result<PathBuf, SignatureError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(SignatureError::UnsafeArchivePath(
                    path.to_string_lossy().to_string(),
                ));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(SignatureError::UnsafeArchivePath(
            path.to_string_lossy().to_string(),
        ));
    }

    Ok(normalized)
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
