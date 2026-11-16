//! UI-independent application orchestration shared by the CLI and desktop.
pub mod encrypted_sessions;
pub mod identity;
pub mod import;
pub mod registry;
pub mod state;

use rootglass_core::identity::{
    IdentityProviderError, ImageMasterKey, ProviderCapabilities, RecipientIdentityProvider,
};
use rootglass_core::runtime_assets::RuntimeAssetsResolver;
use rootglass_core::storage::{LocalWritableOverlay, WritableOverlaySelection};
use rootglass_core::{
    CoreError, HostCapabilities, LaunchRequest, LifecycleState, RootglassRuntime,
};
use rootglass_manifest::BootData;
pub use rootglass_manifest::MinimumRecipientHardware;
use rootglass_qemu::{QemuLaunchPlan, QemuProcess, SerialChunk};
use rootglass_signature::trust_config::{load_trust_store, LoadedTrustStore, TrustConfigError};
use rootglass_signature::{SignatureError, TrustStore, VerifiedArtifactReader};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Maximum time allowed for a guest to show its profile-specific ready prompt.
pub const GUEST_READY_TIMEOUT: Duration = Duration::from_secs(90);
const ERROR_SCAN_TAIL_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlassSummary {
    pub path: PathBuf,
    pub name: String,
    pub identifier: String,
    pub version: String,
    pub description: String,
    pub architecture: String,
    pub profile: String,
    pub acceleration: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
    pub signed_subject_count: usize,
}

/// Manifest and envelope claims shown while a bundle's signing key is unknown.
/// These fields are explicitly unverified until the key is trusted and the
/// archive passes the normal signature verification path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedPublisherReview {
    pub path: PathBuf,
    pub name: String,
    pub identifier: String,
    pub version: String,
    pub description: Option<String>,
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
}

/// Manifest-only validation output. Publisher fields are declarations from the manifest;
/// this report does not verify a signature or establish trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestValidationReport {
    pub path: PathBuf,
    pub name: String,
    pub identifier: String,
    pub version: String,
    pub description: Option<String>,
    pub declared_publisher_name: String,
    pub declared_publisher_identity: String,
    pub architecture: String,
    pub profile: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub rootfs: PathBuf,
    pub network_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    pub summary: GlassSummary,
    pub readiness: Vec<String>,
}

/// Public claims produced only after the publisher, recipient authority, archive layout,
/// encrypted artifact digest, and selected provider have passed preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedArtifactPreflightInfo {
    pub glass_identifier: String,
    pub glass_version: String,
    pub content_sha256: String,
    pub artifact_id: String,
    pub recipient_id: String,
    pub max_concurrent_instances: u8,
    pub minimum_recipient_hardware: MinimumRecipientHardware,
    pub provider: ProviderCapabilities,
}

/// Public recipient claims from a publisher-verified archive. Wrapped image
/// keys and other envelope payload bytes are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRecipientInfo {
    pub recipient_id: String,
    pub provider_id: String,
    pub wrapping_suite_id: String,
    pub minimum_recipient_hardware: MinimumRecipientHardware,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    Verified,
    Failed,
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationCheck {
    pub status: VerificationStatus,
    pub detail: String,
}

impl VerificationCheck {
    fn verified(detail: impl Into<String>) -> Self {
        Self {
            status: VerificationStatus::Verified,
            detail: detail.into(),
        }
    }

    fn failed(detail: impl Into<String>) -> Self {
        Self {
            status: VerificationStatus::Failed,
            detail: detail.into(),
        }
    }

    fn not_checked(detail: impl Into<String>) -> Self {
        Self {
            status: VerificationStatus::NotChecked,
            detail: detail.into(),
        }
    }
}

/// Read-only results for each independently reported encrypted-bundle check.
/// This report contains public metadata only and never unwraps recipient keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedArchiveInspection {
    pub content_sha256: Option<String>,
    pub glass_identifier: Option<String>,
    pub glass_version: Option<String>,
    pub publisher_signature: VerificationCheck,
    pub recipient_envelope: VerificationCheck,
    pub encrypted_rootfs: VerificationCheck,
    pub signed_policy: VerificationCheck,
    pub recipients: Vec<VerifiedRecipientInfo>,
    pub max_concurrent_instances: Option<u8>,
    pub minimum_recipient_hardware: Option<MinimumRecipientHardware>,
}

impl Default for EncryptedArchiveInspection {
    fn default() -> Self {
        Self {
            content_sha256: None,
            glass_identifier: None,
            glass_version: None,
            publisher_signature: VerificationCheck::not_checked("Not checked."),
            recipient_envelope: VerificationCheck::not_checked("Not checked."),
            encrypted_rootfs: VerificationCheck::not_checked("Not checked."),
            signed_policy: VerificationCheck::not_checked("Not checked."),
            recipients: Vec::new(),
            max_concurrent_instances: None,
            minimum_recipient_hardware: None,
        }
    }
}

/// A fully verified encrypted artifact with its IMK unwrapped as the final preflight step.
/// It is not cloneable or serializable; callers should pass it directly to QEMU setup.
pub struct PreparedEncryptedArtifact {
    info: EncryptedArtifactPreflightInfo,
    artifact_reader: VerifiedArtifactReader,
    image_master_key: ImageMasterKey,
}

impl PreparedEncryptedArtifact {
    pub fn info(&self) -> &EncryptedArtifactPreflightInfo {
        &self.info
    }

    pub fn artifact_reader(&mut self) -> &mut VerifiedArtifactReader {
        &mut self.artifact_reader
    }

    /// Borrow the zeroizing key wrapper for immediate QEMU secret setup.
    pub fn image_master_key(&self) -> &ImageMasterKey {
        &self.image_master_key
    }

    pub fn into_parts(
        self,
    ) -> (
        EncryptedArtifactPreflightInfo,
        VerifiedArtifactReader,
        ImageMasterKey,
    ) {
        (self.info, self.artifact_reader, self.image_master_key)
    }
}

impl std::fmt::Debug for PreparedEncryptedArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedEncryptedArtifact")
            .field("info", &self.info)
            .field("artifact_length", &self.artifact_reader.len())
            .field("image_master_key", &self.image_master_key)
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub state_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPublisherStatus {
    pub name: String,
    pub identity: String,
    pub key_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustPolicyStatus {
    pub config_path: Option<PathBuf>,
    pub trusted_publishers: Vec<TrustedPublisherStatus>,
    pub revoked_key_ids: Vec<String>,
    pub error: Option<String>,
}

/// A bounded snapshot of host-side QEMU stderr for the run Logs view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticLogSnapshot {
    pub text: String,
    pub truncated: bool,
}

/// One consistent snapshot of the selected bundle's writable states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateStoreSnapshot {
    pub records: Vec<state::StateRecord>,
    pub locked: bool,
    pub selected_state_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    StateChanged {
        run: RunId,
        state: LifecycleState,
    },
    SerialChunk {
        run: RunId,
        bytes: Vec<u8>,
        truncated: bool,
    },
    GuestReady {
        run: RunId,
        evidence: String,
    },
    Exited {
        run: RunId,
        exit_code: Option<i32>,
        expected: bool,
    },
    Failed {
        run: RunId,
        phase: LifecycleState,
        summary: String,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppError {
    pub phase: LifecycleState,
    pub summary: String,
    pub detail: String,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\n{}", self.summary, self.detail)
    }
}
impl std::error::Error for AppError {}

struct ActiveRun {
    id: RunId,
    encrypted_scope: Option<String>,
    process: QemuProcess,
    state: LifecycleState,
    login_seen: bool,
    require_login_prompt: bool,
    login_marker_tail: Vec<u8>,
    diagnostic_tail: Vec<u8>,
    ready_deadline: Instant,
    _state_lock: Option<state::StateRunLock>,
    _encrypted_session: Option<encrypted_sessions::EncryptedSessionLease>,
}

/// Cloneable library handle for background archive verification and registry work.
#[derive(Clone)]
pub struct PackageLibrary {
    root: PathBuf,
    trust_store: TrustStore,
    revoked_key_ids: BTreeSet<String>,
}

impl PackageLibrary {
    fn validate_installed_package_path(
        &self,
        installed: &registry::InstalledGlass,
    ) -> Result<(), AppError> {
        let expected_package = self.root.join("packages").join(&installed.content_sha256);
        if installed.package_path != expected_package {
            return Err(AppError {
                phase: LifecycleState::Verifying,
                summary: "The installed-glass registry contains an invalid package path."
                    .to_string(),
                detail: installed.package_path.display().to_string(),
            });
        }
        Ok(())
    }

    pub fn import_archive(
        &self,
        source: impl AsRef<Path>,
    ) -> Result<registry::InstalledGlass, AppError> {
        let registry_path = self.root.join("registry.json");
        let mut registry = registry::PackageRegistry::open(&registry_path).map_err(|error| {
            import_app_error("The installed-glass registry could not be opened.", error)
        })?;
        import::import_glass_archive(
            source,
            &self.root,
            &mut registry,
            &self.trust_store,
            &import::ImportLimits::default(),
        )
        .map_err(|error| match &error {
            import::ImportError::Signature(SignatureError::UnknownKey(key_id))
                if self.revoked_key_ids.contains(key_id) =>
            {
                revoked_key_app_error(key_id)
            }
            _ => import_app_error(
                "The .glass archive could not be verified or installed.",
                error,
            ),
        })
    }

    /// Open a verified encrypted disk from the immutable archive paired with an installed glass.
    pub fn open_encrypted_artifact_reader(
        &self,
        installed: &registry::InstalledGlass,
        artifact_id: &str,
    ) -> Result<rootglass_signature::VerifiedArtifactReader, AppError> {
        if self.revoked_key_ids.contains(&installed.key_id) {
            return Err(revoked_key_app_error(&installed.key_id));
        }
        self.validate_installed_package_path(installed)?;
        crate::import::open_installed_encrypted_artifact(
            installed,
            artifact_id,
            &self.trust_store,
            &crate::import::ImportLimits::default(),
        )
        .map_err(|error| import_app_error("The encrypted artifact could not be opened.", error))
    }

    /// Verify the installed archive and recipient authority before returning
    /// public recipient IDs. The encrypted wrapped key bytes are never exposed.
    pub fn inspect_encrypted_recipients(
        &self,
        installed: &registry::InstalledGlass,
    ) -> Result<Vec<VerifiedRecipientInfo>, AppError> {
        if self.revoked_key_ids.contains(&installed.key_id) {
            return Err(revoked_key_app_error(&installed.key_id));
        }
        self.validate_installed_package_path(installed)?;
        let verified = import::open_installed_verified_archive(
            installed,
            &self.trust_store,
            &import::ImportLimits::default(),
        )
        .map_err(|error| {
            import_app_error(
                "The encrypted glass could not be verified for recipient inspection.",
                error,
            )
        })?;
        let envelope = verified.verify_recipient_envelope().map_err(|error| {
            import_app_error("The recipient authorization could not be verified.", error)
        })?;
        Ok(envelope
            .payload()
            .recipients
            .iter()
            .map(|recipient| VerifiedRecipientInfo {
                recipient_id: recipient.recipient_id.clone(),
                provider_id: recipient.provider_id.id().to_string(),
                wrapping_suite_id: recipient.wrapping_suite_id.id().to_string(),
                minimum_recipient_hardware: envelope.minimum_recipient_hardware().clone(),
            })
            .collect())
    }

    /// Verify a signed encrypted archive and return public recipient metadata
    /// without adding the archive to the installed package library.
    pub fn inspect_encrypted_archive_recipients(
        &self,
        source: impl AsRef<Path>,
    ) -> Result<Vec<VerifiedRecipientInfo>, AppError> {
        let (report, envelope) = import::open_archive_verified_recipient_envelope(
            source,
            &self.trust_store,
            &import::ImportLimits::default(),
        )
        .map_err(|error| match &error {
            import::ImportError::Signature(SignatureError::UnknownKey(key_id))
                if self.revoked_key_ids.contains(key_id) =>
            {
                revoked_key_app_error(key_id)
            }
            _ => import_app_error(
                "The encrypted glass could not be verified for recipient inspection.",
                error,
            ),
        })?;
        if self.revoked_key_ids.contains(&report.key_id) {
            return Err(revoked_key_app_error(&report.key_id));
        }
        Ok(envelope
            .payload()
            .recipients
            .iter()
            .map(|recipient| VerifiedRecipientInfo {
                recipient_id: recipient.recipient_id.clone(),
                provider_id: recipient.provider_id.id().to_string(),
                wrapping_suite_id: recipient.wrapping_suite_id.id().to_string(),
                minimum_recipient_hardware: envelope.minimum_recipient_hardware().clone(),
            })
            .collect())
    }

