//! UI-independent application orchestration shared by the CLI and desktop.
pub mod import;
pub mod registry;
pub mod state;

use rootglass_core::storage::{LocalWritableOverlay, WritableOverlaySelection};
use rootglass_core::runtime_assets::RuntimeAssetsResolver;
use rootglass_core::{HostCapabilities, LaunchRequest, LifecycleState, RootglassRuntime};
use rootglass_qemu::{QemuLaunchPlan, QemuProcess, SerialChunk};
use rootglass_signature::trust_config::{load_trust_store, LoadedTrustStore, TrustConfigError};
use rootglass_signature::TrustStore;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Maximum time allowed for a guest to show its profile-specific ready prompt.
pub const GUEST_READY_TIMEOUT: Duration = Duration::from_secs(90);
const ERROR_SCAN_TAIL_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlassSummary {
    pub path: PathBuf,
    pub name: String,
    pub identifier: String,
    pub version: String,
    pub description: String,
    pub architecture: String,
    pub profile: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
    pub signed_subject_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    pub summary: GlassSummary,
    pub readiness: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub state_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    StateChanged {
        run: RunId,
        state: LifecycleState,
    },
    SerialChunk {
        run: RunId,
        bytes: Vec<u8>,
        truncated: bool,
    },
    GuestReady {
        run: RunId,
        evidence: String,
    },
    Exited {
        run: RunId,
        exit_code: Option<i32>,
        expected: bool,
    },
    Failed {
        run: RunId,
        phase: LifecycleState,
        summary: String,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppError {
    pub phase: LifecycleState,
    pub summary: String,
    pub detail: String,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\n{}", self.summary, self.detail)
    }
}
impl std::error::Error for AppError {}

struct ActiveRun {
    id: RunId,
    process: QemuProcess,
    state: LifecycleState,
    login_seen: bool,
    login_marker_tail: Vec<u8>,
    diagnostic_tail: Vec<u8>,
    ready_deadline: Instant,
    _state_lock: Option<state::StateRunLock>,
}

/// Cloneable library handle for background archive verification and registry work.
#[derive(Clone)]
pub struct PackageLibrary {
    root: PathBuf,
    trust_store: TrustStore,
}

impl PackageLibrary {
    pub fn import_archive(
        &self,
        source: impl AsRef<Path>,
    ) -> Result<registry::InstalledGlass, AppError> {
        let registry_path = self.root.join("registry.json");
        let mut registry = registry::PackageRegistry::open(&registry_path).map_err(|error| {
            import_app_error("The installed-glass registry could not be opened.", error)
        })?;
        import::import_glass_archive(
            source,
            &self.root,
            &mut registry,
            &self.trust_store,
            &import::ImportLimits::default(),
        )
        .map_err(|error| {
            import_app_error(
                "The .glass archive could not be verified or installed.",
                error,
            )
        })
    }

    pub fn installed_glasses(&self) -> Result<Vec<registry::InstalledGlass>, AppError> {
        let registry =
            registry::PackageRegistry::open(self.root.join("registry.json")).map_err(|error| {
                import_app_error("The installed-glass registry could not be opened.", error)
            })?;
        Ok(registry.packages().to_vec())
    }

    pub fn remove_installed_glass(
        &self,
        content_sha256: &str,
    ) -> Result<Option<registry::InstalledGlass>, AppError> {
        let registry_path = self.root.join("registry.json");
        let mut registry = registry::PackageRegistry::open(&registry_path).map_err(|error| {
            import_app_error("The installed-glass registry could not be opened.", error)
        })?;
        registry.remove(content_sha256).map_err(|error| {
            import_app_error("The installed-glass registry could not be updated.", error)
        })
    }
}

/// Owns validation and no more than one active QEMU process.
pub struct AppService {
    runtime: RootglassRuntime,
    state_root: PathBuf,
    library_root: PathBuf,
    trust_store: TrustStore,
    next_run_id: u64,
    active: Option<ActiveRun>,
    pending: VecDeque<RuntimeEvent>,
    last_stopped: Option<RunId>,
}

impl Default for AppService {
    fn default() -> Self {
        Self::with_trust_store(TrustStore::example_fixture())
    }
}

impl AppService {
    /// Construct a service with an explicit trust policy. Production callers should load a
    /// local publisher trust configuration and pass its verifier-compatible store here.
    pub fn with_trust_store(trust_store: TrustStore) -> Self {
        Self {
            runtime: RootglassRuntime::with_trust_store(trust_store.clone()),
            state_root: default_state_root(),
            library_root: default_library_root(),
            trust_store,
            next_run_id: 1,
            active: None,
            pending: VecDeque::new(),
            last_stopped: None,
        }
    }

    /// Construct a service from a validated local production trust configuration.
    pub fn with_loaded_trust_store(loaded: LoadedTrustStore) -> Self {
        Self::with_trust_store(loaded.trust_store().clone())
    }

    /// Use integrity-checked, versioned runtime assets for packaged launches.
    pub fn with_verified_runtime_assets(
        mut self,
        resolver: RuntimeAssetsResolver,
        runtime_version: impl Into<String>,
    ) -> Self {
        self.runtime = RootglassRuntime::with_trust_store(self.trust_store.clone())
            .with_verified_runtime_assets(resolver, runtime_version);
        self
    }

    /// Load and use a local production trust configuration.
    pub fn from_trust_config(path: impl AsRef<Path>) -> Result<Self, TrustConfigError> {
        load_trust_store(path).map(Self::with_loaded_trust_store)
    }

    /// Construct a default-deny service for callers that could not load trust policy.
    pub fn deny_all() -> Self {
        Self::with_trust_store(TrustStore::default())
    }

    /// Override the per-user location used for local writable state images.
    pub fn with_state_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.state_root = root.into();
        self
    }

    /// Override the per-user package and registry location.
    pub fn with_library_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.library_root = root.into();
        self
    }

    /// Return a cloneable handle suitable for off-thread library work.
    pub fn package_library(&self) -> PackageLibrary {
        PackageLibrary {
            root: self.library_root.clone(),
            trust_store: self.trust_store.clone(),
        }
    }

    /// Verify and install a bounded ZIP `.glass` archive into the local library.
    pub fn import_archive(
        &self,
        source: impl AsRef<Path>,
    ) -> Result<registry::InstalledGlass, AppError> {
        self.package_library().import_archive(source)
    }

    /// Return the persisted verified releases in the local library.
    pub fn installed_glasses(&self) -> Result<Vec<registry::InstalledGlass>, AppError> {
        self.package_library().installed_glasses()
    }

    /// Remove a release from the visible library. Its immutable content may remain on disk
    /// until a later garbage-collection pass.
    pub fn remove_installed_glass(
        &self,
        content_sha256: &str,
    ) -> Result<Option<registry::InstalledGlass>, AppError> {
        self.package_library()
            .remove_installed_glass(content_sha256)
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn inspect(&mut self, source: &Path) -> Result<GlassSummary, AppError> {
        let launch = self.prepare(source)?;
        summarize(source, &launch)
    }

    /// Build a developer launch plan without requiring generated disk images.
    pub fn launch_plan(&mut self, source: &Path) -> Result<(GlassSummary, String), AppError> {
        let launch = self.prepare(source)?;
        let summary = summarize(source, &launch)?;
        let command = QemuLaunchPlan::for_launch_request(&launch).command_line();
        Ok((summary, command))
    }

    pub fn preflight(&mut self, source: &Path) -> Result<PreflightReport, AppError> {
        let launch = self.prepare(source)?;
        validate_assets(&launch).map_err(|detail| AppError {
            phase: LifecycleState::Ready,
            summary: "The glass runtime assets are incomplete.".to_string(),
            detail,
        })?;
        Ok(PreflightReport {
            summary: summarize(source, &launch)?,
            readiness: vec!["Manifest, signature, and required files verified.".to_string()],
        })
    }

    pub fn start(&mut self, source: &Path, options: RunOptions) -> Result<RunId, AppError> {
        if self.active.is_some() {
            return Err(AppError {
                phase: LifecycleState::Starting,
                summary: "Another glass is already running.".to_string(),
                detail: "Stop the active run before starting another.".to_string(),
            });
        }
        let mut launch = self.prepare(source)?;
        validate_assets(&launch).map_err(|detail| AppError {
            phase: LifecycleState::Ready,
            summary: "The glass runtime assets are incomplete.".to_string(),
            detail,
        })?;
        let state_lock = if let Some(template) = launch
            .storage
            .writable_overlay
            .as_ref()
            .map(|overlay| overlay.path().to_path_buf())
        {
            let selected = options.state_id.as_deref().unwrap_or("default");
            let store_root = self.state_root.join(safe_state_namespace(&launch));
            let store = state::InstanceStateStore::new(store_root, template);
            let record = match store
                .list()
                .map_err(state_app_error)?
                .into_iter()
                .find(|item| item.id == selected)
            {
                Some(record) => record,
                None if selected == "default" => store.create(selected).map_err(state_app_error)?,
                None => {
                    return Err(state_app_error(state::StateError::NotFound(
                        selected.to_string(),
                    )))
                }
            };
            let lock = store.acquire_run_lock().map_err(state_app_error)?;
            launch.writable_overlay = Some(record.image_path.clone());
            launch.storage.writable_overlay =
                Some(WritableOverlaySelection::Local(LocalWritableOverlay {
                    name: record.name,
                    path: record.image_path,
                }));
            Some(lock)
        } else {
            if options.state_id.is_some() {
                return Err(state_app_error(state::StateError::InvalidImage(
                    "this glass has no state template".into(),
                )));
            }
            None
        };
        let plan = QemuLaunchPlan::for_launch_request(&launch);
        let process = plan.spawn_supervised().map_err(|error| AppError {
            phase: LifecycleState::Starting,
            summary: "QEMU could not be started.".to_string(),
            detail: error.to_string(),
        })?;
        let id = RunId(self.next_run_id);
        self.next_run_id = self.next_run_id.wrapping_add(1).max(1);
        self.pending.push_back(RuntimeEvent::StateChanged {
            run: id.clone(),
            state: LifecycleState::Booting,
        });
        self.active = Some(ActiveRun {
            id: id.clone(),
            process,
            state: LifecycleState::Booting,
            login_seen: false,
            login_marker_tail: Vec::new(),
            diagnostic_tail: Vec::new(),
            ready_deadline: Instant::now() + GUEST_READY_TIMEOUT,
            _state_lock: state_lock,
        });
        Ok(id)
    }

    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<RuntimeEvent>, AppError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        let read_timeout = if active.login_seen {
            timeout
        } else {
            timeout.min(
                active
                    .ready_deadline
                    .saturating_duration_since(Instant::now()),
            )
        };
        if let Some(SerialChunk { bytes, truncated }) = active.process.read_serial(read_timeout) {
            let run = active.id.clone();
            let marker = b"rootglass-hello login:";
            let mut scan = Vec::with_capacity(active.login_marker_tail.len() + bytes.len());
            scan.extend_from_slice(&active.login_marker_tail);
            scan.extend_from_slice(&bytes);
            let marker_found = scan.windows(marker.len()).any(|window| window == marker);
            let tail_len = marker.len().saturating_sub(1);
            active.login_marker_tail = scan[scan.len().saturating_sub(tail_len)..].to_vec();
            let diagnostic = scan_diagnostics(&active.diagnostic_tail, &bytes);
            active.diagnostic_tail = diagnostic.tail;

            if let Some(detail) = diagnostic.failure {
                let _ = active
                    .process
                    .stop(Duration::from_secs(1), Duration::from_secs(1));
                self.active = None;
                self.pending.push_back(RuntimeEvent::Failed {
                    run: run.clone(),
                    phase: LifecycleState::Failed,
                    summary: "The guest reported a boot failure.".to_string(),
                    detail,
                });
                self.pending.push_front(RuntimeEvent::SerialChunk {
                    run,
                    bytes,
                    truncated,
                });
                return Ok(self.pending.pop_front());
            }

            if !active.login_seen && Instant::now() >= active.ready_deadline {
                let _ = active
                    .process
                    .stop(Duration::from_secs(1), Duration::from_secs(1));
                self.active = None;
                self.pending.push_back(RuntimeEvent::Failed {
                    run: run.clone(),
                    phase: LifecycleState::Failed,
                    summary: "The guest did not become ready in time.".to_string(),
                    detail: format!(
                        "No rootglass-hello login prompt arrived within {} seconds.",
                        GUEST_READY_TIMEOUT.as_secs()
                    ),
                });
                self.pending.push_front(RuntimeEvent::SerialChunk {
                    run,
                    bytes,
                    truncated,
                });
                return Ok(self.pending.pop_front());
            }

            if !active.login_seen && marker_found {
                active.login_seen = true;
                active.login_marker_tail.clear();
                active.state = LifecycleState::Running;
                self.pending.push_back(RuntimeEvent::StateChanged {
                    run: run.clone(),
                    state: LifecycleState::Running,
                });
                self.pending.push_back(RuntimeEvent::GuestReady {
                    run: run.clone(),
                    evidence: "rootglass-hello login prompt".to_string(),
                });
                self.pending.push_back(RuntimeEvent::SerialChunk {
                    run,
                    bytes,
                    truncated,
                });
                return Ok(self.pending.pop_front());
            }
            return Ok(Some(RuntimeEvent::SerialChunk {
                run,
                bytes,
                truncated,
            }));
        }
        if !active.login_seen && Instant::now() >= active.ready_deadline {
            let run = active.id.clone();
            let _ = active
                .process
                .stop(Duration::from_secs(1), Duration::from_secs(1));
            self.active = None;
            return Ok(Some(RuntimeEvent::Failed {
                run,
                phase: LifecycleState::Failed,
                summary: "The guest did not become ready in time.".to_string(),
                detail: format!(
                    "No rootglass-hello login prompt arrived within {} seconds.",
                    GUEST_READY_TIMEOUT.as_secs()
                ),
            }));
        }
        if let Some(status) = active.process.try_wait().map_err(|error| AppError {
            phase: active.state,
            summary: "QEMU status could not be read.".to_string(),
            detail: error.to_string(),
        })? {
            let id = active.id.clone();
            let ready = active.login_seen;
            self.active = None;
            if !ready {
                return Ok(Some(RuntimeEvent::Failed {
                    run: id,
                    phase: LifecycleState::Failed,
                    summary: "QEMU exited before the guest became ready.".to_string(),
                    detail: format!(
                        "QEMU exited with status {} before the login prompt appeared.",
                        status
                    ),
                }));
            }
            return Ok(Some(RuntimeEvent::Exited {
                run: id,
                exit_code: status.code(),
                expected: false,
            }));
        }
        Ok(None)
    }

    pub fn write_serial(&mut self, run: &RunId, bytes: &[u8]) -> Result<(), AppError> {
        let active = self.require_active(run)?;
        active
            .process
            .write_serial(bytes)
            .map_err(|error| AppError {
                phase: active.state,
                summary: "Guest console input failed.".to_string(),
                detail: error.to_string(),
            })
    }

    pub fn stop(&mut self, run: &RunId) -> Result<(), AppError> {
        if self.active.is_none() && self.last_stopped.as_ref() == Some(run) {
            return Ok(());
        }
        let Some(active_id) = self.active.as_ref().map(|active| active.id.clone()) else {
            return Err(AppError {
                phase: LifecycleState::Stopped,
                summary: "There is no active glass.".to_string(),
                detail: "Start a glass before requesting run control.".to_string(),
            });
        };
        if &active_id != run {
            return Err(AppError {
                phase: LifecycleState::Failed,
                summary: "The run ID does not match the active glass.".to_string(),
                detail: format!("Requested run {}, active run {}.", run.0, active_id.0),
            });
        }
        self.pending.push_back(RuntimeEvent::StateChanged {
            run: run.clone(),
            state: LifecycleState::Stopping,
        });
        self.active
            .as_mut()
            .expect("active run was validated")
            .state = LifecycleState::Stopping;
        self.active
            .as_mut()
            .expect("active run was validated")
            .process
            .stop(Duration::from_secs(5), Duration::from_secs(2))
            .map_err(|error| AppError {
                phase: LifecycleState::Stopping,
                summary: "QEMU could not be stopped cleanly.".to_string(),
                detail: error.to_string(),
            })?;
        self.active = None;
        self.last_stopped = Some(run.clone());
        self.pending.push_back(RuntimeEvent::StateChanged {
            run: run.clone(),
            state: LifecycleState::Stopped,
        });
        Ok(())
    }

    /// Preserve the existing CLI's terminal-based developer workflow.
    pub fn boot_interactive(&mut self, source: &Path) -> Result<(), AppError> {
        let launch = self.prepare(source)?;
        validate_assets(&launch).map_err(|detail| AppError {
            phase: LifecycleState::Ready,
            summary: "The glass runtime assets are incomplete.".to_string(),
            detail,
        })?;
        let mut child = QemuLaunchPlan::for_launch_request(&launch)
            .spawn_interactive()
            .map_err(|error| AppError {
                phase: LifecycleState::Starting,
                summary: "QEMU could not be started.".to_string(),
                detail: error.to_string(),
            })?;
        let status = child.wait().map_err(|error| AppError {
            phase: LifecycleState::Stopping,
            summary: "QEMU status could not be read.".to_string(),
            detail: error.to_string(),
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(AppError {
                phase: LifecycleState::Failed,
                summary: "QEMU exited unsuccessfully.".to_string(),
                detail: format!("QEMU exited with status {status}"),
            })
        }
    }

    fn prepare(&mut self, source: &Path) -> Result<LaunchRequest, AppError> {
        self.runtime
            .prepare_launch_with_host(source, &HostCapabilities::detect_current())
            .map_err(|error| {
                let user_error = self.runtime.status().last_error.as_ref();
                AppError {
                    phase: user_error
                        .map(|item| item.stage)
                        .unwrap_or(LifecycleState::Failed),
                    summary: user_error
                        .map(|item| item.summary.clone())
                        .unwrap_or_else(|| "The glass could not be prepared.".to_string()),
                    detail: user_error
                        .map(|item| item.detail.clone())
                        .unwrap_or_else(|| error.to_string()),
                }
            })
    }

    fn require_active(&mut self, run: &RunId) -> Result<&mut ActiveRun, AppError> {
        let Some(active) = self.active.as_mut() else {
            return Err(AppError {
                phase: LifecycleState::Stopped,
                summary: "There is no active glass.".to_string(),
                detail: "Start a glass before requesting run control.".to_string(),
            });
        };
        if &active.id != run {
            return Err(AppError {
                phase: active.state,
                summary: "The run identifier is stale.".to_string(),
                detail: "Use the RunId returned by the most recent start.".to_string(),
            });
        }
        Ok(active)
    }
}

