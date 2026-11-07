use super::{
    dsse_pae, hex_sha256, normalize_relative_path, verify_glass_archive, DsseEnvelope,
    SignatureError, TrustStore, TrustedPublisher, VerificationLimits, DSSE_PAYLOAD_TYPE,
    PREDICATE_TYPE, STATEMENT_TYPE,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningReport {
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
    pub subject_count: usize,
}

#[derive(Debug)]
struct GlassFile {
    name: String,
    path: PathBuf,
    digest: String,
    size: u64,
}

const MAX_SIGNING_KEY_FILE_BYTES: u64 = 256;

/// Load a raw 32-byte Ed25519 seed or base64url-encoded seed from a secret file.
pub fn load_signing_key(path: impl AsRef<Path>) -> Result<SigningKey, SignatureError> {
    let path = path.as_ref();
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take(MAX_SIGNING_KEY_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|source| SignatureError::ReadSigningKey {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_SIGNING_KEY_FILE_BYTES {
        return Err(SignatureError::InvalidSigningKey(path.to_path_buf()));
    }
    let seed = if bytes.len() == 32 {
        bytes
    } else {
        let encoded = std::str::from_utf8(&bytes)
            .map_err(|_| SignatureError::InvalidSigningKey(path.to_path_buf()))?
            .trim();
        URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| SignatureError::InvalidSigningKey(path.to_path_buf()))?
    };
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| SignatureError::InvalidSigningKey(path.to_path_buf()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Sign each regular file in a development directory except its DSSE envelope.
pub fn sign_glass_directory(
    glass_root: impl AsRef<Path>,
    signing_key: &SigningKey,
) -> Result<SigningReport, SignatureError> {
    sign_glass_directory_at(glass_root, signing_key, &current_timestamp())
}

/// Sign a directory using a caller-supplied RFC 3339 timestamp.
///
/// Supplying the same timestamp and inputs produces the same DSSE envelope because Ed25519
/// signing is deterministic. This is useful for reproducible fixtures and release builds.
pub fn sign_glass_directory_at(
    glass_root: impl AsRef<Path>,
    signing_key: &SigningKey,
    created_at: &str,
) -> Result<SigningReport, SignatureError> {
    let root = glass_root.as_ref();
    let limits = VerificationLimits::default();
    let (signature_path, envelope, report, _) =
        prepare_signature(root, signing_key, created_at, &limits)?;
    let envelope_path = root.join(signature_path);
    let parent = envelope_path.parent().unwrap_or(root);
    fs::create_dir_all(parent).map_err(|source| SignatureError::WriteEnvelope {
        path: envelope_path.clone(),
        source,
    })?;
    let encoded = serde_json::to_vec_pretty(&envelope)?;
    if encoded.len() as u64 + 1 > limits.max_signature_bytes {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: limits.max_signature_bytes,
        });
    }
    fs::write(&envelope_path, [encoded.as_slice(), b"\n"].concat()).map_err(|source| {
        SignatureError::WriteEnvelope {
            path: envelope_path,
            source,
        }
    })?;
    Ok(report)
}

/// Create and verify a deterministic ZIP .glass archive before publishing it.
pub fn package_glass_directory(
    glass_root: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    signing_key: &SigningKey,
) -> Result<SigningReport, SignatureError> {
    package_glass_directory_at(glass_root, output_path, signing_key, &current_timestamp())
}

/// Create a canonical ZIP package using a caller-supplied RFC 3339 timestamp.
pub fn package_glass_directory_at(
    glass_root: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    signing_key: &SigningKey,
    created_at: &str,
) -> Result<SigningReport, SignatureError> {
    let root = glass_root.as_ref();
    let output = output_path.as_ref();
    if output.exists() {
        return Err(SignatureError::PackageOutputExists(output.to_path_buf()));
    }

    let limits = VerificationLimits::default();
    let (signature_path, envelope, report, files) =
        prepare_signature(root, signing_key, created_at, &limits)?;
    let signature_name = super::path_to_string(&signature_path);
    let encoded = serde_json::to_vec_pretty(&envelope)?;
    let envelope_bytes = [encoded.as_slice(), b"\n"].concat();
    if envelope_bytes.len() as u64 > limits.max_signature_bytes {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: limits.max_signature_bytes,
        });
    }
    let trust = signer_trust_store(
        signing_key,
        &report.publisher_name,
        &report.publisher_identity,
    );
    let temporary = temporary_sibling(output)?;

    let write_result = write_archive(
        &temporary,
        &files,
        &signature_name,
        &envelope_bytes,
        &limits,
    )
    .and_then(|()| {
        verify_glass_archive(&temporary, &signature_name, &trust)
            .map(|_| ())
            .map_err(|error| SignatureError::WritePackage {
                path: temporary.clone(),
                source: std::io::Error::other(format!(
                    "newly generated package did not verify: {error}"
                )),
            })
    });

    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    // Hard-link publication is atomic and refuses to overwrite a raced path.
    if let Err(source) = fs::hard_link(&temporary, output) {
        let _ = fs::remove_file(&temporary);
        return Err(SignatureError::WritePackage {
            path: output.to_path_buf(),
            source,
        });
    }
    fs::remove_file(&temporary).map_err(|source| SignatureError::WritePackage {
        path: temporary,
        source,
    })?;
    Ok(report)
}