    /// Verify publisher, recipient, encrypted-rootfs, and signed-policy claims
    /// without installing the archive or unwrapping any recipient key.
    pub fn inspect_encrypted_archive(
        &self,
        source: impl AsRef<Path>,
    ) -> EncryptedArchiveInspection {
        let source = source.as_ref();
        let mut inspection = EncryptedArchiveInspection::default();
        let archive = match import::open_archive_verified(
            source,
            &self.trust_store,
            &import::ImportLimits::default(),
        ) {
            Ok(archive) => archive,
            Err(error) => {
                inspection.publisher_signature =
                    VerificationCheck::failed(format!("Publisher verification failed: {error}"));
                inspection.recipient_envelope = VerificationCheck::not_checked(
                    "Recipient authority cannot be checked until publisher verification passes.",
                );
                inspection.encrypted_rootfs = VerificationCheck::not_checked(
                    "Encrypted rootfs cannot be checked until publisher verification passes.",
                );
                inspection.signed_policy = VerificationCheck::not_checked(
                    "Signed policy cannot be checked until publisher verification passes.",
                );
                return inspection;
            }
        };
        inspection.content_sha256 = import::archive_content_sha256(source).ok();
        let report = archive.report();
        if self.revoked_key_ids.contains(&report.key_id) {
            inspection.publisher_signature = VerificationCheck::failed(format!(
                "Publisher signing key '{}' is revoked by the local trust policy.",
                report.key_id
            ));
            inspection.recipient_envelope = VerificationCheck::not_checked(
                "Recipient authority cannot be checked after publisher revocation.",
            );
            inspection.encrypted_rootfs = VerificationCheck::not_checked(
                "Encrypted rootfs cannot be checked after publisher revocation.",
            );
            inspection.signed_policy = VerificationCheck::not_checked(
                "Signed policy cannot be checked after publisher revocation.",
            );
            return inspection;
        }
        inspection.glass_identifier = Some(report.glass_identifier.clone());
        inspection.glass_version = Some(report.glass_version.clone());
        inspection.publisher_signature = VerificationCheck::verified(format!(
            "Publisher signature and {} signed archive member(s) verified.",
            report.subject_count
        ));

        match archive.verify_recipient_envelope() {
            Ok(envelope) => {
                let recipients = envelope
                    .payload()
                    .recipients
                    .iter()
                    .map(|recipient| VerifiedRecipientInfo {
                        recipient_id: recipient.recipient_id.clone(),
                        provider_id: recipient.provider_id.id().to_string(),
                        wrapping_suite_id: recipient.wrapping_suite_id.id().to_string(),
                        minimum_recipient_hardware: envelope.minimum_recipient_hardware().clone(),
                    })
                    .collect::<Vec<_>>();
                inspection.recipient_envelope = VerificationCheck::verified(format!(
                    "Recipient authority authenticated; {} public recipient(s) listed.",
                    recipients.len()
                ));
                inspection.recipients = recipients;
            }
            Err(error) => {
                inspection.recipient_envelope = VerificationCheck::failed(format!(
                    "Recipient-envelope authentication failed: {error}"
                ));
            }
        }

        let Some(catalog) = report.artifact_catalog.as_ref() else {
            inspection.encrypted_rootfs = VerificationCheck::not_checked(
                "The publisher-verified archive has no schema-2 artifact catalog.",
            );
            inspection.signed_policy = VerificationCheck::not_checked(
                "The publisher-verified archive has no signed encryption policy.",
            );
            return inspection;
        };

        let rootfs_ids = catalog
            .profiles
            .values()
            .flat_map(|profile| profile.nodes.iter())
            .filter_map(|node| match &node.boot {
                BootData::Linux {
                    rootfs_artifact, ..
                } => Some(rootfs_artifact.clone()),
                BootData::Mcu { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        if rootfs_ids.is_empty() {
            inspection.encrypted_rootfs = VerificationCheck::failed(
                "The signed catalog does not declare a Linux rootfs artifact.",
            );
        } else {
            let mut unencrypted = Vec::new();
            let mut invalid = Vec::new();
            for artifact_id in &rootfs_ids {
                if !catalog
                    .artifacts
                    .get(artifact_id)
                    .is_some_and(|artifact| artifact.encryption.is_some())
                {
                    unencrypted.push(artifact_id.clone());
                    continue;
                }
                if archive.open_encrypted_artifact(artifact_id).is_err() {
                    invalid.push(artifact_id.clone());
                }
            }
            inspection.encrypted_rootfs = if !invalid.is_empty() {
                VerificationCheck::failed(format!(
                    "Encrypted rootfs artifact range(s) are invalid: {}.",
                    invalid.join(", ")
                ))
            } else if !unencrypted.is_empty() {
                VerificationCheck::failed(format!(
                    "Linux rootfs artifact(s) are not encrypted: {}.",
                    unencrypted.join(", ")
                ))
            } else {
                VerificationCheck::verified(format!(
                    "{} Linux rootfs artifact(s) are declared encrypted in the signed catalog.",
                    rootfs_ids.len()
                ))
            };
        }

        inspection.signed_policy = match catalog.encryption.as_ref() {
            None => {
                VerificationCheck::failed("The signed artifact catalog has no encryption policy.")
            }
            Some(policy) => {
                inspection.max_concurrent_instances = Some(policy.max_concurrent_instances);
                inspection.minimum_recipient_hardware =
                    Some(policy.minimum_recipient_hardware.clone());
                if !(1..=encrypted_sessions::MAX_ENCRYPTED_INSTANCES)
                    .contains(&policy.max_concurrent_instances)
                {
                    VerificationCheck::failed(format!(
                        "Signed instance limit {} is outside the supported range 1–{}.",
                        policy.max_concurrent_instances,
                        encrypted_sessions::MAX_ENCRYPTED_INSTANCES
                    ))
                } else if policy.recipient_authorities.is_empty() {
                    VerificationCheck::failed(
                        "The signed encryption policy has no recipient authority.",
                    )
                } else {
                    let hardware = match policy.minimum_recipient_hardware {
                        MinimumRecipientHardware::SoftwareAllowed => "software identities allowed",
                        MinimumRecipientHardware::HardwareBackedRequired => {
                            "hardware-backed identity required"
                        }
                    };
                    VerificationCheck::verified(format!(
                        "Signed policy allows up to {} concurrent instance(s); {hardware}.",
                        policy.max_concurrent_instances
                    ))
                }
            }
        };

        inspection
    }

    /// Verify an installed encrypted artifact and unwrap its selected recipient key only after
    /// publisher trust, signed-subject digests, ZIP layout, recipient authority, and provider
    /// capability have all passed. This method does not start QEMU.
    pub fn preflight_encrypted_artifact(
        &self,
        installed: &registry::InstalledGlass,
        artifact_id: &str,
        recipient_id: &str,
        provider: &dyn RecipientIdentityProvider,
    ) -> Result<PreparedEncryptedArtifact, AppError> {
        if self.revoked_key_ids.contains(&installed.key_id) {
            return Err(revoked_key_app_error(&installed.key_id));
        }
        self.validate_installed_package_path(installed)?;

        let verified = import::open_installed_verified_archive(
            installed,
            &self.trust_store,
            &import::ImportLimits::default(),
        )
        .map_err(|error| {
            import_app_error(
                "The encrypted glass could not be verified for preflight.",
                error,
            )
        })?;
        let report = verified.report();
        let catalog = report.artifact_catalog.as_ref().ok_or_else(|| AppError {
            phase: LifecycleState::Verifying,
            summary: "The installed glass has no verified encrypted artifact catalog.".to_string(),
            detail: "Encrypted preflight requires a publisher-verified schema-2 artifact catalog."
                .to_string(),
        })?;
        let artifact = catalog.artifacts.get(artifact_id).ok_or_else(|| AppError {
            phase: LifecycleState::Verifying,
            summary: "The selected encrypted artifact is not in this glass.".to_string(),
            detail: format!("Artifact '{artifact_id}' is not declared by the signed catalog."),
        })?;
        if artifact.encryption.is_none() {
            return Err(AppError {
                phase: LifecycleState::Verifying,
                summary: "The selected artifact is not encrypted.".to_string(),
                detail: format!("Artifact '{artifact_id}' has no signed encryption declaration."),
            });
        }
        let max_concurrent_instances = catalog
            .encryption
            .as_ref()
            .map(|policy| policy.max_concurrent_instances)
            .filter(|limit| (1..=encrypted_sessions::MAX_ENCRYPTED_INSTANCES).contains(limit))
            .ok_or_else(|| AppError {
                phase: LifecycleState::Verifying,
                summary: "The encrypted glass has an invalid instance policy.".to_string(),
                detail: "The signed concurrent instance limit must be between 1 and 5.".to_string(),
            })?;

        // Opening the recipient sidecar and disk range happens only after the full publisher
        // subject set has been verified by `open_installed_verified_archive`.
        let envelope = verified.verify_recipient_envelope().map_err(|error| {
            import_app_error("The recipient authorization could not be verified.", error)
        })?;
        let artifact_reader = verified
            .open_encrypted_artifact(artifact_id)
            .map_err(|error| import_app_error("The encrypted disk layout is invalid.", error))?;
        let recipient = envelope
            .recipient(recipient_id)
            .ok_or_else(|| identity_provider_app_error(IdentityProviderError::RecipientNotFound))?;

        let capabilities = provider.capabilities();
        if !capabilities.can_unwrap
            || capabilities.provider_id != recipient.provider_id
            || capabilities.wrapping_suite_id != recipient.wrapping_suite_id
        {
            return Err(identity_provider_app_error(
                IdentityProviderError::UnsupportedRecipient,
            ));
        }
        if !capabilities.satisfies(envelope.minimum_recipient_hardware()) {
            return Err(identity_provider_app_error(
                IdentityProviderError::HardwarePolicyUnsatisfied,
            ));
        }

        // Hardware policy and provider-suite checks precede all key-store access.
        let identities = provider
            .list_identities()
            .map_err(identity_provider_app_error)?;
        if !identities.iter().any(|identity| {
            identity.recipient_id == recipient_id
                && identity.provider_id == capabilities.provider_id
                && identity.wrapping_suite_id == capabilities.wrapping_suite_id
        }) {
            return Err(identity_provider_app_error(
                IdentityProviderError::IdentityNotFound,
            ));
        }

        // The provider receives an envelope whose authority, manifest binding, and hardware
        // requirement are already verified. Key unwrap is the final preflight operation.
        let image_master_key = provider
            .unwrap_image_key(&envelope, recipient_id)
            .map_err(identity_provider_app_error)?;

        Ok(PreparedEncryptedArtifact {
            info: EncryptedArtifactPreflightInfo {
                glass_identifier: report.glass_identifier.clone(),
                glass_version: report.glass_version.clone(),
                content_sha256: installed.content_sha256.clone(),
                artifact_id: artifact_id.to_string(),
                recipient_id: recipient_id.to_string(),
                max_concurrent_instances,
                minimum_recipient_hardware: envelope.minimum_recipient_hardware().clone(),
                provider: capabilities,
            },
            artifact_reader,
            image_master_key,
        })
    }

    pub fn installed_glasses(&self) -> Result<Vec<registry::InstalledGlass>, AppError> {
        let registry =
            registry::PackageRegistry::open(self.root.join("registry.json")).map_err(|error| {
                import_app_error("The installed-glass registry could not be opened.", error)
            })?;
        let packages = registry.packages().to_vec();
        if packages.is_empty() {
            return Ok(packages);
        }
        let packages_root = self.root.join("packages");
        let packages_metadata = fs::symlink_metadata(&packages_root).map_err(|error| AppError {
            phase: LifecycleState::Verifying,
            summary: "The managed package store could not be opened.".to_string(),
            detail: error.to_string(),
        })?;
        if packages_metadata.file_type().is_symlink() || !packages_metadata.is_dir() {
            return Err(AppError {
                phase: LifecycleState::Verifying,
                summary: "The managed package store is not a regular directory.".to_string(),
                detail: packages_root.display().to_string(),
            });
        }
        for package in &packages {
            let expected = self.root.join("packages").join(&package.content_sha256);
            if package.package_path != expected {
                return Err(AppError {
                    phase: LifecycleState::Verifying,
                    summary: "The installed-glass registry contains an invalid package path."
                        .to_string(),
                    detail: format!(
                        "The registry path '{}' does not match the managed package store.",
                        package.package_path.display()
                    ),
                });
            }
            let metadata = fs::symlink_metadata(&expected).map_err(|error| AppError {
                phase: LifecycleState::Verifying,
                summary: "An installed package is missing from the managed store.".to_string(),
                detail: error.to_string(),
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(AppError {
                    phase: LifecycleState::Verifying,
                    summary: "An installed package is not a regular directory.".to_string(),
                    detail: expected.display().to_string(),
                });
            }
        }
        Ok(packages)
    }

    pub fn remove_installed_glass(
        &self,
        content_sha256: &str,
    ) -> Result<Option<registry::InstalledGlass>, AppError> {
        let registry_path = self.root.join("registry.json");
        let mut registry = registry::PackageRegistry::open(&registry_path).map_err(|error| {
            import_app_error("The installed-glass registry could not be opened.", error)
        })?;
        registry.remove(content_sha256).map_err(|error| {
            import_app_error("The installed-glass registry could not be updated.", error)
        })
    }
}

/// Owns validation, legacy single-run launches, and signed-limit encrypted sessions.
pub struct AppService {
    runtime: RootglassRuntime,
    state_root: PathBuf,
    library_root: PathBuf,
    trust_store: TrustStore,
    revoked_key_ids: BTreeSet<String>,
    trust_config_path: Option<PathBuf>,
    trust_load_error: Option<String>,
    next_run_id: u64,
    active: Option<ActiveRun>,
    encrypted_runs: HashMap<RunId, ActiveRun>,
    encrypted_instance_limit: Option<u8>,
    poll_cursor: Option<RunId>,
    pending: VecDeque<RuntimeEvent>,
    last_stopped: HashSet<RunId>,
}

impl Default for AppService {
    fn default() -> Self {
        Self::with_trust_store(TrustStore::example_fixture())
    }
}

impl AppService {
    /// Construct a service with an explicit trust policy. Production callers should load a
    /// local publisher trust configuration and pass its verifier-compatible store here.
    pub fn with_trust_store(trust_store: TrustStore) -> Self {
        Self {
            runtime: RootglassRuntime::with_trust_store(trust_store.clone()),
            state_root: default_state_root(),
            library_root: default_library_root(),
            trust_store,
            revoked_key_ids: BTreeSet::new(),
            trust_config_path: None,
            trust_load_error: None,
            next_run_id: 1,
            active: None,
            encrypted_runs: HashMap::new(),
            encrypted_instance_limit: None,
            poll_cursor: None,
            pending: VecDeque::new(),
            last_stopped: HashSet::new(),
        }
    }

    /// Construct a service from a validated local production trust configuration.
    pub fn with_loaded_trust_store(loaded: LoadedTrustStore) -> Self {
        let revoked_key_ids = loaded.revoked_key_ids().map(str::to_owned).collect();
        let mut service = Self::with_trust_store(loaded.trust_store().clone());
        service.revoked_key_ids = revoked_key_ids;
        service
    }

    /// Use integrity-checked, versioned runtime assets for packaged launches.
    pub fn with_verified_runtime_assets(
        mut self,
        resolver: RuntimeAssetsResolver,
        runtime_version: impl Into<String>,
    ) -> Self {
        self.runtime = RootglassRuntime::with_trust_store(self.trust_store.clone())
            .with_verified_runtime_assets(resolver, runtime_version);
        self
    }

    /// Configure the versioned runtime assets bundled beside a packaged app.
    ///
    /// Package builds set `ROOTGLASS_RUNTIME_VERSION`; developer builds leave it
    /// unset and retain the explicit system-QEMU development behavior.
    pub fn with_packaged_runtime_assets(self) -> Self {
        let Some(runtime_version) = option_env!("ROOTGLASS_RUNTIME_VERSION") else {
            return self;
        };
        self.with_verified_runtime_assets(
            RuntimeAssetsResolver::new(packaged_resource_root(), app_data_root()),
            runtime_version,
        )
    }

    /// Load and use a local production trust configuration.
    pub fn from_trust_config(path: impl AsRef<Path>) -> Result<Self, TrustConfigError> {
        let path = path.as_ref().to_path_buf();
        load_trust_store(&path)
            .map(Self::with_loaded_trust_store)
            .map(|service| service.with_trust_config_path(path))
    }

    /// Construct a default-deny service for callers that could not load trust policy.
    pub fn deny_all() -> Self {
        Self::with_trust_store(TrustStore::default())
    }

    /// Attach the source and error shown when a local trust policy could not be loaded.
    pub fn with_trust_load_error(
        mut self,
        path: Option<PathBuf>,
        error: impl Into<String>,
    ) -> Self {
        self.trust_config_path = path;
        self.trust_load_error = Some(error.into());
        self
    }

    fn with_trust_config_path(mut self, path: PathBuf) -> Self {
        self.trust_config_path = Some(path);
        self.trust_load_error = None;
        self
    }

    /// Return the active local trust policy without exposing public-key material.
    pub fn trust_policy_status(&self) -> TrustPolicyStatus {
        TrustPolicyStatus {
            config_path: self.trust_config_path.clone(),
            trusted_publishers: self
                .trust_store
                .publishers()
                .map(|publisher| TrustedPublisherStatus {
                    name: publisher.name.clone(),
                    identity: publisher.identity.clone(),
                    key_id: publisher.key_id.clone(),
                })
                .collect(),
            revoked_key_ids: self.revoked_key_ids.iter().cloned().collect(),
            error: self.trust_load_error.clone(),
        }
    }

    /// List named writable states for a verified glass bundle.
    pub fn list_states(&mut self, source: &Path) -> Result<Vec<state::StateRecord>, AppError> {
        self.state_store_for(source)?
            .list()
            .map_err(state_app_error)
    }

    /// Read records, lock status, and selected state after preparing the bundle once.
    pub fn state_snapshot(&mut self, source: &Path) -> Result<StateStoreSnapshot, AppError> {
        let store = self.state_store_for(source)?;
        let records = store.list().map_err(state_app_error)?;
        let locked = store.is_in_use();
        let selected_state_id = store.selected_state_id().map_err(state_app_error)?;
        Ok(StateStoreSnapshot {
            records,
            locked,
            selected_state_id,
        })
    }

    /// Advisory lock status for disabling TUI state actions while a guest owns the store.
    /// Each mutating store call still reacquires the lock to handle races safely.
    pub fn states_in_use(&mut self, source: &Path) -> Result<bool, AppError> {
        Ok(self.state_store_for(source)?.is_in_use())
    }

    /// Return the state selected for this glass, defaulting to `default`.
    pub fn selected_state_id(&mut self, source: &Path) -> Result<String, AppError> {
        self.state_store_for(source)?
            .selected_state_id()
            .map_err(state_app_error)
    }

    /// Persist the state selected for future boots and restarts.
    pub fn select_state(&mut self, source: &Path, id: &str) -> Result<String, AppError> {
        self.state_store_for(source)?
            .select(id)
            .map_err(state_app_error)
    }

    /// Create a named copy of the immutable state template.
    pub fn create_state(
        &mut self,
        source: &Path,
        name: &str,
    ) -> Result<state::StateRecord, AppError> {
        self.state_store_for(source)?
            .create(name)
            .map_err(state_app_error)
    }

    /// Clone an existing named state into an independent image.
    pub fn clone_state(
        &mut self,
        source: &Path,
        source_id: &str,
        new_name: &str,
    ) -> Result<state::StateRecord, AppError> {
        self.state_store_for(source)?
            .clone_state(source_id, new_name)
            .map_err(state_app_error)
    }

    /// Replace a named state with the immutable template.
    pub fn reset_state(&mut self, source: &Path, id: &str) -> Result<state::StateRecord, AppError> {
        self.state_store_for(source)?
            .reset(id)
            .map_err(state_app_error)
    }

    /// Delete a named state image.
    pub fn delete_state(&mut self, source: &Path, id: &str) -> Result<(), AppError> {
        self.state_store_for(source)?
            .delete(id)
            .map_err(state_app_error)
    }

    fn state_store_for(&mut self, source: &Path) -> Result<state::InstanceStateStore, AppError> {
        let launch = self.prepare(source)?;
        let template = launch
            .storage
            .writable_overlay
            .as_ref()
            .map(|overlay| overlay.path().to_path_buf())
            .ok_or_else(|| {
                state_app_error(state::StateError::InvalidImage(
                    "this glass has no writable-state template".into(),
                ))
            })?;
        let store_root = self.state_root.join(safe_state_namespace(&launch));
        Ok(state::InstanceStateStore::new(store_root, template))
    }

    /// Override the per-user location used for local writable state images.
    pub fn with_state_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.state_root = root.into();
        self
    }

    /// Set a local encrypted-instance preference. The signed glass policy can
    /// always lower this value, and values outside the supported 1–5 range fail.
    pub fn with_encrypted_instance_limit(mut self, limit: u8) -> Result<Self, AppError> {
        effective_encrypted_instance_limit(
            encrypted_sessions::MAX_ENCRYPTED_INSTANCES,
            Some(limit),
        )?;
        self.encrypted_instance_limit = Some(limit);
        Ok(self)
    }

    /// Override the per-user package and registry location.
    pub fn with_library_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.library_root = root.into();
        self
    }

    /// Return a cloneable handle suitable for off-thread library work.
    pub fn package_library(&self) -> PackageLibrary {
        PackageLibrary {
            root: self.library_root.clone(),
            trust_store: self.trust_store.clone(),
            revoked_key_ids: self.revoked_key_ids.clone(),
        }
    }

    /// Verify and install a bounded ZIP `.glass` archive into the local library.
    pub fn import_archive(
        &self,
        source: impl AsRef<Path>,
    ) -> Result<registry::InstalledGlass, AppError> {
        self.package_library().import_archive(source)
    }

    /// Inspect bounded archive metadata for a publisher review screen without
    /// importing the bundle. A key listed as revoked remains a hard failure.
    pub fn review_archive(
        &self,
        source: impl AsRef<Path>,
    ) -> Result<Option<UntrustedPublisherReview>, AppError> {
        let review = import::review_untrusted_glass_archive(
            source,
            &self.trust_store,
            &import::ImportLimits::default(),
        )
        .map_err(|error| match &error {
            import::ImportError::Signature(SignatureError::UnknownKey(key_id))
                if self.revoked_key_ids.contains(key_id) =>
            {
                revoked_key_app_error(key_id)
            }
            _ => import_app_error("The .glass archive could not be reviewed.", error),
        })?;
        if let Some(review) = review
            .as_ref()
            .filter(|review| self.revoked_key_ids.contains(&review.key_id))
        {
            return Err(revoked_key_app_error(&review.key_id));
        }
        Ok(review)
    }

    /// Return the persisted verified releases in the local library.
    pub fn installed_glasses(&self) -> Result<Vec<registry::InstalledGlass>, AppError> {
        self.package_library().installed_glasses()
    }

    /// Remove a release from the visible library. Its immutable content may remain on disk
    /// until a later garbage-collection pass.
    pub fn remove_installed_glass(
        &self,
        content_sha256: &str,
    ) -> Result<Option<registry::InstalledGlass>, AppError> {
        self.package_library()
            .remove_installed_glass(content_sha256)
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Validate only the manifest schema and declared fields, matching the CLI `validate`
    /// command. This deliberately does not verify the bundle signature or publisher trust.
    pub fn validate_manifest_only(
        &self,
        source: &Path,
    ) -> Result<ManifestValidationReport, AppError> {
        let glass = rootglass_manifest::Glass::load(source).map_err(|error| AppError {
            phase: LifecycleState::Validating,
            summary: "The manifest could not be loaded.".to_string(),
            detail: error.to_string(),
        })?;
        glass.validate_manifest_only().map_err(|error| AppError {
            phase: LifecycleState::Validating,
            summary: "The manifest is invalid.".to_string(),
            detail: error.to_string(),
        })?;
        let manifest = glass.manifest;
        Ok(ManifestValidationReport {
            path: glass.root,
            name: manifest.name,
            identifier: manifest.identifier,
            version: manifest.version,
            description: manifest.description,
            declared_publisher_name: manifest.publisher.name,
            declared_publisher_identity: manifest.publisher.identity,
            architecture: manifest.runtime.architecture,
            profile: manifest.runtime.profile,
            memory_mb: manifest.resources.memory_mb,
            cpus: manifest.resources.cpus,
            rootfs: PathBuf::from(manifest.boot.rootfs),
            network_mode: manifest.network.mode,
        })
    }

    pub fn inspect(&mut self, source: &Path) -> Result<GlassSummary, AppError> {
        let launch = self.prepare(source)?;
        summarize(source, &launch)
    }

    /// Build a developer launch plan without requiring generated disk images.
    pub fn launch_plan(&mut self, source: &Path) -> Result<(GlassSummary, String), AppError> {
        let mut launch = self.prepare(source)?;
        let summary = summarize(source, &launch)?;
        if let Some(template) = launch
            .storage
            .writable_overlay
            .as_ref()
            .map(|overlay| overlay.path().to_path_buf())
        {
            let store_root = self.state_root.join(safe_state_namespace(&launch));
            let store = state::InstanceStateStore::new(store_root, template);
            let path = store.path_for("default").map_err(state_app_error)?;
            launch.writable_overlay = Some(path.clone());
            launch.storage.writable_overlay =
                Some(WritableOverlaySelection::Local(LocalWritableOverlay {
                    name: "default".to_string(),
                    path,
                }));
        }
        let command = QemuLaunchPlan::for_launch_request(&launch).command_line();
        Ok((summary, command))
    }

    pub fn preflight(&mut self, source: &Path) -> Result<PreflightReport, AppError> {
        let launch = self.prepare(source)?;
        validate_assets(&launch).map_err(|detail| AppError {
            phase: LifecycleState::Ready,
            summary: "The glass runtime assets are incomplete.".to_string(),
            detail,
        })?;
        Ok(PreflightReport {
            summary: summarize(source, &launch)?,
            readiness: vec!["Manifest, signature, and required files verified.".to_string()],
        })
    }

    pub fn start(&mut self, source: &Path, options: RunOptions) -> Result<RunId, AppError> {
        self.start_with_readiness(source, options, true)
    }

    /// Start a verified guest for a headless console without requiring the
    /// profile-specific login prompt used by desktop readiness reporting.
    pub fn start_headless(&mut self, source: &Path) -> Result<RunId, AppError> {
        self.start_with_readiness(source, RunOptions::default(), false)
    }

    /// Start an encrypted rootfs after callers have derived `launch` from the
    /// publisher-verified schema-2 profile and completed recipient preflight.
    /// Each run owns an independently keyed ephemeral overlay and QEMU session.
    pub fn start_prepared_encrypted(
        &mut self,
        launch: &LaunchRequest,
        prepared: PreparedEncryptedArtifact,
    ) -> Result<RunId, AppError> {
        if self.active.is_some() {
            return Err(AppError {
                phase: LifecycleState::Starting,
                summary: "A legacy glass is already running.".to_string(),
                detail: "Stop the active run before starting an encrypted instance.".to_string(),
            });
        }
        let info = prepared.info().clone();
        let limit = effective_encrypted_instance_limit(
            info.max_concurrent_instances,
            self.encrypted_instance_limit,
        )?;
        let active_for_glass = self
            .encrypted_runs
            .values()
            .filter(|run| run.encrypted_scope.as_deref() == Some(&info.content_sha256))
            .count();
        if active_for_glass >= usize::from(limit) {
            return Err(AppError {
                phase: LifecycleState::Starting,
                summary: "The encrypted glass has reached its instance limit.".to_string(),
                detail: format!(
                    "Stop one of its {limit} active instances before starting another."
                ),
            });
        }
        let plan =
            QemuLaunchPlan::for_encrypted_launch_request(launch).map_err(|error| AppError {
                phase: LifecycleState::Starting,
                summary: "The encrypted launch configuration is invalid.".to_string(),
                detail: error.to_string(),
            })?;
        let session_root = self
            .state_root
            .join("encrypted-sessions")
            .join(&info.content_sha256);
        encrypted_sessions::EncryptedSessionStore::recover_all(
            &self.state_root.join("encrypted-sessions"),
        )
        .map_err(encrypted_session_app_error)?;
        let mut session = encrypted_sessions::EncryptedSessionStore::new(&session_root)
            .reserve(limit)
            .map_err(encrypted_session_app_error)?;
        let (_, artifact_reader, image_master_key) = prepared.into_parts();
        let artifact_length = artifact_reader.len();
        let mut overlay_key_bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut overlay_key_bytes[..]).map_err(|_| AppError {
            phase: LifecycleState::Starting,
            summary: "The encrypted instance key could not be generated.".to_string(),
            detail: "The operating system random number generator is unavailable.".to_string(),
        })?;
        let overlay_key = ImageMasterKey::new(overlay_key_bytes);
        let process = plan
            .spawn_encrypted_overlay_supervised(
                artifact_reader,
                artifact_length,
                image_master_key,
                overlay_key,
                &session_root,
                |pid, directory| {
                    let result = match pid {
                        None => session.record_session_dir(directory),
                        Some(pid) => session.record_qemu(pid, directory),
                    };
                    result.map_err(|error| std::io::Error::other(error.to_string()))
                },
            )
            .map_err(|error| AppError {
                phase: LifecycleState::Starting,
                summary: "The encrypted guest could not be started.".to_string(),
                detail: error.to_string(),
            })?;
        let id = RunId(self.next_run_id);
        self.next_run_id = self.next_run_id.wrapping_add(1).max(1);
        self.pending.push_back(RuntimeEvent::StateChanged {
            run: id.clone(),
            state: LifecycleState::Booting,
        });
        self.encrypted_runs.insert(
            id.clone(),
            ActiveRun {
                id: id.clone(),
                encrypted_scope: Some(info.content_sha256),
                process,
                state: LifecycleState::Booting,
                login_seen: false,
                // Schema-2 profiles do not yet declare readiness evidence. E11/E12
                // will connect profile-specific readiness without guessing from
                // generic serial output.
                require_login_prompt: false,
                login_marker_tail: Vec::new(),
                diagnostic_tail: Vec::new(),
                ready_deadline: Instant::now() + GUEST_READY_TIMEOUT,
                _state_lock: None,
                _encrypted_session: Some(session),
            },
        );
        Ok(id)
    }

    /// Import a trusted encrypted ZIP archive, select one eligible local
    /// recipient, prepare its signed default profile, and start it headlessly.
    pub fn start_encrypted_archive(
        &mut self,
        source: impl AsRef<Path>,
        recipient_id: Option<&str>,
        provider: &dyn RecipientIdentityProvider,
    ) -> Result<RunId, AppError> {
        let installed = self.import_archive(source)?;
        let library = self.package_library();
        let (launch, prepared) =
            self.prepare_encrypted_boot(&library, &installed, recipient_id, provider)?;
        self.start_prepared_encrypted(&launch, prepared)
    }

    /// List encrypted instances owned by live Rootglass processes for this user.
    pub fn list_encrypted_instances(
        &self,
    ) -> Result<Vec<encrypted_sessions::EncryptedInstanceInfo>, AppError> {
        let root = self.state_root.join("encrypted-sessions");
        encrypted_sessions::EncryptedSessionStore::recover_all(&root)
            .map_err(encrypted_session_app_error)?;
        encrypted_sessions::EncryptedSessionStore::list_all(&root)
            .map_err(encrypted_session_app_error)
    }

    /// Request a different Rootglass process to stop its encrypted QEMU run.
    /// This uses the private session mailbox serviced by the owner process.
    pub fn request_stop_encrypted_instance(
        &self,
        run_id: &str,
        timeout: Duration,
    ) -> Result<(), AppError> {
        let root = self.state_root.join("encrypted-sessions");
        encrypted_sessions::EncryptedSessionStore::request_stop(&root, run_id, timeout)
            .map_err(encrypted_session_app_error)
    }

    fn start_with_readiness(
        &mut self,
        source: &Path,
        options: RunOptions,
        require_login_prompt: bool,
    ) -> Result<RunId, AppError> {
        if self.active.is_some() || !self.encrypted_runs.is_empty() {
            return Err(AppError {
                phase: LifecycleState::Starting,
                summary: "Another glass is already running.".to_string(),
                detail: "Stop the active run before starting another.".to_string(),
            });
        }
        let (launch, state_lock) = self.prepare_for_run(source, options)?;
        let plan = QemuLaunchPlan::for_launch_request(&launch);
        let process = plan.spawn_supervised().map_err(|error| AppError {
            phase: LifecycleState::Starting,
            summary: "QEMU could not be started.".to_string(),
            detail: error.to_string(),
        })?;
        let id = RunId(self.next_run_id);
        self.next_run_id = self.next_run_id.wrapping_add(1).max(1);
        self.pending.push_back(RuntimeEvent::StateChanged {
            run: id.clone(),
            state: LifecycleState::Booting,
        });
        self.active = Some(ActiveRun {
            id: id.clone(),
            encrypted_scope: None,
            process,
            state: LifecycleState::Booting,
            login_seen: false,
            require_login_prompt,
            login_marker_tail: Vec::new(),
            diagnostic_tail: Vec::new(),
            ready_deadline: Instant::now() + GUEST_READY_TIMEOUT,
            _state_lock: state_lock,
            _encrypted_session: None,
        });
        Ok(id)
    }

    fn prepare_for_run(
        &mut self,
        source: &Path,
        options: RunOptions,
    ) -> Result<(LaunchRequest, Option<state::StateRunLock>), AppError> {
        let mut launch = self.prepare(source)?;
        validate_assets(&launch).map_err(|detail| AppError {
            phase: LifecycleState::Ready,
            summary: "The glass runtime assets are incomplete.".to_string(),
            detail,
        })?;
        let state_lock = if let Some(template) = launch
            .storage
            .writable_overlay
            .as_ref()
            .map(|overlay| overlay.path().to_path_buf())
        {
            let store_root = self.state_root.join(safe_state_namespace(&launch));
            let store = state::InstanceStateStore::new(store_root, template);
            let (record, lock) = match options.state_id.as_deref() {
                Some(selected) => store.select_for_run(selected),
                None => store.select_current_for_run(),
            }
            .map_err(state_app_error)?;
            launch.writable_overlay = Some(record.image_path.clone());
            launch.storage.writable_overlay =
                Some(WritableOverlaySelection::Local(LocalWritableOverlay {
                    name: record.name,
                    path: record.image_path,
                }));
            Some(lock)
        } else {
            if options.state_id.is_some() {
                return Err(state_app_error(state::StateError::InvalidImage(
                    "this glass has no state template".into(),
                )));
            }
            None
        };
        Ok((launch, state_lock))
    }

    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<RuntimeEvent>, AppError> {
        if let Some((run, artifact_root, nonce)) = self.take_remote_stop_request()? {
            let stopped = self
                .stop_with_timeouts(&run, Duration::from_secs(5), Duration::from_secs(2))
                .map(|_| ());
            encrypted_sessions::EncryptedSessionStore::write_stop_response(
                &artifact_root,
                &nonce,
                stopped.is_ok(),
            )
            .map_err(encrypted_session_app_error)?;
            stopped?;
        }
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        let mut ids = self.encrypted_runs.keys().cloned().collect::<Vec<_>>();
        if let Some(active) = &self.active {
            ids.push(active.id.clone());
        }
        if ids.is_empty() {
            return Ok(None);
        }
        ids.sort_by_key(|id| id.0);
        let first = self
            .poll_cursor
            .as_ref()
            .and_then(|cursor| ids.iter().position(|id| id.0 > cursor.0))
            .unwrap_or(0);
        ids.rotate_left(first);
        for (index, id) in ids.into_iter().enumerate() {
            self.poll_cursor = Some(id.clone());
            let read_timeout = if index == 0 { timeout } else { Duration::ZERO };
            let outcome = if self.active.as_ref().is_some_and(|active| active.id == id) {
                poll_active_run(
                    self.active.as_mut().expect("active run was checked"),
                    read_timeout,
                )?
            } else if let Some(active) = self.encrypted_runs.get_mut(&id) {
                poll_active_run(active, read_timeout)?
            } else {
                continue;
            };
            if outcome.finished {
                self.remove_run(&id);
            }
            if !outcome.events.is_empty() {
                self.pending.extend(outcome.events);
                return Ok(self.pending.pop_front());
            }
        }
        Ok(None)
    }

    pub fn write_serial(&mut self, run: &RunId, bytes: &[u8]) -> Result<(), AppError> {
        let active = self.require_active(run)?;
        active
            .process
            .write_serial(bytes)
            .map_err(|error| AppError {
                phase: active.state,
                summary: "Guest console input failed.".to_string(),
                detail: error.to_string(),
            })
    }

    /// Return bounded QEMU stderr for an active run, kept separate from guest serial output.
    pub fn qemu_diagnostics(&mut self, run: &RunId) -> Result<DiagnosticLogSnapshot, AppError> {
        let active = self.require_active(run)?;
        let tail = active.process.stderr_tail().map_err(|error| AppError {
            phase: active.state,
            summary: "QEMU diagnostics could not be read.".to_string(),
            detail: error.to_string(),
        })?;
        let text = String::from_utf8_lossy(&tail.bytes)
            .chars()
            .map(|character| {
                if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                    '\u{fffd}'
                } else {
                    character
                }
            })
            .collect();
        Ok(DiagnosticLogSnapshot {
            text,
            truncated: tail.truncated,
        })
    }

    pub fn stop(&mut self, run: &RunId) -> Result<(), AppError> {
        if !self.contains_run(run) && self.last_stopped.contains(run) {
            return Ok(());
        }
        self.stop_with_timeouts(run, Duration::from_secs(5), Duration::from_secs(2))
            .map(|_| ())
    }

    /// Stop a run with caller-selected guest and QMP shutdown timeouts.
    ///
    /// Desktop callers should use [`Self::stop`], which retains the existing
    /// five-second guest shutdown timeout. Headless callers can allow more
    /// time for the guest to flush its disks before QEMU is forced to exit.
    pub fn stop_with_timeouts(
        &mut self,
        run: &RunId,
        guest_shutdown_timeout: Duration,
        qmp_quit_timeout: Duration,
    ) -> Result<ExitStatus, AppError> {
        self.require_active(run)?;
        self.pending.push_back(RuntimeEvent::StateChanged {
            run: run.clone(),
            state: LifecycleState::Stopping,
        });
        let active = self.require_active(run)?;
        active.state = LifecycleState::Stopping;
        let status = active
            .process
            .stop(guest_shutdown_timeout, qmp_quit_timeout)
            .map_err(|error| AppError {
                phase: LifecycleState::Stopping,
                summary: "QEMU could not be stopped cleanly.".to_string(),
                detail: error.to_string(),
            })?;
        self.remove_run(run);
        self.last_stopped.insert(run.clone());
        self.pending.push_back(RuntimeEvent::StateChanged {
            run: run.clone(),
            state: LifecycleState::Stopped,
        });
        Ok(status)
    }

    /// Preserve the existing CLI's terminal-based developer workflow.
    pub fn boot_interactive(&mut self, source: &Path) -> Result<(), AppError> {
        let (launch, _state_lock) = self.prepare_for_run(source, RunOptions::default())?;
        let mut child = QemuLaunchPlan::for_launch_request(&launch)
            .spawn_interactive()
            .map_err(|error| AppError {
                phase: LifecycleState::Starting,
                summary: "QEMU could not be started.".to_string(),
                detail: error.to_string(),
            })?;
        let status = child.wait().map_err(|error| AppError {
            phase: LifecycleState::Stopping,
            summary: "QEMU status could not be read.".to_string(),
            detail: error.to_string(),
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(AppError {
                phase: LifecycleState::Failed,
                summary: "QEMU exited unsuccessfully.".to_string(),
                detail: format!("QEMU exited with status {status}"),
            })
        }
    }

    /// Run a verified glass with no graphical display and return QEMU's process status.
    pub fn run_headless(&mut self, source: &Path) -> Result<ExitStatus, AppError> {
        let (launch, _state_lock) = self.prepare_for_run(source, RunOptions::default())?;
        let mut child = QemuLaunchPlan::for_headless_launch_request(&launch)
            .spawn_interactive()
            .map_err(|error| AppError {
                phase: LifecycleState::Starting,
                summary: "QEMU could not be started.".to_string(),
                detail: error.to_string(),
            })?;
        match child.wait() {
            Ok(status) => Ok(status),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(AppError {
                    phase: LifecycleState::Stopping,
                    summary: "QEMU status could not be read.".to_string(),
                    detail: error.to_string(),
                })
            }
        }
    }

    fn prepare(&mut self, source: &Path) -> Result<LaunchRequest, AppError> {
        self.runtime
            .prepare_launch_with_host(source, &HostCapabilities::detect_current())
            .map_err(|error| {
                if let CoreError::Signature(SignatureError::UnknownKey(key_id)) = &error {
                    if self.revoked_key_ids.contains(key_id) {
                        return revoked_key_app_error(key_id);
                    }
                }
                let user_error = self.runtime.status().last_error.as_ref();
                AppError {
                    phase: user_error
                        .map(|item| item.stage)
                        .unwrap_or(LifecycleState::Failed),
                    summary: user_error
                        .map(|item| item.summary.clone())
                        .unwrap_or_else(|| "The glass could not be prepared.".to_string()),
                    detail: user_error
                        .map(|item| item.detail.clone())
                        .unwrap_or_else(|| error.to_string()),
                }
            })
    }

    fn require_active(&mut self, run: &RunId) -> Result<&mut ActiveRun, AppError> {
        if self.active.as_ref().is_some_and(|active| &active.id == run) {
            return Ok(self.active.as_mut().expect("active run was checked"));
        }
        let current = self
            .active
            .as_ref()
            .map(|active| (active.id.clone(), active.state))
            .or_else(|| {
                self.encrypted_runs
                    .values()
                    .next()
                    .map(|active| (active.id.clone(), active.state))
            });
        if let Some(active) = self.encrypted_runs.get_mut(run) {
            return Ok(active);
        }
        Err(
            active_run_error(current.as_ref().map(|(id, state)| (id, *state)), run)
                .expect("a missing run is always an error"),
        )
    }

    fn contains_run(&self, run: &RunId) -> bool {
        self.active.as_ref().is_some_and(|active| &active.id == run)
            || self.encrypted_runs.contains_key(run)
    }

    fn remove_run(&mut self, run: &RunId) {
        if self.active.as_ref().is_some_and(|active| &active.id == run) {
            self.active = None;
        } else {
            self.encrypted_runs.remove(run);
        }
    }

    fn prepare_encrypted_boot(
        &mut self,
        library: &PackageLibrary,
        installed: &registry::InstalledGlass,
        requested_recipient: Option<&str>,
        provider: &dyn RecipientIdentityProvider,
    ) -> Result<(LaunchRequest, PreparedEncryptedArtifact), AppError> {
        if library.revoked_key_ids.contains(&installed.key_id) {
            return Err(revoked_key_app_error(&installed.key_id));
        }
        library.validate_installed_package_path(installed)?;
        let verified = import::open_installed_verified_archive(
            installed,
            &library.trust_store,
            &import::ImportLimits::default(),
        )
        .map_err(|error| {
            import_app_error(
                "The encrypted glass could not be verified for launch.",
                error,
            )
        })?;
        let report = verified.report().clone();
        let catalog = report.artifact_catalog.as_ref().ok_or_else(|| AppError {
            phase: LifecycleState::Verifying,
            summary: "The glass does not contain an encrypted launch profile.".to_string(),
            detail: "Encrypted boot requires a publisher-verified schema-2 artifact catalog."
                .to_string(),
        })?;
        let profile = catalog
            .profiles
            .get(&catalog.default_profile)
            .ok_or_else(|| AppError {
                phase: LifecycleState::Verifying,
                summary: "The signed default launch profile is missing.".to_string(),
                detail: "Repair the bundle manifest and publish a new signed archive.".to_string(),
            })?;
        let artifact_id = profile
            .nodes
            .iter()
            .find_map(|node| match &node.boot {
                rootglass_manifest::BootData::Linux {
                    rootfs_artifact, ..
                } => Some(rootfs_artifact.as_str()),
                rootglass_manifest::BootData::Mcu { .. } => None,
            })
            .ok_or_else(|| AppError {
                phase: LifecycleState::Verifying,
                summary: "The default profile has no Linux root disk.".to_string(),
                detail: "Encrypted boot currently requires one encrypted Linux root disk."
                    .to_string(),
            })?
            .to_string();

        let glass =
            rootglass_manifest::Glass::load(&installed.package_path).map_err(|error| AppError {
                phase: LifecycleState::Verifying,
                summary: "The installed manifest could not be loaded.".to_string(),
                detail: error.to_string(),
            })?;
        let manifest_bytes = fs::read(&glass.manifest_path).map_err(|error| AppError {
            phase: LifecycleState::Verifying,
            summary: "The installed manifest could not be verified.".to_string(),
            detail: error.to_string(),
        })?;
        let manifest_digest = format!("{:x}", Sha256::digest(&manifest_bytes));
        if report.manifest_sha256.as_deref() != Some(manifest_digest.as_str())
            || report.glass_identifier != glass.manifest.identifier
            || report.glass_version != glass.manifest.version
        {
            return Err(AppError {
                phase: LifecycleState::Verifying,
                summary: "The installed manifest does not match the verified archive.".to_string(),
                detail: "Re-import the archive before starting its encrypted guest.".to_string(),
            });
        }
        let launch = self
            .runtime
            .prepare_encrypted_launch(&glass, report.clone())
            .map_err(|error| {
                let user_error = error.to_user_facing(LifecycleState::Validating);
                AppError {
                    phase: user_error.stage,
                    summary: user_error.summary,
                    detail: user_error.detail,
                }
            })?;

        let envelope = verified.verify_recipient_envelope().map_err(|error| {
            import_app_error("The recipient authorization could not be verified.", error)
        })?;
        let capabilities = provider.capabilities();
        if !capabilities.can_unwrap {
            return Err(identity_provider_app_error(
                IdentityProviderError::UnsupportedRecipient,
            ));
        }
        if !capabilities.satisfies(envelope.minimum_recipient_hardware()) {
            return Err(identity_provider_app_error(
                IdentityProviderError::HardwarePolicyUnsatisfied,
            ));
        }
        let identities = provider
            .list_identities()
            .map_err(identity_provider_app_error)?;
        let recipient_id = match requested_recipient {
            Some(recipient_id) => recipient_id.to_string(),
            None => {
                let candidates = envelope
                    .payload()
                    .recipients
                    .iter()
                    .filter(|recipient| {
                        recipient.provider_id == capabilities.provider_id
                            && recipient.wrapping_suite_id == capabilities.wrapping_suite_id
                            && identities.iter().any(|identity| {
                                identity.recipient_id == recipient.recipient_id
                                    && identity.provider_id == capabilities.provider_id
                                    && identity.wrapping_suite_id == capabilities.wrapping_suite_id
                            })
                    })
                    .map(|recipient| recipient.recipient_id.as_str())
                    .collect::<Vec<_>>();
                match candidates.as_slice() {
                    [recipient_id] => (*recipient_id).to_string(),
                    [] if identities.is_empty() => {
                        return Err(identity_provider_app_error(
                            IdentityProviderError::IdentityNotFound,
                        ));
                    }
                    [] => {
                        return Err(AppError {
                            phase: LifecycleState::Verifying,
                            summary: "No local identity can open this glass.".to_string(),
                            detail: "Create or import an identity matching a recipient in the verified archive.".to_string(),
                        });
                    }
                    _ => {
                        return Err(AppError {
                            phase: LifecycleState::Verifying,
                            summary: "More than one local identity can open this glass."
                                .to_string(),
                            detail:
                                "Pass `--recipient <recipient-id>` to select the intended identity."
                                    .to_string(),
                        });
                    }
                }
            }
        };

        let prepared = library.preflight_encrypted_artifact(
            installed,
            &artifact_id,
            &recipient_id,
            provider,
        )?;
        Ok((launch, prepared))
    }

    fn take_remote_stop_request(&mut self) -> Result<Option<(RunId, PathBuf, String)>, AppError> {
        if let Some(active) = self.active.as_ref() {
            if let Some(session) = active._encrypted_session.as_ref() {
                if let Some(nonce) = session
                    .take_stop_request()
                    .map_err(encrypted_session_app_error)?
                {
                    return Ok(Some((
                        active.id.clone(),
                        session.artifact_root().to_path_buf(),
                        nonce,
                    )));
                }
            }
        }
        for active in self.encrypted_runs.values() {
            if let Some(session) = active._encrypted_session.as_ref() {
                if let Some(nonce) = session
                    .take_stop_request()
                    .map_err(encrypted_session_app_error)?
                {
                    return Ok(Some((
                        active.id.clone(),
                        session.artifact_root().to_path_buf(),
                        nonce,
                    )));
                }
            }
        }
        Ok(None)
    }
}

struct RunPollOutcome {
    events: Vec<RuntimeEvent>,
    finished: bool,
}

fn poll_active_run(active: &mut ActiveRun, timeout: Duration) -> Result<RunPollOutcome, AppError> {
    let read_timeout = if active.login_seen || !active.require_login_prompt {
        timeout
    } else {
        timeout.min(
            active
                .ready_deadline
                .saturating_duration_since(Instant::now()),
        )
    };
    if let Some(SerialChunk { bytes, truncated }) = active.process.read_serial(read_timeout) {
        let run = active.id.clone();
        let marker = b"rootglass-hello login:";
        let mut scan = Vec::with_capacity(active.login_marker_tail.len() + bytes.len());
        scan.extend_from_slice(&active.login_marker_tail);
        scan.extend_from_slice(&bytes);
        let marker_found = scan.windows(marker.len()).any(|window| window == marker);
        let tail_len = marker.len().saturating_sub(1);
        active.login_marker_tail = scan[scan.len().saturating_sub(tail_len)..].to_vec();
        let diagnostic = scan_diagnostics(&active.diagnostic_tail, &bytes);
        active.diagnostic_tail = diagnostic.tail;

        if let Some(detail) = diagnostic.failure {
            let _ = active
                .process
                .stop(Duration::from_secs(1), Duration::from_secs(1));
            return Ok(RunPollOutcome {
                events: vec![
                    RuntimeEvent::SerialChunk {
                        run: run.clone(),
                        bytes,
                        truncated,
                    },
                    RuntimeEvent::Failed {
                        run,
                        phase: LifecycleState::Failed,
                        summary: "The guest reported a boot failure.".to_string(),
                        detail,
                    },
                ],
                finished: true,
            });
        }

        if active.require_login_prompt
            && !active.login_seen
            && Instant::now() >= active.ready_deadline
        {
            let _ = active
                .process
                .stop(Duration::from_secs(1), Duration::from_secs(1));
            return Ok(RunPollOutcome {
                events: vec![
                    RuntimeEvent::SerialChunk {
                        run: run.clone(),
                        bytes,
                        truncated,
                    },
                    RuntimeEvent::Failed {
                        run,
                        phase: LifecycleState::Failed,
                        summary: "The guest did not become ready in time.".to_string(),
                        detail: format!(
                            "No rootglass-hello login prompt arrived within {} seconds.",
                            GUEST_READY_TIMEOUT.as_secs()
                        ),
                    },
                ],
                finished: true,
            });
        }

        if active.require_login_prompt && !active.login_seen && marker_found {
            active.login_seen = true;
            active.login_marker_tail.clear();
            active.state = LifecycleState::Running;
            return Ok(RunPollOutcome {
                events: vec![
                    RuntimeEvent::StateChanged {
                        run: run.clone(),
                        state: LifecycleState::Running,
                    },
                    RuntimeEvent::GuestReady {
                        run: run.clone(),
                        evidence: "rootglass-hello login prompt".to_string(),
                    },
                    RuntimeEvent::SerialChunk {
                        run,
                        bytes,
                        truncated,
                    },
                ],
                finished: false,
            });
        }
        return Ok(RunPollOutcome {
            events: vec![RuntimeEvent::SerialChunk {
                run,
                bytes,
                truncated,
            }],
            finished: false,
        });
    }

    if active.require_login_prompt && !active.login_seen && Instant::now() >= active.ready_deadline
    {
        let run = active.id.clone();
        let _ = active
            .process
            .stop(Duration::from_secs(1), Duration::from_secs(1));
        return Ok(RunPollOutcome {
            events: vec![RuntimeEvent::Failed {
                run,
                phase: LifecycleState::Failed,
                summary: "The guest did not become ready in time.".to_string(),
                detail: format!(
                    "No rootglass-hello login prompt arrived within {} seconds.",
                    GUEST_READY_TIMEOUT.as_secs()
                ),
            }],
            finished: true,
        });
    }
    if let Some(status) = active.process.try_wait().map_err(|error| AppError {
        phase: active.state,
        summary: "QEMU status could not be read.".to_string(),
        detail: error.to_string(),
    })? {
        let id = active.id.clone();
        let ready = active.login_seen || !active.require_login_prompt;
        let event = if !ready {
            RuntimeEvent::Failed {
                run: id,
                phase: LifecycleState::Failed,
                summary: "QEMU exited before the guest became ready.".to_string(),
                detail: format!(
                    "QEMU exited with status {} before the login prompt appeared.",
                    status
                ),
            }
        } else {
            RuntimeEvent::Exited {
                run: id,
                exit_code: status.code(),
                expected: false,
            }
        };
        return Ok(RunPollOutcome {
            events: vec![event],
            finished: true,
        });
    }
    Ok(RunPollOutcome {
        events: Vec::new(),
        finished: false,
    })
}

fn packaged_resource_root() -> PathBuf {
    let executable = std::env::current_exe().unwrap_or_default();
    #[cfg(target_os = "macos")]
    {
        if let Some(contents) = executable.parent().and_then(Path::parent) {
            return contents.join("Resources").join("rootglass");
        }
    }
    #[cfg(target_os = "linux")]
    {
        let installed = PathBuf::from("/usr/lib/rootglass/rootglass");
        if installed.join("runtime-assets").is_dir() {
            return installed;
        }
        let installed = PathBuf::from("/usr/lib/rootglass");
        if installed.join("runtime-assets").is_dir() {
            return installed;
        }
    }
    executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("rootglass")
}

fn app_data_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path).join("Rootglass");
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home)
                .join("AppData")
                .join("Local")
                .join("Rootglass");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Rootglass");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path.join("rootglass");
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("rootglass");
        }
    }
    std::env::temp_dir().join("rootglass")
}

