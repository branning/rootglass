//! Cross-process slot reservations and crash cleanup for encrypted guest runs.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

pub const MAX_ENCRYPTED_INSTANCES: u8 = 5;
const MAX_RECORD_BYTES: u64 = 16 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("encrypted instance limit must be between 1 and {MAX_ENCRYPTED_INSTANCES}")]
    InvalidLimit,
    #[error("all {limit} encrypted instance slots are in use")]
    Capacity { limit: u8 },
    #[error("encrypted session record is invalid: {0}")]
    InvalidRecord(String),
    #[error("encrypted session I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("timed out coordinating encrypted instance slots")]
    LockTimeout,
    #[error(
        "a crashed encrypted session still has a QEMU process that could not be safely reaped"
    )]
    OrphanQemu,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRecord {
    owner_pid: u32,
    qemu_pid: Option<u32>,
    session_dir: Option<PathBuf>,
}

pub struct EncryptedSessionStore {
    root: PathBuf,
}

pub struct EncryptedSessionLease {
    root: PathBuf,
    slot: PathBuf,
    record: SessionRecord,
}

impl EncryptedSessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Recover sessions for every previously installed encrypted artifact. This
    /// catches orphan guests from a different glass before a new run starts.
    pub fn recover_all(root: &Path) -> Result<(), SessionError> {
        let metadata = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(SessionError::Io {
                    path: root.to_path_buf(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SessionError::InvalidRecord(
                "encrypted session collection is not a directory".to_string(),
            ));
        }
        let entries = fs::read_dir(root).map_err(|source| SessionError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| SessionError::Io {
                path: root.to_path_buf(),
                source,
            })?;
            let name = entry.file_name();
            if name.to_str().is_none_or(|name| {
                name.len() != 64 || !name.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                continue;
            }
            let path = entry.path();
            let child_metadata =
                fs::symlink_metadata(&path).map_err(|source| SessionError::Io {
                    path: path.clone(),
                    source,
                })?;
            if child_metadata.file_type().is_symlink() || !child_metadata.is_dir() {
                return Err(SessionError::InvalidRecord(
                    "encrypted artifact session path is not a directory".to_string(),
                ));
            }
            Self::new(path).recover_stale_sessions()?;
        }
        Ok(())
    }

    fn recover_stale_sessions(&self) -> Result<(), SessionError> {
        create_private_dir(&self.root)?;
        let _lock = ManagerLock::acquire(&self.root)?;
        for index in 0..MAX_ENCRYPTED_INSTANCES {
            let slot = self.root.join(format!("slot-{index}.json"));
            if !slot.exists() {
                continue;
            }
            let record = read_record(&slot)?;
            if !process_is_alive(record.owner_pid) {
                recover_dead_session(&self.root, &slot, &record)?;
            }
        }
        Ok(())
    }

    /// Atomically reserve one of the signed per-glass run slots. Dead owners are
    /// reclaimed only after their QEMU process is safely reaped.
    pub fn reserve(&self, limit: u8) -> Result<EncryptedSessionLease, SessionError> {
        if !(1..=MAX_ENCRYPTED_INSTANCES).contains(&limit) {
            return Err(SessionError::InvalidLimit);
        }
        create_private_dir(&self.root)?;
        let _lock = ManagerLock::acquire(&self.root)?;
        let mut occupied = 0u8;
        let mut available = None;
        for index in 0..MAX_ENCRYPTED_INSTANCES {
            let slot = self.root.join(format!("slot-{index}.json"));
            if slot.exists() {
                let record = read_record(&slot)?;
                let owner_alive = process_is_alive(record.owner_pid);
                if owner_alive {
                    occupied = occupied.saturating_add(1);
                    continue;
                }
                recover_dead_session(&self.root, &slot, &record)?;
            }
            if available.is_none() {
                available = Some(slot);
            }
        }
        if occupied >= limit {
            return Err(SessionError::Capacity { limit });
        }
        let slot = available.ok_or(SessionError::Capacity { limit })?;
        let record = SessionRecord {
            owner_pid: std::process::id(),
            qemu_pid: None,
            session_dir: None,
        };
        write_record(&slot, &record, false)?;
        Ok(EncryptedSessionLease {
            root: self.root.clone(),
            slot,
            record,
        })
    }
}

