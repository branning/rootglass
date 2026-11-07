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

    /// Report whether a guest process currently owns this store's run lock.
    /// Mutating operations still acquire the lock themselves to close the race
    /// between this advisory status and the requested action.
    pub fn is_in_use(&self) -> bool {
        self.root.join(".state.lock").exists()
    }

    /// Return the persisted state chosen for the next guest run, defaulting to `default`.
    pub fn selected_state_id(&self) -> Result<String, StateError> {
        let path = self.selected_state_path();
        if !path.exists() {
            return Ok("default".to_string());
        }
        let bytes = fs::read(&path).map_err(|source| StateError::Io {
            path: path.clone(),
            source,
        })?;
        let id: String = serde_json::from_slice(&bytes)?;
        validate_name(&id)
    }

    /// Persist the state used by later boots and restarts, rejecting changes while a guest
    /// owns the store. The implicit default is valid before its image is first created.
    pub fn select(&self, id: &str) -> Result<String, StateError> {
        let id = validate_name(id)?;
        let _guard = self.acquire_run_lock()?;
        if id != "default" && !self.read_records()?.iter().any(|record| record.id == id) {
            return Err(StateError::NotFound(id));
        }
        self.write_selected_state_id_locked(&id)?;
        Ok(id)
    }

    /// Create a pristine state by copying the immutable bundle template.
    pub fn create(&self, name: &str) -> Result<StateRecord, StateError> {
        let name = validate_name(name)?;
        let _guard = self.acquire_run_lock()?;
        self.create_locked(&name)
    }

    fn create_locked(&self, name: &str) -> Result<StateRecord, StateError> {
        let name = name.to_string();
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
        if let Err(error) = self.write_records(&records) {
            let _ = fs::remove_file(&record.image_path);
            return Err(error);
        }
        Ok(record)
    }

    /// Select a state and retain the cross-process lock for the complete guest run.
    /// The default state is created while the same lock is held, avoiding a race
    /// between state creation and QEMU opening the image.
    pub fn select_for_run(&self, id: &str) -> Result<(StateRecord, StateRunLock), StateError> {
        let id = validate_name(id)?;
        let lock = self.acquire_run_lock()?;
        self.select_for_run_locked(&id, lock)
    }

    /// Acquire the run lock and select the persisted state atomically with guest startup.
    pub fn select_current_for_run(&self) -> Result<(StateRecord, StateRunLock), StateError> {
        let lock = self.acquire_run_lock()?;
        let id = self.selected_state_id()?;
        self.select_for_run_locked(&id, lock)
    }

    fn select_for_run_locked(
        &self,
        id: &str,
        lock: StateRunLock,
    ) -> Result<(StateRecord, StateRunLock), StateError> {
        let record = self.read_records()?.into_iter().find(|item| item.id == id);
        match record {
            Some(record) => Ok((record, lock)),
            None if id == "default" => Ok((self.create_locked(id)?, lock)),
            None => Err(StateError::NotFound(id.to_string())),
        }
    }

    /// Resolve where a state image will live without creating it or taking a run lock.
    pub fn path_for(&self, id: &str) -> Result<PathBuf, StateError> {
        let id = validate_name(id)?;
        Ok(self.state_path(&id))
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
        if let Err(error) = self.write_records(&records) {
            let _ = fs::remove_file(&record.image_path);
            return Err(error);
        }
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
        let state_path = self.state_path(&id);
        let backup = temp_path(&state_path);
        copy_image_atomic(&state_path, &backup, false)?;
        let bytes = match copy_image_atomic(&self.template, &state_path, true) {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = fs::remove_file(&backup);
                return Err(error);
            }
        };
        records[index].image_bytes = bytes;
        if let Err(error) = self.write_records(&records) {
            replace_file(&backup, &state_path).map_err(|restore| StateError::Io {
                path: state_path,
                source: io::Error::other(format!(
                    "reset failed ({error}) and restoring the previous state also failed: {restore}"
                )),
            })?;
            return Err(error);
        }
        fs::remove_file(&backup).map_err(|source| StateError::Io {
            path: backup,
            source,
        })?;
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
        let selected = self.selected_state_id()?;
        if selected == id {
            self.write_selected_state_id_locked("default")?;
        }
        if let Err(error) = staged_remove(&target, || self.write_records(&records)) {
            if selected == id {
                if let Err(restore) = self.write_selected_state_id_locked(&selected) {
                    return Err(StateError::Io {
                        path: self.selected_state_path(),
                        source: io::Error::other(format!(
                            "deletion failed ({error}) and restoring selected state also failed: {restore}"
                        )),
                    });
                }
            }
            return Err(error);
        }
        Ok(())
    }

    /// Hold this guard for the full guest run. A create_new lock prevents a second
    /// process from modifying or deleting images concurrently.
    pub fn acquire_run_lock(&self) -> Result<StateRunLock, StateError> {
        fs::create_dir_all(&self.root).map_err(|source| StateError::Io {
            path: self.root.clone(),
            source,
        })?;
        let path = self.root.join(".state.lock");
        let mut file = OpenOptions::new()
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
        if let Err(source) = writeln!(file, "{}", std::process::id()).and_then(|_| file.sync_all())
        {
            let _ = fs::remove_file(&path);
            return Err(StateError::Io { path, source });
        }
        Ok(StateRunLock { path })
    }

    fn state_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.img"))
    }
    fn records_path(&self) -> PathBuf {
        self.root.join("states.json")
    }
    fn selected_state_path(&self) -> PathBuf {
        self.root.join("selected-state.json")
    }
    fn write_selected_state_id_locked(&self, id: &str) -> Result<(), StateError> {
        fs::create_dir_all(&self.root).map_err(|source| StateError::Io {
            path: self.root.clone(),
            source,
        })?;
        let path = self.selected_state_path();
        let bytes = serde_json::to_vec(id)?;
        write_atomic(&path, &bytes)
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
        replace_file(temp, destination).map_err(|source| StateError::Io {
            path: destination.into(),
            source,
        })?;
        sync_parent(destination);
        Ok(())
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
fn replace_file(temp: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temp, destination)
}

