//! Bounded import of signed `.glass` ZIP bundles into an immutable content store.
//!
//! The signature crate currently buffers every uncompressed archive member while
//! verifying. `ImportLimits::default()` therefore caps the aggregate uncompressed
//! size at 512 MiB (and the compressed archive at 256 MiB) before invoking it.
//! This gives the current verifier a strict upper bound, but verification is not
//! streaming; callers that need larger bundles should first add a streaming API
//! to `rootglass-signature` rather than increasing these limits casually.

use crate::registry::{InstalledGlass, PackageRegistry, RegistryError};
use rootglass_manifest::Glass;
use rootglass_signature::{
    verify_glass_archive_with_limits, verify_glass_dir_with_limits, TrustStore, VerificationLimits,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use zip::ZipArchive;

const SIGNATURE_PATH: &str = "signatures/glass.dsse.json";
static TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Hard bounds checked before signature verification and again while extracting.
#[derive(Debug, Clone)]
pub struct ImportLimits {
    pub max_archive_bytes: u64,
    pub max_members: usize,
    pub max_member_bytes: u64,
    pub max_total_uncompressed_bytes: u64,
    pub max_compression_ratio: u64,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 256 * 1024 * 1024,
            max_members: 4096,
            max_member_bytes: 256 * 1024 * 1024,
            max_total_uncompressed_bytes: 512 * 1024 * 1024,
            max_compression_ratio: 200,
        }
    }
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid ZIP archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("signature verification failed: {0}")]
    Signature(#[from] rootglass_signature::SignatureError),
    #[error("manifest is invalid: {0}")]
    Manifest(#[from] rootglass_manifest::ManifestError),
    #[error("registry update failed: {0}")]
    Registry(#[from] RegistryError),
    #[error("archive exceeds the configured compressed size limit ({limit} bytes)")]
    ArchiveTooLarge { limit: u64 },
    #[error("archive has more than the configured {limit} entries")]
    TooManyMembers { limit: usize },
    #[error("archive member '{path}' exceeds the configured {limit} byte limit")]
    MemberTooLarge { path: String, limit: u64 },
    #[error("archive exceeds the configured aggregate uncompressed limit ({limit} bytes)")]
    TotalTooLarge { limit: u64 },
    #[error("archive member '{path}' exceeds the configured compression ratio limit ({limit}:1)")]
    CompressionRatio { path: String, limit: u64 },
    #[error("unsafe archive path: {0}")]
    UnsafePath(String),
    #[error("duplicate or conflicting normalized archive path: {0}")]
    DuplicatePath(String),
    #[error("archive contains a link or non-regular special file: {0}")]
    UnsupportedFileType(String),
    #[error("archive changed while it was being imported: {detail}")]
    ArchiveChanged { detail: String },
    #[error("archive is missing required manifest '{0}'")]
    MissingManifest(&'static str),
}

/// Verify and install a signed archive, then atomically publish its registry row.
/// The caller supplies trust policy; this function never silently trusts a key.
pub fn import_glass_archive(
    source: impl AsRef<Path>,
    store_root: impl AsRef<Path>,
    registry: &mut PackageRegistry,
    trust_store: &TrustStore,
    limits: &ImportLimits,
) -> Result<InstalledGlass, ImportError> {
    let source = source.as_ref();
    let store_root = store_root.as_ref();
    validate_limits(limits)?;
    fs::create_dir_all(store_root).map_err(|source| io_at(store_root, source))?;

    // Snapshot to a private temporary file, so verification and extraction consume
    // exactly the bytes whose digest names the immutable installed package.
    let snapshot = temp_path(store_root, "archive");
    let snapshot_result = (|| {
        let input = File::open(source).map_err(|error| io_at(source, error))?;
        let mut snapshot_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&snapshot)
            .map_err(|source| io_at(&snapshot, source))?;
        let mut hasher = Sha256::new();
        let mut count = 0u64;
        let mut reader = input;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = reader
                .read(&mut buffer)
                .map_err(|error| io_at(source, error))?;
            if n == 0 {
                break;
            }
            count = count
                .checked_add(n as u64)
                .ok_or(ImportError::ArchiveTooLarge {
                    limit: limits.max_archive_bytes,
                })?;
            if count > limits.max_archive_bytes {
                return Err(ImportError::ArchiveTooLarge {
                    limit: limits.max_archive_bytes,
                });
            }
            snapshot_file
                .write_all(&buffer[..n])
                .map_err(|source| io_at(&snapshot, source))?;
            hasher.update(&buffer[..n]);
        }
        snapshot_file
            .sync_all()
            .map_err(|source| io_at(&snapshot, source))?;
        Ok(encode_hex(&hasher.finalize()))
    })();
    let digest = match snapshot_result {
        Ok(digest) => digest,
        Err(error) => {
            let _ = fs::remove_file(&snapshot);
            return Err(error);
        }
    };

    let result = import_snapshot(
        source,
        &snapshot,
        store_root,
        registry,
        trust_store,
        limits,
        &digest,
    );
    let _ = fs::remove_file(&snapshot);
    result
}

fn import_snapshot(
    source: &Path,
    snapshot: &Path,
    store_root: &Path,
    registry: &mut PackageRegistry,
    trust_store: &TrustStore,
    limits: &ImportLimits,
    digest: &str,
) -> Result<InstalledGlass, ImportError> {
    let verification_limits = VerificationLimits {
        max_archive_bytes: limits.max_archive_bytes,
        max_entries: limits.max_members,
        max_member_bytes: limits.max_member_bytes,
        max_total_uncompressed_bytes: limits.max_total_uncompressed_bytes,
        max_compression_ratio: limits.max_compression_ratio,
        max_signature_bytes: limits
            .max_total_uncompressed_bytes
            .min(rootglass_signature::MAX_SIGNATURE_ENVELOPE_BYTES),
    };
    let verification = verify_glass_archive_with_limits(
        snapshot,
        SIGNATURE_PATH,
        trust_store,
        &verification_limits,
    )?;
    inspect_archive(snapshot, limits)?;

    let packages_dir = store_root.join("packages");
    fs::create_dir_all(&packages_dir).map_err(|source| io_at(&packages_dir, source))?;
    let final_path = packages_dir.join(digest);
    if let Ok(metadata) = fs::symlink_metadata(&final_path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ImportError::UnsupportedFileType(
                final_path.display().to_string(),
            ));
        }
    }
    if !final_path.exists() {
        let stage = temp_path(&packages_dir, "package");
        fs::create_dir(&stage).map_err(|source| io_at(&stage, source))?;
        let extract_result = extract_archive(snapshot, &stage, limits);
        if let Err(error) = extract_result {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
        let glass = match Glass::load(&stage) {
            Ok(glass) => glass,
            Err(error) => {
                let _ = fs::remove_dir_all(&stage);
                return Err(error.into());
            }
        };
        if let Err(error) = glass.validate_launch_ready() {
            let _ = fs::remove_dir_all(&stage);
            return Err(error.into());
        }
        if let Err(error) = make_tree_contents_readonly(&stage) {
            make_tree_writable(&stage);
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
        match fs::rename(&stage, &final_path) {
            Ok(()) => {
                if let Err(error) = make_path_readonly(&final_path) {
                    make_tree_writable(&final_path);
                    let _ = fs::remove_dir_all(&final_path);
                    return Err(error);
                }
            }
            Err(_error) if final_path.is_dir() => {
                make_tree_writable(&stage);
                let _ = fs::remove_dir_all(&stage);
            }
            Err(source) => {
                make_tree_writable(&stage);
                let _ = fs::remove_dir_all(&stage);
                return Err(io_at(&final_path, source));
            }
        }
        if let Ok(dir) = File::open(&packages_dir) {
            let _ = dir.sync_all();
        }
    }

    // Loading from the content store also validates the manifest when the target
    // already existed, which prevents a stale/corrupt directory becoming trusted.
    let installed_verification = verify_glass_dir_with_limits(
        &final_path,
        SIGNATURE_PATH,
        trust_store,
        &verification_limits,
    )?;
    if installed_verification.publisher_identity != verification.publisher_identity
        || installed_verification.key_id != verification.key_id
    {
        return Err(ImportError::ArchiveChanged {
            detail: "installed publisher identity or key ID differs from the verified archive"
                .to_string(),
        });
    }
    verify_installed_matches_archive(snapshot, &final_path, limits)?;
    let glass = Glass::load(&final_path)?;
    glass.validate_launch_ready()?;
    let manifest = glass.manifest;
    let installed = InstalledGlass {
        content_sha256: digest.to_owned(),
        package_path: final_path,
        identifier: manifest.identifier,
        name: manifest.name,
        version: manifest.version,
        publisher_name: verification.publisher_name,
        publisher_identity: verification.publisher_identity,
        key_id: verification.key_id,
        original_source: source.to_path_buf(),
        imported_at_unix_seconds: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    registry.upsert(installed.clone())?;
    Ok(installed)
}

fn inspect_archive(path: &Path, limits: &ImportLimits) -> Result<(), ImportError> {
    let file = File::open(path).map_err(|source| io_at(path, source))?;
    let mut archive = ZipArchive::new(file)?;
    if archive.len() > limits.max_members {
        return Err(ImportError::TooManyMembers {
            limit: limits.max_members,
        });
    }
    let mut seen = HashSet::<String>::new();
    let mut kinds = std::collections::HashMap::<String, bool>::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_owned();
        let normalized = normalized_name(&name, entry.is_dir())?;
        let collision_key = normalized.to_lowercase();
        if !seen.insert(collision_key.clone()) {
            return Err(ImportError::DuplicatePath(normalized));
        }
        let is_dir = entry.is_dir();
        if let Some(existing_dir) = kinds.get(&collision_key) {
            if *existing_dir != is_dir {
                return Err(ImportError::DuplicatePath(normalized));
            }
        }
        // Reject file-as-parent conflicts, including order-independent collisions.
        let mut parent = collision_key.rsplit_once('/').map(|(parent, _)| parent);
        while let Some(parent_name) = parent {
            if kinds.get(parent_name) == Some(&false) {
                return Err(ImportError::DuplicatePath(parent_name.to_owned()));
            }
            parent = parent_name.rsplit_once('/').map(|(parent, _)| parent);
        }
        if !is_dir {
            if let Some((child, _)) = kinds
                .iter()
                .find(|(p, _)| p.starts_with(&(collision_key.clone() + "/")))
            {
                return Err(ImportError::DuplicatePath(child.clone()));
            }
        }
        kinds.insert(collision_key, is_dir);

        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind == 0o120000
                || (kind != 0 && kind != 0o100000 && kind != 0o040000)
                || (kind == 0o040000 && !is_dir)
                || (kind == 0o100000 && is_dir)
            {
                return Err(ImportError::UnsupportedFileType(normalized));
            }
        }
        if is_dir {
            continue;
        }
        let size = entry.size();
        let compressed = entry.compressed_size();
        if size > limits.max_member_bytes {
            return Err(ImportError::MemberTooLarge {
                path: normalized,
                limit: limits.max_member_bytes,
            });
        }
        total = total.checked_add(size).ok_or(ImportError::TotalTooLarge {
            limit: limits.max_total_uncompressed_bytes,
        })?;
        if total > limits.max_total_uncompressed_bytes {
            return Err(ImportError::TotalTooLarge {
                limit: limits.max_total_uncompressed_bytes,
            });
        }
        if size > 0
            && (compressed == 0
                || size / compressed.max(1) > limits.max_compression_ratio
                || size > compressed.saturating_mul(limits.max_compression_ratio))
        {
            return Err(ImportError::CompressionRatio {
                path: normalized,
                limit: limits.max_compression_ratio,
            });
        }
    }
    if !seen.contains("rootglass.toml") {
        return Err(ImportError::MissingManifest("rootglass.toml"));
    }
    if !seen.contains(SIGNATURE_PATH) {
        return Err(ImportError::MissingManifest(SIGNATURE_PATH));
    }
    Ok(())
}