fn validate_assets(request: &LaunchRequest) -> Result<(), String> {
    let assets = [
        Some(("kernel", request.kernel.as_path())),
        request.initrd.as_deref().map(|path| ("initrd", path)),
        request
            .device_tree
            .as_deref()
            .map(|path| ("device tree", path)),
        Some(("rootfs", request.rootfs.as_path())),
        request
            .writable_overlay
            .as_deref()
            .map(|path| ("writable state", path)),
    ];
    for (label, path) in assets.into_iter().flatten() {
        if !path.is_file() {
            return Err(format!("missing {label} asset: {}", path.display()));
        }
    }
    Ok(())
}

fn safe_state_namespace(request: &LaunchRequest) -> String {
    request
        .glass_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn default_state_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path).join("Rootglass").join("states");
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home)
                .join("AppData")
                .join("Local")
                .join("Rootglass")
                .join("states");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Rootglass")
                .join("states");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path.join("rootglass").join("states");
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("rootglass")
                .join("states");
        }
    }
    std::env::temp_dir().join("rootglass").join("states")
}

fn default_library_root() -> PathBuf {
    default_state_root()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("library")
}

fn import_app_error(summary: &str, error: impl std::fmt::Display) -> AppError {
    AppError {
        phase: LifecycleState::Verifying,
        summary: summary.to_string(),
        detail: error.to_string(),
    }
}

