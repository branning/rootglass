pub mod identity;
pub mod runtime_assets;
pub mod runtime_profile;
pub mod storage;

use rootglass_manifest::{
    ArtifactKind, BootData, Glass, ManifestError, RequiredFeature, TargetArchitecture,
    TargetMachine, TypedDevice, LEGACY_SCHEMA_VERSION, MULTI_ARTIFACT_SCHEMA_VERSION,
};
use rootglass_signature::{verify_glass_dir, SignatureError, TrustStore, VerificationReport};
use runtime_assets::{RuntimeAssetsError, RuntimeAssetsResolver};
use runtime_profile::{ResolvedRuntimeAssets, RuntimeAssetResolver, RuntimeProfileError};
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::sync::OnceLock;
use storage::{OverlayStore, StorageSelection};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Stopped,
    Validating,
    Verifying,
    Ready,
    Starting,
    Booting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub state: LifecycleState,
    pub last_error: Option<UserFacingError>,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self {
            state: LifecycleState::Stopped,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserFacingError {
    pub summary: String,
    pub detail: String,
    pub stage: LifecycleState,
    pub suggestion: Option<String>,
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("glass validation failed: {0}")]
    Manifest(#[from] ManifestError),

    #[error("host preflight failed: {0}")]
    HostPreflight(#[from] HostPreflightError),

    #[error("signature verification failed: {0}")]
    Signature(#[from] SignatureError),

    #[error("runtime profile resolution failed: {0}")]
    RuntimeProfile(#[from] RuntimeProfileError),

    #[error("runtime asset resolution failed: {0}")]
    RuntimeAssets(#[from] RuntimeAssetsError),

    #[error("encrypted launch profile is not supported: {0}")]
    UnsupportedEncryptedProfile(String),
}

impl CoreError {
    pub fn to_user_facing(&self, stage: LifecycleState) -> UserFacingError {
        match self {
            CoreError::Manifest(error) => UserFacingError {
                summary: "This glass is not ready to run.".to_string(),
                detail: error.to_string(),
                stage,
                suggestion: Some(
                    "Check the glass manifest and required image files, then try again.".to_string(),
                ),
            },
            CoreError::HostPreflight(error) => UserFacingError {
                summary: "This computer does not meet the glass runtime requirements.".to_string(),
                detail: error.to_string(),
                stage,
                suggestion: Some(
                    "Use a supported host, enable virtualization support, or choose a glass that does not require hardware acceleration.".to_string(),
                ),
            },
            CoreError::Signature(error) => UserFacingError {
                summary: "This glass could not be verified.".to_string(),
                detail: error.to_string(),
                stage,
                suggestion: Some(
                    "Only open signed glasses from trusted publishers. Re-download the glass or contact the publisher if verification fails.".to_string(),
                ),
            },
            CoreError::RuntimeAssets(error) => UserFacingError {
                summary: "The installed runtime assets could not be verified.".to_string(),
                detail: error.to_string(),
                stage,
                suggestion: Some(
                    "Repair the Rootglass runtime installation or choose a valid runtime version.".to_string(),
                ),
            },
            CoreError::RuntimeProfile(error) => UserFacingError {
                summary: "This glass uses a runtime profile that cannot be resolved.".to_string(),
                detail: error.to_string(),
                stage,
                suggestion: Some(
                    "Install or generate the required Rootglass runtime assets, then try again.".to_string(),
                ),
            },
            CoreError::UnsupportedEncryptedProfile(detail) => UserFacingError {
                summary: "This encrypted launch profile is not supported yet.".to_string(),
                detail: detail.clone(),
                stage,
                suggestion: Some(
                    "Choose a profile with one encrypted AArch64 Linux root disk on QEMU virt and no additional devices.".to_string(),
                ),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPlatform {
    MacOs,
    Windows,
    Linux,
    Other,
}

impl HostPlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAccelerator {
    Hvf,
    Whpx,
    Kvm,
}

impl HostAccelerator {
    pub fn qemu_name(self) -> &'static str {
        match self {
            Self::Hvf => "hvf",
            Self::Whpx => "whpx",
            Self::Kvm => "kvm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCapabilities {
    pub platform: HostPlatform,
    pub hvf_available: bool,
    pub whpx_available: bool,
    pub kvm_available: bool,
}

impl HostCapabilities {
    pub fn detect_current() -> Self {
        let platform = HostPlatform::current();
        Self {
            platform,
            hvf_available: platform == HostPlatform::MacOs && detect_hvf_available(),
            // Rootglass does not yet probe each bundled Windows QEMU runtime
            // for WHPX support. Keep acceleration disabled until the shipped
            // runtime variants are verified and capability detection exists.
            whpx_available: false,
            kvm_available: platform == HostPlatform::Linux
                && cfg!(target_arch = "aarch64")
                && Path::new("/dev/kvm").exists(),
        }
    }

    pub fn without_acceleration(platform: HostPlatform) -> Self {
        Self {
            platform,
            hvf_available: false,
            whpx_available: false,
            kvm_available: false,
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn detect_hvf_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(probe_hvf_available)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn probe_hvf_available() -> bool {
    use std::ffi::c_void;

    #[link(name = "Hypervisor", kind = "framework")]
    extern "C" {
        fn hv_vm_create(config: *const c_void, flags: u64) -> u32;
        fn hv_vm_destroy() -> u32;
    }

    // Creating and destroying a probe VM checks that Hypervisor.framework is
    // usable by this process, which an OS-version check cannot establish on
    // hosted or otherwise virtualized macOS systems.
    unsafe {
        if hv_vm_create(std::ptr::null(), 0) != 0 {
            return false;
        }
        hv_vm_destroy() == 0
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn detect_hvf_available() -> bool {
    false
}

#[derive(Debug, Error)]
pub enum HostPreflightError {
    #[error("the glass requires hardware acceleration, but no supported accelerator is available for this host")]
    AccelerationRequiredButUnavailable,

    #[error("the glass requires macOS Hypervisor.framework acceleration, but it is not available")]
    MissingHvf,

    #[error("the glass requires Windows WHPX acceleration, but it is not available")]
    MissingWhpx,

    #[error("the glass requires Linux KVM acceleration, but /dev/kvm is not available")]
    MissingKvm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureVerification {
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
    pub subject_count: usize,
}

impl From<VerificationReport> for SignatureVerification {
    fn from(report: VerificationReport) -> Self {
        Self {
            publisher_name: report.publisher_name,
            publisher_identity: report.publisher_identity,
            key_id: report.key_id,
            subject_count: report.subject_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub glass_root: PathBuf,
    pub runtime_profile: String,
    pub architecture: String,
    pub kernel_ref: String,
    pub device_tree_ref: String,
    pub kernel: PathBuf,
    pub device_tree: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub rootfs: PathBuf,
    pub writable_overlay: Option<PathBuf>,
    pub storage: StorageSelection,
    pub signature: SignatureVerification,
    pub kernel_args: Vec<String>,
    pub memory_mb: u32,
    pub cpus: u32,
    pub network_mode: String,
    pub requires_hardware_acceleration: bool,
    pub accelerator: Option<HostAccelerator>,
    /// Verified QEMU executable for packaged runtimes; `None` uses the developer PATH resolver.
    pub qemu_binary: Option<PathBuf>,
}

#[derive(Debug)]
pub struct RootglassRuntime {
    status: RuntimeStatus,
    overlay_store: OverlayStore,
    trust_store: TrustStore,
    asset_resolver: RuntimeAssetResolver,
    verified_asset_resolver: Option<VerifiedAssetResolver>,
    selected_local_overlay: Option<String>,
}

#[derive(Debug, Clone)]
struct VerifiedAssetResolver {
    resolver: RuntimeAssetsResolver,
    runtime_version: String,
}

impl Default for RootglassRuntime {
    fn default() -> Self {
        Self {
            status: RuntimeStatus::default(),
            overlay_store: OverlayStore::new(".rootglass/overlays"),
            trust_store: TrustStore::example_fixture(),
            asset_resolver: RuntimeAssetResolver::default(),
            verified_asset_resolver: None,
            selected_local_overlay: None,
        }
    }
}

impl RootglassRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_overlay_store(overlay_store: OverlayStore) -> Self {
        Self {
            overlay_store,
            ..Self::default()
        }
    }

    pub fn with_trust_store(trust_store: TrustStore) -> Self {
        Self {
            trust_store,
            ..Self::default()
        }
    }

    pub fn with_asset_resolver(asset_resolver: RuntimeAssetResolver) -> Self {
        Self {
            asset_resolver,
            ..Self::default()
        }
    }

    /// Use a versioned, integrity-checked runtime set from app resources or user app data.
    pub fn with_verified_runtime_assets(
        mut self,
        resolver: RuntimeAssetsResolver,
        runtime_version: impl Into<String>,
    ) -> Self {
        self.verified_asset_resolver = Some(VerifiedAssetResolver {
            resolver,
            runtime_version: runtime_version.into(),
        });
        self
    }

    pub fn select_local_overlay(&mut self, overlay_name: impl Into<String>) {
        self.selected_local_overlay = Some(overlay_name.into());
    }

    pub fn clear_local_overlay_selection(&mut self) {
        self.selected_local_overlay = None;
    }

    pub fn status(&self) -> &RuntimeStatus {
        &self.status
    }

    pub fn prepare_launch(
        &mut self,
        glass_root: impl AsRef<Path>,
    ) -> Result<LaunchRequest, CoreError> {
        let capabilities = HostCapabilities::detect_current();
        self.prepare_launch_with_host(glass_root, &capabilities)
    }

    pub fn prepare_launch_with_host(
        &mut self,
        glass_root: impl AsRef<Path>,
        capabilities: &HostCapabilities,
    ) -> Result<LaunchRequest, CoreError> {
        self.transition(LifecycleState::Validating);

        let glass = match Glass::load(glass_root) {
            Ok(glass) => glass,
            Err(error) => {
                self.fail(CoreError::Manifest(error), LifecycleState::Validating)?;
                unreachable!("fail always returns Err")
            }
        };

        if let Err(error) = glass.validate_manifest_only() {
            self.fail(CoreError::Manifest(error), LifecycleState::Validating)?;
            unreachable!("fail always returns Err")
        }
        if glass.manifest.schema_version != LEGACY_SCHEMA_VERSION {
            self.fail(
                CoreError::Manifest(ManifestError::UnsupportedLaunchSchema {
                    actual: glass.manifest.schema_version,
                }),
                LifecycleState::Validating,
            )?;
            unreachable!("fail always returns Err")
        }

        let accelerator = match select_accelerator(&glass, capabilities) {
            Ok(accelerator) => accelerator,
            Err(error) => {
                self.fail(CoreError::HostPreflight(error), LifecycleState::Validating)?;
                unreachable!("fail always returns Err")
            }
        };

        let (runtime_assets, qemu_binary) = {
            let profile_assets = match self.asset_resolver.resolve(
                &glass.manifest.boot.kernel,
                &glass.manifest.boot.device_tree,
            ) {
                Ok(assets) => assets,
                Err(error) => {
                    self.fail(CoreError::RuntimeProfile(error), LifecycleState::Validating)?;
                    unreachable!("fail always returns Err")
                }
            };
            if let Some(config) = &self.verified_asset_resolver {
                let resolved = match config.resolver.resolve(&config.runtime_version) {
                    Ok(assets) => assets,
                    Err(error) => {
                        self.fail(CoreError::RuntimeAssets(error), LifecycleState::Validating)?;
                        unreachable!("fail always returns Err")
                    }
                };
                (
                    ResolvedRuntimeAssets {
                        kernel: resolved.kernel,
                        device_tree: profile_assets.device_tree,
                        initrd: Some(resolved.initramfs),
                    },
                    Some(resolved.qemu),
                )
            } else {
                (profile_assets, None)
            }
        };

        self.transition(LifecycleState::Verifying);

        let signature = match verify_glass_dir(
            &glass.root,
            &glass.manifest.publisher.signature,
            &self.trust_store,
        ) {
            Ok(report) => SignatureVerification::from(report),
            Err(error) => {
                self.fail(CoreError::Signature(error), LifecycleState::Verifying)?;
                unreachable!("fail always returns Err")
            }
        };

        self.transition(LifecycleState::Ready);
        Ok(LaunchRequest::from_glass(
            &glass,
            accelerator,
            runtime_assets,
            qemu_binary,
            signature,
            &self.overlay_store,
            self.selected_local_overlay.as_deref(),
        ))
    }

    /// Prepare the currently supported encrypted schema-2 profile. This
    /// deliberately accepts only one AArch64 Linux QEMU-virt node with one
    /// encrypted virtio root disk; unsupported devices and multi-node profiles
    /// fail before the QEMU adapter is constructed.
    pub fn prepare_encrypted_launch(
        &mut self,
        glass: &Glass,
        signature: VerificationReport,
    ) -> Result<LaunchRequest, CoreError> {
        self.transition(LifecycleState::Validating);
        glass.validate_manifest_only()?;
        if glass.manifest.schema_version != MULTI_ARTIFACT_SCHEMA_VERSION {
            return Err(CoreError::Manifest(
                ManifestError::UnsupportedLaunchSchema {
                    actual: glass.manifest.schema_version,
                },
            ));
        }

        let catalog = glass.manifest.artifact_catalog.as_ref().ok_or_else(|| {
            CoreError::UnsupportedEncryptedProfile(
                "the signed manifest has no artifact catalog".to_string(),
            )
        })?;
        if signature.schema_version != MULTI_ARTIFACT_SCHEMA_VERSION
            || signature.artifact_catalog.as_ref() != Some(catalog)
        {
            return Err(CoreError::UnsupportedEncryptedProfile(
                "the launch manifest does not match the verified archive report".to_string(),
            ));
        }
        let profile = catalog
            .profiles
            .get(&catalog.default_profile)
            .ok_or_else(|| {
                CoreError::UnsupportedEncryptedProfile(
                    "the signed default launch profile is missing".to_string(),
                )
            })?;
        for required in [
            RequiredFeature::LaunchProfilesV1,
            RequiredFeature::LinuxNodeV1,
            RequiredFeature::EncryptedArtifactsV1,
            RequiredFeature::TypedDevicesV1,
        ] {
            if !profile.required_features.contains(&required) {
                return Err(CoreError::UnsupportedEncryptedProfile(format!(
                    "the default profile does not declare required feature '{}'",
                    format!("{required:?}").to_ascii_lowercase()
                )));
            }
        }
        if profile.nodes.len() != 1 {
            return Err(CoreError::UnsupportedEncryptedProfile(
                "the default profile must contain exactly one node".to_string(),
            ));
        }
        let node = &profile.nodes[0];
        if node.startup_order != 0
            || node.architecture != TargetArchitecture::Aarch64
            || node.machine != TargetMachine::QemuVirt
        {
            return Err(CoreError::UnsupportedEncryptedProfile(
                "the only supported node is an AArch64 QEMU virt node at startup order zero"
                    .to_string(),
            ));
        }
        let (kernel_ref, device_tree_ref, rootfs_artifact, kernel_args) = match &node.boot {
            BootData::Linux {
                kernel,
                device_tree,
                rootfs_artifact,
                kernel_args,
            } => (kernel, device_tree, rootfs_artifact, kernel_args),
            BootData::Mcu { .. } => {
                return Err(CoreError::UnsupportedEncryptedProfile(
                    "MCU nodes do not run as an encrypted Linux rootfs".to_string(),
                ));
            }
        };
        if kernel_ref != rootglass_manifest::BUILTIN_AARCH64_KERNEL
            || device_tree_ref != rootglass_manifest::BUILTIN_AARCH64_DEVICE_TREE
        {
            return Err(CoreError::UnsupportedEncryptedProfile(
                "the node must use the built-in AArch64 Linux kernel and QEMU virt device tree"
                    .to_string(),
            ));
        }
        let root_artifact = catalog.artifacts.get(rootfs_artifact).ok_or_else(|| {
            CoreError::UnsupportedEncryptedProfile(
                "the Linux root disk artifact is missing".to_string(),
            )
        })?;
        if root_artifact.kind != ArtifactKind::LinuxBootDisk || root_artifact.encryption.is_none() {
            return Err(CoreError::UnsupportedEncryptedProfile(
                "the selected Linux root disk must be publisher-declared as encrypted".to_string(),
            ));
        }
        if profile.devices.len() != 1
            || !matches!(
                &profile.devices[0],
                TypedDevice::VirtioBlkMmio {
                    node_id,
                    artifact_id,
                    read_only: true,
                    ..
                } if node_id == &node.id && artifact_id == rootfs_artifact
            )
        {
            return Err(CoreError::UnsupportedEncryptedProfile(
                "the default profile must attach only its encrypted root disk as a read-only virtio block device".to_string(),
            ));
        }

        let (runtime_assets, qemu_binary) = if let Some(config) = &self.verified_asset_resolver {
            let resolved = config.resolver.resolve(&config.runtime_version)?;
            let profile_assets = self.asset_resolver.resolve(kernel_ref, device_tree_ref)?;
            (
                ResolvedRuntimeAssets {
                    kernel: resolved.kernel,
                    device_tree: profile_assets.device_tree,
                    initrd: Some(resolved.initramfs),
                },
                Some(resolved.qemu),
            )
        } else {
            (
                self.asset_resolver.resolve(kernel_ref, device_tree_ref)?,
                None,
            )
        };
        let placeholder = glass.root.join("encrypted-root-artifact-placeholder");
        let launch = LaunchRequest {
            glass_root: glass.root.clone(),
            runtime_profile: "virt-linux".to_string(),
            architecture: "aarch64".to_string(),
            kernel_ref: kernel_ref.clone(),
            device_tree_ref: device_tree_ref.clone(),
            kernel: runtime_assets.kernel,
            device_tree: runtime_assets.device_tree,
            initrd: runtime_assets.initrd,
            rootfs: placeholder.clone(),
            writable_overlay: None,
            storage: StorageSelection {
                read_only_rootfs: placeholder,
                writable_overlay: None,
            },
            signature: SignatureVerification::from(signature),
            kernel_args: kernel_args.clone(),
            memory_mb: profile.resources.memory_mb,
            cpus: profile.resources.cpus,
            network_mode: profile.network.mode.clone(),
            requires_hardware_acceleration: false,
            accelerator: None,
            qemu_binary,
        };
        self.transition(LifecycleState::Ready);
        Ok(launch)
    }

    fn transition(&mut self, state: LifecycleState) {
        self.status.state = state;
        self.status.last_error = None;
    }

    fn fail<T>(&mut self, error: CoreError, stage: LifecycleState) -> Result<T, CoreError> {
        self.status.state = LifecycleState::Failed;
        self.status.last_error = Some(error.to_user_facing(stage));
        Err(error)
    }
}

fn select_accelerator(
    glass: &Glass,
    capabilities: &HostCapabilities,
) -> Result<Option<HostAccelerator>, HostPreflightError> {
    let runtime = &glass.manifest.runtime;
    let constraints = &runtime.host_constraints;

    match capabilities.platform {
        HostPlatform::MacOs => {
            let requires_hvf = runtime.requires_hardware_acceleration
                || constraints
                    .macos
                    .as_ref()
                    .is_some_and(|macos| macos.hypervisor);
            if requires_hvf && !capabilities.hvf_available {
                return Err(HostPreflightError::MissingHvf);
            }
            Ok(capabilities.hvf_available.then_some(HostAccelerator::Hvf))
        }
        HostPlatform::Windows => {
            let requires_whpx = runtime.requires_hardware_acceleration
                || constraints
                    .windows
                    .as_ref()
                    .is_some_and(|windows| windows.whpx);
            if requires_whpx && !capabilities.whpx_available {
                return Err(HostPreflightError::MissingWhpx);
            }
            Ok(capabilities.whpx_available.then_some(HostAccelerator::Whpx))
        }
        HostPlatform::Linux => {
            let requires_kvm = runtime.requires_hardware_acceleration
                || constraints.linux.as_ref().is_some_and(|linux| linux.kvm);
            if requires_kvm && !capabilities.kvm_available {
                return Err(HostPreflightError::MissingKvm);
            }
            Ok(capabilities.kvm_available.then_some(HostAccelerator::Kvm))
        }
        HostPlatform::Other => {
            if runtime.requires_hardware_acceleration {
                Err(HostPreflightError::AccelerationRequiredButUnavailable)
            } else {
                Ok(None)
            }
        }
    }
}

impl LaunchRequest {
    pub fn from_glass(
        glass: &Glass,
        accelerator: Option<HostAccelerator>,
        runtime_assets: ResolvedRuntimeAssets,
        qemu_binary: Option<PathBuf>,
        signature: SignatureVerification,
        overlay_store: &OverlayStore,
        selected_local_overlay: Option<&str>,
    ) -> Self {
        let manifest = &glass.manifest;
        let storage = overlay_store.selection_for(
            &glass.root,
            &manifest.identifier,
            &manifest.boot.rootfs,
            manifest.boot.writable_overlay.as_deref(),
            selected_local_overlay,
        );
        let writable_overlay = storage
            .writable_overlay
            .as_ref()
            .map(|selection| selection.path().to_path_buf());

        Self {
            glass_root: glass.root.clone(),
            runtime_profile: manifest.runtime.profile.clone(),
            architecture: manifest.runtime.architecture.clone(),
            kernel_ref: manifest.boot.kernel.clone(),
            device_tree_ref: manifest.boot.device_tree.clone(),
            kernel: runtime_assets.kernel,
            device_tree: runtime_assets.device_tree,
            initrd: runtime_assets.initrd,
            rootfs: storage.read_only_rootfs.clone(),
            writable_overlay,
            storage,
            signature,
            kernel_args: manifest.boot.kernel_args.clone(),
            memory_mb: manifest.resources.memory_mb,
            cpus: manifest.resources.cpus,
            network_mode: manifest.network.mode.clone(),
            requires_hardware_acceleration: manifest.runtime.requires_hardware_acceleration,
            accelerator,
            qemu_binary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::WritableOverlaySelection;

    #[test]
    fn prepares_launch_request_from_sample_glass() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("launch preparation succeeds");

        assert_eq!(runtime.status().state, LifecycleState::Ready);
        assert_eq!(request.architecture, "aarch64");
        assert_eq!(request.runtime_profile, "virt-linux");
        assert_eq!(request.network_mode, "user");
        assert_eq!(request.accelerator, None);
        assert_eq!(
            request.signature.publisher_identity,
            "rootglass.dev/examples"
        );
        assert_eq!(
            request.kernel,
            PathBuf::from("runtime-assets/alpine-aarch64/vmlinuz-virt")
        );
        assert_eq!(request.device_tree, None);
        assert_eq!(
            request.initrd,
            Some(PathBuf::from(
                "runtime-assets/alpine-aarch64/initramfs-virt"
            ))
        );
        assert!(request.rootfs.ends_with("rootfs.img"));
        assert!(matches!(
            request.storage.writable_overlay,
            Some(WritableOverlaySelection::Bundled(_))
        ));
        assert!(request
            .writable_overlay
            .as_ref()
            .expect("overlay declared")
            .ends_with("state.img"));
    }

    #[test]
    fn records_user_facing_error_when_glass_is_missing() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let result =
            runtime.prepare_launch_with_host("../../examples/does-not-exist.glass", &capabilities);

        assert!(result.is_err());
        assert_eq!(runtime.status().state, LifecycleState::Failed);
        let error = runtime
            .status()
            .last_error
            .as_ref()
            .expect("user-facing error is recorded");
        assert_eq!(error.stage, LifecycleState::Validating);
        assert!(error.detail.contains("failed to read manifest"));
    }

    #[test]
    fn records_user_facing_error_when_signature_is_untrusted() {
        let mut runtime = RootglassRuntime::with_trust_store(TrustStore::default());
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let result =
            runtime.prepare_launch_with_host("../../examples/hello-world.glass", &capabilities);

        assert!(result.is_err());
        assert_eq!(runtime.status().state, LifecycleState::Failed);
        let error = runtime
            .status()
            .last_error
            .as_ref()
            .expect("user-facing error is recorded");
        assert_eq!(error.stage, LifecycleState::Verifying);
        assert!(error.detail.contains("unknown signing key"));
    }

    #[test]
    fn selects_kvm_when_linux_kvm_is_available() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities {
            platform: HostPlatform::Linux,
            hvf_available: false,
            whpx_available: false,
            kvm_available: true,
        };

        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass can use available KVM");

        assert_eq!(request.accelerator, Some(HostAccelerator::Kvm));
    }

    #[test]
    fn uses_tcg_when_macos_hvf_is_unavailable_and_acceleration_is_optional() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::MacOs);

        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass can use software emulation");

        assert_eq!(request.accelerator, None);
    }

    #[test]
    fn uses_tcg_when_windows_whpx_is_unavailable_and_acceleration_is_optional() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Windows);

        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass can use software emulation");

        assert_eq!(request.accelerator, None);
    }

    #[test]
    fn fails_preflight_when_required_host_acceleration_is_unavailable() {
        let mut glass =
            Glass::load("../../examples/hello-world.glass").expect("sample glass loads");
        glass.manifest.runtime.requires_hardware_acceleration = true;

        assert!(matches!(
            select_accelerator(
                &glass,
                &HostCapabilities::without_acceleration(HostPlatform::MacOs)
            ),
            Err(HostPreflightError::MissingHvf)
        ));
        assert!(matches!(
            select_accelerator(
                &glass,
                &HostCapabilities::without_acceleration(HostPlatform::Windows)
            ),
            Err(HostPreflightError::MissingWhpx)
        ));
        assert!(matches!(
            select_accelerator(
                &glass,
                &HostCapabilities::without_acceleration(HostPlatform::Linux)
            ),
            Err(HostPreflightError::MissingKvm)
        ));
        assert!(matches!(
            select_accelerator(
                &glass,
                &HostCapabilities::without_acceleration(HostPlatform::Other)
            ),
            Err(HostPreflightError::AccelerationRequiredButUnavailable)
        ));
    }

    #[test]
    fn selected_local_overlay_replaces_bundled_overlay() {
        let mut runtime =
            RootglassRuntime::with_overlay_store(OverlayStore::new("/tmp/rootglass/overlays"));
        runtime.select_local_overlay("experiment-1");
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);

        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass can select local overlay");

        assert!(matches!(
            request.storage.writable_overlay,
            Some(WritableOverlaySelection::Local(_))
        ));
        assert!(request
            .writable_overlay
            .as_ref()
            .expect("overlay selected")
            .ends_with("experiment-1.img"));
    }
}