fn extract_archive(
    path: &Path,
    destination: &Path,
    limits: &ImportLimits,
) -> Result<(), ImportError> {
    let file = File::open(path).map_err(|source| io_at(path, source))?;
    let mut archive = ZipArchive::new(file)?;
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_owned();
        let relative = PathBuf::from(normalized_name(&name, entry.is_dir())?);
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind == 0o120000 || (kind != 0 && kind != 0o100000 && kind != 0o040000) {
                return Err(ImportError::UnsupportedFileType(name));
            }
        }
        let output = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&output).map_err(|source| io_at(&output, source))?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).map_err(|source| io_at(parent, source))?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .map_err(|source| io_at(&output, source))?;
        let mut member = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = entry
                .read(&mut buffer)
                .map_err(|source| io_at(&output, source))?;
            if n == 0 {
                break;
            }
            member = member
                .checked_add(n as u64)
                .ok_or(ImportError::MemberTooLarge {
                    path: name.clone(),
                    limit: limits.max_member_bytes,
                })?;
            total = total
                .checked_add(n as u64)
                .ok_or(ImportError::TotalTooLarge {
                    limit: limits.max_total_uncompressed_bytes,
                })?;
            if member > limits.max_member_bytes {
                return Err(ImportError::MemberTooLarge {
                    path: name,
                    limit: limits.max_member_bytes,
                });
            }
            if total > limits.max_total_uncompressed_bytes {
                return Err(ImportError::TotalTooLarge {
                    limit: limits.max_total_uncompressed_bytes,
                });
            }
            file.write_all(&buffer[..n])
                .map_err(|source| io_at(&output, source))?;
        }
        if member != entry.size() {
            return Err(ImportError::ArchiveChanged {
                detail: format!("extracted member '{name}' size differs from its ZIP record"),
            });
        }
        file.sync_all().map_err(|source| io_at(&output, source))?;
    }
    Ok(())
}