fn active_run_error(
    active: Option<(&RunId, LifecycleState)>,
    requested: &RunId,
) -> Option<AppError> {
    match active {
        None => Some(AppError {
            phase: LifecycleState::Stopped,
            summary: "There is no active glass.".to_string(),
            detail: "Start a glass before requesting run control.".to_string(),
        }),
        Some((active_id, state)) if active_id != requested => Some(AppError {
            phase: state,
            summary: "The run identifier is stale.".to_string(),
            detail: "Use the RunId returned by the most recent start.".to_string(),
        }),
        Some(_) => None,
    }
}

fn validate_assets(request: &LaunchRequest) -> Result<(), String> {
    let assets = [
        Some(("kernel", request.kernel.as_path())),
        request.initrd.as_deref().map(|path| ("initrd", path)),
        request
            .device_tree
            .as_deref()
            .map(|path| ("device tree", path)),
        Some(("rootfs", request.rootfs.as_path())),
        request
            .writable_overlay
            .as_deref()
            .map(|path| ("writable state", path)),
    ];
    for (label, path) in assets.into_iter().flatten() {
        if !path.is_file() {
            return Err(format!("missing {label} asset: {}", path.display()));
        }
    }
    Ok(())
}

fn safe_state_namespace(request: &LaunchRequest) -> String {
    request
        .glass_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn default_state_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path).join("Rootglass").join("states");
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home)
                .join("AppData")
                .join("Local")
                .join("Rootglass")
                .join("states");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Rootglass")
                .join("states");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path.join("rootglass").join("states");
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("rootglass")
                .join("states");
        }
    }
    std::env::temp_dir().join("rootglass").join("states")
}

