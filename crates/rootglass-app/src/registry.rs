//! Small, versioned on-disk registry for verified immutable glass packages.
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

const REGISTRY_SCHEMA: u32 = 1;
const MAX_REGISTRY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REGISTRY_PACKAGES: usize = 10_000;
const MAX_REGISTRY_FIELD_BYTES: usize = 4096;
const MAX_REGISTRY_PATH_BYTES: usize = 32 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledGlass {
    pub content_sha256: String,
    pub package_path: PathBuf,
    pub identifier: String,
    pub name: String,
    pub version: String,
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
    pub original_source: PathBuf,
    pub imported_at_unix_seconds: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    schema_version: u32,
    packages: Vec<InstalledGlass>,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("failed to read registry at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write registry at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse registry: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported registry schema {0}")]
    UnsupportedSchema(u32),
    #[error("registry exceeds the configured {limit}-byte limit")]
    TooLarge { limit: u64 },
    #[error("registry has more than the configured {limit} packages")]
    TooManyPackages { limit: usize },
    #[error("registry contains an invalid or duplicate content digest: {0}")]
    InvalidContentDigest(String),
    #[error("registry path is not a regular file: {0}")]
    UnsupportedFileType(PathBuf),
    #[error("registry package path does not match its content digest: {0}")]
    InvalidPackagePath(PathBuf),
    #[error("registry package has an invalid or oversized field: {0}")]
    InvalidPackageField(&'static str),
}

/// Registry mutations become visible only after a complete file has been flushed and replaced.
/// Replacement uses same-directory atomic rename on Unix and `MoveFileExW` replacement
/// on Windows, avoiding the `std::fs::rename` no-overwrite behavior on Windows.
pub struct PackageRegistry {
    path: PathBuf,
    packages: Vec<InstalledGlass>,
}

impl PackageRegistry {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let path = path.into();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(RegistryError::UnsupportedFileType(path));
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    packages: Vec::new(),
                });
            }
            Err(source) => {
                return Err(RegistryError::Read {
                    path: path.clone(),
                    source,
                });
            }
        }
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|file| file.take(MAX_REGISTRY_BYTES + 1).read_to_end(&mut bytes))
            .map_err(|source| RegistryError::Read {
                path: path.clone(),
                source,
            })?;
        if bytes.len() as u64 > MAX_REGISTRY_BYTES {
            return Err(RegistryError::TooLarge {
                limit: MAX_REGISTRY_BYTES,
            });
        }
        let file: RegistryFile = serde_json::from_slice(&bytes)?;
        if file.schema_version != REGISTRY_SCHEMA {
            return Err(RegistryError::UnsupportedSchema(file.schema_version));
        }
        validate_packages(&file.packages)?;
        Ok(Self {
            path,
            packages: file.packages,
        })
    }

    pub fn packages(&self) -> &[InstalledGlass] {
        &self.packages
    }

    pub fn get(&self, content_sha256: &str) -> Option<&InstalledGlass> {
        self.packages
            .iter()
            .find(|item| item.content_sha256 == content_sha256)
    }

    pub fn upsert(&mut self, package: InstalledGlass) -> Result<(), RegistryError> {
        let mut candidate = self.packages.clone();
        if let Some(existing) = candidate
            .iter_mut()
            .find(|item| item.content_sha256 == package.content_sha256)
        {
            *existing = package;
        } else {
            if candidate.len() >= MAX_REGISTRY_PACKAGES {
                return Err(RegistryError::TooManyPackages {
                    limit: MAX_REGISTRY_PACKAGES,
                });
            }
            candidate.push(package);
            candidate.sort_by(|a, b| {
                a.identifier
                    .cmp(&b.identifier)
                    .then(a.version.cmp(&b.version))
                    .then(a.content_sha256.cmp(&b.content_sha256))
            });
        }
        validate_packages(&candidate)?;
        self.persist(&candidate)?;
        self.packages = candidate;
        Ok(())
    }

    pub fn remove(
        &mut self,
        content_sha256: &str,
    ) -> Result<Option<InstalledGlass>, RegistryError> {
        let Some(index) = self
            .packages
            .iter()
            .position(|item| item.content_sha256 == content_sha256)
        else {
            return Ok(None);
        };
        let removed = self.packages[index].clone();
        let mut candidate = self.packages.clone();
        candidate.remove(index);
        validate_packages(&candidate)?;
        self.persist(&candidate)?;
        self.packages = candidate;
        Ok(Some(removed))
    }

    fn persist(&self, packages: &[InstalledGlass]) -> Result<(), RegistryError> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| RegistryError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
        let temp = temporary_sibling(&self.path);
        let bytes = serde_json::to_vec_pretty(&RegistryFile {
            schema_version: REGISTRY_SCHEMA,
            packages: packages.to_vec(),
        })?;
        if bytes.len() as u64 + 1 > MAX_REGISTRY_BYTES {
            return Err(RegistryError::TooLarge {
                limit: MAX_REGISTRY_BYTES,
            });
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|source| RegistryError::Write {
                path: temp.clone(),
                source,
            })?;
        if let Err(source) = file
            .write_all(&bytes)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
        {
            let _ = fs::remove_file(&temp);
            return Err(RegistryError::Write { path: temp, source });
        }
        drop(file);
        replace_registry_file(&temp, &self.path).map_err(|source| {
            let _ = fs::remove_file(&temp);
            RegistryError::Write {
                path: self.path.clone(),
                source,
            }
        })?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

