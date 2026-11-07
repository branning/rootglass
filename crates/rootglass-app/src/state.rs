//! Persistent per-glass writable state images.
//!
//! This module manages host-side state files only. It does not configure a guest
//! overlay filesystem; attaching one of these images as a second disk does not
//! make a read-only root filesystem writable.
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateRecord {
    pub id: String,
    pub name: String,
    pub image_path: PathBuf,
    pub image_bytes: u64,
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("invalid glass identifier or state name: {0}")]
    InvalidName(String),
    #[error("state image exceeds the 16 GiB byte limit")]
    TooLarge,
    #[error("state {0} does not exist")]
    NotFound(String),
    #[error("state already exists: {0}")]
    AlreadyExists(String),
    #[error("glass state is currently in use")]
    InUse,
    #[error("state file is missing or invalid: {0}")]
    InvalidImage(String),
    #[error("state I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("state registry error: {0}")]
    Registry(#[from] serde_json::Error),
}

/// Host-side manager for isolated state images belonging to one glass release.
pub struct InstanceStateStore {
    root: PathBuf,
    template: PathBuf,
}

impl InstanceStateStore {
    pub fn new(root: impl Into<PathBuf>, template: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            template: template.into(),
        }
    }

    pub fn list(&self) -> Result<Vec<StateRecord>, StateError> {
        let mut entries = self.read_records()?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Create a pristine state by copying the immutable bundle template.
    pub fn create(&self, name: &str) -> Result<StateRecord, StateError> {
        let name = validate_name(name)?;
        let _guard = self.acquire_run_lock()?;
        let records = self.read_records()?;
        if records.iter().any(|r| r.name == name) {
            return Err(StateError::AlreadyExists(name.clone()));
        }
        let image_path = self.state_path(&name);
        let bytes = copy_image_atomic(&self.template, &image_path, false)?;
        let record = StateRecord {
            id: name.clone(),
            name,
            image_path,
            image_bytes: bytes,
        };
        let mut records = self.read_records()?;
        records.push(record.clone());
        self.write_records(&records)?;
        Ok(record)
    }

    /// Clone an existing state into a new independent image.
    pub fn clone_state(&self, source_id: &str, new_name: &str) -> Result<StateRecord, StateError> {
        let new_name = validate_name(new_name)?;
        let _guard = self.acquire_run_lock()?;
        let records = self.read_records()?;
        let source = records
            .iter()
            .find(|r| r.id == source_id)
            .ok_or_else(|| StateError::NotFound(source_id.into()))?;
        if records.iter().any(|r| r.name == new_name) {
            return Err(StateError::AlreadyExists(new_name.clone()));
        }
        let image_path = self.state_path(&new_name);
        let bytes = copy_image_atomic(&source.image_path, &image_path, false)?;
        let record = StateRecord {
            id: new_name.clone(),
            name: new_name,
            image_path,
            image_bytes: bytes,
        };
        let mut records = records;
        records.push(record.clone());
        self.write_records(&records)?;
        Ok(record)
    }

    /// Replace a stopped state with a fresh copy of the immutable template.
    pub fn reset(&self, id: &str) -> Result<StateRecord, StateError> {
        let id = validate_name(id)?;
        let _guard = self.acquire_run_lock()?;
        let mut records = self.read_records()?;
        let index = records
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| StateError::NotFound(id.clone()))?;
        let bytes = copy_image_atomic(&self.template, &self.state_path(&id), true)?;
        records[index].image_bytes = bytes;
        self.write_records(&records)?;
        Ok(records[index].clone())
    }

    pub fn delete(&self, id: &str) -> Result<(), StateError> {
        let id = validate_name(id)?;
        let _guard = self.acquire_run_lock()?;
        let mut records = self.read_records()?;
        let index = records
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| StateError::NotFound(id.clone()))?;
        let target = self.state_path(&id);
        records.remove(index);
        staged_remove(&target, || self.write_records(&records))
    }

    /// Hold this guard for the full guest run. A create_new lock prevents a second
    /// process from modifying or deleting images concurrently.
    pub fn acquire_run_lock(&self) -> Result<StateRunLock, StateError> {
        fs::create_dir_all(&self.root).map_err(|source| StateError::Io {
            path: self.root.clone(),
            source,
        })?;
        let path = self.root.join(".state.lock");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::AlreadyExists {
                    StateError::InUse
                } else {
                    StateError::Io {
                        path: path.clone(),
                        source,
                    }
                }
            })?;
        Ok(StateRunLock { path })
    }

    fn state_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.img"))
    }
    fn records_path(&self) -> PathBuf {
        self.root.join("states.json")
    }
    fn read_records(&self) -> Result<Vec<StateRecord>, StateError> {
        let path = self.records_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&path).map_err(|source| StateError::Io {
            path: path.clone(),
            source,
        })?;
        let records: Vec<StateRecord> = serde_json::from_slice(&bytes)?;
        for record in &records {
            let id = validate_name(&record.id)?;
            let name = validate_name(&record.name)?;
            if id != name || record.image_path != self.state_path(&id) {
                return Err(StateError::InvalidImage(format!(
                    "registry path does not match state id {:?}",
                    record.id
                )));
            }
            let metadata =
                fs::symlink_metadata(&record.image_path).map_err(|source| StateError::Io {
                    path: record.image_path.clone(),
                    source,
                })?;
            if !metadata.file_type().is_file() {
                return Err(StateError::InvalidImage(format!(
                    "state path is not a regular file: {}",
                    record.image_path.display()
                )));
            }
        }
        Ok(records)
    }
    fn write_records(&self, records: &[StateRecord]) -> Result<(), StateError> {
        fs::create_dir_all(&self.root).map_err(|source| StateError::Io {
            path: self.root.clone(),
            source,
        })?;
        let path = self.records_path();
        let bytes = serde_json::to_vec_pretty(records)?;
        write_atomic(&path, &bytes)
    }
}

