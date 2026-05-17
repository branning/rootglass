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
        }
    }
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

        self.transition(LifecycleState::Verifying);

        // Signature verification will be wired here after the signature trade study is resolved.
        // For now, the manifest crate enforces that publisher/signature metadata is present.

        self.transition(LifecycleState::Ready);
        Ok(LaunchRequest::from_glass(&glass))
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

impl LaunchRequest {
    pub fn from_glass(glass: &Glass) -> Self {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_launch_request_from_sample_glass() {
        let mut runtime = RootglassRuntime::new();
        let request = runtime
            .prepare_launch("../../examples/hello-world.glass")
            .expect("manifest-only launch preparation succeeds");

        assert_eq!(runtime.status().state, LifecycleState::Ready);
        assert_eq!(request.architecture, "aarch64");
        assert_eq!(request.runtime_profile, "virt-linux");
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
        let result = runtime.prepare_launch("../../examples/does-not-exist.glass");

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
}