fn default_library_root() -> PathBuf {
    default_state_root()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("library")
}

fn import_app_error(summary: &str, error: impl std::fmt::Display) -> AppError {
    AppError {
        phase: LifecycleState::Verifying,
        summary: summary.to_string(),
        detail: error.to_string(),
    }
}

fn identity_provider_app_error(error: IdentityProviderError) -> AppError {
    AppError {
        phase: LifecycleState::Verifying,
        summary: "The selected recipient identity cannot open this encrypted glass.".to_string(),
        detail: error.to_string(),
    }
}

fn encrypted_instance_policy_error(limit: u8) -> AppError {
    AppError {
        phase: LifecycleState::Starting,
        summary: "The encrypted instance limit is invalid.".to_string(),
        detail: format!(
            "The local limit must be between 1 and {}, received {limit}.",
            encrypted_sessions::MAX_ENCRYPTED_INSTANCES
        ),
    }
}

fn effective_encrypted_instance_limit(signed: u8, local: Option<u8>) -> Result<u8, AppError> {
    if !(1..=encrypted_sessions::MAX_ENCRYPTED_INSTANCES).contains(&signed) {
        return Err(encrypted_instance_policy_error(signed));
    }
    if let Some(local) = local {
        if !(1..=encrypted_sessions::MAX_ENCRYPTED_INSTANCES).contains(&local) {
            return Err(encrypted_instance_policy_error(local));
        }
    }
    Ok(local.unwrap_or(signed).min(signed))
}

