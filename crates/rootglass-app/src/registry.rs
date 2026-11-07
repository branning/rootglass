//! Small, versioned on-disk registry for verified immutable glass packages.
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

const REGISTRY_SCHEMA: u32 = 1;
static TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
        if !path.exists() {
            return Ok(Self {
                path,
                packages: Vec::new(),
            });
        }
        let bytes = fs::read(&path).map_err(|source| RegistryError::Read {
            path: path.clone(),
            source,
        })?;
        let file: RegistryFile = serde_json::from_slice(&bytes)?;
        if file.schema_version != REGISTRY_SCHEMA {
            return Err(RegistryError::UnsupportedSchema(file.schema_version));
        }
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
            candidate.push(package);
            candidate.sort_by(|a, b| {
                a.identifier
                    .cmp(&b.identifier)
                    .then(a.version.cmp(&b.version))
                    .then(a.content_sha256.cmp(&b.content_sha256))
            });
        }
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
            content_sha256: "abc".into(),
            package_path: PathBuf::from("packages/abc"),
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
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"block").unwrap();
        let mut registry = PackageRegistry::open(blocker.join("registry.json")).unwrap();
        assert!(registry.upsert(package()).is_err());
        assert!(registry.packages().is_empty());
        let _ = fs::remove_dir_all(root);
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
}