fn verify_installed_matches_archive(
    archive_path: &Path,
    installed_root: &Path,
    limits: &ImportLimits,
) -> Result<(), ImportError> {
    let file = File::open(archive_path).map_err(|source| io_at(archive_path, source))?;
    let mut archive = ZipArchive::new(file)?;
    if archive.len() > limits.max_members {
        return Err(ImportError::TooManyMembers {
            limit: limits.max_members,
        });
    }
    let mut archive_files = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let normalized = normalized_name(&name, false)?;
        if !archive_files.insert(normalized.clone()) {
            return Err(ImportError::DuplicatePath(normalized));
        }
        let size = entry.size();
        if size > limits.max_member_bytes {
            return Err(ImportError::MemberTooLarge {
                path: normalized,
                limit: limits.max_member_bytes,
            });
        }
        total = total.checked_add(size).ok_or(ImportError::TotalTooLarge {
            limit: limits.max_total_uncompressed_bytes,
        })?;
        if total > limits.max_total_uncompressed_bytes {
            return Err(ImportError::TotalTooLarge {
                limit: limits.max_total_uncompressed_bytes,
            });
        }

        let relative = PathBuf::from(&normalized);
        ensure_no_symlink_components(installed_root, &relative)?;
        let installed_path = installed_root.join(&relative);
        let metadata = fs::symlink_metadata(&installed_path)
            .map_err(|source| io_at(&installed_path, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ImportError::UnsupportedFileType(
                installed_path.display().to_string(),
            ));
        }
        if metadata.len() != size {
            return Err(ImportError::ArchiveChanged {
                detail: format!(
                    "installed member '{normalized}' has size {}, archive records {size}",
                    metadata.len()
                ),
            });
        }
        let mut installed =
            File::open(&installed_path).map_err(|source| io_at(&installed_path, source))?;
        let mut archived_buffer = [0u8; 64 * 1024];
        let mut installed_buffer = [0u8; 64 * 1024];
        let mut compared = 0u64;
        while compared < size {
            // ZIP decompression and file reads can return different short reads.
            let count = (size - compared).min(archived_buffer.len() as u64) as usize;
            entry
                .read_exact(&mut archived_buffer[..count])
                .map_err(|source| io_at(archive_path, source))?;
            installed
                .read_exact(&mut installed_buffer[..count])
                .map_err(|source| io_at(&installed_path, source))?;
            if archived_buffer[..count] != installed_buffer[..count] {
                return Err(ImportError::ArchiveChanged {
                    detail: format!("installed member '{normalized}' differs from archive bytes"),
                });
            }
            compared += count as u64;
        }
    }

    let mut installed_files = BTreeSet::new();
    collect_installed_files(installed_root, installed_root, &mut installed_files)?;
    if installed_files != archive_files {
        return Err(ImportError::ArchiveChanged {
            detail: "installed file list differs from archive contents".to_string(),
        });
    }
    Ok(())
}