fn encrypted_session_app_error(error: encrypted_sessions::SessionError) -> AppError {
    AppError {
        phase: LifecycleState::Starting,
        summary: "An encrypted instance slot could not be reserved.".to_string(),
        detail: error.to_string(),
    }
}

fn revoked_key_app_error(key_id: &str) -> AppError {
    AppError {
        phase: LifecycleState::Verifying,
        summary: "This publisher's signing key has been revoked.".to_string(),
        detail: format!(
            "Signing key '{key_id}' is listed in the local trust policy's revokedKeyIds; this glass cannot be opened or run."
        ),
    }
}

fn state_app_error(error: state::StateError) -> AppError {
    AppError {
        phase: LifecycleState::Ready,
        summary: "The selected writable state could not be prepared.".to_string(),
        detail: error.to_string(),
    }
}

fn summarize(source: &Path, launch: &LaunchRequest) -> Result<GlassSummary, AppError> {
    let glass = rootglass_manifest::Glass::load(source).map_err(|error| AppError {
        phase: LifecycleState::Validating,
        summary: "This glass is not ready to run.".to_string(),
        detail: error.to_string(),
    })?;
    Ok(GlassSummary {
        path: source.to_path_buf(),
        name: glass.manifest.name,
        identifier: glass.manifest.identifier,
        version: glass.manifest.version,
        description: glass.manifest.description.unwrap_or_default(),
        architecture: launch.architecture.clone(),
        profile: launch.runtime_profile.clone(),
        acceleration: launch
            .accelerator
            .map(|accelerator| accelerator.qemu_name().to_ascii_uppercase())
            .unwrap_or_else(|| "TCG (software emulation)".to_string()),
        memory_mb: launch.memory_mb,
        cpus: launch.cpus,
        publisher_name: launch.signature.publisher_name.clone(),
        publisher_identity: launch.signature.publisher_identity.clone(),
        key_id: launch.signature.key_id.clone(),
        signed_subject_count: launch.signature.subject_count,
    })
}

struct DiagnosticScan {
    tail: Vec<u8>,
    failure: Option<String>,
}

fn scan_diagnostics(previous_tail: &[u8], bytes: &[u8]) -> DiagnosticScan {
    let mut scan = Vec::with_capacity(previous_tail.len() + bytes.len());
    scan.extend_from_slice(previous_tail);
    scan.extend(bytes.iter().map(u8::to_ascii_lowercase));

    let failure = scan.split(|byte| *byte == b'\n').find_map(|line| {
        if contains(line, b"kernel panic") {
            Some("Kernel panic detected in the guest console.".to_string())
        } else if contains(line, b"mount: mounting rootglass-state on /state failed") {
            Some("The guest could not mount persistent state at /state.".to_string())
        } else if contains(line, b"mounting root: failed")
            || contains(line, b"failed to mount root")
            || (contains(line, b"mount: mounting ") && contains(line, b" failed"))
        {
            Some("The guest could not mount its root filesystem.".to_string())
        } else if contains(line, b"initramfs emergency")
            || contains(line, b"emergency recovery shell")
        {
            Some("The guest entered its initramfs emergency shell.".to_string())
        } else {
            None
        }
    });
    let keep = scan.len().min(ERROR_SCAN_TAIL_BYTES);
    let tail = scan[scan.len() - keep..].to_vec();
    DiagnosticScan { tail, failure }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;

    #[test]
    fn guest_panic_emergency_and_mount_errors_are_actionable_across_chunks() {
        let prefix = scan_diagnostics(&[], b"Kernel pa");
        let panic = scan_diagnostics(&prefix.tail, b"nic - not syncing: fatal exception\n");
        assert_eq!(
            panic.failure.as_deref(),
            Some("Kernel panic detected in the guest console.")
        );

        let emergency = scan_diagnostics(&[], b"Entering initramfs emergency recovery shell\n");
        assert_eq!(
            emergency.failure.as_deref(),
            Some("The guest entered its initramfs emergency shell.")
        );

        let root_mount = scan_diagnostics(&[], b"Mounting root: failed to mount root\n");
        assert_eq!(
            root_mount.failure.as_deref(),
            Some("The guest could not mount its root filesystem.")
        );

        let state_mount =
            scan_diagnostics(&[], b"mount: mounting rootglass-state on /state failed\n");
        assert_eq!(
            state_mount.failure.as_deref(),
            Some("The guest could not mount persistent state at /state.")
        );
    }

    #[test]
    fn diagnostic_tail_is_bounded() {
        let output = vec![b'x'; ERROR_SCAN_TAIL_BYTES * 4];
        let scan = scan_diagnostics(&[], &output);
        assert_eq!(scan.tail.len(), ERROR_SCAN_TAIL_BYTES);
    }

    #[test]
    fn stale_run_ids_are_rejected_before_guest_io() {
        let active = RunId(2);
        let error = active_run_error(Some((&active, LifecycleState::Running)), &RunId(1))
            .expect("a stale run id must fail");
        assert_eq!(error.summary, "The run identifier is stale.");
        assert_eq!(error.phase, LifecycleState::Running);
        assert!(active_run_error(Some((&active, LifecycleState::Running)), &active).is_none());
    }

    #[test]
    fn encrypted_instance_limits_are_validated_and_local_preferences_only_lower_policy() {
        for limit in 1..=encrypted_sessions::MAX_ENCRYPTED_INSTANCES {
            let service = AppService::new()
                .with_encrypted_instance_limit(limit)
                .unwrap();
            assert_eq!(service.encrypted_instance_limit, Some(limit));
        }
        for invalid in [0, encrypted_sessions::MAX_ENCRYPTED_INSTANCES + 1] {
            assert!(AppService::new()
                .with_encrypted_instance_limit(invalid)
                .is_err());
            assert!(effective_encrypted_instance_limit(invalid, None).is_err());
        }
        assert_eq!(effective_encrypted_instance_limit(3, Some(5)).unwrap(), 3);
        assert_eq!(effective_encrypted_instance_limit(5, Some(2)).unwrap(), 2);
    }
}