pub struct StateRunLock {
    path: PathBuf,
}
impl Drop for StateRunLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn validate_name(name: &str) -> Result<String, StateError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(StateError::InvalidName(name.into()));
    }
    Ok(name.to_string())
}

fn copy_image_atomic(source: &Path, destination: &Path, replace: bool) -> Result<u64, StateError> {
    let meta = fs::symlink_metadata(source).map_err(|source_err| StateError::Io {
        path: source.into(),
        source: source_err,
    })?;
    if !meta.file_type().is_file() || meta.len() == 0 {
        return Err(StateError::InvalidImage(source.display().to_string()));
    }
    if meta.len() > MAX_STATE_BYTES {
        return Err(StateError::TooLarge);
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| StateError::Io {
        path: parent.into(),
        source,
    })?;
    if destination.exists() && !replace {
        return Err(StateError::AlreadyExists(destination.display().to_string()));
    }
    let temp = temp_path(destination);
    let source_path = source;
    let result = (|| {
        let mut input = File::open(source_path).map_err(|error| StateError::Io {
            path: source_path.into(),
            source: error,
        })?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|source| StateError::Io {
                path: temp.clone(),
                source,
            })?;
        let mut copied = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = input.read(&mut buffer).map_err(|error| StateError::Io {
                path: source_path.into(),
                source: error,
            })?;
            if count == 0 {
                break;
            }
            copied += count as u64;
            if copied > MAX_STATE_BYTES {
                return Err(StateError::TooLarge);
            }
            output
                .write_all(&buffer[..count])
                .map_err(|source| StateError::Io {
                    path: temp.clone(),
                    source,
                })?;
        }
        output.sync_all().map_err(|source| StateError::Io {
            path: temp.clone(),
            source,
        })?;
        drop(output);
        install_temp(&temp, destination, replace)?;
        Ok(copied)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn install_temp(temp: &Path, destination: &Path, replace: bool) -> Result<(), StateError> {
    if replace {
        replace_file(temp, destination)
    } else {
        fs::hard_link(temp, destination).map_err(|source| StateError::Io {
            path: destination.into(),
            source,
        })?;
        fs::remove_file(temp).map_err(|source| StateError::Io {
            path: temp.into(),
            source,
        })
    }
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, destination: &Path) -> Result<(), StateError> {
    fs::rename(temp, destination).map_err(|source| StateError::Io {
        path: destination.into(),
        source,
    })
}

#[cfg(windows)]
fn replace_file(temp: &Path, destination: &Path) -> Result<(), StateError> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let existing: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let new: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let succeeded = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            new.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(StateError::Io {
            path: destination.into(),
            source: io::Error::last_os_error(),
        })
    } else {
        Ok(())
    }
}

