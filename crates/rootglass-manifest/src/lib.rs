use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const MANIFEST_FILE_NAME: &str = "rootglass.toml";
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const SUPPORTED_SCHEMA_VERSION: u32 = 2;
pub const LEGACY_SCHEMA_VERSION: u32 = 1;
pub const MULTI_ARTIFACT_SCHEMA_VERSION: u32 = 2;
pub const CATALOG_VERSION: u32 = 1;
pub const MAX_CATALOG_ARTIFACTS: usize = 256;
pub const MAX_CATALOG_PROFILES: usize = 64;
pub const MAX_CATALOG_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_CATALOG_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_RECIPIENT_AUTHORITIES: usize = 64;
pub const MAX_CONCURRENT_ENCRYPTED_INSTANCES: u8 = 5;
pub const SUPPORTED_ARCHITECTURE: &str = "aarch64";
pub const SUPPORTED_RUNTIME_PROFILE: &str = "virt-linux";
pub const BUILTIN_AARCH64_KERNEL: &str = "builtin:aarch64-linux";
pub const BUILTIN_AARCH64_DEVICE_TREE: &str = "builtin:aarch64-virt";

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to read manifest at {path}: {source}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("manifest at {path} exceeds the {limit}-byte limit")]
    ManifestTooLarge { path: PathBuf, limit: u64 },

    #[error("failed to parse manifest at {path}: {source}")]
    ParseManifest {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("unsupported schema version {actual}; supported version is {supported}")]
    UnsupportedSchemaVersion { actual: u32, supported: u32 },

    #[error("unsupported guest architecture '{actual}'; supported architecture is '{supported}'")]
    UnsupportedArchitecture {
        actual: String,
        supported: &'static str,
    },

    #[error("unsupported runtime profile '{actual}'; supported profile is '{supported}'")]
    UnsupportedRuntimeProfile {
        actual: String,
        supported: &'static str,
    },

    #[error("MVP only supports builtin kernel '{expected}', got '{actual}'")]
    UnsupportedKernelReference {
        actual: String,
        expected: &'static str,
    },

    #[error("MVP only supports builtin device tree '{expected}', got '{actual}'")]
    UnsupportedDeviceTreeReference {
        actual: String,
        expected: &'static str,
    },

    #[error("required rootfs image is missing: {path}")]
    MissingRootfs { path: PathBuf },

    #[error("declared writable overlay is missing: {path}")]
    MissingWritableOverlay { path: PathBuf },

    #[error("publisher name is required")]
    MissingPublisherName,

    #[error("publisher identity is required")]
    MissingPublisherIdentity,

    #[error("signature path is required")]
    MissingSignature,

    #[error("invalid artifact catalog: {0}")]
    InvalidArtifactCatalog(String),

    #[error("schema version {actual} is understood for inspection but is not launchable")]
    UnsupportedLaunchSchema { actual: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: u32,
    pub name: String,
    pub identifier: String,
    pub version: String,
    pub description: Option<String>,
    pub publisher: Publisher,
    pub runtime: Runtime,
    pub boot: Boot,
    pub resources: Resources,
    pub display: Display,
    pub network: Network,
    /// Present only for schema 2. Schema-1 keeps its original fields and wire shape.
    pub artifact_catalog: Option<ArtifactCatalog>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Schema1Manifest {
    schema_version: u32,
    name: String,
    identifier: String,
    version: String,
    description: Option<String>,
    publisher: Publisher,
    runtime: Runtime,
    boot: Boot,
    resources: Resources,
    display: Display,
    network: Network,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Schema2Manifest {
    schema_version: u32,
    name: String,
    identifier: String,
    version: String,
    description: Option<String>,
    publisher: Publisher,
    artifact_catalog: ArtifactCatalog,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCatalog {
    pub catalog_version: u32,
    pub default_profile: String,
    pub artifacts: BTreeMap<String, Artifact>,
    pub profiles: BTreeMap<String, LaunchProfile>,
    /// Present only when one or more catalog artifacts are encrypted.
    #[serde(default)]
    pub encryption: Option<EncryptionPolicy>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum ArtifactKind {
    LinuxBootDisk,
    McuFirmware,
    BackendExecutable,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum ArtifactFormat {
    Raw,
    Binary,
    Executable,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub format: ArtifactFormat,
    pub size_bytes: u64,
    pub sha256: String,
    pub storage: ArtifactStorage,
    pub target_architecture: Option<TargetArchitecture>,
    pub target_machine: Option<TargetMachine>,
    pub host_os: Option<HostOs>,
    pub backend: Option<BackendMetadata>,
    /// Encryption is attached to the stable artifact ID, never its ZIP path.
    #[serde(default)]
    pub encryption: Option<ArtifactEncryption>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncryptionPolicy {
    pub minimum_recipient_hardware: MinimumRecipientHardware,
    pub max_concurrent_instances: u8,
    pub recipient_authorities: Vec<RecipientAuthority>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum MinimumRecipientHardware {
    SoftwareAllowed,
    HardwareBackedRequired,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecipientAuthority {
    /// Canonical `ed25519:<sha256>` ID of the raw Ed25519 public key.
    pub key_id: String,
    /// Canonical URL-safe base64 without padding of a 32-byte public key.
    pub public_key_base64url: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactEncryption {
    pub suite_id: EncryptionSuite,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum EncryptionSuite {
    QemuLuksV1,
}

impl EncryptionSuite {
    pub const fn id(&self) -> &'static str {
        match self {
            Self::QemuLuksV1 => "qemu-luks-v1",
        }
    }
}

/// The canonical data that E03 will bind into the recipient envelope.
/// Entries are ordered by UTF-8 artifact ID and contain no physical ZIP path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedArtifactSetMember {
    pub id: String,
    pub ciphertext_sha256: String,
    pub ciphertext_size: u64,
    pub suite_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactStorage {
    pub kind: StorageKind,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum StorageKind {
    ZipEntry,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum TargetArchitecture {
    Aarch64,
    Arm,
    #[serde(rename = "x86_64")]
    X86_64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum TargetMachine {
    #[serde(rename = "qemu:virt")]
    QemuVirt,
    #[serde(rename = "qemu:mps2-an385")]
    QemuMps2An385,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum HostOs {
    Linux,
    Macos,
    Windows,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackendMetadata {
    pub publisher_identity: String,
    pub capabilities: BTreeSet<BackendCapability>,
    pub ipc: BTreeSet<IpcTransport>,
    pub lifecycle: BackendLifecycle,
    pub isolation: BackendIsolation,
    #[serde(default)]
    pub qom_device_ids: BTreeSet<String>,
    #[serde(default)]
    pub virtio_backend_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum BackendCapability {
    QomDeviceV1,
    VirtioBackendV1,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum IpcTransport {
    UnixDomainSocket,
    NamedPipe,
    Stdio,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum BackendLifecycle {
    SpawnPerRun,
    LongRunning,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum BackendIsolation {
    SeparateProcess,
    PlatformSandbox,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchProfile {
    pub required_features: BTreeSet<RequiredFeature>,
    pub resources: Resources,
    pub network: Network,
    pub nodes: Vec<LaunchNode>,
    pub devices: Vec<TypedDevice>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum RequiredFeature {
    LaunchProfilesV1,
    LinuxNodeV1,
    McuNodeV1,
    EncryptedArtifactsV1,
    TypedDevicesV1,
    BackendAssetsV1,
    BackendProcessIsolationV1,
    ExternalQomBackendV1,
    ExternalVirtioBackendV1,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchNode {
    pub id: String,
    pub startup_order: u16,
    pub process_group: String,
    pub architecture: TargetArchitecture,
    pub machine: TargetMachine,
    pub boot: BootData,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BootData {
    Linux {
        kernel: String,
        device_tree: String,
        rootfs_artifact: String,
        #[serde(default)]
        kernel_args: Vec<String>,
    },
    Mcu {
        firmware_artifact: String,
    },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TypedDevice {
    VirtioBlkMmio {
        id: String,
        node_id: String,
        artifact_id: String,
        read_only: bool,
    },
    Pl011Uart {
        id: String,
        node_id: String,
    },
    Mps2Uart {
        id: String,
        node_id: String,
    },
    ExternalVirtioBackend {
        id: String,
        node_id: String,
        backend_artifact_id: String,
        backend_id: String,
    },
    ExternalQomBackend {
        id: String,
        node_id: String,
        backend_artifact_id: String,
        qom_device_id: String,
    },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Publisher {
    pub name: String,
    pub identity: String,
    pub signature: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub architecture: String,
    pub profile: String,
    #[serde(default)]
    pub requires_hardware_acceleration: bool,
    #[serde(default)]
    pub host_constraints: HostConstraints,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostConstraints {
    pub macos: Option<MacOsConstraints>,
    pub windows: Option<WindowsConstraints>,
    pub linux: Option<LinuxConstraints>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MacOsConstraints {
    #[serde(default)]
    pub hypervisor: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WindowsConstraints {
    #[serde(default)]
    pub whpx: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LinuxConstraints {
    #[serde(default)]
    pub kvm: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Boot {
    pub kernel: String,
    pub device_tree: String,
    pub rootfs: String,
    pub writable_overlay: Option<String>,
    #[serde(default)]
    pub kernel_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub memory_mb: u32,
    pub cpus: u32,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Display {
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glass {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
}

impl Glass {
    pub fn load(root: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let root = root.as_ref().to_path_buf();
        let manifest_path = root.join(MANIFEST_FILE_NAME);
        let manifest = Manifest::from_path(&manifest_path)?;
        Ok(Self {
            root,
            manifest_path,
            manifest,
        })
    }

    pub fn validate_manifest_only(&self) -> Result<(), ManifestError> {
        self.manifest.validate_manifest_only()
    }

    pub fn validate_launch_ready(&self) -> Result<(), ManifestError> {
        self.validate_manifest_only()?;

        if self.manifest.schema_version != LEGACY_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedLaunchSchema {
                actual: self.manifest.schema_version,
            });
        }

        let rootfs_path = self.root.join(&self.manifest.boot.rootfs);
        if !rootfs_path.is_file() {
            return Err(ManifestError::MissingRootfs { path: rootfs_path });
        }

        if let Some(writable_overlay) = &self.manifest.boot.writable_overlay {
            let overlay_path = self.root.join(writable_overlay);
            if !overlay_path.is_file() {
                return Err(ManifestError::MissingWritableOverlay { path: overlay_path });
            }
        }

        Ok(())
    }
}

impl Manifest {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let path = path.as_ref();
        let mut content = String::new();
        File::open(path)
            .and_then(|file| {
                file.take(MAX_MANIFEST_BYTES + 1)
                    .read_to_string(&mut content)
            })
            .map_err(|source| ManifestError::ReadManifest {
                path: path.to_path_buf(),
                source,
            })?;
        if content.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(ManifestError::ManifestTooLarge {
                path: path.to_path_buf(),
                limit: MAX_MANIFEST_BYTES,
            });
        }

        Self::from_toml_str(&content).map_err(|source| ManifestError::ParseManifest {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn from_toml_str(content: &str) -> Result<Self, toml::de::Error> {
        if content.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(<toml::de::Error as serde::de::Error>::custom(format!(
                "manifest text exceeds the {MAX_MANIFEST_BYTES}-byte limit"
            )));
        }
        #[derive(Deserialize)]
        struct VersionProbe {
            schema_version: u32,
        }

        let version = toml::from_str::<VersionProbe>(content)?.schema_version;
        match version {
            LEGACY_SCHEMA_VERSION => {
                let manifest: Schema1Manifest = toml::from_str(content)?;
                Ok(manifest.into())
            }
            MULTI_ARTIFACT_SCHEMA_VERSION => {
                let manifest: Schema2Manifest = toml::from_str(content)?;
                Ok(manifest.into())
            }
            _ => {
                // Keep the typed unsupported-version diagnostic for v1-shaped manifests. A
                // future schema with a different shape is rejected by the strict v1 parser.
                let manifest: Schema1Manifest = toml::from_str(content)?;
                Ok(manifest.into())
            }
        }
    }

    pub fn validate_manifest_only(&self) -> Result<(), ManifestError> {
        if !is_supported_schema_version(self.schema_version) {
            return Err(ManifestError::UnsupportedSchemaVersion {
                actual: self.schema_version,
                supported: SUPPORTED_SCHEMA_VERSION,
            });
        }

        if self.publisher.name.trim().is_empty() {
            return Err(ManifestError::MissingPublisherName);
        }

        if self.publisher.identity.trim().is_empty() {
            return Err(ManifestError::MissingPublisherIdentity);
        }

        if self.publisher.signature.trim().is_empty() {
            return Err(ManifestError::MissingSignature);
        }

        match self.schema_version {
            LEGACY_SCHEMA_VERSION => self.validate_schema1_runtime()?,
            MULTI_ARTIFACT_SCHEMA_VERSION => self
                .artifact_catalog
                .as_ref()
                .ok_or_else(|| {
                    ManifestError::InvalidArtifactCatalog(
                        "schema 2 requires an artifact_catalog table".to_string(),
                    )
                })?
                .validate(&self.publisher.signature)?,
            _ => unreachable!("supported schema versions were checked above"),
        }

        Ok(())
    }

    fn validate_schema1_runtime(&self) -> Result<(), ManifestError> {
        if self.runtime.architecture != SUPPORTED_ARCHITECTURE {
            return Err(ManifestError::UnsupportedArchitecture {
                actual: self.runtime.architecture.clone(),
                supported: SUPPORTED_ARCHITECTURE,
            });
        }

        if self.runtime.profile != SUPPORTED_RUNTIME_PROFILE {
            return Err(ManifestError::UnsupportedRuntimeProfile {
                actual: self.runtime.profile.clone(),
                supported: SUPPORTED_RUNTIME_PROFILE,
            });
        }

        if self.boot.kernel != BUILTIN_AARCH64_KERNEL {
            return Err(ManifestError::UnsupportedKernelReference {
                actual: self.boot.kernel.clone(),
                expected: BUILTIN_AARCH64_KERNEL,
            });
        }

        if self.boot.device_tree != BUILTIN_AARCH64_DEVICE_TREE {
            return Err(ManifestError::UnsupportedDeviceTreeReference {
                actual: self.boot.device_tree.clone(),
                expected: BUILTIN_AARCH64_DEVICE_TREE,
            });
        }

        Ok(())
    }
}

impl ArtifactCatalog {
    /// Returns the encrypted assets in the exact ID order used by ADR-0004's
    /// recipient set binding. Physical storage paths are intentionally omitted.
    pub fn encrypted_artifact_set(&self) -> Vec<EncryptedArtifactSetMember> {
        let mut encrypted = self
            .artifacts
            .iter()
            .filter_map(|(id, artifact)| {
                let encryption = artifact.encryption.as_ref()?;
                Some(EncryptedArtifactSetMember {
                    id: id.clone(),
                    ciphertext_sha256: artifact.sha256.clone(),
                    ciphertext_size: artifact.size_bytes,
                    suite_id: encryption.suite_id.id().to_string(),
                })
            })
            .collect::<Vec<_>>();
        encrypted.sort_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
        encrypted
    }

    fn validate(&self, signature_path: &str) -> Result<(), ManifestError> {
        if self.catalog_version != CATALOG_VERSION {
            return Err(catalog_error(format!(
                "unsupported catalog version {}; supported version is {CATALOG_VERSION}",
                self.catalog_version
            )));
        }
        if self.artifacts.is_empty() || self.artifacts.len() > MAX_CATALOG_ARTIFACTS {
            return Err(catalog_error(format!(
                "artifact count must be between 1 and {MAX_CATALOG_ARTIFACTS}"
            )));
        }
        if self.profiles.is_empty() || self.profiles.len() > MAX_CATALOG_PROFILES {
            return Err(catalog_error(format!(
                "profile count must be between 1 and {MAX_CATALOG_PROFILES}"
            )));
        }
        if !is_valid_id(&self.default_profile) || !self.profiles.contains_key(&self.default_profile)
        {
            return Err(catalog_error(format!(
                "default profile '{}' is missing or has an invalid ID",
                self.default_profile
            )));
        }

        let mut total_bytes = 0u64;
        let mut storage_paths = BTreeSet::new();
        for (id, artifact) in &self.artifacts {
            if !is_valid_id(id) {
                return Err(catalog_error(format!(
                    "artifact ID '{id}' is not valid ASCII"
                )));
            }
            if artifact.size_bytes == 0 || artifact.size_bytes > MAX_CATALOG_ARTIFACT_BYTES {
                return Err(catalog_error(format!(
                    "artifact '{id}' size must be 1..={MAX_CATALOG_ARTIFACT_BYTES} bytes"
                )));
            }
            total_bytes = total_bytes
                .checked_add(artifact.size_bytes)
                .ok_or_else(|| catalog_error("aggregate artifact size overflowed".to_string()))?;
            if total_bytes > MAX_CATALOG_TOTAL_BYTES {
                return Err(catalog_error(format!(
                    "aggregate artifact size exceeds {MAX_CATALOG_TOTAL_BYTES} bytes"
                )));
            }
            if !is_lower_sha256(&artifact.sha256) {
                return Err(catalog_error(format!(
                    "artifact '{id}' must have one lowercase SHA-256 digest"
                )));
            }
            let path = &artifact.storage.path;
            if !is_safe_storage_path(path) || path == MANIFEST_FILE_NAME || path == signature_path {
                return Err(catalog_error(format!(
                    "artifact '{id}' has an unsafe or reserved storage path '{path}'"
                )));
            }
            if !storage_paths.insert(path) {
                return Err(catalog_error(format!(
                    "multiple artifacts map to storage path '{path}'"
                )));
            }
            validate_artifact(id, artifact)?;
        }

        let encrypted_artifacts = self.encrypted_artifact_set();
        match (&self.encryption, encrypted_artifacts.is_empty()) {
            (Some(_), true) => {
                return Err(catalog_error(
                    "encryption policy is present but the catalog has no encrypted artifacts"
                        .to_string(),
                ));
            }
            (None, false) => {
                return Err(catalog_error(
                    "encrypted artifacts require an artifact_catalog.encryption policy".to_string(),
                ));
            }
            (Some(policy), false) => policy.validate()?,
            (None, true) => {}
        }

        let mut referenced_backends = BTreeSet::new();
        for (profile_id, profile) in &self.profiles {
            if !is_valid_id(profile_id) {
                return Err(catalog_error(format!(
                    "profile ID '{profile_id}' is not valid ASCII"
                )));
            }
            validate_profile(
                profile_id,
                profile,
                &self.artifacts,
                &mut referenced_backends,
            )?;
        }

        for (id, artifact) in &self.artifacts {
            if artifact.kind == ArtifactKind::BackendExecutable && !referenced_backends.contains(id)
            {
                return Err(catalog_error(format!(
                    "backend executable artifact '{id}' is not referenced by a typed backend device"
                )));
            }
        }

        Ok(())
    }
}

impl EncryptionPolicy {
    fn validate(&self) -> Result<(), ManifestError> {
        if !(1..=MAX_CONCURRENT_ENCRYPTED_INSTANCES).contains(&self.max_concurrent_instances) {
            return Err(catalog_error(format!(
                "encrypted max_concurrent_instances must be between 1 and {MAX_CONCURRENT_ENCRYPTED_INSTANCES}"
            )));
        }
        if self.recipient_authorities.is_empty()
            || self.recipient_authorities.len() > MAX_RECIPIENT_AUTHORITIES
        {
            return Err(catalog_error(format!(
                "recipient authority count must be between 1 and {MAX_RECIPIENT_AUTHORITIES}"
            )));
        }

        let mut key_ids = BTreeSet::new();
        for (index, authority) in self.recipient_authorities.iter().enumerate() {
            if !is_ed25519_key_id(&authority.key_id) {
                return Err(catalog_error(format!(
                    "recipient authority {index} key_id must be ed25519 followed by a lowercase SHA-256 fingerprint"
                )));
            }
            if !key_ids.insert(authority.key_id.as_str()) {
                return Err(catalog_error(format!(
                    "recipient authority key_id '{}' is duplicated",
                    authority.key_id
                )));
            }

            let public_key = URL_SAFE_NO_PAD
                .decode(authority.public_key_base64url.as_bytes())
                .map_err(|error| {
                    catalog_error(format!(
                        "recipient authority '{}' has invalid URL-safe base64 public key: {error}",
                        authority.key_id
                    ))
                })?;
            let public_key: [u8; 32] = public_key.as_slice().try_into().map_err(|_| {
                catalog_error(format!(
                    "recipient authority '{}' public key must decode to 32 bytes",
                    authority.key_id
                ))
            })?;
            if URL_SAFE_NO_PAD.encode(public_key) != authority.public_key_base64url {
                return Err(catalog_error(format!(
                    "recipient authority '{}' public key must use canonical URL-safe base64 without padding",
                    authority.key_id
                )));
            }
            VerifyingKey::from_bytes(&public_key).map_err(|error| {
                catalog_error(format!(
                    "recipient authority '{}' is not a valid Ed25519 public key: {error}",
                    authority.key_id
                ))
            })?;

            let expected_key_id = format!("ed25519:{}", lower_hex(&Sha256::digest(public_key)));
            if authority.key_id != expected_key_id {
                return Err(catalog_error(format!(
                    "recipient authority key_id '{}' does not match its public key (expected '{expected_key_id}')",
                    authority.key_id
                )));
            }
        }

        Ok(())
    }
}

fn validate_artifact(id: &str, artifact: &Artifact) -> Result<(), ManifestError> {
    let invalid = |reason: &str| catalog_error(format!("artifact '{id}' {reason}"));
    match &artifact.kind {
        ArtifactKind::LinuxBootDisk => {
            if artifact.format != ArtifactFormat::Raw
                || artifact.target_architecture != Some(TargetArchitecture::Aarch64)
                || artifact.target_machine != Some(TargetMachine::QemuVirt)
                || artifact.host_os.is_some()
                || artifact.backend.is_some()
            {
                return Err(invalid(
                    "must be a raw aarch64 QEMU virt disk with no host-backend metadata",
                ));
            }
        }
        ArtifactKind::McuFirmware => {
            if artifact.format != ArtifactFormat::Binary
                || artifact.target_architecture != Some(TargetArchitecture::Arm)
                || artifact.target_machine != Some(TargetMachine::QemuMps2An385)
                || artifact.host_os.is_some()
                || artifact.backend.is_some()
            {
                return Err(invalid(
                    "must be binary ARM firmware for qemu:mps2-an385 with no host-backend metadata",
                ));
            }
        }
        ArtifactKind::BackendExecutable => {
            if artifact.format != ArtifactFormat::Executable
                || !matches!(
                    artifact.target_architecture.as_ref(),
                    Some(TargetArchitecture::Aarch64 | TargetArchitecture::X86_64)
                )
                || artifact.target_machine.is_some()
                || artifact.host_os.is_none()
            {
                return Err(invalid(
                    "must declare executable format, a supported host architecture, and host OS",
                ));
            }
            let backend = artifact
                .backend
                .as_ref()
                .ok_or_else(|| invalid("must declare signed backend policy metadata"))?;
            if backend.publisher_identity.trim().is_empty()
                || backend.capabilities.is_empty()
                || backend.ipc.is_empty()
                || backend
                    .qom_device_ids
                    .iter()
                    .any(|device_id| !is_valid_id(device_id))
                || backend
                    .virtio_backend_ids
                    .iter()
                    .any(|backend_id| !is_valid_id(backend_id))
                || (!backend.qom_device_ids.is_empty()
                    && !backend
                        .capabilities
                        .contains(&BackendCapability::QomDeviceV1))
                || (!backend.virtio_backend_ids.is_empty()
                    && !backend
                        .capabilities
                        .contains(&BackendCapability::VirtioBackendV1))
            {
                return Err(invalid(
                    "backend policy requires valid publisher identity, capabilities, IPC transports, and declared device IDs",
                ));
            }
        }
    }
    if artifact.encryption.is_some() && artifact.kind != ArtifactKind::LinuxBootDisk {
        return Err(invalid(
            "may declare encryption only for a Linux boot disk in this schema version",
        ));
    }
    Ok(())
}

fn validate_profile(
    profile_id: &str,
    profile: &LaunchProfile,
    artifacts: &BTreeMap<String, Artifact>,
    referenced_backends: &mut BTreeSet<String>,
) -> Result<(), ManifestError> {
    let invalid = |reason: String| catalog_error(format!("profile '{profile_id}' {reason}"));
    if !profile
        .required_features
        .contains(&RequiredFeature::LaunchProfilesV1)
    {
        return Err(invalid("must require launch-profiles-v1".to_string()));
    }
    if profile.resources.memory_mb == 0
        || profile.resources.memory_mb > 262_144
        || profile.resources.cpus == 0
        || profile.resources.cpus > 256
    {
        return Err(invalid(
            "resource limits must specify 1..=262144 MiB and 1..=256 CPUs".to_string(),
        ));
    }
    if !matches!(profile.network.mode.as_str(), "none" | "user") {
        return Err(invalid(format!(
            "has unsupported network mode '{}'",
            profile.network.mode
        )));
    }
    if profile.nodes.is_empty() || profile.nodes.len() > 128 {
        return Err(invalid("node count must be between 1 and 128".to_string()));
    }
    if profile.devices.len() > 256 {
        return Err(invalid("device count exceeds 256".to_string()));
    }

    let mut nodes = BTreeMap::new();
    let mut process_groups = BTreeMap::new();
    let mut required_root_devices = BTreeSet::new();
    let mut has_linux = false;
    let mut has_mcu = false;
    let mut uses_encrypted_artifacts = false;
    for node in &profile.nodes {
        if !is_valid_id(&node.id) || !is_valid_id(&node.process_group) {
            return Err(invalid(format!(
                "node '{}' has an invalid node or process-group ID",
                node.id
            )));
        }
        if nodes.insert(node.id.as_str(), node).is_some() {
            return Err(invalid(format!("has duplicate node ID '{}'", node.id)));
        }
        if let Some((architecture, machine)) = process_groups.get(node.process_group.as_str()) {
            if *architecture != &node.architecture || *machine != &node.machine {
                return Err(invalid(format!(
                    "process group '{}' mixes incompatible architecture or machine targets",
                    node.process_group
                )));
            }
        } else {
            process_groups.insert(
                node.process_group.as_str(),
                (&node.architecture, &node.machine),
            );
        }
        match &node.boot {
            BootData::Linux {
                kernel,
                device_tree,
                rootfs_artifact,
                kernel_args,
            } => {
                has_linux = true;
                if node.architecture != TargetArchitecture::Aarch64
                    || node.machine != TargetMachine::QemuVirt
                    || kernel != BUILTIN_AARCH64_KERNEL
                    || device_tree != BUILTIN_AARCH64_DEVICE_TREE
                {
                    return Err(invalid(format!(
                        "Linux node '{}' must use aarch64, qemu:virt, and the built-in kernel/device tree references",
                        node.id
                    )));
                }
                if kernel_args.len() > 128
                    || kernel_args
                        .iter()
                        .any(|arg| arg.len() > 4096 || arg.chars().any(char::is_control))
                {
                    return Err(invalid(format!(
                        "Linux node '{}' has invalid or oversized kernel arguments",
                        node.id
                    )));
                }
                let root = artifacts.get(rootfs_artifact).ok_or_else(|| {
                    invalid(format!(
                        "Linux node '{}' references missing rootfs artifact '{rootfs_artifact}'",
                        node.id
                    ))
                })?;
                if root.kind != ArtifactKind::LinuxBootDisk {
                    return Err(invalid(format!(
                        "Linux node '{}' rootfs reference '{rootfs_artifact}' is not a Linux boot disk",
                        node.id
                    )));
                }
                uses_encrypted_artifacts |= root.encryption.is_some();
                required_root_devices.insert((node.id.as_str(), rootfs_artifact.as_str()));
            }
            BootData::Mcu { firmware_artifact } => {
                has_mcu = true;
                if node.architecture != TargetArchitecture::Arm
                    || node.machine != TargetMachine::QemuMps2An385
                {
                    return Err(invalid(format!(
                        "MCU node '{}' must use ARM and the concrete qemu:mps2-an385 board",
                        node.id
                    )));
                }
                let firmware = artifacts.get(firmware_artifact).ok_or_else(|| {
                    invalid(format!(
                        "MCU node '{}' references missing firmware artifact '{firmware_artifact}'",
                        node.id
                    ))
                })?;
                if firmware.kind != ArtifactKind::McuFirmware {
                    return Err(invalid(format!(
                        "MCU node '{}' firmware reference '{firmware_artifact}' is not MCU firmware",
                        node.id
                    )));
                }
            }
        }
    }

    let mut device_ids = BTreeSet::new();
    let mut required_features = BTreeSet::from([RequiredFeature::LaunchProfilesV1]);
    if has_linux {
        required_features.insert(RequiredFeature::LinuxNodeV1);
    }
    if has_mcu {
        required_features.insert(RequiredFeature::McuNodeV1);
    }
    if !profile.devices.is_empty() {
        required_features.insert(RequiredFeature::TypedDevicesV1);
    }
    if uses_encrypted_artifacts {
        required_features.insert(RequiredFeature::EncryptedArtifactsV1);
    }

    for device in &profile.devices {
        match device {
            TypedDevice::VirtioBlkMmio {
                id,
                node_id,
                artifact_id,
                read_only,
            } => {
                validate_device_common(&mut device_ids, id, node_id, &nodes, profile_id)?;
                let node = nodes[node_id.as_str()];
                if !matches!(&node.boot, BootData::Linux { .. }) {
                    return Err(invalid(format!(
                        "virtio block device '{id}' must attach to a Linux node"
                    )));
                }
                let artifact = artifacts.get(artifact_id).ok_or_else(|| {
                    invalid(format!(
                        "device '{id}' references missing artifact '{artifact_id}'"
                    ))
                })?;
                if artifact.kind != ArtifactKind::LinuxBootDisk {
                    return Err(invalid(format!(
                        "virtio block device '{id}' must reference a Linux boot disk"
                    )));
                }
                if artifact.encryption.is_some() {
                    uses_encrypted_artifacts = true;
                    required_features.insert(RequiredFeature::EncryptedArtifactsV1);
                }
                if let BootData::Linux {
                    rootfs_artifact, ..
                } = &node.boot
                {
                    if artifact_id == rootfs_artifact && !read_only {
                        return Err(invalid(format!("rootfs device '{id}' must be read-only")));
                    }
                    if artifact_id == rootfs_artifact {
                        required_root_devices.remove(&(node_id.as_str(), artifact_id.as_str()));
                    }
                }
            }
            TypedDevice::Pl011Uart { id, node_id } => {
                validate_device_common(&mut device_ids, id, node_id, &nodes, profile_id)?;
                let node = nodes[node_id.as_str()];
                if node.machine != TargetMachine::QemuVirt
                    || !matches!(&node.boot, BootData::Linux { .. })
                {
                    return Err(invalid(format!(
                        "PL011 UART '{id}' must attach to a Linux qemu:virt node"
                    )));
                }
            }
            TypedDevice::Mps2Uart { id, node_id } => {
                validate_device_common(&mut device_ids, id, node_id, &nodes, profile_id)?;
                let node = nodes[node_id.as_str()];
                if node.machine != TargetMachine::QemuMps2An385
                    || !matches!(&node.boot, BootData::Mcu { .. })
                {
                    return Err(invalid(format!(
                        "MPS2 UART '{id}' must attach to an MCU qemu:mps2-an385 node"
                    )));
                }
            }
            TypedDevice::ExternalVirtioBackend {
                id,
                node_id,
                backend_artifact_id,
                backend_id,
            } => {
                validate_device_common(&mut device_ids, id, node_id, &nodes, profile_id)?;
                let node = nodes[node_id.as_str()];
                if !matches!(&node.boot, BootData::Linux { .. }) {
                    return Err(invalid(format!(
                        "external virtio backend '{id}' must attach to a Linux node"
                    )));
                }
                let backend = artifacts.get(backend_artifact_id).ok_or_else(|| {
                    invalid(format!(
                        "device '{id}' references missing backend artifact '{backend_artifact_id}'"
                    ))
                })?;
                if backend.kind != ArtifactKind::BackendExecutable {
                    return Err(invalid(format!(
                        "device '{id}' reference '{backend_artifact_id}' is not an approved backend executable"
                    )));
                }
                let metadata = backend
                    .backend
                    .as_ref()
                    .expect("backend kind was validated");
                if !metadata
                    .capabilities
                    .contains(&BackendCapability::VirtioBackendV1)
                    || !metadata.virtio_backend_ids.contains(backend_id)
                {
                    return Err(invalid(format!(
                        "device '{id}' references undeclared virtio backend ID '{backend_id}'"
                    )));
                }
                referenced_backends.insert(backend_artifact_id.clone());
                required_features.insert(RequiredFeature::BackendAssetsV1);
                required_features.insert(RequiredFeature::BackendProcessIsolationV1);
                required_features.insert(RequiredFeature::ExternalVirtioBackendV1);
            }
            TypedDevice::ExternalQomBackend {
                id,
                node_id,
                backend_artifact_id,
                qom_device_id,
            } => {
                validate_device_common(&mut device_ids, id, node_id, &nodes, profile_id)?;
                let backend = artifacts.get(backend_artifact_id).ok_or_else(|| {
                    invalid(format!(
                        "device '{id}' references missing backend artifact '{backend_artifact_id}'"
                    ))
                })?;
                if backend.kind != ArtifactKind::BackendExecutable {
                    return Err(invalid(format!(
                        "device '{id}' reference '{backend_artifact_id}' is not an approved backend executable"
                    )));
                }
                let metadata = backend
                    .backend
                    .as_ref()
                    .expect("backend kind was validated");
                if !metadata
                    .capabilities
                    .contains(&BackendCapability::QomDeviceV1)
                    || !metadata.qom_device_ids.contains(qom_device_id)
                {
                    return Err(invalid(format!(
                        "device '{id}' references undeclared QOM device ID '{qom_device_id}'"
                    )));
                }
                referenced_backends.insert(backend_artifact_id.clone());
                required_features.insert(RequiredFeature::BackendAssetsV1);
                required_features.insert(RequiredFeature::BackendProcessIsolationV1);
                required_features.insert(RequiredFeature::ExternalQomBackendV1);
            }
        }
    }

    if let Some((node_id, artifact_id)) = required_root_devices.into_iter().next() {
        return Err(invalid(format!(
            "Linux node '{node_id}' rootfs artifact '{artifact_id}' needs a matching typed read-only block device"
        )));
    }
    if uses_encrypted_artifacts
        && !profile
            .required_features
            .contains(&RequiredFeature::EncryptedArtifactsV1)
    {
        return Err(invalid(
            "uses encrypted artifacts but omits required feature 'encrypted-artifacts-v1'"
                .to_string(),
        ));
    }
    if profile
        .required_features
        .iter()
        .any(|feature| !required_features.contains(feature))
    {
        return Err(invalid(
            "declares a required feature that is not supported by this profile".to_string(),
        ));
    }
    for feature in required_features {
        if !profile.required_features.contains(&feature) {
            return Err(invalid(format!(
                "is missing required feature '{}'",
                required_feature_id(&feature)
            )));
        }
    }
    Ok(())
}

fn validate_device_common(
    ids: &mut BTreeSet<String>,
    id: &str,
    node_id: &str,
    nodes: &BTreeMap<&str, &LaunchNode>,
    profile_id: &str,
) -> Result<(), ManifestError> {
    if !is_valid_id(id) {
        return Err(catalog_error(format!(
            "profile '{profile_id}' has invalid device ID '{id}'"
        )));
    }
    if !ids.insert(id.to_string()) {
        return Err(catalog_error(format!(
            "profile '{profile_id}' has duplicate device ID '{id}'"
        )));
    }
    if !nodes.contains_key(node_id) {
        return Err(catalog_error(format!(
            "profile '{profile_id}' device '{id}' references missing node '{node_id}'"
        )));
    }
    Ok(())
}

fn required_feature_id(feature: &RequiredFeature) -> &'static str {
    match feature {
        RequiredFeature::LaunchProfilesV1 => "launch-profiles-v1",
        RequiredFeature::LinuxNodeV1 => "linux-node-v1",
        RequiredFeature::McuNodeV1 => "mcu-node-v1",
        RequiredFeature::EncryptedArtifactsV1 => "encrypted-artifacts-v1",
        RequiredFeature::TypedDevicesV1 => "typed-devices-v1",
        RequiredFeature::BackendAssetsV1 => "backend-assets-v1",
        RequiredFeature::BackendProcessIsolationV1 => "backend-process-isolation-v1",
        RequiredFeature::ExternalQomBackendV1 => "external-qom-backend-v1",
        RequiredFeature::ExternalVirtioBackendV1 => "external-virtio-backend-v1",
    }
}

fn is_valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_ed25519_key_id(value: &str) -> bool {
    value.strip_prefix("ed25519:").is_some_and(is_lower_sha256)
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn is_safe_storage_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn catalog_error(message: String) -> ManifestError {
    ManifestError::InvalidArtifactCatalog(message)
}

pub const fn is_supported_schema_version(version: u32) -> bool {
    matches!(
        version,
        LEGACY_SCHEMA_VERSION | MULTI_ARTIFACT_SCHEMA_VERSION
    )
}

impl From<Schema1Manifest> for Manifest {
    fn from(manifest: Schema1Manifest) -> Self {
        Self {
            schema_version: manifest.schema_version,
            name: manifest.name,
            identifier: manifest.identifier,
            version: manifest.version,
            description: manifest.description,
            publisher: manifest.publisher,
            runtime: manifest.runtime,
            boot: manifest.boot,
            resources: manifest.resources,
            display: manifest.display,
            network: manifest.network,
            artifact_catalog: None,
        }
    }
}

impl From<Schema2Manifest> for Manifest {
    fn from(manifest: Schema2Manifest) -> Self {
        // Schema 2 is inspectable and signable here. The existing runtime still uses
        // the schema-1 single-guest fields; sentinel values make accidental launch
        // resolution fail before QEMU while later profile execution work is pending.
        Self {
            schema_version: manifest.schema_version,
            name: manifest.name,
            identifier: manifest.identifier,
            version: manifest.version,
            description: manifest.description,
            publisher: manifest.publisher,
            runtime: Runtime {
                architecture: "catalog-v2".to_string(),
                profile: "catalog-v2".to_string(),
                requires_hardware_acceleration: false,
                host_constraints: HostConstraints::default(),
            },
            boot: Boot {
                kernel: "catalog:v2".to_string(),
                device_tree: "catalog:v2".to_string(),
                rootfs: String::new(),
                writable_overlay: None,
                kernel_args: Vec::new(),
            },
            resources: Resources {
                memory_mb: 0,
                cpus: 0,
            },
            display: Display {
                mode: "profile-defined".to_string(),
            },
            network: Network {
                mode: "profile-defined".to_string(),
            },
            artifact_catalog: Some(manifest.artifact_catalog),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema2_manifest() -> String {
        let mut content = String::from(
            r#"
schema_version = 2
name = "Catalog Example"
identifier = "io.rootglass.examples.catalog"
version = "0.2.0"

[publisher]
name = "Rootglass Examples"
identity = "rootglass.dev/examples"
signature = "signatures/glass.dsse.json"

[artifact_catalog]
catalog_version = 1
default_profile = "default"
"#,
        );
        for index in 0..5 {
            content.push_str(&format!(
                r#"
[artifact_catalog.artifacts.linux-{index}]
kind = "linux-boot-disk"
format = "raw"
size_bytes = 1
sha256 = "{}"
target_architecture = "aarch64"
target_machine = "qemu:virt"

[artifact_catalog.artifacts.linux-{index}.storage]
kind = "zip-entry"
path = "images/linux-{index}.raw"
"#,
                "0".repeat(64)
            ));
        }
        for index in 0..5 {
            content.push_str(&format!(
                r#"
[artifact_catalog.artifacts.firmware-{index}]
kind = "mcu-firmware"
format = "binary"
size_bytes = 1
sha256 = "{}"
target_architecture = "arm"
target_machine = "qemu:mps2-an385"

[artifact_catalog.artifacts.firmware-{index}.storage]
kind = "zip-entry"
path = "firmware/controller-{index}.bin"
"#,
                "1".repeat(64)
            ));
        }
        content.push_str(
            r#"
[artifact_catalog.artifacts.device-backend]
kind = "backend-executable"
format = "executable"
size_bytes = 1
sha256 = "2222222222222222222222222222222222222222222222222222222222222222"
target_architecture = "x86_64"
host_os = "linux"

[artifact_catalog.artifacts.device-backend.storage]
kind = "zip-entry"
path = "backends/device-backend"

[artifact_catalog.artifacts.device-backend.backend]
publisher_identity = "rootglass.dev/device-backend"
capabilities = ["qom-device-v1", "virtio-backend-v1"]
ipc = ["stdio"]
lifecycle = "spawn-per-run"
isolation = "separate-process"
qom_device_ids = ["sensor-v1"]
virtio_backend_ids = ["sensor-storage-v1"]
"#,
        );
        content.push_str(
            r#"
[artifact_catalog.profiles.default]
required_features = [
  "launch-profiles-v1", "linux-node-v1", "mcu-node-v1", "typed-devices-v1",
  "backend-assets-v1", "backend-process-isolation-v1",
  "external-qom-backend-v1", "external-virtio-backend-v1"
]

[artifact_catalog.profiles.default.resources]
memory_mb = 1024
cpus = 2

[artifact_catalog.profiles.default.network]
mode = "none"

[[artifact_catalog.profiles.default.nodes]]
id = "linux-main"
startup_order = 0
process_group = "linux"
architecture = "aarch64"
machine = "qemu:virt"
boot = { kind = "linux", kernel = "builtin:aarch64-linux", device_tree = "builtin:aarch64-virt", rootfs_artifact = "linux-0", kernel_args = [] }

[[artifact_catalog.profiles.default.nodes]]
id = "controller"
startup_order = 1
process_group = "mcu"
architecture = "arm"
machine = "qemu:mps2-an385"
boot = { kind = "mcu", firmware_artifact = "firmware-0" }

[[artifact_catalog.profiles.default.devices]]
kind = "virtio-blk-mmio"
id = "root-disk"
node_id = "linux-main"
artifact_id = "linux-0"
read_only = true

[[artifact_catalog.profiles.default.devices]]
kind = "pl011-uart"
id = "linux-console"
node_id = "linux-main"

[[artifact_catalog.profiles.default.devices]]
kind = "mps2-uart"
id = "mcu-console"
node_id = "controller"

[[artifact_catalog.profiles.default.devices]]
kind = "external-qom-backend"
id = "sensor-device"
node_id = "linux-main"
backend_artifact_id = "device-backend"
qom_device_id = "sensor-v1"

[[artifact_catalog.profiles.default.devices]]
kind = "external-virtio-backend"
id = "sensor-storage"
node_id = "linux-main"
backend_artifact_id = "device-backend"
backend_id = "sensor-storage-v1"
"#,
        );
        content
    }

    fn encrypted_schema2_manifest() -> String {
        const AUTHORITY_KEY_ID: &str =
            "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0";
        const AUTHORITY_PUBLIC_KEY: &str = "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw";

        let mut content = schema2_manifest()
            .replace(
                "\"launch-profiles-v1\", \"linux-node-v1\", \"mcu-node-v1\", \"typed-devices-v1\",",
                "\"launch-profiles-v1\", \"linux-node-v1\", \"mcu-node-v1\", \"encrypted-artifacts-v1\", \"typed-devices-v1\",",
            )
            .replace(
                "default_profile = \"default\"\n",
                &format!(
                    "default_profile = \"default\"\n\n[artifact_catalog.encryption]\nminimum_recipient_hardware = \"software-allowed\"\nmax_concurrent_instances = 3\nrecipient_authorities = [{{ key_id = \"{AUTHORITY_KEY_ID}\", public_key_base64url = \"{AUTHORITY_PUBLIC_KEY}\" }}]\n"
                ),
            );
        content = content.replace(
            "[artifact_catalog.artifacts.linux-0.storage]\n",
            "[artifact_catalog.artifacts.linux-0.encryption]\nsuite_id = \"qemu-luks-v1\"\n\n[artifact_catalog.artifacts.linux-0.storage]\n",
        );
        content.replace(
            "[artifact_catalog.artifacts.linux-1.storage]\n",
            "[artifact_catalog.artifacts.linux-1.encryption]\nsuite_id = \"qemu-luks-v1\"\n\n[artifact_catalog.artifacts.linux-1.storage]\n",
        )
    }

    #[test]
    fn schema2_catalog_supports_five_linux_and_five_mcu_artifacts() {
        let manifest = Manifest::from_toml_str(&schema2_manifest()).expect("schema 2 parses");
        manifest
            .validate_manifest_only()
            .expect("5+5 catalog validates");

        let catalog = manifest.artifact_catalog.expect("catalog is present");
        assert_eq!(manifest.schema_version, MULTI_ARTIFACT_SCHEMA_VERSION);
        assert!(catalog.encryption.is_none());
        assert_eq!(
            catalog
                .artifacts
                .values()
                .filter(|artifact| artifact.kind == ArtifactKind::LinuxBootDisk)
                .count(),
            5
        );
        assert_eq!(
            catalog
                .artifacts
                .values()
                .filter(|artifact| artifact.kind == ArtifactKind::McuFirmware)
                .count(),
            5
        );
        assert_eq!(catalog.artifacts.len(), 11);
        assert_eq!(catalog.profiles["default"].nodes.len(), 2);
        assert!(matches!(
            &catalog.artifacts["linux-0"].kind,
            ArtifactKind::LinuxBootDisk
        ));
        assert!(matches!(
            &catalog.artifacts["firmware-4"].kind,
            ArtifactKind::McuFirmware
        ));
        assert!(is_supported_schema_version(LEGACY_SCHEMA_VERSION));
        assert!(is_supported_schema_version(MULTI_ARTIFACT_SCHEMA_VERSION));
        assert_eq!(
            catalog.artifacts["device-backend"]
                .backend
                .as_ref()
                .expect("backend policy exists")
                .qom_device_ids
                .iter()
                .next()
                .map(String::as_str),
            Some("sensor-v1")
        );
    }

    #[test]
    fn schema2_encryption_uses_logical_ids_and_binds_the_complete_encrypted_set() {
        let manifest = Manifest::from_toml_str(&encrypted_schema2_manifest())
            .expect("encrypted schema parses");
        manifest
            .validate_manifest_only()
            .expect("approved encrypted catalog validates");

        let catalog = manifest.artifact_catalog.expect("catalog is present");
        let policy = catalog
            .encryption
            .as_ref()
            .expect("encryption policy is signed");
        assert_eq!(policy.max_concurrent_instances, 3);
        assert_eq!(
            policy.minimum_recipient_hardware,
            MinimumRecipientHardware::SoftwareAllowed
        );
        assert_eq!(policy.recipient_authorities.len(), 1);

        let encrypted_set = catalog.encrypted_artifact_set();
        assert_eq!(
            encrypted_set,
            vec![
                EncryptedArtifactSetMember {
                    id: "linux-0".to_string(),
                    ciphertext_sha256: "0".repeat(64),
                    ciphertext_size: 1,
                    suite_id: "qemu-luks-v1".to_string(),
                },
                EncryptedArtifactSetMember {
                    id: "linux-1".to_string(),
                    ciphertext_sha256: "0".repeat(64),
                    ciphertext_size: 1,
                    suite_id: "qemu-luks-v1".to_string(),
                },
            ]
        );

        let hardware_required =
            encrypted_schema2_manifest().replace("software-allowed", "hardware-backed-required");
        let hardware_manifest =
            Manifest::from_toml_str(&hardware_required).expect("hardware policy parses");
        hardware_manifest
            .validate_manifest_only()
            .expect("hardware-backed minimum is an approved policy");
        assert_eq!(
            hardware_manifest
                .artifact_catalog
                .expect("catalog is present")
                .encryption
                .expect("encryption policy is present")
                .minimum_recipient_hardware,
            MinimumRecipientHardware::HardwareBackedRequired
        );
    }

    #[test]
    fn schema2_encryption_rejects_incomplete_or_contradictory_policy() {
        let encrypted_without_policy = encrypted_schema2_manifest().replace(
            "[artifact_catalog.encryption]\nminimum_recipient_hardware = \"software-allowed\"\nmax_concurrent_instances = 3\nrecipient_authorities = [{ key_id = \"ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0\", public_key_base64url = \"2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw\" }]\n\n",
            "",
        );
        let parsed = Manifest::from_toml_str(&encrypted_without_policy)
            .expect("missing policy is a semantic validation error");
        assert!(matches!(
            parsed.validate_manifest_only(),
            Err(ManifestError::InvalidArtifactCatalog(message))
                if message.contains("require an artifact_catalog.encryption policy")
        ));

        let policy_without_encrypted_assets = encrypted_schema2_manifest()
            .replace(
                "[artifact_catalog.artifacts.linux-0.encryption]\nsuite_id = \"qemu-luks-v1\"\n\n",
                "",
            )
            .replace(
                "[artifact_catalog.artifacts.linux-1.encryption]\nsuite_id = \"qemu-luks-v1\"\n\n",
                "",
            );
        let parsed = Manifest::from_toml_str(&policy_without_encrypted_assets)
            .expect("policy without assets is a semantic validation error");
        assert!(matches!(
            parsed.validate_manifest_only(),
            Err(ManifestError::InvalidArtifactCatalog(message))
                if message.contains("no encrypted artifacts")
        ));

        for invalid_policy in [
            encrypted_schema2_manifest().replace(
                "max_concurrent_instances = 3",
                "max_concurrent_instances = 0",
            ),
            encrypted_schema2_manifest().replace(
                "max_concurrent_instances = 3",
                "max_concurrent_instances = 6",
            ),
            encrypted_schema2_manifest().replace(
                "recipient_authorities = [{",
                "recipient_authorities = []\n# [{",
            ),
            encrypted_schema2_manifest().replace(
                "minimum_recipient_hardware = \"software-allowed\"",
                "minimum_recipient_hardware = \"unknown\"",
            ),
        ] {
            if let Ok(parsed) = Manifest::from_toml_str(&invalid_policy) {
                assert!(parsed.validate_manifest_only().is_err());
            }
        }

        let missing_encryption_feature = encrypted_schema2_manifest().replace(
            "\"mcu-node-v1\", \"encrypted-artifacts-v1\",",
            "\"mcu-node-v1\",",
        );
        let parsed = Manifest::from_toml_str(&missing_encryption_feature)
            .expect("missing feature is a semantic validation error");
        assert!(matches!(
            parsed.validate_manifest_only(),
            Err(ManifestError::InvalidArtifactCatalog(message))
                if message.contains("omits required feature 'encrypted-artifacts-v1'")
        ));
    }

    #[test]
    fn schema2_encryption_rejects_bad_authorities_suites_and_secret_fields() {
        let wrong_key_id = encrypted_schema2_manifest().replace(
            "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0",
            &format!("ed25519:{}", "0".repeat(64)),
        );
        let invalid_public_key = encrypted_schema2_manifest().replace(
            "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw",
            "not-a-public-key",
        );
        let duplicate_authority = encrypted_schema2_manifest().replace(
            "recipient_authorities = [{ key_id = \"ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0\", public_key_base64url = \"2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw\" }]",
            "recipient_authorities = [{ key_id = \"ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0\", public_key_base64url = \"2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw\" }, { key_id = \"ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0\", public_key_base64url = \"2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw\" }]",
        );
        let unknown_suite = encrypted_schema2_manifest()
            .replace("suite_id = \"qemu-luks-v1\"", "suite_id = \"qemu-aes-v1\"");
        let secret_field = encrypted_schema2_manifest().replace(
            "max_concurrent_instances = 3",
            "max_concurrent_instances = 3\nimk = \"never-accept-secret-material\"",
        );

        for invalid in [wrong_key_id, invalid_public_key, duplicate_authority] {
            let parsed = Manifest::from_toml_str(&invalid).expect("syntax parses");
            assert!(parsed.validate_manifest_only().is_err());
        }
        assert!(Manifest::from_toml_str(&unknown_suite).is_err());
        assert!(Manifest::from_toml_str(&secret_field).is_err());

        let encrypted_firmware = encrypted_schema2_manifest().replace(
            "[artifact_catalog.artifacts.firmware-0.storage]\n",
            "[artifact_catalog.artifacts.firmware-0.encryption]\nsuite_id = \"qemu-luks-v1\"\n\n[artifact_catalog.artifacts.firmware-0.storage]\n",
        );
        let parsed = Manifest::from_toml_str(&encrypted_firmware).expect("syntax parses");
        assert!(parsed.validate_manifest_only().is_err());
    }

    #[test]
    fn schema1_encryption_fields_remain_unknown_and_rejected() {
        let legacy = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/hello-world.glass/rootglass.toml"
        ))
        .expect("legacy signed fixture manifest is available");
        assert!(Manifest::from_toml_str(&legacy).is_ok());

        let with_encryption = legacy.replace("[publisher]\n", "encryption = {}\n\n[publisher]\n");
        assert!(Manifest::from_toml_str(&with_encryption).is_err());
    }

    #[test]
    fn schema2_manifest_is_not_treated_as_a_schema1_launch_request() {
        let manifest = Manifest::from_toml_str(&schema2_manifest()).expect("schema 2 parses");
        let glass = Glass {
            root: PathBuf::from("."),
            manifest_path: PathBuf::from("rootglass.toml"),
            manifest,
        };

        assert!(matches!(
            glass.validate_launch_ready(),
            Err(ManifestError::UnsupportedLaunchSchema { actual: 2 })
        ));
    }

    #[test]
    fn schema2_rejects_duplicate_and_dangling_node_references() {
        let duplicate =
            schema2_manifest().replace("id = \"controller\"\n", "id = \"linux-main\"\n");
        let parsed = Manifest::from_toml_str(&duplicate).expect("duplicate ID syntax parses");
        assert!(matches!(
            parsed.validate_manifest_only(),
            Err(ManifestError::InvalidArtifactCatalog(_))
        ));

        let dangling = schema2_manifest().replace(
            "rootfs_artifact = \"linux-0\"",
            "rootfs_artifact = \"missing\"",
        );
        let parsed = Manifest::from_toml_str(&dangling).expect("dangling reference syntax parses");
        assert!(matches!(
            parsed.validate_manifest_only(),
            Err(ManifestError::InvalidArtifactCatalog(_))
        ));

        let unknown_backend = schema2_manifest().replace(
            "qom_device_id = \"sensor-v1\"",
            "qom_device_id = \"not-declared\"",
        );
        let parsed =
            Manifest::from_toml_str(&unknown_backend).expect("backend reference syntax parses");
        assert!(matches!(
            parsed.validate_manifest_only(),
            Err(ManifestError::InvalidArtifactCatalog(_))
        ));

        let incompatible_process_group =
            schema2_manifest().replace("process_group = \"mcu\"", "process_group = \"linux\"");
        let parsed = Manifest::from_toml_str(&incompatible_process_group)
            .expect("process-group syntax parses");
        assert!(matches!(
            parsed.validate_manifest_only(),
            Err(ManifestError::InvalidArtifactCatalog(_))
        ));
    }

    #[test]
    fn schema2_rejects_unknown_artifact_device_and_required_feature_ids() {
        for invalid in [
            schema2_manifest().replace("kind = \"linux-boot-disk\"", "kind = \"unknown-kind\""),
            schema2_manifest().replace("kind = \"pl011-uart\"", "kind = \"unknown-device\""),
            schema2_manifest().replace("typed-devices-v1", "unrecognized-feature-v9"),
            schema2_manifest().replace(
                "path = \"images/linux-1.raw\"",
                "path = \"images/linux-0.raw\"",
            ),
        ] {
            if let Ok(manifest) = Manifest::from_toml_str(&invalid) {
                assert!(manifest.validate_manifest_only().is_err());
            }
        }
    }

    #[test]
    fn parses_sample_glass_manifest() {
        let glass = Glass::load("../../examples/hello-world.glass").expect("sample glass loads");

        assert_eq!(glass.manifest.schema_version, 1);
        assert_eq!(glass.manifest.name, "Hello World");
        assert_eq!(glass.manifest.runtime.architecture, "aarch64");
        assert_eq!(glass.manifest.runtime.profile, "virt-linux");
        assert_eq!(glass.manifest.boot.rootfs, "rootfs.img");
        assert_eq!(
            glass.manifest.boot.writable_overlay.as_deref(),
            Some("state.img")
        );

        glass
            .validate_manifest_only()
            .expect("manifest-only validation passes");
    }

    #[test]
    fn launch_readiness_fails_when_rootfs_is_missing() {
        let source_manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/hello-world.glass/rootglass.toml");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after unix epoch")
            .as_nanos();
        let fixture_root = std::env::temp_dir().join(format!(
            "rootglass-manifest-missing-rootfs-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&fixture_root).expect("fixture directory is created");
        std::fs::copy(source_manifest, fixture_root.join(MANIFEST_FILE_NAME))
            .expect("sample manifest is copied into an isolated fixture");
        let glass = Glass::load(&fixture_root).expect("fixture manifest loads");
        let error = glass
            .validate_launch_ready()
            .expect_err("fixture intentionally omits rootfs.img");

        assert!(matches!(error, ManifestError::MissingRootfs { .. }));
        std::fs::remove_dir_all(fixture_root).expect("fixture directory is removed");
    }

    #[test]
    fn rejects_unsupported_architecture() {
        let content = r#"
schema_version = 1
name = "Bad Arch"
identifier = "io.rootglass.examples.bad-arch"
version = "0.1.0"

[publisher]
name = "Rootglass Examples"
identity = "rootglass.dev/examples"
signature = "signatures/bundle.sig"

[runtime]
architecture = "x86_64"
profile = "virt-linux"
requires_hardware_acceleration = false

[boot]
kernel = "builtin:aarch64-linux"
device_tree = "builtin:aarch64-virt"
rootfs = "rootfs.img"

[resources]
memory_mb = 512
cpus = 1

[display]
mode = "serial-console"

[network]
mode = "user"
"#;

        let manifest = Manifest::from_toml_str(content).expect("manifest parses");
        let error = manifest
            .validate_manifest_only()
            .expect_err("unsupported architecture should fail");

        assert!(matches!(
            error,
            ManifestError::UnsupportedArchitecture { .. }
        ));
    }

    #[test]
    fn rejects_non_builtin_kernel_reference_for_mvp() {
        let content = r#"
schema_version = 1
name = "Bad Kernel"
identifier = "io.rootglass.examples.bad-kernel"
version = "0.1.0"

[publisher]
name = "Rootglass Examples"
identity = "rootglass.dev/examples"
signature = "signatures/bundle.sig"

[runtime]
architecture = "aarch64"
profile = "virt-linux"
requires_hardware_acceleration = false

[boot]
kernel = "assets/Image"
device_tree = "builtin:aarch64-virt"
rootfs = "rootfs.img"

[resources]
memory_mb = 512
cpus = 1

[display]
mode = "serial-console"

[network]
mode = "user"
"#;

        let manifest = Manifest::from_toml_str(content).expect("manifest parses");
        let error = manifest
            .validate_manifest_only()
            .expect_err("non-builtin kernel should fail");

        assert!(matches!(
            error,
            ManifestError::UnsupportedKernelReference { .. }
        ));
    }

    #[test]
    fn rejects_unknown_manifest_fields() {
        let content = include_str!("../../../examples/hello-world.glass/rootglass.toml");
        let content = format!("{content}\n[unexpected]\nvalue = true\n");
        assert!(Manifest::from_toml_str(&content).is_err());

        let content = include_str!("../../../examples/hello-world.glass/rootglass.toml");
        let content = content.replace("[runtime]", "[runtime]\nunknown_runtime_key = true");
        assert!(Manifest::from_toml_str(&content).is_err());
    }

    #[test]
    fn rejects_manifest_files_over_the_read_limit() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "rootglass-manifest-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&vec![b'a'; MAX_MANIFEST_BYTES as usize + 1])
            .unwrap();
        drop(file);
        assert!(matches!(
            Manifest::from_path(&path),
            Err(ManifestError::ManifestTooLarge { .. })
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_oversized_manifest_text_before_parsing() {
        let content = "x".repeat(MAX_MANIFEST_BYTES as usize + 1);
        assert!(Manifest::from_toml_str(&content).is_err());
    }
}
