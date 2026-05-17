use rootglass_manifest::{Glass, ManifestError};
use std::path::{Path, PathBuf};
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
            hvf_available: platform == HostPlatform::MacOs,
            whpx_available: platform == HostPlatform::Windows,
            kvm_available: platform == HostPlatform::Linux && Path::new("/dev/kvm").exists(),
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
pub struct LaunchRequest {
    pub glass_root: PathBuf,
    pub runtime_profile: String,
    pub architecture: String,
    pub kernel: String,
    pub device_tree: String,
    pub rootfs: PathBuf,
    pub writable_overlay: Option<PathBuf>,
    pub kernel_args: Vec<String>,
    pub memory_mb: u32,
    pub cpus: u32,
    pub requires_hardware_acceleration: bool,
    pub accelerator: Option<HostAccelerator>,
}

#[derive(Debug, Default)]
pub struct RootglassRuntime {
    status: RuntimeStatus,
}

impl RootglassRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn status(&self) -> &RuntimeStatus {
        &self.status
    }

    pub fn prepare_launch(&mut self, glass_root: impl AsRef<Path>) -> Result<LaunchRequest, CoreError> {
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

        let accelerator = match select_accelerator(&glass, capabilities) {
            Ok(accelerator) => accelerator,
            Err(error) => {
                self.fail(CoreError::HostPreflight(error), LifecycleState::Validating)?;
                unreachable!("fail always returns Err")
            }
        };

        self.transition(LifecycleState::Verifying);

        // Signature verification will be wired here after the signature trade study is resolved.
        // For now, the manifest crate enforces that publisher/signature metadata is present.

        self.transition(LifecycleState::Ready);
        Ok(LaunchRequest::from_glass(&glass, accelerator))
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
                || constraints.macos.as_ref().is_some_and(|macos| macos.hypervisor);
            if requires_hvf && !capabilities.hvf_available {
                return Err(HostPreflightError::MissingHvf);
            }
            Ok(capabilities.hvf_available.then_some(HostAccelerator::Hvf))
        }
        HostPlatform::Windows => {
            let requires_whpx = runtime.requires_hardware_acceleration
                || constraints.windows.as_ref().is_some_and(|windows| windows.whpx);
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
    pub fn from_glass(glass: &Glass, accelerator: Option<HostAccelerator>) -> Self {
        let manifest = &glass.manifest;
        Self {
            glass_root: glass.root.clone(),
            runtime_profile: manifest.runtime.profile.clone(),
            architecture: manifest.runtime.architecture.clone(),
            kernel: manifest.boot.kernel.clone(),
            device_tree: manifest.boot.device_tree.clone(),
            rootfs: glass.root.join(&manifest.boot.rootfs),
            writable_overlay: manifest
                .boot
                .writable_overlay
                .as_ref()
                .map(|path| glass.root.join(path)),
            kernel_args: manifest.boot.kernel_args.clone(),
            memory_mb: manifest.resources.memory_mb,
            cpus: manifest.resources.cpus,
            requires_hardware_acceleration: manifest.runtime.requires_hardware_acceleration,
            accelerator,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_launch_request_from_sample_glass() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("manifest-only launch preparation succeeds");

        assert_eq!(runtime.status().state, LifecycleState::Ready);
        assert_eq!(request.architecture, "aarch64");
        assert_eq!(request.runtime_profile, "virt-linux");
        assert_eq!(request.accelerator, None);
        assert!(request.rootfs.ends_with("rootfs.img"));
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
        let result = runtime.prepare_launch_with_host("../../examples/does-not-exist.glass", &capabilities);

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
}
