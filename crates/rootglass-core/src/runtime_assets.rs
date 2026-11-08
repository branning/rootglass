//! Integrity checked discovery for versioned Rootglass runtime payloads.
//!
//! The resolver is deliberately independent of the process working directory: callers
//! provide the application resource directory and the per-user data directory. A runtime
//! manifest hashes the kernel, initramfs, QEMU executable, and any QEMU support files.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAssetKind {
    Kernel,
    Initramfs,
    Qemu,
    /// Shared libraries and data files required by the bundled QEMU build.
    Support,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RuntimeAssetEntry {
    pub kind: RuntimeAssetKind,
    /// Relative to the directory containing the versioned asset manifest.
    pub path: PathBuf,
    /// Lowercase or uppercase hexadecimal SHA-256 digest.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RuntimeAssetManifest {
    pub schema_version: u32,
    pub runtime_version: String,
    pub assets: Vec<RuntimeAssetEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRuntimeSet {
    pub runtime_version: String,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub qemu: PathBuf,
    pub support_assets: Vec<PathBuf>,
}

#[derive(Debug, Error)]
pub enum RuntimeAssetsError {
    #[error("runtime asset manifest is missing: {0}")]
    MissingManifest(PathBuf),
    #[error("failed to read runtime asset manifest {path}: {source}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse runtime asset manifest: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported runtime asset manifest schema {0}")]
    UnsupportedSchema(u32),
    #[error("runtime asset manifest exceeds the 1 MiB size limit: {0}")]
    ManifestTooLarge(PathBuf),
    #[error("invalid runtime version")]
    InvalidVersion,
    #[error("runtime manifest must contain exactly one {0} asset")]
    MissingOrDuplicate(&'static str),
    #[error("unsafe runtime asset path: {0}")]
    UnsafePath(PathBuf),
    #[error("runtime asset is missing or not a regular file: {0}")]
    MissingAsset(PathBuf),
    #[error("invalid SHA-256 in runtime manifest for {0}")]
    InvalidDigest(PathBuf),
    #[error("runtime asset integrity check failed for {path}: expected {expected}, got {actual}")]
    DigestMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("failed to read runtime asset {path}: {source}")]
    ReadAsset {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Searches an app-bundled runtime first, then the user-managed app-data runtime.
/// Each root is expected to contain `runtime-assets/<version>/assets.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAssetsResolver {
    resource_root: PathBuf,
    app_data_root: PathBuf,
}

impl RuntimeAssetsResolver {
    pub fn new(resource_root: impl Into<PathBuf>, app_data_root: impl Into<PathBuf>) -> Self {
        Self {
            resource_root: resource_root.into(),
            app_data_root: app_data_root.into(),
        }
    }

    pub fn resolve(&self, runtime_version: &str) -> Result<ResolvedRuntimeSet, RuntimeAssetsError> {
        if runtime_version.is_empty()
            || !runtime_version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
        {
            return Err(RuntimeAssetsError::InvalidVersion);
        }
        let relative = Path::new("runtime-assets")
            .join(runtime_version)
            .join("assets.json");
        let (manifest_path, allowed_root) = [&self.resource_root, &self.app_data_root]
            .iter()
            .filter_map(|root| {
                let path = root.join(&relative);
                path.is_file().then(|| (path, (*root).clone()))
            })
            .next()
            .ok_or_else(|| {
                RuntimeAssetsError::MissingManifest(self.resource_root.join(relative))
            })?;
        let canonical_manifest =
            manifest_path
                .canonicalize()
                .map_err(|source| RuntimeAssetsError::ReadManifest {
                    path: manifest_path.clone(),
                    source,
                })?;
        let canonical_root =
            allowed_root
                .canonicalize()
                .map_err(|source| RuntimeAssetsError::ReadManifest {
                    path: allowed_root.clone(),
                    source,
                })?;
        if !canonical_manifest.starts_with(&canonical_root) {
            return Err(RuntimeAssetsError::UnsafePath(manifest_path));
        }
        let metadata =
            manifest_path
                .metadata()
                .map_err(|source| RuntimeAssetsError::ReadManifest {
                    path: manifest_path.clone(),
                    source,
                })?;
        if metadata.len() > MAX_MANIFEST_BYTES {
            return Err(RuntimeAssetsError::ManifestTooLarge(manifest_path));
        }
        let bytes =
            std::fs::read(&manifest_path).map_err(|source| RuntimeAssetsError::ReadManifest {
                path: manifest_path.clone(),
                source,
            })?;
        let manifest: RuntimeAssetManifest = serde_json::from_slice(&bytes)?;
        if manifest.schema_version != 1 {
            return Err(RuntimeAssetsError::UnsupportedSchema(
                manifest.schema_version,
            ));
        }
        if manifest.runtime_version != runtime_version {
            return Err(RuntimeAssetsError::InvalidVersion);
        }
        let base = canonical_manifest.parent().expect("manifest has parent");
        let kernel = self.resolve_kind(base, &manifest, RuntimeAssetKind::Kernel, "kernel")?;
        let initramfs =
            self.resolve_kind(base, &manifest, RuntimeAssetKind::Initramfs, "initramfs")?;
        let qemu = self.resolve_kind(base, &manifest, RuntimeAssetKind::Qemu, "qemu")?;
        let support_assets = manifest
            .assets
            .iter()
            .filter(|asset| asset.kind == RuntimeAssetKind::Support)
            .map(|asset| self.resolve_entry(base, asset))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ResolvedRuntimeSet {
            runtime_version: manifest.runtime_version,
            kernel,
            initramfs,
            qemu,
            support_assets,
        })
    }

    fn resolve_kind(
        &self,
        base: &Path,
        manifest: &RuntimeAssetManifest,
        kind: RuntimeAssetKind,
        name: &'static str,
    ) -> Result<PathBuf, RuntimeAssetsError> {
        let mut found = manifest.assets.iter().filter(|asset| asset.kind == kind);
        let asset = found
            .next()
            .ok_or(RuntimeAssetsError::MissingOrDuplicate(name))?;
        if found.next().is_some() {
            return Err(RuntimeAssetsError::MissingOrDuplicate(name));
        }
        self.resolve_entry(base, asset)
    }

    fn resolve_entry(
        &self,
        base: &Path,
        asset: &RuntimeAssetEntry,
    ) -> Result<PathBuf, RuntimeAssetsError> {
        validate_relative(&asset.path)?;
        if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RuntimeAssetsError::InvalidDigest(asset.path.clone()));
        }
        let base = base
            .canonicalize()
            .map_err(|source| RuntimeAssetsError::ReadAsset {
                path: base.to_path_buf(),
                source,
            })?;
        let joined = base.join(&asset.path);
        let resolved = joined
            .canonicalize()
            .map_err(|_| RuntimeAssetsError::MissingAsset(joined.clone()))?;
        if !resolved.starts_with(&base) {
            return Err(RuntimeAssetsError::UnsafePath(asset.path.clone()));
        }
        if !resolved.is_file() {
            return Err(RuntimeAssetsError::MissingAsset(resolved));
        }
        let actual = digest_file(&resolved)?;
        let expected = asset.sha256.to_ascii_lowercase();
        if actual != expected {
            return Err(RuntimeAssetsError::DigestMismatch {
                path: resolved,
                expected,
                actual,
            });
        }
        Ok(resolved)
    }
}