fn prepare_signature(
    root: &Path,
    signing_key: &SigningKey,
    created_at: &str,
    limits: &VerificationLimits,
) -> Result<(PathBuf, DsseEnvelope, SigningReport, Vec<GlassFile>), SignatureError> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|source| SignatureError::ReadSignedFile {
            path: root.to_path_buf(),
            source,
        })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(SignatureError::UnsupportedArchiveEntry(
            root.display().to_string(),
        ));
    }
    let glass = rootglass_manifest::Glass::load(root)
        .map_err(|error| SignatureError::InvalidManifest(error.to_string()))?;
    glass
        .validate_manifest_only()
        .map_err(|error| SignatureError::InvalidManifest(error.to_string()))?;

    let signature_path = normalize_relative_path(Path::new(&glass.manifest.publisher.signature))?;
    let mut total = 0u64;
    let mut file_count = 0usize;
    let mut files = collect_glass_files(
        root,
        root,
        &signature_path,
        limits,
        &mut total,
        &mut file_count,
    )?;
    files.sort_by(|left, right| left.name.cmp(&right.name));

    let public_key = signing_key.verifying_key().to_bytes();
    let key_id = format!("ed25519:{}", hex_sha256(&public_key));
    let report = SigningReport {
        publisher_name: glass.manifest.publisher.name.clone(),
        publisher_identity: glass.manifest.publisher.identity.clone(),
        key_id: key_id.clone(),
        subject_count: files.len(),
    };
    let subjects = files
        .iter()
        .map(|file| {
            json!({
                "name": file.name,
                "digest": {"sha256": file.digest}
            })
        })
        .collect::<Vec<_>>();
    let statement = json!({
        "_type": STATEMENT_TYPE,
        "predicate": {
            "createdAt": created_at,
            "glassIdentifier": glass.manifest.identifier,
            "glassVersion": glass.manifest.version,
            "publisher": {
                "identity": glass.manifest.publisher.identity,
                "keyId": key_id,
                "name": glass.manifest.publisher.name
            },
            "schemaVersion": glass.manifest.schema_version
        },
        "predicateType": PREDICATE_TYPE,
        "subject": subjects
    });
    let payload = serde_json::to_vec(&statement)?;
    let signature = signing_key.sign(&dsse_pae(DSSE_PAYLOAD_TYPE.as_bytes(), &payload));
    let envelope: DsseEnvelope = serde_json::from_value(json!({
        "payloadType": DSSE_PAYLOAD_TYPE,
        "payload": URL_SAFE_NO_PAD.encode(payload),
        "signatures": [{
            "keyid": report.key_id,
            "sig": URL_SAFE_NO_PAD.encode(signature.to_bytes())
        }]
    }))?;
    let payload_size = URL_SAFE_NO_PAD
        .decode(&envelope.payload)
        .map_err(|error| SignatureError::InvalidPayloadEncoding(error.to_string()))?
        .len() as u64;
    if payload_size > limits.max_signature_bytes {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: limits.max_signature_bytes,
        });
    }
    let envelope_size = serde_json::to_vec(&envelope)?.len() as u64 + 1;
    if envelope_size > limits.max_signature_bytes {
        return Err(SignatureError::SignatureEnvelopeTooLarge {
            limit: limits.max_signature_bytes,
        });
    }
    if total.saturating_add(envelope_size) > limits.max_total_uncompressed_bytes {
        return Err(SignatureError::ArchiveUncompressedTooLarge {
            limit: limits.max_total_uncompressed_bytes,
        });
    }
    Ok((signature_path, envelope, report, files))
}