fn collect_installed_files(
    root: &Path,
    current: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), ImportError> {
    for entry in fs::read_dir(current).map_err(|source| io_at(current, source))? {
        let entry = entry.map_err(|source| io_at(current, source))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| io_at(&path, source))?;
        if metadata.file_type().is_symlink() {
            return Err(ImportError::UnsupportedFileType(path.display().to_string()));
        }
        if metadata.is_dir() {
            collect_installed_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("enumerated path is under installed root");
            let name = relative
                .to_str()
                .ok_or_else(|| ImportError::UnsafePath(relative.display().to_string()))?
                .replace(std::path::MAIN_SEPARATOR, "/");
            let normalized = normalized_name(&name, false)?;
            if !files.insert(normalized.clone()) {
                return Err(ImportError::DuplicatePath(normalized));
            }
        } else {
            return Err(ImportError::UnsupportedFileType(path.display().to_string()));
        }
    }
    Ok(())
}

fn ensure_no_symlink_components(root: &Path, relative: &Path) -> Result<(), ImportError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(ImportError::UnsafePath(relative.display().to_string()));
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|source| io_at(&current, source))?;
        if metadata.file_type().is_symlink() {
            return Err(ImportError::UnsupportedFileType(
                current.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn make_tree_contents_readonly(root: &Path) -> Result<(), ImportError> {
    for entry in fs::read_dir(root).map_err(|source| io_at(root, source))? {
        let entry = entry.map_err(|source| io_at(root, source))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| io_at(&path, source))?;
        if metadata.file_type().is_symlink() {
            return Err(ImportError::UnsupportedFileType(path.display().to_string()));
        }
        if metadata.is_dir() {
            make_tree_contents_readonly(&path)?;
        }
    }

    for entry in fs::read_dir(root).map_err(|source| io_at(root, source))? {
        let entry = entry.map_err(|source| io_at(root, source))?;
        make_path_readonly(&entry.path())?;
    }
    Ok(())
}

fn make_path_readonly(path: &Path) -> Result<(), ImportError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_at(path, source))?;
    if metadata.file_type().is_symlink() {
        return Err(ImportError::UnsupportedFileType(path.display().to_string()));
    }
    let mut permissions = metadata.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions).map_err(|source| io_at(path, source))
}