fn state_app_error(error: state::StateError) -> AppError {
    AppError {
        phase: LifecycleState::Ready,
        summary: "The selected writable state could not be prepared.".to_string(),
        detail: error.to_string(),
    }
}

fn summarize(source: &Path, launch: &LaunchRequest) -> Result<GlassSummary, AppError> {
    let glass = rootglass_manifest::Glass::load(source).map_err(|error| AppError {
        phase: LifecycleState::Validating,
        summary: "This glass is not ready to run.".to_string(),
        detail: error.to_string(),
    })?;
    Ok(GlassSummary {
        path: source.to_path_buf(),
        name: glass.manifest.name,
        identifier: glass.manifest.identifier,
        version: glass.manifest.version,
        description: glass.manifest.description.unwrap_or_default(),
        architecture: launch.architecture.clone(),
        profile: launch.runtime_profile.clone(),
        memory_mb: launch.memory_mb,
        cpus: launch.cpus,
        publisher_name: launch.signature.publisher_name.clone(),
        publisher_identity: launch.signature.publisher_identity.clone(),
        key_id: launch.signature.key_id.clone(),
        signed_subject_count: launch.signature.subject_count,
    })
}

struct DiagnosticScan {
    tail: Vec<u8>,
    failure: Option<String>,
}

fn scan_diagnostics(previous_tail: &[u8], bytes: &[u8]) -> DiagnosticScan {
    let mut scan = Vec::with_capacity(previous_tail.len() + bytes.len());
    scan.extend_from_slice(previous_tail);
    scan.extend(bytes.iter().map(u8::to_ascii_lowercase));

    let failure = scan.split(|byte| *byte == b'\n').find_map(|line| {
        if contains(line, b"kernel panic") {
            Some("Kernel panic detected in the guest console.".to_string())
        } else if contains(line, b"mounting root: failed")
            || contains(line, b"failed to mount root")
            || (contains(line, b"mount: mounting ") && contains(line, b" failed"))
        {
            Some("The guest could not mount its root filesystem.".to_string())
        } else if contains(line, b"initramfs emergency")
            || contains(line, b"emergency recovery shell")
        {
            Some("The guest entered its initramfs emergency shell.".to_string())
        } else {
            None
        }
    });
    let keep = scan.len().min(ERROR_SCAN_TAIL_BYTES);
    let tail = scan[scan.len() - keep..].to_vec();
    DiagnosticScan { tail, failure }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod library_tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::fs::{self, File};
    use std::io;
    use std::time::{SystemTime, UNIX_EPOCH};
    use zip::write::SimpleFileOptions;

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rootglass-library-{}-{nonce}", std::process::id()))
    }

    fn add_directory(
        source_root: &Path,
        directory: &Path,
        writer: &mut zip::ZipWriter<File>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(source_root).expect("child path");
            let name = relative.to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                writer.add_directory(format!("{name}/"), SimpleFileOptions::default())?;
                add_directory(source_root, &path, writer)?;
            } else {
                writer.start_file(name, SimpleFileOptions::default())?;
                let mut file = File::open(path)?;
                io::copy(&mut file, writer)?;
            }
        }
        Ok(())
    }

    fn signed_launch_ready_fixture(root: &Path) {
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        fs::create_dir_all(root.join("metadata")).unwrap();
        fs::create_dir_all(root.join("signatures")).unwrap();
        for name in ["README.md", "metadata/README.md", "rootglass.toml"] {
            fs::copy(example.join(name), root.join(name)).unwrap();
        }
        fs::write(root.join("rootfs.img"), b"synthetic test root filesystem").unwrap();
        fs::write(root.join("state.img"), b"synthetic test state image").unwrap();

        let subjects = [
            "README.md",
            "metadata/README.md",
            "rootfs.img",
            "rootglass.toml",
            "state.img",
        ]
        .map(|name| {
            let digest = Sha256::digest(fs::read(root.join(name)).unwrap());
            let hex_digest = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            json!({"name": name, "digest": {"sha256": hex_digest}})
        });
        let statement = json!({
            "_type": "https://in-toto.io/Statement/v1",
            "predicate": {
                "createdAt": "2026-11-07T00:00:00Z",
                "glassIdentifier": "io.rootglass.examples.hello-world",
                "glassVersion": "0.1.0",
                "publisher": {
                    "identity": "rootglass.dev/examples",
                    "keyId": "ed25519:rootglass-example-fixture",
                    "name": "Rootglass Examples"
                },
                "schemaVersion": 1
            },
            "predicateType": "https://rootglass.dev/predicate/glass/v1",
            "subject": subjects
        });
        let payload = serde_json::to_vec(&statement).unwrap();
        let pae = dsse_pae(b"application/vnd.in-toto+json", &payload);
        // This is the public deterministic sample key documented by the fixture signer.
        let seed: [u8; 32] =
            Sha256::digest(b"Rootglass public development fixture signing key v1").into();
        let signature = SigningKey::from_bytes(&seed).sign(&pae);
        let envelope = json!({
            "payloadType": "application/vnd.in-toto+json",
            "payload": URL_SAFE_NO_PAD.encode(payload),
            "signatures": [{
                "keyid": "ed25519:rootglass-example-fixture",
                "sig": URL_SAFE_NO_PAD.encode(signature.to_bytes())
            }]
        });
        fs::write(
            root.join("signatures/glass.dsse.json"),
            serde_json::to_vec_pretty(&envelope).unwrap(),
        )
        .unwrap();
    }

    fn dsse_pae(payload_type: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = b"DSSEv1 ".to_vec();
        out.extend_from_slice(payload_type.len().to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(payload_type);
        out.push(b' ');
        out.extend_from_slice(payload.len().to_string().as_bytes());
        out.push(b' ');
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn imports_signed_zip_and_persists_library_rows() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let archive = root.join("hello-world.glass");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let source = root.join("source.glass");
        signed_launch_ready_fixture(&source);
        add_directory(&source, &source, &mut writer).unwrap();
        writer.finish().unwrap();

        let library = root.join("library");
        let service =
            AppService::with_trust_store(TrustStore::example_fixture()).with_library_root(&library);
        let installed = service.import_archive(&archive).unwrap();
        assert_eq!(installed.identifier, "io.rootglass.examples.hello-world");
        assert_eq!(
            service.installed_glasses().unwrap(),
            vec![installed.clone()]
        );

        let reopened =
            AppService::with_trust_store(TrustStore::example_fixture()).with_library_root(&library);
        assert_eq!(
            reopened.installed_glasses().unwrap(),
            vec![installed.clone()]
        );
        reopened
            .remove_installed_glass(&installed.content_sha256)
            .unwrap();
        assert!(reopened.installed_glasses().unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod verified_runtime_launch_tests {
    use super::*;
    use rootglass_signature::TrustStore;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rootglass-verified-runtime-{}-{nonce}", std::process::id()))
    }

    fn digest(path: &Path) -> String {
        Sha256::digest(fs::read(path).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn install_test_runtime(root: &Path) -> PathBuf {
        let version_root = root.join("runtime-assets/test-1");
        fs::create_dir_all(&version_root).unwrap();
        for (name, bytes) in [
            ("kernel", b"verified test kernel".as_slice()),
            ("initramfs", b"verified test initramfs".as_slice()),
            ("qemu", b"verified test qemu".as_slice()),
        ] {
            fs::write(version_root.join(name), bytes).unwrap();
        }
        let manifest = json!({
            "schema_version": 1,
            "runtime_version": "test-1",
            "assets": [
                {"kind": "kernel", "path": "kernel", "sha256": digest(&version_root.join("kernel"))},
                {"kind": "initramfs", "path": "initramfs", "sha256": digest(&version_root.join("initramfs"))},
                {"kind": "qemu", "path": "qemu", "sha256": digest(&version_root.join("qemu"))}
            ]
        });
        fs::write(
            version_root.join("assets.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        version_root
    }

    fn service_with_assets(root: &Path) -> AppService {
        AppService::with_trust_store(TrustStore::example_fixture()).with_verified_runtime_assets(
            RuntimeAssetsResolver::new(root, root.join("user-data")),
            "test-1",
        )
    }

    #[test]
    fn app_launch_plan_uses_integrity_checked_assets_and_qemu() {
        let root = temp_root();
        let version_root = install_test_runtime(&root);
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");

        let (_summary, command) = service_with_assets(&root)
            .launch_plan(&source)
            .expect("verified app launch plan should be prepared");

        assert!(command.starts_with(&version_root.join("qemu").display().to_string()));
        assert!(command.contains(&format!("-kernel {}", version_root.join("kernel").display())));
        assert!(command.contains(&format!("-initrd {}", version_root.join("initramfs").display())));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_launch_plan_rejects_tampered_runtime_asset_before_spawn() {
        let root = temp_root();
        let version_root = install_test_runtime(&root);
        fs::write(version_root.join("kernel"), b"tampered kernel").unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");

        let error = service_with_assets(&root)
            .launch_plan(&source)
            .expect_err("tampered runtime must not produce a launch plan");

        assert!(error.detail.contains("runtime asset integrity check failed"));
        let _ = fs::remove_dir_all(root);
    }
}
