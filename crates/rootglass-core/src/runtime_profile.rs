use rootglass_manifest::{
    BUILTIN_AARCH64_DEVICE_TREE, BUILTIN_AARCH64_KERNEL, ManifestError,
};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeProfileError {
    #[error("unsupported kernel reference: {0}")]
    UnsupportedKernel(String),

    #[error("unsupported device tree reference: {0}")]
    UnsupportedDeviceTree(String),

    #[error("runtime asset root is not configured")]
    MissingAssetRoot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAssetResolver {
    asset_root: PathBuf,
}

impl Default for RuntimeAssetResolver {
    fn default() -> Self {
        Self::new("runtime-assets")
    }
}

impl RuntimeAssetResolver {
    pub fn new(asset_root: impl Into<PathBuf>) -> Self {
        Self {
            asset_root: asset_root.into(),
        }
    }

    pub fn asset_root(&self) -> &Path {
        self.asset_root.as_path()
    }

    pub fn resolve(
        &self,
        kernel_ref: &str,
        device_tree_ref: &str,
    ) -> Result<ResolvedRuntimeAssets, RuntimeProfileError> {
        if self.asset_root.as_os_str().is_empty() {
            return Err(RuntimeProfileError::MissingAssetRoot);
        }

        let kernel = match kernel_ref {
            BUILTIN_AARCH64_KERNEL => self.asset_root.join("alpine-aarch64/vmlinuz-virt"),
            other => return Err(RuntimeProfileError::UnsupportedKernel(other.to_string())),
        };

        let device_tree = match device_tree_ref {
            // QEMU's aarch64 virt machine can synthesize the device tree. We keep the
            // manifest reference so bundles declare the expected profile, but do not
            // pass a `-dtb` argument for the MVP profile.
            BUILTIN_AARCH64_DEVICE_TREE => None,
            other => {
                return Err(RuntimeProfileError::UnsupportedDeviceTree(
                    other.to_string(),
                ))
            }
        };

        Ok(ResolvedRuntimeAssets {
            kernel,
            device_tree,
            initrd: Some(self.asset_root.join("alpine-aarch64/initramfs-virt")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRuntimeAssets {
    pub kernel: PathBuf,
    pub device_tree: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
}

impl From<ManifestError> for RuntimeProfileError {
    fn from(error: ManifestError) -> Self {
        match error {
            ManifestError::UnsupportedKernelReference { actual, .. } => {
                RuntimeProfileError::UnsupportedKernel(actual)
            }
            ManifestError::UnsupportedDeviceTreeReference { actual, .. } => {
                RuntimeProfileError::UnsupportedDeviceTree(actual)
            }
            other => RuntimeProfileError::UnsupportedKernel(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_builtin_aarch64_profile_assets() {
        let resolver = RuntimeAssetResolver::new("runtime-assets");
        let assets = resolver
            .resolve(BUILTIN_AARCH64_KERNEL, BUILTIN_AARCH64_DEVICE_TREE)
            .expect("builtin assets resolve");

        assert_eq!(
            assets.kernel,
            PathBuf::from("runtime-assets/alpine-aarch64/vmlinuz-virt")
        );
        assert_eq!(assets.device_tree, None);
        assert_eq!(
            assets.initrd,
            Some(PathBuf::from("runtime-assets/alpine-aarch64/initramfs-virt"))
        );
    }
}
