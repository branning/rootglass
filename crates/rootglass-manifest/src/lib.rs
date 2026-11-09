use serde::Deserialize;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const MANIFEST_FILE_NAME: &str = "rootglass.toml";
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;
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
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
        toml::from_str(content)
    }

    pub fn validate_manifest_only(&self) -> Result<(), ManifestError> {
        if self.schema_version != SUPPORTED_SCHEMA_VERSION {
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

#[cfg(test)]
mod tests {
    use super::*;

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