fn validate_packages(packages: &[InstalledGlass]) -> Result<(), RegistryError> {
    if packages.len() > MAX_REGISTRY_PACKAGES {
        return Err(RegistryError::TooManyPackages {
            limit: MAX_REGISTRY_PACKAGES,
        });
    }
    let mut digests = std::collections::HashSet::with_capacity(packages.len());
    for package in packages {
        let valid_digest = package.content_sha256.len() == 64
            && package
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if !valid_digest || !digests.insert(package.content_sha256.as_str()) {
            return Err(RegistryError::InvalidContentDigest(
                package.content_sha256.clone(),
            ));
        }
        if package.identifier.is_empty() || package.identifier.len() > 512 {
            return Err(RegistryError::InvalidPackageField("identifier"));
        }
        if package.name.is_empty() || package.name.len() > MAX_REGISTRY_FIELD_BYTES {
            return Err(RegistryError::InvalidPackageField("name"));
        }
        if package.version.is_empty() || package.version.len() > 256 {
            return Err(RegistryError::InvalidPackageField("version"));
        }
        if package.publisher_name.is_empty()
            || package.publisher_name.len() > MAX_REGISTRY_FIELD_BYTES
        {
            return Err(RegistryError::InvalidPackageField("publisher_name"));
        }
        if package.publisher_identity.is_empty()
            || package.publisher_identity.len() > MAX_REGISTRY_FIELD_BYTES
        {
            return Err(RegistryError::InvalidPackageField("publisher_identity"));
        }
        if package.key_id.is_empty() || package.key_id.len() > 256 {
            return Err(RegistryError::InvalidPackageField("key_id"));
        }
        if package.package_path.to_string_lossy().len() > MAX_REGISTRY_PATH_BYTES {
            return Err(RegistryError::InvalidPackageField("package_path"));
        }
        if package.original_source.to_string_lossy().len() > MAX_REGISTRY_PATH_BYTES {
            return Err(RegistryError::InvalidPackageField("original_source"));
        }
        if package
            .package_path
            .file_name()
            .and_then(|name| name.to_str())
            != Some(package.content_sha256.as_str())
            || package
                .package_path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                != Some("packages")
        {
            return Err(RegistryError::InvalidPackagePath(
                package.package_path.clone(),
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_registry_file(temp: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temp, destination)
}

#[cfg(windows)]
fn replace_registry_file(temp: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let existing: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let new: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Both paths are siblings, so the replacement stays on one volume.
    let succeeded = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            new.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rootglass-registry-{}-{nonce}", std::process::id()))
    }

    fn package() -> InstalledGlass {
        InstalledGlass {
            content_sha256: "a".repeat(64),
            package_path: PathBuf::from(format!("packages/{}", "a".repeat(64))),
            identifier: "test.glass".into(),
            name: "Test".into(),
            version: "1".into(),
            publisher_name: "Test Publisher".into(),
            publisher_identity: "test".into(),
            key_id: "key".into(),
            original_source: PathBuf::from("test.glass"),
            imported_at_unix_seconds: 1,
        }
    }

    #[test]
    fn failed_persist_does_not_publish_in_memory_mutation() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("registry.json");
        let mut registry = PackageRegistry::open(&path).unwrap();
        fs::remove_dir_all(&root).unwrap();
        fs::write(&root, b"block").unwrap();
        assert!(registry.upsert(package()).is_err());
        assert!(registry.packages().is_empty());
        let _ = fs::remove_file(root);
    }

    #[test]
    fn registry_round_trips_schema_versioned_rows() {
        let root = temp_root();
        let path = root.join("registry.json");
        let mut registry = PackageRegistry::open(&path).unwrap();
        registry.upsert(package()).unwrap();
        let reopened = PackageRegistry::open(&path).unwrap();
        assert_eq!(reopened.packages(), &[package()]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_rejects_files_over_the_read_limit() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("registry.json");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_REGISTRY_BYTES + 1).unwrap();
        drop(file);
        assert!(matches!(
            PackageRegistry::open(&path),
            Err(RegistryError::TooLarge { .. })
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_rejects_unknown_fields_and_duplicate_digests() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("registry.json");
        fs::write(
            &path,
            br#"{"schema_version":1,"packages":[],"future":true}"#,
        )
        .unwrap();
        assert!(matches!(
            PackageRegistry::open(&path),
            Err(RegistryError::Parse(_))
        ));

        let duplicate = package();
        let bytes = serde_json::to_vec(&RegistryFile {
            schema_version: REGISTRY_SCHEMA,
            packages: vec![duplicate.clone(), duplicate],
        })
        .unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            PackageRegistry::open(&path),
            Err(RegistryError::InvalidContentDigest(_))
        ));

        fs::write(&path, br#"{"schema_version":99,"packages":[]}"#).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            PackageRegistry::open(&path),
            Err(RegistryError::UnsupportedSchema(99))
        ));
        assert_eq!(fs::read(&path).unwrap(), before);

        let mut invalid_path = package();
        invalid_path.package_path = PathBuf::from(format!("external/{}", "a".repeat(64)));
        let registry = PackageRegistry::open(root.join("invalid-path.json")).unwrap();
        let mut registry = registry;
        assert!(matches!(
            registry.upsert(invalid_path),
            Err(RegistryError::InvalidPackagePath(_))
        ));
        let _ = fs::remove_dir_all(root);
    }
}