fn recover_dead_session(
    root: &Path,
    slot: &Path,
    record: &SessionRecord,
) -> Result<(), SessionError> {
    if let Some(session_dir) = record.session_dir.as_deref() {
        let session_dir = validate_session_dir(root, session_dir)?;
        for pid in matching_qemu_processes(&session_dir)? {
            terminate_qemu(pid)?;
        }
    } else if record.qemu_pid.is_some_and(process_is_alive) {
        // A child without its recorded private directory cannot be matched to
        // this run safely, so preserve its slot and encrypted files.
        return Err(SessionError::OrphanQemu);
    }
    remove_session_dir(root, record.session_dir.as_deref())?;
    fs::remove_file(slot).map_err(|source| SessionError::Io {
        path: slot.to_path_buf(),
        source,
    })
}

impl EncryptedSessionLease {
    /// Persist the private directory before QEMU is spawned. If the app crashes
    /// between spawn and PID recording, recovery can still identify QEMU by its
    /// session-specific QMP endpoint.
    pub fn record_session_dir(&mut self, session_dir: &Path) -> Result<(), SessionError> {
        let canonical_session = validate_session_dir(&self.root, session_dir)?;
        self.record.session_dir = Some(canonical_session);
        let _lock = ManagerLock::acquire(&self.root)?;
        write_record(&self.slot, &self.record, true)
    }

    /// Record the QEMU child and its private temporary directory immediately
    /// after spawn, before QMP setup begins.
    pub fn record_qemu(&mut self, pid: u32, session_dir: &Path) -> Result<(), SessionError> {
        let canonical_session = validate_session_dir(&self.root, session_dir)?;
        let _lock = ManagerLock::acquire(&self.root)?;
        self.record.qemu_pid = Some(pid);
        self.record.session_dir = Some(canonical_session);
        write_record(&self.slot, &self.record, true)
    }
}

impl Drop for EncryptedSessionLease {
    fn drop(&mut self) {
        let Ok(_lock) = ManagerLock::acquire(&self.root) else {
            return;
        };
        if let Some(session_dir) = self.record.session_dir.as_deref() {
            match matching_qemu_processes(session_dir) {
                Ok(pids) if !pids.is_empty() => {
                    self.record.owner_pid = 0;
                    let _ = write_record(&self.slot, &self.record, true);
                    return;
                }
                Err(_) => {
                    self.record.owner_pid = 0;
                    let _ = write_record(&self.slot, &self.record, true);
                    return;
                }
                _ => {}
            }
        }
        if remove_session_dir(&self.root, self.record.session_dir.as_deref()).is_ok() {
            let _ = fs::remove_file(&self.slot);
        }
    }
}

struct ManagerLock {
    path: PathBuf,
}

impl ManagerLock {
    fn acquire(root: &Path) -> Result<Self, SessionError> {
        let path = root.join("manager.lock");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    let pid = std::process::id().to_string();
                    if let Err(source) = file
                        .write_all(pid.as_bytes())
                        .and_then(|()| file.sync_all())
                    {
                        let _ = fs::remove_file(&path);
                        return Err(SessionError::Io {
                            path: path.clone(),
                            source,
                        });
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match fs::read_to_string(&path) {
                        Ok(contents) => match contents.parse::<u32>() {
                            Ok(pid) if !process_is_alive(pid) => {
                                let _ = fs::remove_file(&path);
                            }
                            Ok(_) => {}
                            Err(_) if contents.trim().is_empty() => {}
                            Err(_) => return Err(SessionError::LockTimeout),
                        },
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => return Err(SessionError::LockTimeout),
                    }
                    if Instant::now() >= deadline {
                        return Err(SessionError::LockTimeout);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(source) => {
                    return Err(SessionError::Io {
                        path: path.clone(),
                        source,
                    });
                }
            }
        }
    }
}