fn collect_glass_files(
    root: &Path,
    current: &Path,
    signature_path: &Path,
    limits: &VerificationLimits,
    total: &mut u64,
    file_count: &mut usize,
) -> Result<Vec<GlassFile>, SignatureError> {
    let mut files = Vec::new();
    let entries = fs::read_dir(current).map_err(|source| SignatureError::ReadSignedFile {
        path: current.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| SignatureError::ReadSignedFile {
            path: current.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| SignatureError::ReadSignedFile {
                path: path.clone(),
                source,
            })?;
        let relative = path
            .strip_prefix(root)
            .expect("enumerated path is under root");
        let relative_text = relative
            .to_str()
            .ok_or_else(|| SignatureError::NonUtf8Path(relative.to_path_buf()))?;
        if metadata.file_type().is_symlink() {
            return Err(SignatureError::SymlinkInGlass(relative_text.to_string()));
        }
        if metadata.is_dir() {
            files.extend(collect_glass_files(
                root,
                &path,
                signature_path,
                limits,
                total,
                file_count,
            )?);
            continue;
        }
        if !metadata.is_file() {
            return Err(SignatureError::UnsafeArchivePath(relative_text.to_string()));
        }
        let normalized = normalize_relative_path(relative)?;
        if normalized == signature_path {
            continue;
        }
        let name = super::path_to_string(&normalized);
        let size = metadata.len();
        if size > limits.max_member_bytes {
            return Err(SignatureError::SignedFileTooLarge {
                path: name,
                limit: limits.max_member_bytes,
            });
        }
        *total = total
            .checked_add(size)
            .ok_or(SignatureError::ArchiveUncompressedTooLarge {
                limit: limits.max_total_uncompressed_bytes,
            })?;
        if *total > limits.max_total_uncompressed_bytes {
            return Err(SignatureError::ArchiveUncompressedTooLarge {
                limit: limits.max_total_uncompressed_bytes,
            });
        }
        if *file_count >= limits.max_entries.saturating_sub(1) {
            return Err(SignatureError::TooManyArchiveEntries {
                limit: limits.max_entries,
            });
        }
        *file_count += 1;
        let digest = hash_file(&path, size, limits.max_member_bytes)?;
        files.push(GlassFile {
            name,
            path,
            digest,
            size,
        });
    }
    Ok(files)
}

fn hash_file(path: &Path, expected_size: u64, limit: u64) -> Result<String, SignatureError> {
    let file = File::open(path).map_err(|source| SignatureError::ReadSignedFile {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = file.take(limit.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| SignatureError::ReadSignedFile {
                path: path.to_path_buf(),
                source,
            })?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > limit {
            return Err(SignatureError::SignedFileTooLarge {
                path: path.display().to_string(),
                limit,
            });
        }
        hasher.update(&buffer[..count]);
    }
    if total != expected_size {
        return Err(SignatureError::InvalidStatement(format!(
            "file '{}' changed while it was being signed",
            path.display()
        )));
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_archive(
    output: &Path,
    files: &[GlassFile],
    signature_name: &str,
    envelope: &[u8],
    limits: &VerificationLimits,
) -> Result<(), SignatureError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|source| SignatureError::WritePackage {
            path: output.to_path_buf(),
            source,
        })?;
    let mut writer = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(0o644);
    let mut order = files
        .iter()
        .map(|file| file.name.as_str())
        .chain(std::iter::once(signature_name))
        .collect::<Vec<_>>();
    order.sort_unstable();

    for name in order {
        writer
            .start_file(name, options)
            .map_err(|error| SignatureError::WritePackage {
                path: output.to_path_buf(),
                source: std::io::Error::other(error.to_string()),
            })?;
        if name == signature_name {
            writer
                .write_all(envelope)
                .map_err(|source| SignatureError::WritePackage {
                    path: output.to_path_buf(),
                    source,
                })?;
        } else {
            let file = files
                .iter()
                .find(|file| file.name == name)
                .expect("archive order was built from files");
            let input =
                File::open(&file.path).map_err(|source| SignatureError::ReadSignedFile {
                    path: file.path.clone(),
                    source,
                })?;
            let copied = std::io::copy(
                &mut input.take(limits.max_member_bytes.saturating_add(1)),
                &mut writer,
            )
            .map_err(|source| SignatureError::WritePackage {
                path: output.to_path_buf(),
                source,
            })?;
            if copied > limits.max_member_bytes {
                return Err(SignatureError::SignedFileTooLarge {
                    path: file.name.clone(),
                    limit: limits.max_member_bytes,
                });
            }
            if copied != file.size {
                return Err(SignatureError::InvalidStatement(format!(
                    "file '{}' changed while it was packaged",
                    file.name
                )));
            }
        }
    }

    let file = writer
        .finish()
        .map_err(|error| SignatureError::WritePackage {
            path: output.to_path_buf(),
            source: std::io::Error::other(error.to_string()),
        })?;
    file.sync_all()
        .map_err(|source| SignatureError::WritePackage {
            path: output.to_path_buf(),
            source,
        })?;
    let size = file
        .metadata()
        .map_err(|source| SignatureError::WritePackage {
            path: output.to_path_buf(),
            source,
        })?
        .len();
    if size > limits.max_archive_bytes {
        return Err(SignatureError::ArchiveTooLarge {
            limit: limits.max_archive_bytes,
        });
    }
    Ok(())
}

fn signer_trust_store(
    signing_key: &SigningKey,
    publisher_name: &str,
    publisher_identity: &str,
) -> TrustStore {
    let public_key = signing_key.verifying_key().to_bytes();
    let key_id = format!("ed25519:{}", hex_sha256(&public_key));
    TrustStore::new([TrustedPublisher {
        name: publisher_name.to_string(),
        identity: publisher_identity.to_string(),
        key_id,
        public_key_base64url: URL_SAFE_NO_PAD.encode(public_key),
    }])
}

fn temporary_sibling(output: &Path) -> Result<PathBuf, SignatureError> {
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = output
        .file_name()
        .ok_or_else(|| SignatureError::UnsafeArchivePath(output.to_string_lossy().to_string()))?
        .to_string_lossy();
    for _ in 0..100 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{name}.rootglass-{}-{id}.tmp", std::process::id()));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(SignatureError::WritePackage {
        path: output.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a temporary package path",
        ),
    })
}

fn current_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