fn validate_relative(path: &Path) -> Result<(), RuntimeAssetsError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(RuntimeAssetsError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn digest_file(path: &Path) -> Result<String, RuntimeAssetsError> {
    let mut file = File::open(path).map_err(|source| RuntimeAssetsError::ReadAsset {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| RuntimeAssetsError::ReadAsset {
                path: path.to_path_buf(),
                source,
            })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rootglass-assets-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(path.join("runtime-assets/v1")).unwrap();
            Self(path)
        }
        fn put(&self, name: &str, bytes: &[u8]) -> String {
            std::fs::write(self.0.join("runtime-assets/v1").join(name), bytes).unwrap();
            digest_file(&self.0.join("runtime-assets/v1").join(name)).unwrap()
        }
        fn manifest(&self, kernel: &str, initramfs: &str, qemu: &str) {
            let json = format!(
                r#"{{"schema_version":1,"runtime_version":"v1","assets":[{{"kind":"kernel","path":"kernel","sha256":"{kernel}"}},{{"kind":"initramfs","path":"initrd","sha256":"{initramfs}"}},{{"kind":"qemu","path":"qemu","sha256":"{qemu}"}}]}}"#
            );
            std::fs::write(self.0.join("runtime-assets/v1/assets.json"), json).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_and_hash_checks_bundle_assets() {
        let fixture = Fixture::new();
        let k = fixture.put("kernel", b"kernel bytes");
        let i = fixture.put("initrd", b"initrd bytes");
        let q = fixture.put("qemu", b"qemu bytes");
        fixture.manifest(&k, &i, &q);
        let assets = RuntimeAssetsResolver::new(&fixture.0, fixture.0.join("user-data"))
            .resolve("v1")
            .unwrap();
        assert_eq!(assets.kernel, fixture.0.join("runtime-assets/v1/kernel"));
        assert_eq!(assets.initramfs, fixture.0.join("runtime-assets/v1/initrd"));
        assert_eq!(assets.qemu, fixture.0.join("runtime-assets/v1/qemu"));
    }

    #[test]
    fn rejects_tampered_asset_before_returning_runtime() {
        let fixture = Fixture::new();
        let k = fixture.put("kernel", b"kernel bytes");
        let i = fixture.put("initrd", b"initrd bytes");
        let q = fixture.put("qemu", b"qemu bytes");
        fixture.manifest(&k, &i, &q);
        std::fs::write(fixture.0.join("runtime-assets/v1/kernel"), b"changed").unwrap();
        assert!(matches!(
            RuntimeAssetsResolver::new(&fixture.0, "").resolve("v1"),
            Err(RuntimeAssetsError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn rejects_manifest_path_traversal() {
        let fixture = Fixture::new();
        let k = fixture.put("kernel", b"kernel bytes");
        let i = fixture.put("initrd", b"initrd bytes");
        let q = fixture.put("qemu", b"qemu bytes");
        fixture.manifest(&k, &i, &q);
        let path = fixture.0.join("runtime-assets/v1/assets.json");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"path\":\"kernel\"", "\"path\":\"../kernel\"");
        std::fs::write(path, text).unwrap();
        assert!(matches!(
            RuntimeAssetsResolver::new(&fixture.0, "").resolve("v1"),
            Err(RuntimeAssetsError::UnsafePath(_))
        ));
    }
}