fn write_atomic(destination: &Path, bytes: &[u8]) -> Result<(), StateError> {
    let temp = temp_path(destination);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|source| StateError::Io {
            path: temp.clone(),
            source,
        })?;
    let result = file
        .write_all(bytes)
        .and_then(|_| file.sync_all())
        .and_then(|_| fs::rename(&temp, destination));
    if let Err(source) = result {
        let _ = fs::remove_file(&temp);
        return Err(StateError::Io {
            path: destination.into(),
            source,
        });
    }
    Ok(())
}

fn staged_remove(
    path: &Path,
    persist: impl FnOnce() -> Result<(), StateError>,
) -> Result<(), StateError> {
    let tombstone = temp_path(path);
    fs::rename(path, &tombstone).map_err(|source| StateError::Io {
        path: path.into(),
        source,
    })?;
    if let Err(error) = persist() {
        fs::rename(&tombstone, path).map_err(|source| StateError::Io {
            path: path.into(),
            source,
        })?;
        return Err(error);
    }
    fs::remove_file(&tombstone).map_err(|source| StateError::Io {
        path: tombstone,
        source,
    })
}
fn temp_path(path: &Path) -> PathBuf {
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rootglass-state-test-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn create_clone_reset_and_delete_are_isolated_from_template() {
        let dir = temp_dir();
        let template = dir.join("template.img");
        fs::write(&template, b"pristine").unwrap();
        let store = InstanceStateStore::new(dir.join("state"), &template);
        let first = store.create("default").unwrap();
        fs::write(&first.image_path, b"modified").unwrap();
        let cloned = store.clone_state("default", "copy").unwrap();
        assert_eq!(fs::read(&cloned.image_path).unwrap(), b"modified");
        store.reset("default").unwrap();
        assert_eq!(fs::read(&first.image_path).unwrap(), b"pristine");
        assert_eq!(fs::read(&template).unwrap(), b"pristine");
        store.delete("copy").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_lock_excludes_mutations_and_another_run() {
        let dir = temp_dir();
        let template = dir.join("template.img");
        fs::write(&template, b"state").unwrap();
        let store = InstanceStateStore::new(dir.join("state"), template);
        let lock = store.acquire_run_lock().unwrap();
        assert!(matches!(store.create("default"), Err(StateError::InUse)));
        assert!(matches!(store.acquire_run_lock(), Err(StateError::InUse)));
        drop(lock);
        assert!(store.create("default").is_ok());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_path_like_names_and_empty_templates() {
        assert!(matches!(
            validate_name("../escape"),
            Err(StateError::InvalidName(_))
        ));
        let dir = temp_dir();
        let template = dir.join("empty.img");
        fs::write(&template, []).unwrap();
        let store = InstanceStateStore::new(dir.join("state"), template);
        assert!(matches!(
            store.create("valid"),
            Err(StateError::InvalidImage(_))
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_registry_persist_restores_staged_state_file() {
        let dir = temp_dir();
        let image = dir.join("state.img");
        fs::write(&image, b"preserve me").unwrap();
        let result = staged_remove(&image, || Err(StateError::InUse));
        assert!(matches!(result, Err(StateError::InUse)));
        assert_eq!(fs::read(&image).unwrap(), b"preserve me");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_registry_path_escape_before_delete() {
        let dir = temp_dir();
        let template = dir.join("template.img");
        fs::write(&template, b"x").unwrap();
        let store = InstanceStateStore::new(dir.join("state"), &template);
        let created = store.create("safe").unwrap();
        let external = dir.join("outside.img");
        fs::write(&external, b"do not remove").unwrap();
        let record = StateRecord {
            image_path: external.clone(),
            ..created
        };
        fs::write(
            store.records_path(),
            serde_json::to_vec(&vec![record]).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            store.delete("safe"),
            Err(StateError::InvalidImage(_))
        ));
        assert_eq!(fs::read(external).unwrap(), b"do not remove");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reset_replaces_existing_image_atomically() {
        let dir = temp_dir();
        let template = dir.join("template.img");
        fs::write(&template, b"pristine").unwrap();
        let store = InstanceStateStore::new(dir.join("state"), &template);
        let record = store.create("default").unwrap();
        fs::write(&record.image_path, b"changed").unwrap();
        store.reset("default").unwrap();
        assert_eq!(fs::read(record.image_path).unwrap(), b"pristine");
        fs::remove_dir_all(dir).unwrap();
    }
}