#[cfg(test)]
mod library_tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::fs::{self, File};
    use std::io::{self, Read, Write};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rootglass-library-{}-{nonce}", std::process::id()))
    }

    fn add_directory(
        source_root: &Path,
        directory: &Path,
        writer: &mut zip::ZipWriter<File>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(source_root).expect("child path");
            let name = relative.to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                writer.add_directory(format!("{name}/"), SimpleFileOptions::default())?;
                add_directory(source_root, &path, writer)?;
            } else {
                writer.start_file(name, SimpleFileOptions::default())?;
                let mut file = File::open(path)?;
                io::copy(&mut file, writer)?;
            }
        }
        Ok(())
    }

    fn signed_launch_ready_fixture(root: &Path) {
        signed_launch_ready_fixture_version(root, "0.1.0");
    }

    fn signed_launch_ready_fixture_version(root: &Path, version: &str) {
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        fs::create_dir_all(root.join("metadata")).unwrap();
        fs::create_dir_all(root.join("signatures")).unwrap();
        for name in ["README.md", "metadata/README.md", "rootglass.toml"] {
            fs::copy(example.join(name), root.join(name)).unwrap();
        }
        let manifest_path = root.join("rootglass.toml");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        fs::write(
            &manifest_path,
            manifest.replace("version = \"0.1.0\"", &format!("version = \"{version}\"")),
        )
        .unwrap();
        fs::write(root.join("rootfs.img"), b"synthetic test root filesystem").unwrap();
        fs::write(root.join("state.img"), b"synthetic test state image").unwrap();

        let subjects = [
            "README.md",
            "metadata/README.md",
            "rootfs.img",
            "rootglass.toml",
            "state.img",
        ]
        .map(|name| {
            let digest = Sha256::digest(fs::read(root.join(name)).unwrap());
            let hex_digest = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            json!({"name": name, "digest": {"sha256": hex_digest}})
        });
        let statement = json!({
            "_type": "https://in-toto.io/Statement/v1",
            "predicate": {
                "createdAt": "2026-11-07T00:00:00Z",
                "glassIdentifier": "io.rootglass.examples.hello-world",
                "glassVersion": version,
                "publisher": {
                    "identity": "rootglass.dev/examples",
                    "keyId": "ed25519:rootglass-example-fixture",
                    "name": "Rootglass Examples"
                },
                "schemaVersion": 1
            },
            "predicateType": "https://rootglass.dev/predicate/glass/v1",
            "subject": subjects
        });
        let payload = serde_json::to_vec(&statement).unwrap();
        let pae = dsse_pae(b"application/vnd.in-toto+json", &payload);
        // This is the public deterministic sample key documented by the fixture signer.
        let seed: [u8; 32] =
            Sha256::digest(b"Rootglass public development fixture signing key v1").into();
        let signature = SigningKey::from_bytes(&seed).sign(&pae);
        let envelope = json!({
            "payloadType": "application/vnd.in-toto+json",
            "payload": URL_SAFE_NO_PAD.encode(payload),
            "signatures": [{
                "keyid": "ed25519:rootglass-example-fixture",
                "sig": URL_SAFE_NO_PAD.encode(signature.to_bytes())
            }]
        });
        fs::write(
            root.join("signatures/glass.dsse.json"),
            serde_json::to_vec_pretty(&envelope).unwrap(),
        )
        .unwrap();
    }

    fn package_encrypted_schema2_fixture(root: &Path, archive: &Path) -> TrustStore {
        package_encrypted_schema2_fixture_with_policy(root, archive, "software-allowed")
    }

    fn package_encrypted_schema2_fixture_with_policy(
        root: &Path,
        archive: &Path,
        minimum_hardware: &str,
    ) -> TrustStore {
        fs::create_dir_all(root.join("images")).unwrap();
        fs::create_dir_all(root.join("signatures")).unwrap();
        let disk = b"mock encrypted disk bytes";
        fs::write(root.join("images/root.raw"), disk).unwrap();
        let publisher_key = SigningKey::from_bytes(&[52; 32]);
        let recipient_public_key = SigningKey::from_bytes(&[53; 32]).verifying_key().to_bytes();
        let recipient_key_id = format!(
            "ed25519:{}",
            Sha256::digest(recipient_public_key)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let manifest = format!(
            r#"schema_version = 2
name = "Encrypted Import Test"
identifier = "io.rootglass.tests.encrypted-import"
version = "0.2.0"

[publisher]
name = "Encrypted Import Publisher"
identity = "rootglass.test/encrypted-import"
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

[artifact_catalog.artifacts.linux-root.encryption]
suite_id = "qemu-luks-v1"

[artifact_catalog.artifacts.linux-root.storage]
kind = "zip-entry"
path = "images/root.raw"

[artifact_catalog.encryption]
minimum_recipient_hardware = "{minimum_hardware}"
max_concurrent_instances = 1

[[artifact_catalog.encryption.recipient_authorities]]
key_id = "{}"
public_key_base64url = "{}"

[artifact_catalog.profiles.default]
required_features = ["launch-profiles-v1", "linux-node-v1", "typed-devices-v1", "encrypted-artifacts-v1"]

[artifact_catalog.profiles.default.resources]
memory_mb = 1024
cpus = 1

[artifact_catalog.profiles.default.network]
mode = "none"

[[artifact_catalog.profiles.default.nodes]]
id = "linux"
startup_order = 0
process_group = "linux"
architecture = "aarch64"
machine = "qemu:virt"
boot = {{ kind = "linux", kernel = "builtin:aarch64-linux", device_tree = "builtin:aarch64-virt", rootfs_artifact = "linux-root", kernel_args = [] }}

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
"#,
            disk.len(),
            Sha256::digest(disk)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            recipient_key_id,
            URL_SAFE_NO_PAD.encode(recipient_public_key)
        );
        fs::write(root.join("rootglass.toml"), manifest).unwrap();
        rootglass_signature::package_glass_directory_at(
            root,
            archive,
            &publisher_key,
            "2026-11-15T00:00:00Z",
        )
        .unwrap();
        let public_key = publisher_key.verifying_key().to_bytes();
        let key_id = format!(
            "ed25519:{}",
            Sha256::digest(public_key)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        TrustStore::new([rootglass_signature::TrustedPublisher {
            name: "Encrypted Import Publisher".to_string(),
            identity: "rootglass.test/encrypted-import".to_string(),
            key_id,
            public_key_base64url: URL_SAFE_NO_PAD.encode(public_key),
        }])
    }

    fn personalize_encrypted_fixture(
        root: &Path,
        archive: &Path,
        minimum_hardware: &str,
    ) -> (
        PathBuf,
        TrustStore,
        rootglass_core::identity::RecipientIdentity,
    ) {
        let trust = package_encrypted_schema2_fixture_with_policy(root, archive, minimum_hardware);
        let authority_key = SigningKey::from_bytes(&[53; 32]);
        let verified = rootglass_signature::open_verified_glass_archive(
            archive,
            "signatures/glass.dsse.json",
            &trust,
        )
        .expect("publisher and encrypted disk verify before personalization");
        let age_identity = age::x25519::Identity::generate();
        let public_key = age_identity.to_public().to_string();
        let recipient_id = rootglass_signature::recipient_id_for_public_key(
            rootglass_signature::RecipientWrappingSuite::AgeV1X25519,
            public_key.as_bytes(),
        );
        let age_recipient = age::x25519::Recipient::from_str(&public_key).unwrap();
        let wrapped_imk = age::encrypt(&age_recipient, &[0x42; 32]).unwrap();
        let recipient = rootglass_signature::RecipientRecord {
            recipient_id: recipient_id.clone(),
            provider_id: rootglass_signature::RecipientProvider::AgeX25519,
            wrapping_suite_id: rootglass_signature::RecipientWrappingSuite::AgeV1X25519,
            wrapped_imk,
        };
        let personalized = root.join("personalized-encrypted.glass");
        rootglass_signature::personalize_glass_archive_at(
            &verified,
            &personalized,
            vec![recipient],
            &authority_key,
        )
        .expect("delegated authority personalizes the encrypted fixture");

        (
            personalized,
            trust,
            rootglass_core::identity::RecipientIdentity {
                recipient_id,
                provider_id: rootglass_signature::RecipientProvider::AgeX25519,
                wrapping_suite_id: rootglass_signature::RecipientWrappingSuite::AgeV1X25519,
                public_key,
                label: None,
            },
        )
    }

    fn rewrite_archive_for_test(
        source: &Path,
        destination: &Path,
        mutated_member: Option<(&str, bool)>,
        compression_override: Option<(&str, zip::CompressionMethod)>,
    ) {
        let mut archive = zip::ZipArchive::new(File::open(source).unwrap()).unwrap();
        let mut writer = zip::ZipWriter::new(File::create(destination).unwrap());
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_string();
            if entry.is_dir() {
                writer
                    .add_directory(name, SimpleFileOptions::default())
                    .unwrap();
                continue;
            }
            let compression = entry.compression();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if let Some((mutated_path, signature_only)) = mutated_member {
                if name == mutated_path {
                    if signature_only {
                        let mut envelope: serde_json::Value =
                            serde_json::from_slice(&bytes).unwrap();
                        let signature = envelope["signatures"][0]["sig"].as_str().unwrap();
                        let mut signature = URL_SAFE_NO_PAD.decode(signature).unwrap();
                        signature[0] ^= 0x80;
                        envelope["signatures"][0]["sig"] = json!(URL_SAFE_NO_PAD.encode(signature));
                        bytes = serde_json::to_vec(&envelope).unwrap();
                    } else if !bytes.is_empty() {
                        bytes[0] ^= 0x01;
                    }
                }
            }
            let compression = compression_override
                .filter(|(path, _)| *path == name)
                .map(|(_, compression)| compression)
                .unwrap_or(compression);
            writer
                .start_file(
                    name,
                    SimpleFileOptions::default().compression_method(compression),
                )
                .unwrap();
            writer.write_all(&bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    fn tamper_recipient_envelope(source: &Path, destination: &Path) {
        rewrite_archive_for_test(
            source,
            destination,
            Some(("signatures/recipients.dsse.json", true)),
            None,
        );
    }

    fn install_archive_test_row(
        archive: &Path,
        library_root: &Path,
        trust: &TrustStore,
    ) -> registry::InstalledGlass {
        let bytes = fs::read(archive).unwrap();
        let content_sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let package_path = library_root.join("packages").join(&content_sha256);
        fs::create_dir_all(&package_path).unwrap();
        fs::copy(archive, package_path.with_extension("glass")).unwrap();
        let publisher = trust.publishers().next().unwrap();
        registry::InstalledGlass {
            content_sha256,
            package_path,
            identifier: "io.rootglass.tests.encrypted-import".to_string(),
            name: "Encrypted Import Test".to_string(),
            version: "0.2.0".to_string(),
            publisher_name: publisher.name.clone(),
            publisher_identity: publisher.identity.clone(),
            key_id: publisher.key_id.clone(),
            original_source: archive.to_path_buf(),
            imported_at_unix_seconds: 0,
        }
    }

    struct TestRecipientProvider {
        identity: rootglass_core::identity::RecipientIdentity,
        capabilities: rootglass_core::identity::ProviderCapabilities,
        list_calls: AtomicUsize,
        unwrap_calls: AtomicUsize,
    }

    impl TestRecipientProvider {
        fn age(identity: rootglass_core::identity::RecipientIdentity) -> Self {
            Self::with_capabilities(
                identity,
                rootglass_core::identity::ProviderCapabilities {
                    provider_id: rootglass_signature::RecipientProvider::AgeX25519,
                    wrapping_suite_id: rootglass_signature::RecipientWrappingSuite::AgeV1X25519,
                    hardware_backed: false,
                    can_create: true,
                    can_unwrap: true,
                },
            )
        }

        fn with_capabilities(
            identity: rootglass_core::identity::RecipientIdentity,
            capabilities: rootglass_core::identity::ProviderCapabilities,
        ) -> Self {
            Self {
                identity,
                capabilities,
                list_calls: AtomicUsize::new(0),
                unwrap_calls: AtomicUsize::new(0),
            }
        }
    }

    impl RecipientIdentityProvider for TestRecipientProvider {
        fn capabilities(&self) -> rootglass_core::identity::ProviderCapabilities {
            self.capabilities
        }

        fn list_identities(
            &self,
        ) -> Result<Vec<rootglass_core::identity::RecipientIdentity>, IdentityProviderError>
        {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![self.identity.clone()])
        }

        fn create_identity(
            &self,
        ) -> Result<rootglass_core::identity::RecipientIdentity, IdentityProviderError> {
            Err(IdentityProviderError::UnsupportedPlatform)
        }

        fn delete_identity(&self, _recipient_id: &str) -> Result<(), IdentityProviderError> {
            Err(IdentityProviderError::IdentityNotFound)
        }

        fn unwrap_image_key(
            &self,
            envelope: &rootglass_signature::VerifiedRecipientEnvelope,
            recipient_id: &str,
        ) -> Result<ImageMasterKey, IdentityProviderError> {
            self.unwrap_calls.fetch_add(1, Ordering::SeqCst);
            if envelope.recipient(recipient_id).is_none() {
                return Err(IdentityProviderError::RecipientNotFound);
            }
            Ok(ImageMasterKey::new(zeroize::Zeroizing::new([0x42; 32])))
        }
    }

    fn create_archive(source: &Path, archive: &Path) {
        let file = File::create(archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        add_directory(source, source, &mut writer).unwrap();
        writer.finish().unwrap();
    }

    fn make_tree_writable(root: &Path) {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                    make_tree_writable(&path);
                }
                if let Ok(metadata) = fs::symlink_metadata(&path) {
                    make_path_writable(&path, metadata);
                }
            }
        }
        if let Ok(metadata) = fs::metadata(root) {
            make_path_writable(root, metadata);
        }
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&source_path, &destination_path);
            } else {
                fs::copy(&source_path, &destination_path).unwrap();
            }
        }
    }

    fn make_path_writable(path: &Path, metadata: fs::Metadata) {
        if metadata.file_type().is_symlink() {
            return;
        }
        let mut permissions = metadata.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(permissions.mode() | 0o200);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }

    fn dsse_pae(payload_type: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = b"DSSEv1 ".to_vec();
        out.extend_from_slice(payload_type.len().to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(payload_type);
        out.push(b' ');
        out.extend_from_slice(payload.len().to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn imports_signed_zip_and_persists_library_rows() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let archive = root.join("hello-world.glass");
        let source = root.join("source.glass");
        signed_launch_ready_fixture(&source);
        create_archive(&source, &archive);

        let library = root.join("library");
        let service =
            AppService::with_trust_store(TrustStore::example_fixture()).with_library_root(&library);
        let installed = service.import_archive(&archive).unwrap();
        assert_eq!(installed.identifier, "io.rootglass.examples.hello-world");
        let archive_object = crate::import::archive_object_path(&installed);
        assert_eq!(
            fs::read(&archive_object).unwrap(),
            fs::read(&archive).unwrap()
        );
        assert!(fs::metadata(&archive_object)
            .unwrap()
            .permissions()
            .readonly());
        let reimported = service.import_archive(&archive).unwrap();
        assert_eq!(reimported.content_sha256, installed.content_sha256);
        assert_eq!(
            service.installed_glasses().unwrap(),
            vec![reimported.clone()]
        );

        let source_v2 = root.join("source-v2.glass");
        let archive_v2 = root.join("hello-world-v2.glass");
        signed_launch_ready_fixture_version(&source_v2, "0.2.0");
        create_archive(&source_v2, &archive_v2);
        let installed_v2 = service.import_archive(&archive_v2).unwrap();
        assert_ne!(installed_v2.content_sha256, installed.content_sha256);
        assert_ne!(installed_v2.package_path, installed.package_path);
        assert_eq!(service.installed_glasses().unwrap().len(), 2);

        let reopened =
            AppService::with_trust_store(TrustStore::example_fixture()).with_library_root(&library);
        assert_eq!(reopened.installed_glasses().unwrap().len(), 2);

        let tampered = reimported.package_path.join("README.md");
        make_path_writable(&tampered, fs::metadata(&tampered).unwrap());
        fs::write(&tampered, b"tampered installed package").unwrap();
        let error = service
            .import_archive(&archive)
            .expect_err("reimport must revalidate an existing content-addressed package");
        assert!(error.detail.contains("subject digest mismatch"));
        assert_eq!(service.installed_glasses().unwrap().len(), 2);

        make_tree_writable(&reimported.package_path);
        fs::remove_dir_all(&reimported.package_path).unwrap();
        copy_tree(&installed_v2.package_path, &reimported.package_path);
        let error = service
            .import_archive(&archive)
            .expect_err("a different valid package must not be reused at this digest");
        assert!(error.detail.contains("archive changed"));

        reopened
            .remove_installed_glass(&installed.content_sha256)
            .unwrap();
        assert_eq!(reopened.installed_glasses().unwrap().len(), 1);
        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn encrypted_import_keeps_disk_in_archive_and_opens_by_logical_id() {
        let root = temp_root();
        let source = root.join("encrypted-source");
        let archive = root.join("encrypted.glass");
        let library_root = root.join("library");
        let trust = package_encrypted_schema2_fixture(&source, &archive);
        let mut service = AppService::with_trust_store(trust).with_library_root(&library_root);

        let installed = service.import_archive(&archive).unwrap();
        let launch_error = service
            .start(&source, RunOptions::default())
            .expect_err("schema-2 bundles must not use the schema-1 QEMU launch path");
        assert_eq!(launch_error.phase, LifecycleState::Validating);
        assert!(launch_error.detail.contains("schema version 2"));
        let disk_path = installed.package_path.join("images/root.raw");
        assert!(
            !disk_path.exists(),
            "encrypted disk is not extracted to storage"
        );
        let archive_object = crate::import::archive_object_path(&installed);
        assert_eq!(
            fs::read(&archive_object).unwrap(),
            fs::read(&archive).unwrap()
        );

        let mut reader = service
            .package_library()
            .open_encrypted_artifact_reader(&installed, "linux-root")
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"mock encrypted disk bytes");

        let reimported = service.import_archive(&archive).unwrap();
        assert_eq!(reimported.content_sha256, installed.content_sha256);
        assert!(!reimported.package_path.join("images/root.raw").exists());

        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn encrypted_preflight_verifies_everything_before_unwrapping_the_image_key() {
        let root = temp_root();
        let base_archive = root.join("encrypted-base.glass");
        let (archive, trust, identity) =
            personalize_encrypted_fixture(&root, &base_archive, "software-allowed");
        let library_root = root.join("library");
        let service = AppService::with_trust_store(trust).with_library_root(&library_root);
        let archive_recipients = service
            .package_library()
            .inspect_encrypted_archive_recipients(&archive)
            .expect("verified archive exposes recipients without installation");
        assert!(
            !library_root.exists(),
            "inspection does not install the archive"
        );
        assert_eq!(archive_recipients.len(), 1);
        assert!(!format!("{archive_recipients:?}").contains("wrapped_imk"));

        let installed = service.import_archive(&archive).unwrap();
        let provider = TestRecipientProvider::age(identity.clone());

        let recipients = service
            .package_library()
            .inspect_encrypted_recipients(&installed)
            .expect("verified archive exposes public recipient metadata");
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].recipient_id, identity.recipient_id);
        assert_eq!(recipients[0].provider_id, "age-x25519");
        assert_eq!(recipients[0].wrapping_suite_id, "age-v1-x25519");
        assert!(!format!("{recipients:?}").contains("wrapped_imk"));
        assert_eq!(
            import::inspect_archive_schema_version(&archive, &import::ImportLimits::default())
                .unwrap(),
            rootglass_manifest::MULTI_ARTIFACT_SCHEMA_VERSION
        );

        let mut prepared = service
            .package_library()
            .preflight_encrypted_artifact(
                &installed,
                "linux-root",
                &identity.recipient_id,
                &provider,
            )
            .expect("verified software recipient can open a software-allowed bundle");

        assert_eq!(prepared.info().glass_identifier, installed.identifier);
        assert_eq!(prepared.info().artifact_id, "linux-root");
        assert_eq!(prepared.info().recipient_id, identity.recipient_id);
        assert!(!prepared.info().provider.hardware_backed);
        assert_eq!(provider.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.unwrap_calls.load(Ordering::SeqCst), 1);
        assert_eq!(prepared.image_master_key().as_bytes(), &[0x42; 32]);
        let mut ciphertext = Vec::new();
        prepared
            .artifact_reader()
            .read_to_end(&mut ciphertext)
            .unwrap();
        assert_eq!(ciphertext, b"mock encrypted disk bytes");

        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn encrypted_archive_inspection_reports_stages_without_installing_or_unwrapping() {
        let root = temp_root();
        let base_archive = root.join("encrypted-base.glass");
        let (archive, trust, _identity) =
            personalize_encrypted_fixture(&root, &base_archive, "software-allowed");
        let library_root = root.join("library");
        let service = AppService::with_trust_store(trust).with_library_root(&library_root);

        let inspection = service
            .package_library()
            .inspect_encrypted_archive(&archive);
        assert_eq!(
            inspection.publisher_signature.status,
            VerificationStatus::Verified
        );
        assert_eq!(
            inspection.recipient_envelope.status,
            VerificationStatus::Verified
        );
        assert_eq!(
            inspection.encrypted_rootfs.status,
            VerificationStatus::Verified
        );
        assert_eq!(
            inspection.signed_policy.status,
            VerificationStatus::Verified
        );
        assert_eq!(inspection.recipients.len(), 1);
        assert_eq!(inspection.max_concurrent_instances, Some(1));
        assert!(
            !library_root.exists(),
            "inspection must not install the archive"
        );
        assert!(
            !format!("{inspection:?}").contains("wrapped_imk"),
            "inspection contains public recipient metadata only"
        );

        let tampered_envelope = root.join("tampered-envelope.glass");
        tamper_recipient_envelope(&archive, &tampered_envelope);
        let envelope_failure = service
            .package_library()
            .inspect_encrypted_archive(&tampered_envelope);
        assert_eq!(
            envelope_failure.publisher_signature.status,
            VerificationStatus::Verified
        );
        assert_eq!(
            envelope_failure.recipient_envelope.status,
            VerificationStatus::Failed
        );
        assert_eq!(
            envelope_failure.encrypted_rootfs.status,
            VerificationStatus::Verified
        );
        assert_eq!(
            envelope_failure.signed_policy.status,
            VerificationStatus::Verified
        );
        assert!(envelope_failure.recipients.is_empty());

        let tampered_signed_file = root.join("tampered-signed-file.glass");
        rewrite_archive_for_test(
            &archive,
            &tampered_signed_file,
            Some(("rootglass.toml", false)),
            None,
        );
        let signature_failure = service
            .package_library()
            .inspect_encrypted_archive(&tampered_signed_file);
        assert_eq!(
            signature_failure.publisher_signature.status,
            VerificationStatus::Failed
        );
        assert_eq!(
            signature_failure.recipient_envelope.status,
            VerificationStatus::NotChecked
        );

        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hardware_policy_and_provider_suite_fail_before_key_store_access() {
        let root = temp_root();
        let base_archive = root.join("encrypted-base.glass");
        let (archive, trust, identity) =
            personalize_encrypted_fixture(&root, &base_archive, "hardware-backed-required");
        let library_root = root.join("library");
        let service = AppService::with_trust_store(trust).with_library_root(&library_root);
        let installed = service.import_archive(&archive).unwrap();
        let software_provider = TestRecipientProvider::age(identity.clone());

        let error = service
            .package_library()
            .preflight_encrypted_artifact(
                &installed,
                "linux-root",
                &identity.recipient_id,
                &software_provider,
            )
            .expect_err("Age must not satisfy the signed hardware-backed policy");
        assert!(error.detail.contains("hardware-backed identity policy"));
        assert_eq!(software_provider.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(software_provider.unwrap_calls.load(Ordering::SeqCst), 0);

        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_provider_and_missing_recipient_fail_before_unwrap() {
        let root = temp_root();
        let base_archive = root.join("encrypted-base.glass");
        let (archive, trust, identity) =
            personalize_encrypted_fixture(&root, &base_archive, "software-allowed");
        let library_root = root.join("library");
        let service = AppService::with_trust_store(trust).with_library_root(&library_root);
        let installed = service.import_archive(&archive).unwrap();

        let mut invalid_registry_row = installed.clone();
        invalid_registry_row.package_path = root.join("outside-package");
        let invalid_path_provider = TestRecipientProvider::age(identity.clone());
        let error = service
            .package_library()
            .preflight_encrypted_artifact(
                &invalid_registry_row,
                "linux-root",
                &identity.recipient_id,
                &invalid_path_provider,
            )
            .expect_err("preflight must reject registry paths outside the package store");
        assert!(error.summary.contains("invalid package path"));
        assert_eq!(invalid_path_provider.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(invalid_path_provider.unwrap_calls.load(Ordering::SeqCst), 0);

        let unsupported = TestRecipientProvider::with_capabilities(
            identity.clone(),
            ProviderCapabilities {
                provider_id: rootglass_signature::RecipientProvider::WindowsTpmRsa3072,
                wrapping_suite_id: rootglass_signature::RecipientWrappingSuite::RsaOaepV1Sha256,
                hardware_backed: true,
                can_create: true,
                can_unwrap: true,
            },
        );
        let error = service
            .package_library()
            .preflight_encrypted_artifact(
                &installed,
                "linux-root",
                &identity.recipient_id,
                &unsupported,
            )
            .expect_err("a provider that does not match the signed recipient must fail closed");
        assert!(error.detail.contains("provider or wrapping suite"));
        assert_eq!(unsupported.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(unsupported.unwrap_calls.load(Ordering::SeqCst), 0);

        let age_provider = TestRecipientProvider::age(identity.clone());
        let absent = format!("{}:{}", "age-v1-x25519", "0".repeat(64));
        let error = service
            .package_library()
            .preflight_encrypted_artifact(&installed, "linux-root", &absent, &age_provider)
            .expect_err("the selected recipient must exist in the verified sidecar");
        assert!(error
            .detail
            .contains("not present in the verified envelope"));
        assert_eq!(age_provider.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(age_provider.unwrap_calls.load(Ordering::SeqCst), 0);

        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_recipient_envelope_and_changed_archive_fail_before_unwrap() {
        let root = temp_root();
        let base_archive = root.join("encrypted-base.glass");
        let (valid_archive, trust, identity) =
            personalize_encrypted_fixture(&root, &base_archive, "software-allowed");
        let invalid_archive = root.join("invalid-recipient.glass");
        tamper_recipient_envelope(&valid_archive, &invalid_archive);
        let library_root = root.join("library");
        let service = AppService::with_trust_store(trust).with_library_root(&library_root);
        let installed = service.import_archive(&invalid_archive).unwrap();
        let provider = TestRecipientProvider::age(identity.clone());

        let error = service
            .package_library()
            .preflight_encrypted_artifact(
                &installed,
                "linux-root",
                &identity.recipient_id,
                &provider,
            )
            .expect_err("invalid recipient signatures must fail before key resolution");
        assert!(error.detail.contains("signature") || error.detail.contains("recipient"));
        assert_eq!(provider.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.unwrap_calls.load(Ordering::SeqCst), 0);

        let valid_library = root.join("valid-library");
        let valid_service = AppService::with_trust_store(service.trust_store.clone())
            .with_library_root(&valid_library);
        let valid_installed = valid_service.import_archive(&valid_archive).unwrap();
        let archive_object = crate::import::archive_object_path(&valid_installed);
        make_path_writable(&archive_object, fs::metadata(&archive_object).unwrap());
        fs::OpenOptions::new()
            .append(true)
            .open(&archive_object)
            .unwrap()
            .write_all(b"changed")
            .unwrap();
        let changed_provider = TestRecipientProvider::age(identity.clone());
        let error = valid_service
            .package_library()
            .preflight_encrypted_artifact(
                &valid_installed,
                "linux-root",
                &identity.recipient_id,
                &changed_provider,
            )
            .expect_err("a changed installed archive must fail before provider access");
        assert!(error.detail.contains("archive digest"));
        assert_eq!(changed_provider.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(changed_provider.unwrap_calls.load(Ordering::SeqCst), 0);

        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn publisher_signature_artifact_digest_and_zip_profile_fail_before_key_store_access() {
        let root = temp_root();
        let base_archive = root.join("encrypted-base.glass");
        let (valid_archive, trust, identity) =
            personalize_encrypted_fixture(&root, &base_archive, "software-allowed");
        let publisher_signature_archive = root.join("invalid-publisher-signature.glass");
        rewrite_archive_for_test(
            &valid_archive,
            &publisher_signature_archive,
            Some(("signatures/glass.dsse.json", true)),
            None,
        );
        let changed_artifact_archive = root.join("changed-encrypted-artifact.glass");
        rewrite_archive_for_test(
            &valid_archive,
            &changed_artifact_archive,
            Some(("images/root.raw", false)),
            None,
        );
        let compressed_artifact_archive = root.join("compressed-encrypted-artifact.glass");
        rewrite_archive_for_test(
            &valid_archive,
            &compressed_artifact_archive,
            None,
            Some(("images/root.raw", zip::CompressionMethod::Deflated)),
        );

        for (name, archive) in [
            ("publisher signature", publisher_signature_archive),
            ("signed artifact digest", changed_artifact_archive),
            ("encrypted ZIP profile", compressed_artifact_archive),
        ] {
            let library_root = root.join(format!("library-{name}"));
            let service =
                AppService::with_trust_store(trust.clone()).with_library_root(&library_root);
            let installed = install_archive_test_row(&archive, &library_root, &trust);
            let provider = TestRecipientProvider::age(identity.clone());

            let error = service
                .package_library()
                .preflight_encrypted_artifact(
                    &installed,
                    "linux-root",
                    &identity.recipient_id,
                    &provider,
                )
                .expect_err("publisher and encrypted image validation must precede key access");
            assert_eq!(error.phase, LifecycleState::Verifying, "{name}: {error}");
            assert_eq!(provider.list_calls.load(Ordering::SeqCst), 0, "{name}");
            assert_eq!(provider.unwrap_calls.load(Ordering::SeqCst), 0, "{name}");
        }

        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn import_recovers_when_registry_commit_fails_after_package_publication() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.glass");
        let archive = root.join("hello-world.glass");
        signed_launch_ready_fixture(&source);
        create_archive(&source, &archive);

        let registry_home = root.join("registry-home");
        fs::create_dir_all(&registry_home).unwrap();
        let registry_path = registry_home.join("registry.json");
        let mut registry = crate::registry::PackageRegistry::open(&registry_path).unwrap();
        fs::remove_dir_all(&registry_home).unwrap();
        fs::write(&registry_home, b"block registry parent").unwrap();
        let store = root.join("store");
        let failure = crate::import::import_glass_archive(
            &archive,
            &store,
            &mut registry,
            &TrustStore::example_fixture(),
            &crate::import::ImportLimits::default(),
        )
        .expect_err("registry write should fail after package publication");
        assert!(matches!(failure, crate::import::ImportError::Registry(_)));
        assert!(registry.packages().is_empty());
        assert_eq!(
            fs::read_dir(store.join("packages")).unwrap().count(),
            2,
            "failed registry commit leaves the immutable package and archive objects unreferenced"
        );

        fs::remove_file(&registry_home).unwrap();
        fs::create_dir_all(&registry_home).unwrap();
        let mut recovered = crate::registry::PackageRegistry::open(&registry_path).unwrap();
        let installed = crate::import::import_glass_archive(
            &archive,
            &store,
            &mut recovered,
            &TrustStore::example_fixture(),
            &crate::import::ImportLimits::default(),
        )
        .expect("reimport should verify and reuse the orphaned immutable package");
        assert_eq!(recovered.packages(), &[installed]);
        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod publisher_trust_tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use rootglass_core::runtime_assets::RuntimeAssetsResolver;
    use rootglass_signature::sign_glass_directory_at;
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;

    const PUBLISHER_IDENTITY: &str = "rootglass.dev/examples";
    const TEST_RUNTIME: &str = "trust-policy-test";

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rootglass-publisher-trust-{}-{nonce}",
            std::process::id()
        ))
    }

    fn key_id(key: &SigningKey) -> String {
        let digest = Sha256::digest(key.verifying_key().to_bytes());
        format!(
            "ed25519:{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }

    fn signed_glass(root: &Path, key: &SigningKey) {
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        fs::create_dir_all(root.join("metadata")).unwrap();
        for name in ["README.md", "metadata/README.md", "rootglass.toml"] {
            fs::copy(example.join(name), root.join(name)).unwrap();
        }
        fs::write(
            root.join("rootfs.img"),
            b"synthetic signed test root filesystem",
        )
        .unwrap();
        fs::write(root.join("state.img"), b"synthetic signed test state image").unwrap();
        sign_glass_directory_at(root, key, "2026-11-07T00:00:00Z").unwrap();
    }

    fn untrusted_archive(path: &Path) -> String {
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        let manifest = fs::read(example.join("rootglass.toml")).unwrap();
        let key_id = format!("ed25519:{}", "9".repeat(64));
        let envelope = format!(
            r#"{{"payloadType":"application/vnd.in-toto+json","payload":"e30","signatures":[{{"keyid":"{key_id}","sig":"AA"}}]}}"#
        );
        let file = fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("rootglass.toml", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(&manifest).unwrap();
        archive
            .start_file("signatures/glass.dsse.json", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(envelope.as_bytes()).unwrap();
        archive.finish().unwrap();
        key_id
    }

    fn write_trust_config(path: &Path, trusted: &[&SigningKey], revoked: &[String]) {
        let publishers = trusted
            .iter()
            .map(|key| {
                json!({
                    "name": "Rootglass Example Publisher",
                    "identity": PUBLISHER_IDENTITY,
                    "keyId": key_id(key),
                    "publicKeyBase64url": URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
                })
            })
            .collect::<Vec<Value>>();
        fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "trustedPublishers": publishers,
                "revokedKeyIds": revoked
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn trust_policy_status_lists_trusted_revoked_and_load_error_without_key_material() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let trusted = SigningKey::from_bytes(&[31; 32]);
        let revoked = SigningKey::from_bytes(&[32; 32]);
        let config = root.join("trust.json");
        write_trust_config(&config, &[&trusted], &[key_id(&revoked)]);

        let status = AppService::from_trust_config(&config)
            .expect("trust configuration loads")
            .trust_policy_status();
        assert_eq!(status.config_path.as_deref(), Some(config.as_path()));
        assert_eq!(status.trusted_publishers.len(), 1);
        assert_eq!(status.trusted_publishers[0].key_id, key_id(&trusted));
        assert_eq!(status.revoked_key_ids, [key_id(&revoked)]);
        assert!(status.error.is_none());

        let failed = AppService::deny_all()
            .with_trust_load_error(Some(config.clone()), "unsupported schema")
            .trust_policy_status();
        assert_eq!(failed.config_path, Some(config));
        assert_eq!(failed.error.as_deref(), Some("unsupported schema"));
        assert!(failed.trusted_publishers.is_empty());
        assert!(failed.revoked_key_ids.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn untrusted_archive_review_does_not_install_and_revoked_keys_stay_blocked() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let archive = root.join("unknown-publisher.glass");
        let key_id = untrusted_archive(&archive);
        let library = root.join("library");
        let service =
            AppService::with_trust_store(TrustStore::default()).with_library_root(library.clone());

        let review = service
            .review_archive(&archive)
            .unwrap()
            .expect("unknown signing key should open review");
        assert_eq!(review.key_id, key_id);
        assert_eq!(review.publisher_name, "Rootglass Examples");
        assert_eq!(review.publisher_identity, PUBLISHER_IDENTITY);
        assert!(
            !library.exists(),
            "review must not create or install a package"
        );

        let config = root.join("revoked-trust.json");
        write_trust_config(&config, &[], std::slice::from_ref(&key_id));
        let revoked = AppService::from_trust_config(&config)
            .unwrap()
            .with_library_root(library.clone());
        let error = revoked
            .review_archive(&archive)
            .expect_err("revoked signing key must not be offered for review");
        assert!(is_revoked_signing_key(&error));
        assert!(!library.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn app_state_api_supports_named_operations_and_reports_guest_lock_conflicts() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let key = SigningKey::from_bytes(&[41; 32]);
        let glass = root.join("hello-world.glass");
        fs::create_dir_all(&glass).unwrap();
        signed_glass(&glass, &key);
        let config = root.join("trust.json");
        write_trust_config(&config, &[&key], &[]);
        let mut service = AppService::from_trust_config(&config)
            .expect("valid trust configuration")
            .with_state_root(root.join("states"));

        assert!(service.list_states(&glass).unwrap().is_empty());
        let created = service.create_state(&glass, "work").unwrap();
        let cloned = service.clone_state(&glass, "work", "copy").unwrap();
        assert_eq!(
            fs::read(&created.image_path).unwrap(),
            fs::read(&cloned.image_path).unwrap()
        );
        assert_eq!(service.select_state(&glass, "copy").unwrap(), "copy");
        assert_eq!(service.selected_state_id(&glass).unwrap(), "copy");
        service.reset_state(&glass, "work").unwrap();
        service.delete_state(&glass, "copy").unwrap();
        assert_eq!(service.selected_state_id(&glass).unwrap(), "default");
        assert_eq!(
            service
                .list_states(&glass)
                .unwrap()
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["work"]
        );

        let state_root = created.image_path.parent().unwrap();
        let store = state::InstanceStateStore::new(state_root, glass.join("state.img"));
        let lock = store.acquire_run_lock().unwrap();
        assert!(service.states_in_use(&glass).unwrap());
        let select_error = service
            .select_state(&glass, "work")
            .expect_err("state selection is denied while a guest owns the lock");
        assert!(select_error.detail.contains("currently in use"));
        let error = service
            .reset_state(&glass, "work")
            .expect_err("a running guest owns the state lock");
        assert!(error.detail.contains("currently in use"));
        drop(lock);
        assert!(!service.states_in_use(&glass).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    fn install_test_runtime(root: &Path) -> PathBuf {
        let resource_root = root.join("resources");
        let version_root = resource_root.join("runtime-assets").join(TEST_RUNTIME);
        fs::create_dir_all(&version_root).unwrap();
        for (name, contents) in [
            ("kernel", b"verified test kernel".as_slice()),
            ("initramfs", b"verified test initramfs".as_slice()),
            ("qemu", b"verified test qemu".as_slice()),
        ] {
            fs::write(version_root.join(name), contents).unwrap();
        }
        let digest = |name: &str| {
            Sha256::digest(fs::read(version_root.join(name)).unwrap())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        fs::write(
            version_root.join("assets.json"),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "runtime_version": TEST_RUNTIME,
                "assets": [
                    {"kind": "kernel", "path": "kernel", "sha256": digest("kernel")},
                    {"kind": "initramfs", "path": "initramfs", "sha256": digest("initramfs")},
                    {"kind": "qemu", "path": "qemu", "sha256": digest("qemu")}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        resource_root
    }

    fn production_service(config: &Path, root: &Path) -> AppService {
        AppService::from_trust_config(config)
            .expect("valid offline trust configuration")
            .with_verified_runtime_assets(
                RuntimeAssetsResolver::new(root.join("resources"), root.join("user-data")),
                TEST_RUNTIME,
            )
    }

    fn is_unknown_signing_key(error: &AppError) -> bool {
        error.phase == LifecycleState::Verifying && error.detail.contains("unknown signing key")
    }

    fn is_revoked_signing_key(error: &AppError) -> bool {
        error.phase == LifecycleState::Verifying
            && error.summary.contains("has been revoked")
            && error.detail.contains("revokedKeyIds")
    }

    #[test]
    fn offline_trust_rotation_reaches_preview_preflight_and_start_before_spawn() {
        // These deterministic seeds exist only in this test module; no key material enters app UI.
        let key_a = SigningKey::from_bytes(&[17_u8; 32]);
        let key_b = SigningKey::from_bytes(&[29_u8; 32]);
        let key_a_id = key_id(&key_a);
        let root = temp_root();
        let glass_a = root.join("publisher-a.glass");
        let glass_b = root.join("publisher-b.glass");
        fs::create_dir_all(&root).unwrap();
        signed_glass(&glass_a, &key_a);
        signed_glass(&glass_b, &key_b);
        let assets = install_test_runtime(&root);
        let config = root.join("trust.json");

        write_trust_config(&config, &[&key_a], &[]);
        let mut before_rotation = production_service(&config, &root);
        let preview = before_rotation
            .inspect(&glass_a)
            .expect("explicitly trusted publisher reaches preview");
        assert_eq!(preview.publisher_identity, PUBLISHER_IDENTITY);
        before_rotation
            .launch_plan(&glass_a)
            .expect("explicitly trusted publisher reaches pre-run planning");
        before_rotation
            .preflight(&glass_a)
            .expect("explicitly trusted publisher passes preflight verification");

        // Trust is a startup snapshot. Replacing the file does not mutate an already-running
        // service; the desktop reloads policy by restarting.
        write_trust_config(&config, &[&key_b], &[]);
        before_rotation
            .launch_plan(&glass_a)
            .expect("existing service retains its startup trust snapshot");

        let mut unknown = production_service(&config, &root);
        let unknown_error = unknown
            .inspect(&glass_a)
            .expect_err("a key absent from the new file is unknown");
        assert!(is_unknown_signing_key(&unknown_error));
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        let fixture_error = unknown
            .inspect(&example)
            .expect_err("production trust does not merge the compiled example key");
        assert!(is_unknown_signing_key(&fixture_error));

        write_trust_config(&config, &[&key_b], &[key_a_id]);
        let mut after_rotation = production_service(&config, &root);
        let revoked_error = after_rotation
            .inspect(&glass_a)
            .expect_err("the old key is revoked after rotation");
        assert!(is_revoked_signing_key(&revoked_error));
        let preflight_error = after_rotation
            .preflight(&glass_a)
            .expect_err("revoked bundle must fail before runtime preflight");
        assert!(is_revoked_signing_key(&preflight_error));
        let start_error = after_rotation
            .start(&glass_a, RunOptions::default())
            .expect_err("revoked bundle must fail before QEMU spawn");
        assert!(is_revoked_signing_key(&start_error));

        let rotated_preview = after_rotation
            .inspect(&glass_b)
            .expect("newly trusted key reaches preview");
        assert_eq!(rotated_preview.publisher_identity, PUBLISHER_IDENTITY);
        after_rotation
            .launch_plan(&glass_b)
            .expect("newly trusted key reaches pre-run planning");
        after_rotation
            .preflight(&glass_b)
            .expect("newly trusted key passes preflight verification");

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(assets);
    }

    #[test]
    fn missing_or_invalid_production_trust_never_falls_back_to_fixture_keys() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let missing = root.join("missing-trust.json");
        assert!(AppService::from_trust_config(&missing).is_err());

        let invalid = root.join("invalid-trust.json");
        fs::write(
            &invalid,
            br#"{"schemaVersion":99,"trustedPublishers":[],"revokedKeyIds":[]}"#,
        )
        .unwrap();
        assert!(AppService::from_trust_config(&invalid).is_err());

        let mut denied = AppService::deny_all();
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        let error = denied
            .inspect(&example)
            .expect_err("deny-all policy rejects the test fixture publisher");
        assert!(is_unknown_signing_key(&error));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampered_trusted_bundle_reports_digest_failure_without_launching() {
        let key = SigningKey::from_bytes(&[43_u8; 32]);
        let root = temp_root();
        let glass = root.join("tampered.glass");
        fs::create_dir_all(&root).unwrap();
        signed_glass(&glass, &key);
        install_test_runtime(&root);
        let config = root.join("trust.json");
        write_trust_config(&config, &[&key], &[]);
        fs::write(glass.join("rootfs.img"), b"tampered after signing").unwrap();

        let mut service = production_service(&config, &root);
        let error = service
            .inspect(&glass)
            .expect_err("a trusted publisher cannot make tampered contents valid");
        assert_eq!(error.summary, "This glass could not be verified.");
        assert!(error.detail.contains("subject digest mismatch"));
        assert_eq!(error.phase, LifecycleState::Verifying);

        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod verified_runtime_launch_tests {
    use super::*;
    use rootglass_signature::TrustStore;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rootglass-verified-runtime-{}-{nonce}",
            std::process::id()
        ))
    }

    fn digest(path: &Path) -> String {
        Sha256::digest(fs::read(path).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn install_test_runtime(root: &Path) -> PathBuf {
        let version_root = root.join("runtime-assets/test-1");
        fs::create_dir_all(&version_root).unwrap();
        for (name, bytes) in [
            ("kernel", b"verified test kernel".as_slice()),
            ("initramfs", b"verified test initramfs".as_slice()),
            ("qemu", b"verified test qemu".as_slice()),
        ] {
            fs::write(version_root.join(name), bytes).unwrap();
        }
        let manifest = json!({
            "schema_version": 1,
            "runtime_version": "test-1",
            "assets": [
                {"kind": "kernel", "path": "kernel", "sha256": digest(&version_root.join("kernel"))},
                {"kind": "initramfs", "path": "initramfs", "sha256": digest(&version_root.join("initramfs"))},
                {"kind": "qemu", "path": "qemu", "sha256": digest(&version_root.join("qemu"))}
            ]
        });
        fs::write(
            version_root.join("assets.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        version_root
    }

    fn service_with_assets(root: &Path) -> AppService {
        AppService::with_trust_store(TrustStore::example_fixture()).with_verified_runtime_assets(
            RuntimeAssetsResolver::new(root, root.join("user-data")),
            "test-1",
        )
    }

    #[test]
    fn app_launch_plan_uses_integrity_checked_assets_and_qemu() {
        let root = temp_root();
        let version_root = install_test_runtime(&root);
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");

        let (_summary, command) = service_with_assets(&root)
            .launch_plan(&source)
            .expect("verified app launch plan should be prepared");

        assert!(command.starts_with(&version_root.join("qemu").display().to_string()));
        assert!(command.contains(&format!(
            "-kernel {}",
            version_root.join("kernel").display()
        )));
        assert!(command.contains(&format!(
            "-initrd {}",
            version_root.join("initramfs").display()
        )));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_launch_plan_rejects_tampered_runtime_asset_before_spawn() {
        let root = temp_root();
        let version_root = install_test_runtime(&root);
        fs::write(version_root.join("kernel"), b"tampered kernel").unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");

        let error = service_with_assets(&root)
            .launch_plan(&source)
            .expect_err("tampered runtime must not produce a launch plan");

        assert!(error
            .detail
            .contains("runtime asset integrity check failed"));
        let _ = fs::remove_dir_all(root);
    }
}