impl Drop for ManagerLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_private_dir(path: &Path) -> Result<(), SessionError> {
    fs::create_dir_all(path).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            SessionError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    let metadata = fs::symlink_metadata(path).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionError::InvalidRecord(
            "encrypted session store is not a directory".to_string(),
        ));
    }
    Ok(())
}

fn read_record(path: &Path) -> Result<SessionRecord, SessionError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES
    {
        return Err(SessionError::InvalidRecord(
            "slot record is not a bounded regular file".to_string(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|file| file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(SessionError::InvalidRecord(
            "slot record exceeds its size bound".to_string(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| SessionError::InvalidRecord("slot record could not be parsed".to_string()))
}

fn write_record(
    destination: &Path,
    record: &SessionRecord,
    replace: bool,
) -> Result<(), SessionError> {
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp = destination.with_extension(format!("{}.{}.tmp", std::process::id(), id));
    let bytes = serde_json::to_vec(record).map_err(|_| {
        SessionError::InvalidRecord("slot record could not be serialized".to_string())
    })?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(|source| SessionError::Io {
        path: temp.clone(),
        source,
    })?;
    let write_result = file.write_all(&bytes).and_then(|()| file.sync_all());
    if let Err(source) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(SessionError::Io { path: temp, source });
    }
    drop(file);
    if replace && destination.exists() {
        #[cfg(windows)]
        fs::remove_file(destination).map_err(|source| SessionError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
    }
    if replace {
        fs::rename(&temp, destination).map_err(|source| SessionError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
    } else {
        fs::hard_link(&temp, destination).map_err(|source| SessionError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        fs::remove_file(&temp).map_err(|source| SessionError::Io { path: temp, source })?;
    }
    Ok(())
}

fn remove_session_dir(root: &Path, session_dir: Option<&Path>) -> Result<(), SessionError> {
    let Some(session_dir) = session_dir else {
        return Ok(());
    };
    match fs::symlink_metadata(session_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            SessionError::InvalidRecord("recorded QEMU session is not a directory".to_string()),
        ),
        Ok(_) => {
            let canonical_session = validate_session_dir(root, session_dir)?;
            remove_qmp_endpoint_dir(&canonical_session)?;
            fs::remove_dir_all(&canonical_session).map_err(|source| SessionError::Io {
                path: canonical_session,
                source,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let canonical_root = fs::canonicalize(root).map_err(|source| SessionError::Io {
                path: root.to_path_buf(),
                source,
            })?;
            if !session_dir.starts_with(canonical_root) {
                return Err(SessionError::InvalidRecord(
                    "recorded QEMU directory is outside the encrypted session store".to_string(),
                ));
            }
            remove_qmp_endpoint_dir(session_dir)
        }
        Err(source) => Err(SessionError::Io {
            path: session_dir.to_path_buf(),
            source,
        }),
    }
}

fn validate_session_dir(root: &Path, session_dir: &Path) -> Result<PathBuf, SessionError> {
    let canonical_root = fs::canonicalize(root).map_err(|source| SessionError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let metadata = fs::symlink_metadata(session_dir).map_err(|source| SessionError::Io {
        path: session_dir.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionError::InvalidRecord(
            "QEMU session directory is not a private directory".to_string(),
        ));
    }
    let canonical_session = fs::canonicalize(session_dir).map_err(|source| SessionError::Io {
        path: session_dir.to_path_buf(),
        source,
    })?;
    if !canonical_session.starts_with(canonical_root) {
        return Err(SessionError::InvalidRecord(
            "recorded QEMU directory is outside the encrypted session store".to_string(),
        ));
    }
    Ok(canonical_session)
}

fn session_marker(session_dir: &Path) -> Result<String, SessionError> {
    #[cfg(windows)]
    {
        let session_id = session_id(session_dir)?;
        Ok(format!(r"pipe:\\.\pipe\{session_id}"))
    }
    #[cfg(not(windows))]
    {
        Path::new("/tmp")
            .join(format!(
                "rgqmp-{session_id}",
                session_id = session_id(session_dir)?
            ))
            .join("q")
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| {
                SessionError::InvalidRecord("QEMU session path is not valid UTF-8".into())
            })
    }
}

fn session_id(session_dir: &Path) -> Result<&str, SessionError> {
    let name = session_dir
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| {
            SessionError::InvalidRecord("QEMU session path is not valid UTF-8".into())
        })?;
    let Some(ids) = name.strip_prefix("rootglass-qmp-") else {
        return Err(SessionError::InvalidRecord(
            "QEMU session directory name is invalid".to_string(),
        ));
    };
    let Some((pid, sequence)) = ids.split_once('-') else {
        return Err(SessionError::InvalidRecord(
            "QEMU session directory name is invalid".to_string(),
        ));
    };
    if pid.parse::<u32>().is_err() || sequence.parse::<u64>().is_err() {
        return Err(SessionError::InvalidRecord(
            "QEMU session directory name is invalid".to_string(),
        ));
    }
    Ok(name)
}

#[cfg(unix)]
fn remove_qmp_endpoint_dir(session_dir: &Path) -> Result<(), SessionError> {
    let session_id = session_id(session_dir)?;
    let directory = Path::new("/tmp").join(format!("rgqmp-{session_id}"));
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            SessionError::InvalidRecord("private QMP endpoint path is not a directory".to_string()),
        ),
        Ok(_) => fs::remove_dir_all(&directory).map_err(|source| SessionError::Io {
            path: directory,
            source,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SessionError::Io {
            path: directory,
            source,
        }),
    }
}

#[cfg(windows)]
fn remove_qmp_endpoint_dir(_session_dir: &Path) -> Result<(), SessionError> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn matching_qemu_processes(session_dir: &Path) -> Result<Vec<u32>, SessionError> {
    let marker = session_marker(session_dir)?;
    let entries = fs::read_dir("/proc").map_err(|source| SessionError::Io {
        path: PathBuf::from("/proc"),
        source,
    })?;
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if !process_is_alive(pid) {
            continue;
        }
        let command_line = match fs::read(entry.path().join("cmdline")) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        if command_line.split(|byte| *byte == 0).any(|argument| {
            argument
                .windows(marker.len())
                .any(|window| window == marker.as_bytes())
        }) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

#[cfg(target_os = "macos")]
fn matching_qemu_processes(session_dir: &Path) -> Result<Vec<u32>, SessionError> {
    let marker = session_marker(session_dir)?;
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .map_err(|source| SessionError::Io {
            path: PathBuf::from("ps"),
            source,
        })?;
    if !output.status.success() {
        return Err(SessionError::OrphanQemu);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, command) = line.trim().split_once(char::is_whitespace)?;
            let pid = pid.parse::<u32>().ok()?;
            (command.contains(&marker) && process_is_alive(pid)).then_some(pid)
        })
        .collect())
}

#[cfg(windows)]
fn matching_qemu_processes(session_dir: &Path) -> Result<Vec<u32>, SessionError> {
    let marker = session_marker(session_dir)?;
    let script = "$m=$env:ROOTGLASS_QMP_RECOVERY_MARKER; Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like ('*' + $m + '*') } | ForEach-Object { $_.ProcessId }";
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("ROOTGLASS_QMP_RECOVERY_MARKER", marker)
        .output()
        .map_err(|_| SessionError::OrphanQemu)?;
    if !output.status.success() {
        return Err(SessionError::OrphanQemu);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| process_is_alive(*pid))
        .collect())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn matching_qemu_processes(_session_dir: &Path) -> Result<Vec<u32>, SessionError> {
    Err(SessionError::OrphanQemu)
}

#[cfg(unix)]
fn terminate_qemu(pid: u32) -> Result<(), SessionError> {
    use std::os::raw::c_int;
    const SIGTERM: c_int = 15;
    const SIGKILL: c_int = 9;
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(SessionError::OrphanQemu);
    }
    unsafe {
        if libc::kill(pid as i32, SIGTERM) != 0 {
            return Err(SessionError::OrphanQemu);
        }
    }
    if wait_for_process_exit(pid, Duration::from_secs(2)) {
        return Ok(());
    }
    unsafe {
        if libc::kill(pid as i32, SIGKILL) != 0 && process_is_alive(pid) {
            return Err(SessionError::OrphanQemu);
        }
    }
    if wait_for_process_exit(pid, Duration::from_secs(2)) {
        Ok(())
    } else {
        Err(SessionError::OrphanQemu)
    }
}

#[cfg(windows)]
fn terminate_qemu(pid: u32) -> Result<(), SessionError> {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn TerminateProcess(handle: Handle, exit_code: u32) -> i32;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(SessionError::OrphanQemu);
    }
    let terminated = unsafe { TerminateProcess(handle, 1) } != 0;
    let wait = unsafe { WaitForSingleObject(handle, 2_000) };
    unsafe {
        CloseHandle(handle);
    }
    if terminated && (wait == WAIT_OBJECT_0 || wait == WAIT_TIMEOUT && !process_is_alive(pid)) {
        Ok(())
    } else {
        Err(SessionError::OrphanQemu)
    }
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process_is_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let Some((_, fields)) = stat.rsplit_once(')') else {
                return false;
            };
            if matches!(fields.split_whitespace().next(), Some("Z" | "X")) {
                return false;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => {}
    }
    // Treat permission denial as live so recovery never deletes a session that
    // might still be used by an inaccessible process.
    unsafe {
        libc::kill(pid as i32, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(1)
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    unsafe {
        libc::kill(pid as i32, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(1)
    }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return io::Error::last_os_error().raw_os_error() == Some(5);
    }
    let state = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle);
    }
    state == WAIT_TIMEOUT || (state != WAIT_OBJECT_0 && state != WAIT_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rootglass-encrypted-sessions-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn reservations_enforce_capacity_and_release_each_run_independently() {
        let root = temp_dir();
        let store = EncryptedSessionStore::new(&root);
        let first = store.reserve(2).unwrap();
        let second = store.reserve(2).unwrap();
        assert!(matches!(
            store.reserve(2),
            Err(SessionError::Capacity { limit: 2 })
        ));
        drop(first);
        let third = store.reserve(2).unwrap();
        drop((second, third));
        let names = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(!names
            .iter()
            .any(|name| name == "slot-0.json" || name == "slot-1.json"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_signed_limit() {
        use std::sync::{Arc, Barrier};

        let root = temp_dir();
        let barrier = Arc::new(Barrier::new(11));
        let handles = (0..10)
            .map(|_| {
                let store = EncryptedSessionStore::new(&root);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let reservation = store.reserve(3);
                    barrier.wait();
                    reservation
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();

        let mut leases = Vec::new();
        let mut rejected = 0;
        for handle in handles {
            match handle.join().unwrap() {
                Ok(lease) => leases.push(lease),
                Err(SessionError::Capacity { limit: 3 }) => rejected += 1,
                Err(error) => panic!("unexpected reservation failure: {error}"),
            }
        }
        assert_eq!(leases.len(), 3);
        assert_eq!(rejected, 7);
        drop(leases);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovered_slots_remove_dead_session_files_but_preserve_live_owners() {
        let root = temp_dir();
        let dead_dir = root.join(format!("rootglass-qmp-{}-1", u32::MAX));
        fs::create_dir(&dead_dir).unwrap();
        fs::write(dead_dir.join("overlay.qcow2"), b"ephemeral").unwrap();
        let dead = SessionRecord {
            owner_pid: u32::MAX,
            qemu_pid: None,
            session_dir: Some(dead_dir.clone()),
        };
        write_record(&root.join("slot-0.json"), &dead, false).unwrap();
        let live = SessionRecord {
            owner_pid: std::process::id(),
            qemu_pid: None,
            session_dir: None,
        };
        write_record(&root.join("slot-1.json"), &live, false).unwrap();

        let store = EncryptedSessionStore::new(&root);
        let _lease = store.reserve(2).unwrap();
        assert!(!dead_dir.exists());
        assert!(root.join("slot-1.json").exists());
        drop(_lease);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn global_recovery_visits_every_valid_artifact_session_root() {
        let root = temp_dir();
        let artifact_root = root.join("ab".repeat(32));
        let session_dir = artifact_root.join(format!("rootglass-qmp-{}-7", u32::MAX));
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("overlay.qcow2"), b"ephemeral").unwrap();
        let record = SessionRecord {
            owner_pid: u32::MAX,
            qemu_pid: None,
            session_dir: Some(session_dir.clone()),
        };
        write_record(&artifact_root.join("slot-0.json"), &record, false).unwrap();

        EncryptedSessionStore::recover_all(&root).unwrap();
        assert!(!session_dir.exists());
        assert!(!artifact_root.join("slot-0.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_reaps_a_qemu_process_even_if_the_owner_crashed_before_recording_its_pid() {
        for recorded_pid in [true, false] {
            let root = temp_dir();
            let session_dir = root.join(format!(
                "rootglass-qmp-{}-{}",
                std::process::id(),
                u8::from(recorded_pid)
            ));
            fs::create_dir(&session_dir).unwrap();
            let marker = format!("-qmp=unix:{}", session_marker(&session_dir).unwrap());
            let mut child = std::process::Command::new("python3")
                .args(["-c", "import time; time.sleep(60)"])
                .arg(&marker)
                .spawn()
                .expect("Python is available on the Linux CI runner");
            let record = SessionRecord {
                owner_pid: u32::MAX,
                qemu_pid: recorded_pid.then_some(child.id()),
                session_dir: Some(session_dir.clone()),
            };
            write_record(&root.join("slot-0.json"), &record, false).unwrap();

            let store = EncryptedSessionStore::new(&root);
            let lease = store.reserve(1).unwrap();
            assert!(!process_is_alive(child.id()));
            assert!(!session_dir.exists());
            child.wait().unwrap();
            drop(lease);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropping_a_lease_preserves_a_live_qemu_for_the_next_recovery_pass() {
        let root = temp_dir();
        let session_dir = root.join(format!("rootglass-qmp-{}-2", std::process::id()));
        fs::create_dir(&session_dir).unwrap();
        let marker = format!("-qmp=unix:{}", session_marker(&session_dir).unwrap());
        let mut child = std::process::Command::new("python3")
            .args(["-c", "import time; time.sleep(60)"])
            .arg(&marker)
            .spawn()
            .expect("Python is available on the Linux CI runner");

        let store = EncryptedSessionStore::new(&root);
        let mut lease = store.reserve(1).unwrap();
        lease.record_session_dir(&session_dir).unwrap();
        lease.record_qemu(child.id(), &session_dir).unwrap();
        drop(lease);

        let slot = read_record(&root.join("slot-0.json")).unwrap();
        assert_eq!(slot.owner_pid, 0);
        assert!(session_dir.exists());
        assert!(process_is_alive(child.id()));

        let next_lease = store.reserve(1).unwrap();
        assert!(!session_dir.exists());
        assert!(!process_is_alive(child.id()));
        child.wait().unwrap();
        drop(next_lease);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_invalid_limits_and_session_paths() {
        let root = temp_dir();
        let store = EncryptedSessionStore::new(&root);
        assert!(matches!(store.reserve(0), Err(SessionError::InvalidLimit)));
        assert!(matches!(store.reserve(6), Err(SessionError::InvalidLimit)));
        let mut lease = store.reserve(1).unwrap();
        let outside = temp_dir();
        assert!(matches!(
            lease.record_qemu(std::process::id(), &outside),
            Err(SessionError::InvalidRecord(_))
        ));
        drop(lease);
        fs::remove_dir_all(outside).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
