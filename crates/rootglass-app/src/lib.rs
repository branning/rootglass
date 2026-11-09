//! UI-independent application orchestration shared by the CLI and desktop.
pub mod import;
pub mod registry;
pub mod state;

use rootglass_core::runtime_assets::RuntimeAssetsResolver;
use rootglass_core::storage::{LocalWritableOverlay, WritableOverlaySelection};
use rootglass_core::{
    CoreError, HostCapabilities, LaunchRequest, LifecycleState, RootglassRuntime,
};
use rootglass_qemu::{QemuLaunchPlan, QemuProcess, SerialChunk};
use rootglass_signature::trust_config::{load_trust_store, LoadedTrustStore, TrustConfigError};
use rootglass_signature::{SignatureError, TrustStore};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
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
    pub acceleration: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub publisher_name: String,
    pub publisher_identity: String,
    pub key_id: String,
    pub signed_subject_count: usize,
}

/// Manifest-only validation output. Publisher fields are declarations from the manifest;
/// this report does not verify a signature or establish trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestValidationReport {
    pub path: PathBuf,
    pub name: String,
    pub identifier: String,
    pub version: String,
    pub description: Option<String>,
    pub declared_publisher_name: String,
    pub declared_publisher_identity: String,
    pub architecture: String,
    pub profile: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub rootfs: PathBuf,
    pub network_mode: String,
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
pub struct TrustedPublisherStatus {
    pub name: String,
    pub identity: String,
    pub key_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustPolicyStatus {
    pub config_path: Option<PathBuf>,
    pub trusted_publishers: Vec<TrustedPublisherStatus>,
    pub revoked_key_ids: Vec<String>,
    pub error: Option<String>,
}

/// One consistent snapshot of the selected bundle's writable states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateStoreSnapshot {
    pub records: Vec<state::StateRecord>,
    pub locked: bool,
    pub selected_state_id: String,
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
    revoked_key_ids: BTreeSet<String>,
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
        .map_err(|error| match &error {
            import::ImportError::Signature(SignatureError::UnknownKey(key_id))
                if self.revoked_key_ids.contains(key_id) =>
            {
                revoked_key_app_error(key_id)
            }
            _ => import_app_error(
                "The .glass archive could not be verified or installed.",
                error,
            ),
        })
    }

    pub fn installed_glasses(&self) -> Result<Vec<registry::InstalledGlass>, AppError> {
        let registry =
            registry::PackageRegistry::open(self.root.join("registry.json")).map_err(|error| {
                import_app_error("The installed-glass registry could not be opened.", error)
            })?;
        let packages = registry.packages().to_vec();
        if packages.is_empty() {
            return Ok(packages);
        }
        let packages_root = self.root.join("packages");
        let packages_metadata = fs::symlink_metadata(&packages_root).map_err(|error| AppError {
            phase: LifecycleState::Verifying,
            summary: "The managed package store could not be opened.".to_string(),
            detail: error.to_string(),
        })?;
        if packages_metadata.file_type().is_symlink() || !packages_metadata.is_dir() {
            return Err(AppError {
                phase: LifecycleState::Verifying,
                summary: "The managed package store is not a regular directory.".to_string(),
                detail: packages_root.display().to_string(),
            });
        }
        for package in &packages {
            let expected = self.root.join("packages").join(&package.content_sha256);
            if package.package_path != expected {
                return Err(AppError {
                    phase: LifecycleState::Verifying,
                    summary: "The installed-glass registry contains an invalid package path."
                        .to_string(),
                    detail: format!(
                        "The registry path '{}' does not match the managed package store.",
                        package.package_path.display()
                    ),
                });
            }
            let metadata = fs::symlink_metadata(&expected).map_err(|error| AppError {
                phase: LifecycleState::Verifying,
                summary: "An installed package is missing from the managed store.".to_string(),
                detail: error.to_string(),
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(AppError {
                    phase: LifecycleState::Verifying,
                    summary: "An installed package is not a regular directory.".to_string(),
                    detail: expected.display().to_string(),
                });
            }
        }
        Ok(packages)
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
    revoked_key_ids: BTreeSet<String>,
    trust_config_path: Option<PathBuf>,
    trust_load_error: Option<String>,
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
            revoked_key_ids: BTreeSet::new(),
            trust_config_path: None,
            trust_load_error: None,
            next_run_id: 1,
            active: None,
            pending: VecDeque::new(),
            last_stopped: None,
        }
    }

    /// Construct a service from a validated local production trust configuration.
    pub fn with_loaded_trust_store(loaded: LoadedTrustStore) -> Self {
        let revoked_key_ids = loaded.revoked_key_ids().map(str::to_owned).collect();
        let mut service = Self::with_trust_store(loaded.trust_store().clone());
        service.revoked_key_ids = revoked_key_ids;
        service
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
        let path = path.as_ref().to_path_buf();
        load_trust_store(&path)
            .map(Self::with_loaded_trust_store)
            .map(|service| service.with_trust_config_path(path))
    }

    /// Construct a default-deny service for callers that could not load trust policy.
    pub fn deny_all() -> Self {
        Self::with_trust_store(TrustStore::default())
    }

    /// Attach the source and error shown when a local trust policy could not be loaded.
    pub fn with_trust_load_error(
        mut self,
        path: Option<PathBuf>,
        error: impl Into<String>,
    ) -> Self {
        self.trust_config_path = path;
        self.trust_load_error = Some(error.into());
        self
    }

    fn with_trust_config_path(mut self, path: PathBuf) -> Self {
        self.trust_config_path = Some(path);
        self.trust_load_error = None;
        self
    }

    /// Return the active local trust policy without exposing public-key material.
    pub fn trust_policy_status(&self) -> TrustPolicyStatus {
        TrustPolicyStatus {
            config_path: self.trust_config_path.clone(),
            trusted_publishers: self
                .trust_store
                .publishers()
                .map(|publisher| TrustedPublisherStatus {
                    name: publisher.name.clone(),
                    identity: publisher.identity.clone(),
                    key_id: publisher.key_id.clone(),
                })
                .collect(),
            revoked_key_ids: self.revoked_key_ids.iter().cloned().collect(),
            error: self.trust_load_error.clone(),
        }
    }

    /// List named writable states for a verified glass bundle.
    pub fn list_states(&mut self, source: &Path) -> Result<Vec<state::StateRecord>, AppError> {
        self.state_store_for(source)?
            .list()
            .map_err(state_app_error)
    }

    /// Read records, lock status, and selected state after preparing the bundle once.
    pub fn state_snapshot(&mut self, source: &Path) -> Result<StateStoreSnapshot, AppError> {
        let store = self.state_store_for(source)?;
        let records = store.list().map_err(state_app_error)?;
        let locked = store.is_in_use();
        let selected_state_id = store.selected_state_id().map_err(state_app_error)?;
        Ok(StateStoreSnapshot {
            records,
            locked,
            selected_state_id,
        })
    }

    /// Advisory lock status for disabling TUI state actions while a guest owns the store.
    /// Each mutating store call still reacquires the lock to handle races safely.
    pub fn states_in_use(&mut self, source: &Path) -> Result<bool, AppError> {
        Ok(self.state_store_for(source)?.is_in_use())
    }

    /// Return the state selected for this glass, defaulting to `default`.
    pub fn selected_state_id(&mut self, source: &Path) -> Result<String, AppError> {
        self.state_store_for(source)?
            .selected_state_id()
            .map_err(state_app_error)
    }

    /// Persist the state selected for future boots and restarts.
    pub fn select_state(&mut self, source: &Path, id: &str) -> Result<String, AppError> {
        self.state_store_for(source)?
            .select(id)
            .map_err(state_app_error)
    }

    /// Create a named copy of the immutable state template.
    pub fn create_state(
        &mut self,
        source: &Path,
        name: &str,
    ) -> Result<state::StateRecord, AppError> {
        self.state_store_for(source)?
            .create(name)
            .map_err(state_app_error)
    }

    /// Clone an existing named state into an independent image.
    pub fn clone_state(
        &mut self,
        source: &Path,
        source_id: &str,
        new_name: &str,
    ) -> Result<state::StateRecord, AppError> {
        self.state_store_for(source)?
            .clone_state(source_id, new_name)
            .map_err(state_app_error)
    }

    /// Replace a named state with the immutable template.
    pub fn reset_state(&mut self, source: &Path, id: &str) -> Result<state::StateRecord, AppError> {
        self.state_store_for(source)?
            .reset(id)
            .map_err(state_app_error)
    }

    /// Delete a named state image.
    pub fn delete_state(&mut self, source: &Path, id: &str) -> Result<(), AppError> {
        self.state_store_for(source)?
            .delete(id)
            .map_err(state_app_error)
    }

    fn state_store_for(&mut self, source: &Path) -> Result<state::InstanceStateStore, AppError> {
        let launch = self.prepare(source)?;
        let template = launch
            .storage
            .writable_overlay
            .as_ref()
            .map(|overlay| overlay.path().to_path_buf())
            .ok_or_else(|| {
                state_app_error(state::StateError::InvalidImage(
                    "this glass has no writable-state template".into(),
                ))
            })?;
        let store_root = self.state_root.join(safe_state_namespace(&launch));
        Ok(state::InstanceStateStore::new(store_root, template))
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
            revoked_key_ids: self.revoked_key_ids.clone(),
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

    /// Validate only the manifest schema and declared fields, matching the CLI `validate`
    /// command. This deliberately does not verify the bundle signature or publisher trust.
    pub fn validate_manifest_only(
        &self,
        source: &Path,
    ) -> Result<ManifestValidationReport, AppError> {
        let glass = rootglass_manifest::Glass::load(source).map_err(|error| AppError {
            phase: LifecycleState::Validating,
            summary: "The manifest could not be loaded.".to_string(),
            detail: error.to_string(),
        })?;
        glass.validate_manifest_only().map_err(|error| AppError {
            phase: LifecycleState::Validating,
            summary: "The manifest is invalid.".to_string(),
            detail: error.to_string(),
        })?;
        let manifest = glass.manifest;
        Ok(ManifestValidationReport {
            path: glass.root,
            name: manifest.name,
            identifier: manifest.identifier,
            version: manifest.version,
            description: manifest.description,
            declared_publisher_name: manifest.publisher.name,
            declared_publisher_identity: manifest.publisher.identity,
            architecture: manifest.runtime.architecture,
            profile: manifest.runtime.profile,
            memory_mb: manifest.resources.memory_mb,
            cpus: manifest.resources.cpus,
            rootfs: PathBuf::from(manifest.boot.rootfs),
            network_mode: manifest.network.mode,
        })
    }

    pub fn inspect(&mut self, source: &Path) -> Result<GlassSummary, AppError> {
        let launch = self.prepare(source)?;
        summarize(source, &launch)
    }

    /// Build a developer launch plan without requiring generated disk images.
    pub fn launch_plan(&mut self, source: &Path) -> Result<(GlassSummary, String), AppError> {
        let mut launch = self.prepare(source)?;
        let summary = summarize(source, &launch)?;
        if let Some(template) = launch
            .storage
            .writable_overlay
            .as_ref()
            .map(|overlay| overlay.path().to_path_buf())
        {
            let store_root = self.state_root.join(safe_state_namespace(&launch));
            let store = state::InstanceStateStore::new(store_root, template);
            let path = store.path_for("default").map_err(state_app_error)?;
            launch.writable_overlay = Some(path.clone());
            launch.storage.writable_overlay =
                Some(WritableOverlaySelection::Local(LocalWritableOverlay {
                    name: "default".to_string(),
                    path,
                }));
        }
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
        let (launch, state_lock) = self.prepare_for_run(source, options)?;
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

    fn prepare_for_run(
        &mut self,
        source: &Path,
        options: RunOptions,
    ) -> Result<(LaunchRequest, Option<state::StateRunLock>), AppError> {
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
            let store_root = self.state_root.join(safe_state_namespace(&launch));
            let store = state::InstanceStateStore::new(store_root, template);
            let (record, lock) = match options.state_id.as_deref() {
                Some(selected) => store.select_for_run(selected),
                None => store.select_current_for_run(),
            }
            .map_err(state_app_error)?;
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
        Ok((launch, state_lock))
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
        let (launch, _state_lock) = self.prepare_for_run(source, RunOptions::default())?;
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
                if let CoreError::Signature(SignatureError::UnknownKey(key_id)) = &error {
                    if self.revoked_key_ids.contains(key_id) {
                        return revoked_key_app_error(key_id);
                    }
                }
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
        if let Some(error) = active_run_error(
            self.active
                .as_ref()
                .map(|active| (&active.id, active.state)),
            run,
        ) {
            return Err(error);
        }
        Ok(self.active.as_mut().expect("active run was validated"))
    }
}

fn active_run_error(
    active: Option<(&RunId, LifecycleState)>,
    requested: &RunId,
) -> Option<AppError> {
    match active {
        None => Some(AppError {
            phase: LifecycleState::Stopped,
            summary: "There is no active glass.".to_string(),
            detail: "Start a glass before requesting run control.".to_string(),
        }),
        Some((active_id, state)) if active_id != requested => Some(AppError {
            phase: state,
            summary: "The run identifier is stale.".to_string(),
            detail: "Use the RunId returned by the most recent start.".to_string(),
        }),
        Some(_) => None,
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

fn revoked_key_app_error(key_id: &str) -> AppError {
    AppError {
        phase: LifecycleState::Verifying,
        summary: "This publisher's signing key has been revoked.".to_string(),
        detail: format!(
            "Signing key '{key_id}' is listed in the local trust policy's revokedKeyIds; this glass cannot be opened or run."
        ),
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
        acceleration: launch
            .accelerator
            .map(|accelerator| accelerator.qemu_name().to_ascii_uppercase())
            .unwrap_or_else(|| "TCG (software emulation)".to_string()),
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
        } else if contains(line, b"mount: mounting rootglass-state on /state failed") {
            Some("The guest could not mount persistent state at /state.".to_string())
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
mod diagnostics_tests {
    use super::*;

    #[test]
    fn guest_panic_emergency_and_mount_errors_are_actionable_across_chunks() {
        let prefix = scan_diagnostics(&[], b"Kernel pa");
        let panic = scan_diagnostics(&prefix.tail, b"nic - not syncing: fatal exception\n");
        assert_eq!(
            panic.failure.as_deref(),
            Some("Kernel panic detected in the guest console.")
        );

        let emergency = scan_diagnostics(&[], b"Entering initramfs emergency recovery shell\n");
        assert_eq!(
            emergency.failure.as_deref(),
            Some("The guest entered its initramfs emergency shell.")
        );

        let root_mount = scan_diagnostics(&[], b"Mounting root: failed to mount root\n");
        assert_eq!(
            root_mount.failure.as_deref(),
            Some("The guest could not mount its root filesystem.")
        );

        let state_mount =
            scan_diagnostics(&[], b"mount: mounting rootglass-state on /state failed\n");
        assert_eq!(
            state_mount.failure.as_deref(),
            Some("The guest could not mount persistent state at /state.")
        );
    }

    #[test]
    fn diagnostic_tail_is_bounded() {
        let output = vec![b'x'; ERROR_SCAN_TAIL_BYTES * 4];
        let scan = scan_diagnostics(&[], &output);
        assert_eq!(scan.tail.len(), ERROR_SCAN_TAIL_BYTES);
    }

    #[test]
    fn stale_run_ids_are_rejected_before_guest_io() {
        let active = RunId(2);
        let error = active_run_error(Some((&active, LifecycleState::Running)), &RunId(1))
            .expect("a stale run id must fail");
        assert_eq!(error.summary, "The run identifier is stale.");
        assert_eq!(error.phase, LifecycleState::Running);
        assert!(active_run_error(Some((&active, LifecycleState::Running)), &active).is_none());
    }
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
        signed_launch_ready_fixture_version(root, "0.1.0");
    }

    fn signed_launch_ready_fixture_version(root: &Path, version: &str) {
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        fs::create_dir_all(root.join("metadata")).unwrap();
        fs::create_dir_all(root.join("signatures")).unwrap();
        for name in ["README.md", "metadata/README.md", "rootglass.toml"] {
            fs::copy(example.join(name), root.join(name)).unwrap();
        }
        let manifest_path = root.join("rootglass.toml");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        fs::write(
            &manifest_path,
            manifest.replace("version = \"0.1.0\"", &format!("version = \"{version}\"")),
        )
        .unwrap();
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
                "glassVersion": version,
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

    fn create_archive(source: &Path, archive: &Path) {
        let file = File::create(archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        add_directory(source, source, &mut writer).unwrap();
        writer.finish().unwrap();
    }

    fn make_tree_writable(root: &Path) {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                    make_tree_writable(&path);
                }
                if let Ok(metadata) = fs::symlink_metadata(&path) {
                    make_path_writable(&path, metadata);
                }
            }
        }
        if let Ok(metadata) = fs::metadata(root) {
            make_path_writable(root, metadata);
        }
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&source_path, &destination_path);
            } else {
                fs::copy(&source_path, &destination_path).unwrap();
            }
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
        let source = root.join("source.glass");
        signed_launch_ready_fixture(&source);
        create_archive(&source, &archive);

        let library = root.join("library");
        let service =
            AppService::with_trust_store(TrustStore::example_fixture()).with_library_root(&library);
        let installed = service.import_archive(&archive).unwrap();
        assert_eq!(installed.identifier, "io.rootglass.examples.hello-world");
        let reimported = service.import_archive(&archive).unwrap();
        assert_eq!(reimported.content_sha256, installed.content_sha256);
        assert_eq!(
            service.installed_glasses().unwrap(),
            vec![reimported.clone()]
        );

        let source_v2 = root.join("source-v2.glass");
        let archive_v2 = root.join("hello-world-v2.glass");
        signed_launch_ready_fixture_version(&source_v2, "0.2.0");
        create_archive(&source_v2, &archive_v2);
        let installed_v2 = service.import_archive(&archive_v2).unwrap();
        assert_ne!(installed_v2.content_sha256, installed.content_sha256);
        assert_ne!(installed_v2.package_path, installed.package_path);
        assert_eq!(service.installed_glasses().unwrap().len(), 2);

        let reopened =
            AppService::with_trust_store(TrustStore::example_fixture()).with_library_root(&library);
        assert_eq!(reopened.installed_glasses().unwrap().len(), 2);

        let tampered = reimported.package_path.join("README.md");
        make_path_writable(&tampered, fs::metadata(&tampered).unwrap());
        fs::write(&tampered, b"tampered installed package").unwrap();
        let error = service
            .import_archive(&archive)
            .expect_err("reimport must revalidate an existing content-addressed package");
        assert!(error.detail.contains("subject digest mismatch"));
        assert_eq!(service.installed_glasses().unwrap().len(), 2);

        make_tree_writable(&reimported.package_path);
        fs::remove_dir_all(&reimported.package_path).unwrap();
        copy_tree(&installed_v2.package_path, &reimported.package_path);
        let error = service
            .import_archive(&archive)
            .expect_err("a different valid package must not be reused at this digest");
        assert!(error.detail.contains("archive changed"));

        reopened
            .remove_installed_glass(&installed.content_sha256)
            .unwrap();
        assert_eq!(reopened.installed_glasses().unwrap().len(), 1);
        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn import_recovers_when_registry_commit_fails_after_package_publication() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.glass");
        let archive = root.join("hello-world.glass");
        signed_launch_ready_fixture(&source);
        create_archive(&source, &archive);

        let registry_home = root.join("registry-home");
        fs::create_dir_all(&registry_home).unwrap();
        let registry_path = registry_home.join("registry.json");
        let mut registry = crate::registry::PackageRegistry::open(&registry_path).unwrap();
        fs::remove_dir_all(&registry_home).unwrap();
        fs::write(&registry_home, b"block registry parent").unwrap();
        let store = root.join("store");
        let failure = crate::import::import_glass_archive(
            &archive,
            &store,
            &mut registry,
            &TrustStore::example_fixture(),
            &crate::import::ImportLimits::default(),
        )
        .expect_err("registry write should fail after package publication");
        assert!(matches!(failure, crate::import::ImportError::Registry(_)));
        assert!(registry.packages().is_empty());
        assert_eq!(fs::read_dir(store.join("packages")).unwrap().count(), 1);

        fs::remove_file(&registry_home).unwrap();
        fs::create_dir_all(&registry_home).unwrap();
        let mut recovered = crate::registry::PackageRegistry::open(&registry_path).unwrap();
        let installed = crate::import::import_glass_archive(
            &archive,
            &store,
            &mut recovered,
            &TrustStore::example_fixture(),
            &crate::import::ImportLimits::default(),
        )
        .expect("reimport should verify and reuse the orphaned immutable package");
        assert_eq!(recovered.packages(), &[installed]);
        make_tree_writable(&root);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod publisher_trust_tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use rootglass_core::runtime_assets::RuntimeAssetsResolver;
    use rootglass_signature::sign_glass_directory_at;
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    const PUBLISHER_IDENTITY: &str = "rootglass.dev/examples";
    const TEST_RUNTIME: &str = "trust-policy-test";

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rootglass-publisher-trust-{}-{nonce}",
            std::process::id()
        ))
    }

    fn key_id(key: &SigningKey) -> String {
        let digest = Sha256::digest(key.verifying_key().to_bytes());
        format!(
            "ed25519:{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }

    fn signed_glass(root: &Path, key: &SigningKey) {
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        fs::create_dir_all(root.join("metadata")).unwrap();
        for name in ["README.md", "metadata/README.md", "rootglass.toml"] {
            fs::copy(example.join(name), root.join(name)).unwrap();
        }
        fs::write(
            root.join("rootfs.img"),
            b"synthetic signed test root filesystem",
        )
        .unwrap();
        fs::write(root.join("state.img"), b"synthetic signed test state image").unwrap();
        sign_glass_directory_at(root, key, "2026-11-07T00:00:00Z").unwrap();
    }

    fn write_trust_config(path: &Path, trusted: &[&SigningKey], revoked: &[String]) {
        let publishers = trusted
            .iter()
            .map(|key| {
                json!({
                    "name": "Rootglass Example Publisher",
                    "identity": PUBLISHER_IDENTITY,
                    "keyId": key_id(key),
                    "publicKeyBase64url": URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
                })
            })
            .collect::<Vec<Value>>();
        fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "trustedPublishers": publishers,
                "revokedKeyIds": revoked
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn trust_policy_status_lists_trusted_revoked_and_load_error_without_key_material() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let trusted = SigningKey::from_bytes(&[31; 32]);
        let revoked = SigningKey::from_bytes(&[32; 32]);
        let config = root.join("trust.json");
        write_trust_config(&config, &[&trusted], &[key_id(&revoked)]);

        let status = AppService::from_trust_config(&config)
            .expect("trust configuration loads")
            .trust_policy_status();
        assert_eq!(status.config_path.as_deref(), Some(config.as_path()));
        assert_eq!(status.trusted_publishers.len(), 1);
        assert_eq!(status.trusted_publishers[0].key_id, key_id(&trusted));
        assert_eq!(status.revoked_key_ids, [key_id(&revoked)]);
        assert!(status.error.is_none());

        let failed = AppService::deny_all()
            .with_trust_load_error(Some(config.clone()), "unsupported schema")
            .trust_policy_status();
        assert_eq!(failed.config_path, Some(config));
        assert_eq!(failed.error.as_deref(), Some("unsupported schema"));
        assert!(failed.trusted_publishers.is_empty());
        assert!(failed.revoked_key_ids.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn app_state_api_supports_named_operations_and_reports_guest_lock_conflicts() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let key = SigningKey::from_bytes(&[41; 32]);
        let glass = root.join("hello-world.glass");
        fs::create_dir_all(&glass).unwrap();
        signed_glass(&glass, &key);
        let config = root.join("trust.json");
        write_trust_config(&config, &[&key], &[]);
        let mut service = AppService::from_trust_config(&config)
            .expect("valid trust configuration")
            .with_state_root(root.join("states"));

        assert!(service.list_states(&glass).unwrap().is_empty());
        let created = service.create_state(&glass, "work").unwrap();
        let cloned = service.clone_state(&glass, "work", "copy").unwrap();
        assert_eq!(
            fs::read(&created.image_path).unwrap(),
            fs::read(&cloned.image_path).unwrap()
        );
        assert_eq!(service.select_state(&glass, "copy").unwrap(), "copy");
        assert_eq!(service.selected_state_id(&glass).unwrap(), "copy");
        service.reset_state(&glass, "work").unwrap();
        service.delete_state(&glass, "copy").unwrap();
        assert_eq!(service.selected_state_id(&glass).unwrap(), "default");
        assert_eq!(
            service
                .list_states(&glass)
                .unwrap()
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["work"]
        );

        let state_root = created.image_path.parent().unwrap();
        let store = state::InstanceStateStore::new(state_root, glass.join("state.img"));
        let lock = store.acquire_run_lock().unwrap();
        assert!(service.states_in_use(&glass).unwrap());
        let select_error = service
            .select_state(&glass, "work")
            .expect_err("state selection is denied while a guest owns the lock");
        assert!(select_error.detail.contains("currently in use"));
        let error = service
            .reset_state(&glass, "work")
            .expect_err("a running guest owns the state lock");
        assert!(error.detail.contains("currently in use"));
        drop(lock);
        assert!(!service.states_in_use(&glass).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    fn install_test_runtime(root: &Path) -> PathBuf {
        let resource_root = root.join("resources");
        let version_root = resource_root.join("runtime-assets").join(TEST_RUNTIME);
        fs::create_dir_all(&version_root).unwrap();
        for (name, contents) in [
            ("kernel", b"verified test kernel".as_slice()),
            ("initramfs", b"verified test initramfs".as_slice()),
            ("qemu", b"verified test qemu".as_slice()),
        ] {
            fs::write(version_root.join(name), contents).unwrap();
        }
        let digest = |name: &str| {
            Sha256::digest(fs::read(version_root.join(name)).unwrap())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        fs::write(
            version_root.join("assets.json"),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "runtime_version": TEST_RUNTIME,
                "assets": [
                    {"kind": "kernel", "path": "kernel", "sha256": digest("kernel")},
                    {"kind": "initramfs", "path": "initramfs", "sha256": digest("initramfs")},
                    {"kind": "qemu", "path": "qemu", "sha256": digest("qemu")}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        resource_root
    }

    fn production_service(config: &Path, root: &Path) -> AppService {
        AppService::from_trust_config(config)
            .expect("valid offline trust configuration")
            .with_verified_runtime_assets(
                RuntimeAssetsResolver::new(root.join("resources"), root.join("user-data")),
                TEST_RUNTIME,
            )
    }

    fn is_unknown_signing_key(error: &AppError) -> bool {
        error.phase == LifecycleState::Verifying && error.detail.contains("unknown signing key")
    }

    fn is_revoked_signing_key(error: &AppError) -> bool {
        error.phase == LifecycleState::Verifying
            && error.summary.contains("has been revoked")
            && error.detail.contains("revokedKeyIds")
    }

    #[test]
    fn offline_trust_rotation_reaches_preview_preflight_and_start_before_spawn() {
        // These deterministic seeds exist only in this test module; no key material enters app UI.
        let key_a = SigningKey::from_bytes(&[17_u8; 32]);
        let key_b = SigningKey::from_bytes(&[29_u8; 32]);
        let key_a_id = key_id(&key_a);
        let root = temp_root();
        let glass_a = root.join("publisher-a.glass");
        let glass_b = root.join("publisher-b.glass");
        fs::create_dir_all(&root).unwrap();
        signed_glass(&glass_a, &key_a);
        signed_glass(&glass_b, &key_b);
        let assets = install_test_runtime(&root);
        let config = root.join("trust.json");

        write_trust_config(&config, &[&key_a], &[]);
        let mut before_rotation = production_service(&config, &root);
        let preview = before_rotation
            .inspect(&glass_a)
            .expect("explicitly trusted publisher reaches preview");
        assert_eq!(preview.publisher_identity, PUBLISHER_IDENTITY);
        before_rotation
            .launch_plan(&glass_a)
            .expect("explicitly trusted publisher reaches pre-run planning");
        before_rotation
            .preflight(&glass_a)
            .expect("explicitly trusted publisher passes preflight verification");

        // Trust is a startup snapshot. Replacing the file does not mutate an already-running
        // service; the desktop reloads policy by restarting.
        write_trust_config(&config, &[&key_b], &[]);
        before_rotation
            .launch_plan(&glass_a)
            .expect("existing service retains its startup trust snapshot");

        let mut unknown = production_service(&config, &root);
        let unknown_error = unknown
            .inspect(&glass_a)
            .expect_err("a key absent from the new file is unknown");
        assert!(is_unknown_signing_key(&unknown_error));
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        let fixture_error = unknown
            .inspect(&example)
            .expect_err("production trust does not merge the compiled example key");
        assert!(is_unknown_signing_key(&fixture_error));

        write_trust_config(&config, &[&key_b], &[key_a_id]);
        let mut after_rotation = production_service(&config, &root);
        let revoked_error = after_rotation
            .inspect(&glass_a)
            .expect_err("the old key is revoked after rotation");
        assert!(is_revoked_signing_key(&revoked_error));
        let preflight_error = after_rotation
            .preflight(&glass_a)
            .expect_err("revoked bundle must fail before runtime preflight");
        assert!(is_revoked_signing_key(&preflight_error));
        let start_error = after_rotation
            .start(&glass_a, RunOptions::default())
            .expect_err("revoked bundle must fail before QEMU spawn");
        assert!(is_revoked_signing_key(&start_error));

        let rotated_preview = after_rotation
            .inspect(&glass_b)
            .expect("newly trusted key reaches preview");
        assert_eq!(rotated_preview.publisher_identity, PUBLISHER_IDENTITY);
        after_rotation
            .launch_plan(&glass_b)
            .expect("newly trusted key reaches pre-run planning");
        after_rotation
            .preflight(&glass_b)
            .expect("newly trusted key passes preflight verification");

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(assets);
    }

    #[test]
    fn missing_or_invalid_production_trust_never_falls_back_to_fixture_keys() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let missing = root.join("missing-trust.json");
        assert!(AppService::from_trust_config(&missing).is_err());

        let invalid = root.join("invalid-trust.json");
        fs::write(
            &invalid,
            br#"{"schemaVersion":99,"trustedPublishers":[],"revokedKeyIds":[]}"#,
        )
        .unwrap();
        assert!(AppService::from_trust_config(&invalid).is_err());

        let mut denied = AppService::deny_all();
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        let error = denied
            .inspect(&example)
            .expect_err("deny-all policy rejects the test fixture publisher");
        assert!(is_unknown_signing_key(&error));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampered_trusted_bundle_reports_digest_failure_without_launching() {
        let key = SigningKey::from_bytes(&[43_u8; 32]);
        let root = temp_root();
        let glass = root.join("tampered.glass");
        fs::create_dir_all(&root).unwrap();
        signed_glass(&glass, &key);
        install_test_runtime(&root);
        let config = root.join("trust.json");
        write_trust_config(&config, &[&key], &[]);
        fs::write(glass.join("rootfs.img"), b"tampered after signing").unwrap();

        let mut service = production_service(&config, &root);
        let error = service
            .inspect(&glass)
            .expect_err("a trusted publisher cannot make tampered contents valid");
        assert_eq!(error.summary, "This glass could not be verified.");
        assert!(error.detail.contains("subject digest mismatch"));
        assert_eq!(error.phase, LifecycleState::Verifying);

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
        std::env::temp_dir().join(format!(
            "rootglass-verified-runtime-{}-{nonce}",
            std::process::id()
        ))
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
        assert!(command.contains(&format!(
            "-kernel {}",
            version_root.join("kernel").display()
        )));
        assert!(command.contains(&format!(
            "-initrd {}",
            version_root.join("initramfs").display()
        )));
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

        assert!(error
            .detail
            .contains("runtime asset integrity check failed"));
        let _ = fs::remove_dir_all(root);
    }
}