#[cfg(windows)]
fn replace_file(temp: &Path, destination: &Path) -> io::Result<()> {
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
        Err(io::Error::last_os_error())
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
    let result = file.write_all(bytes).and_then(|_| file.sync_all());
    if let Err(source) = result {
        let _ = fs::remove_file(&temp);
        return Err(StateError::Io {
            path: destination.into(),
            source,
        });
    }
    drop(file);
    replace_file(&temp, destination).map_err(|source| StateError::Io {
        path: destination.into(),
        source,
    })?;
    sync_parent(destination);
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

fn sync_parent(path: &Path) {
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty());
    if let Some(parent) = parent {
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

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
        assert!(!store.is_in_use());
        let lock = store.acquire_run_lock().unwrap();
        assert!(store.is_in_use());
        assert!(matches!(store.create("default"), Err(StateError::InUse)));
        assert!(matches!(
            store.clone_state("default", "copy"),
            Err(StateError::InUse)
        ));
        assert!(matches!(store.reset("default"), Err(StateError::InUse)));
        assert!(matches!(store.delete("default"), Err(StateError::InUse)));
        assert!(matches!(store.acquire_run_lock(), Err(StateError::InUse)));
        drop(lock);
        assert!(!store.is_in_use());
        assert!(store.create("default").is_ok());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selected_default_is_created_under_the_run_lock() {
        let dir = temp_dir();
        let template = dir.join("template.img");
        fs::write(&template, b"pristine").unwrap();
        let store_root = dir.join("state");
        let store = InstanceStateStore::new(&store_root, &template);

        let (record, lock) = store.select_for_run("default").unwrap();
        assert_eq!(fs::read(&record.image_path).unwrap(), b"pristine");
        assert!(matches!(
            InstanceStateStore::new(&store_root, &template).delete("default"),
            Err(StateError::InUse)
        ));
        drop(lock);
        assert_eq!(store.list().unwrap(), vec![record]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selected_state_persists_and_is_used_by_a_later_run() {
        let dir = temp_dir();
        let template = dir.join("template.img");
        fs::write(&template, b"pristine").unwrap();
        let store_root = dir.join("state");
        let store = InstanceStateStore::new(&store_root, &template);
        store.create("default").unwrap();
        store.create("work").unwrap();
        assert_eq!(store.select("work").unwrap(), "work");
        assert_eq!(
            InstanceStateStore::new(&store_root, &template)
                .selected_state_id()
                .unwrap(),
            "work"
        );

        let (record, lock) = InstanceStateStore::new(&store_root, &template)
            .select_current_for_run()
            .unwrap();
        assert_eq!(record.id, "work");
        drop(lock);
        store.delete("work").unwrap();
        assert_eq!(store.selected_state_id().unwrap(), "default");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_lock_excludes_a_different_process() {
        let dir = temp_dir();
        let template = dir.join("template.img");
        fs::write(&template, b"state").unwrap();
        let state_root = dir.join("state");
        fs::create_dir_all(&state_root).unwrap();
        let ready = dir.join("child-ready");
        let release = dir.join("child-release");
        let child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("state::tests::hold_run_lock_for_parent_process")
            .arg("--nocapture")
            .env("ROOTGLASS_STATE_LOCK_CHILD", "1")
            .env("ROOTGLASS_STATE_LOCK_ROOT", &state_root)
            .env("ROOTGLASS_STATE_LOCK_READY", &ready)
            .env("ROOTGLASS_STATE_LOCK_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = child;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                panic!("state-lock child exited before acquiring the lock");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready.exists(), "state-lock child did not acquire the lock");
        let store = InstanceStateStore::new(&state_root, &template);
        assert!(matches!(store.acquire_run_lock(), Err(StateError::InUse)));
        fs::write(&release, b"release").unwrap();
        assert!(child.wait().unwrap().success());
        let lock = store.acquire_run_lock().unwrap();
        drop(lock);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hold_run_lock_for_parent_process() {
        let Some(root) = std::env::var_os("ROOTGLASS_STATE_LOCK_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let template = root.with_file_name("template.img");
        let ready = PathBuf::from(std::env::var_os("ROOTGLASS_STATE_LOCK_READY").unwrap());
        let release = PathBuf::from(std::env::var_os("ROOTGLASS_STATE_LOCK_RELEASE").unwrap());
        let _lock = InstanceStateStore::new(root, template)
            .acquire_run_lock()
            .unwrap();
        fs::write(ready, b"locked").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !release.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(release.exists(), "parent did not release the child lock");
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