fn make_tree_writable(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        {
            make_tree_writable(&path);
        }
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            make_path_writable(&path, metadata);
        }
    }
    if let Ok(metadata) = fs::metadata(root) {
        make_path_writable(root, metadata);
    }
}

fn make_path_writable(path: &Path, metadata: fs::Metadata) {
    if metadata.file_type().is_symlink() {
        return;
    }
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o200);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(false);
    let _ = fs::set_permissions(path, permissions);
}

fn normalized_name(name: &str, is_dir: bool) -> Result<String, ImportError> {
    if name.is_empty()
        || name.len() > 4096
        || name.contains('\\')
        || name.starts_with('/')
        || name.contains(':')
        || name.contains('\0')
    {
        return Err(ImportError::UnsafePath(name.to_owned()));
    }
    let trimmed = if is_dir {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    let mut path = PathBuf::new();
    for segment in trimmed.split('/') {
        let device_name = segment
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.len() > 255
            || segment.ends_with('.')
            || segment.ends_with(' ')
            || segment
                .chars()
                .any(|ch| ch.is_control() || "<>|?*".contains(ch))
            || matches!(
                device_name.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
                    | "COM¹"
                    | "COM²"
                    | "COM³"
                    | "LPT¹"
                    | "LPT²"
                    | "LPT³"
            )
        {
            return Err(ImportError::UnsafePath(name.to_owned()));
        }
        path.push(segment);
    }
    if path.as_os_str().is_empty() {
        return Err(ImportError::UnsafePath(name.to_owned()));
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn validate_limits(limits: &ImportLimits) -> Result<(), ImportError> {
    if limits.max_archive_bytes == 0
        || limits.max_members == 0
        || limits.max_member_bytes == 0
        || limits.max_total_uncompressed_bytes == 0
        || limits.max_compression_ratio == 0
        || limits.max_member_bytes > limits.max_total_uncompressed_bytes
    {
        return Err(ImportError::ArchiveTooLarge {
            limit: limits.max_archive_bytes,
        });
    }
    Ok(())
}

fn temp_path(parent: &Path, label: &str) -> PathBuf {
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{label}.{}.{}.tmp", std::process::id(), id))
}

fn io_at(path: &Path, source: io::Error) -> ImportError {
    ImportError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;

    fn temp_file(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rootglass-{label}-{}-{nonce}.zip",
            std::process::id()
        ))
    }

    fn archive(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn keeps_staging_root_writable_until_atomic_publish() {
        let parent = temp_file("readonly-publish").with_extension("dir");
        let stage = parent.join("stage");
        let published = parent.join("published");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("payload"), b"verified package").unwrap();

        make_tree_contents_readonly(&stage).unwrap();
        assert!(fs::metadata(stage.join("payload"))
            .unwrap()
            .permissions()
            .readonly());
        assert!(!fs::metadata(&stage).unwrap().permissions().readonly());

        fs::rename(&stage, &published).unwrap();
        make_path_readonly(&published).unwrap();
        assert!(fs::metadata(&published).unwrap().permissions().readonly());

        make_tree_writable(&published);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn accepts_bounded_bundle_shape() {
        let path = temp_file("bounded");
        archive(
            &path,
            &[
                ("rootglass.toml", b"manifest"),
                (SIGNATURE_PATH, b"signature"),
            ],
        );
        assert!(inspect_archive(&path, &ImportLimits::default()).is_ok());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_traversal_and_file_parent_conflicts() {
        let traversal = temp_file("traversal");
        archive(
            &traversal,
            &[
                ("../escape", b"x"),
                ("rootglass.toml", b"m"),
                (SIGNATURE_PATH, b"s"),
            ],
        );
        assert!(matches!(
            inspect_archive(&traversal, &ImportLimits::default()),
            Err(ImportError::UnsafePath(_))
        ));
        let _ = fs::remove_file(traversal);

        let duplicate = temp_file("duplicate");
        archive(
            &duplicate,
            &[
                ("a", b"x"),
                ("a/b", b"y"),
                ("rootglass.toml", b"m"),
                (SIGNATURE_PATH, b"s"),
            ],
        );
        assert!(matches!(
            inspect_archive(&duplicate, &ImportLimits::default()),
            Err(ImportError::DuplicatePath(_))
        ));
        let _ = fs::remove_file(duplicate);
    }

    #[test]
    fn rejects_case_collisions_and_nonportable_names() {
        let collision = temp_file("case-collision");
        archive(
            &collision,
            &[
                ("rootglass.toml", b"manifest"),
                (SIGNATURE_PATH, b"signature"),
                ("Assets/Logo.png", b"one"),
                ("assets/logo.png", b"two"),
            ],
        );
        assert!(matches!(
            inspect_archive(&collision, &ImportLimits::default()),
            Err(ImportError::DuplicatePath(_))
        ));
        let _ = fs::remove_file(collision);

        for name in ["CON.txt", "folder/.. ", "trailing.", "a\\b", "C:/root"] {
            assert!(matches!(
                normalized_name(name, false),
                Err(ImportError::UnsafePath(_))
            ));
        }
    }

    #[test]
    fn import_stops_copying_when_archive_byte_limit_is_exceeded() {
        let root = std::env::temp_dir().join(format!(
            "rootglass-import-size-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.glass");
        archive(
            &source,
            &[
                ("rootglass.toml", b"manifest"),
                (SIGNATURE_PATH, b"signature"),
            ],
        );
        let mut registry = PackageRegistry::open(root.join("registry.json")).unwrap();
        let store = root.join("store");
        let limits = ImportLimits {
            max_archive_bytes: 1,
            ..ImportLimits::default()
        };
        assert!(matches!(
            import_glass_archive(
                &source,
                &store,
                &mut registry,
                &TrustStore::default(),
                &limits
            ),
            Err(ImportError::ArchiveTooLarge { .. })
        ));
        assert!(registry.packages().is_empty());
        assert_eq!(fs::read_dir(store).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_member_and_compression_bombs_from_metadata() {
        let path = temp_file("ratio");
        let repetitive = vec![b'x'; 16 * 1024];
        archive(
            &path,
            &[("rootglass.toml", &repetitive), (SIGNATURE_PATH, b"s")],
        );
        let limits = ImportLimits {
            max_member_bytes: 4096,
            max_total_uncompressed_bytes: 8192,
            ..ImportLimits::default()
        };
        assert!(matches!(
            inspect_archive(&path, &limits),
            Err(ImportError::MemberTooLarge { .. })
        ));
        let limits = ImportLimits {
            max_member_bytes: 32 * 1024,
            max_total_uncompressed_bytes: 64 * 1024,
            max_compression_ratio: 2,
            ..ImportLimits::default()
        };
        assert!(matches!(
            inspect_archive(&path, &limits),
            Err(ImportError::CompressionRatio { .. })
        ));
        let _ = fs::remove_file(path);
    }
}
