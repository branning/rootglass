use crate::{DeveloperAction, DeveloperPanel, DeveloperResult};
use rootglass_app::registry::InstalledGlass;
use rootglass_app::state::StateRecord;
use rootglass_app::{
    AppError, AppService, ManifestValidationReport, PreflightReport, RunId, RunOptions,
    RuntimeEvent, TrustPolicyStatus,
};
use rootglass_core::runtime_assets::RuntimeAssetsResolver;
use rootglass_core::LifecycleState;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const MAX_CONSOLE_BYTES: usize = 80_000;
const CONSOLE_TRUNCATED_NOTICE: &str = "[older console output was dropped]\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiState {
    Empty,
    Opening(PathBuf),
    Loading,
    Importing(PathBuf),
    Validating,
    ManifestValidated(ManifestValidationReport),
    Planning,
    Planned(LaunchPlanDetails),
    Removing(RemovalDetails),
    Imported(ImportedDetails),
    Ready(GlassPreview),
    Blocked(BlockedDetails),
    Failed(FailureDetails),
    Booting(RunDetails),
    Stopping(RunDetails),
    Running(RunDetails),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedDetails {
    pub path: PathBuf,
    pub name: String,
    pub version: String,
    pub publisher: String,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPlanDetails {
    pub preview: GlassPreview,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalDetails {
    pub content_sha256: String,
    pub name: String,
    pub version: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlassPreview {
    pub path: PathBuf,
    pub name: String,
    pub identifier: String,
    pub version: String,
    pub description: String,
    pub publisher: String,
    pub signature: SignatureStatus,
    pub preflight: PreflightDetails,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureStatus {
    Verified(String),
    Untrusted(String),
    Invalid(String),
    Tampered(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreflightDetails {
    pub architecture: String,
    pub profile: String,
    pub memory_mb: u32,
    pub cpus: u32,
    pub qemu: String,
    pub acceleration: String,
    pub runtime_assets: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedDetails {
    pub path: Option<PathBuf>,
    pub reason: String,
    pub preflight: PreflightDetails,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureDetails {
    pub path: Option<PathBuf>,
    pub phase: String,
    pub summary: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunDetails {
    pub name: String,
    pub status: String,
    pub elapsed: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiActions {
    pub open: bool,
    pub validate: bool,
    pub plan: bool,
    pub boot: bool,
    pub stop: bool,
    pub restart: bool,
    pub remove: bool,
    pub refresh: bool,
    /// Compatibility alias for resetting the focused named state.
    pub reset_state: bool,
    pub state_manage: bool,
    pub state_create: bool,
    pub state_clone: bool,
    pub state_select: bool,
    pub state_delete: bool,
    pub trust_details: bool,
    pub console_input: bool,
    pub console_pause: bool,
    pub console_clear: bool,
    pub console_copy: bool,
    pub developer: bool,
}

#[derive(Clone)]
pub struct OperationTicket {
    generation: u64,
    cancelled: Arc<AtomicBool>,
}

impl OperationTicket {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectionOutcome {
    Ready(GlassPreview),
    Blocked(BlockedDetails),
    Failed(FailureDetails),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportCompletion {
    pub installed: InstalledGlass,
    pub inspection: InspectionOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateSnapshot {
    pub records: Vec<StateRecord>,
    pub locked: bool,
    pub selected_state_id: String,
}

impl Default for StateSnapshot {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            locked: false,
            selected_state_id: "default".to_string(),
        }
    }
}

pub enum StartEffect {
    Applied,
    StopOrphan(RunId),
    Ignored,
}

pub enum StopRequest {
    CancelStartingOperation,
    StopRun { ticket: OperationTicket, run: RunId },
}

/// Service boundary shared by the standalone TUI and GPUI shell.
pub trait TuiService: Send {
    fn installed_glasses(&mut self) -> Result<Vec<InstalledGlass>, AppError>;
    fn import_archive(&mut self, source: &Path) -> Result<InstalledGlass, AppError>;
    fn inspect_bundle(&mut self, source: &Path) -> InspectionOutcome;
    fn validate_manifest(&mut self, _source: &Path) -> Result<ManifestValidationReport, AppError> {
        Err(AppError {
            phase: LifecycleState::Validating,
            summary: "Manifest validation is unavailable.".to_string(),
            detail: "The active TUI service does not provide the validate workflow.".to_string(),
        })
    }
    fn plan_bundle(&mut self, _source: &Path) -> Result<LaunchPlanDetails, AppError> {
        Err(AppError {
            phase: LifecycleState::Verifying,
            summary: "Launch planning is unavailable.".to_string(),
            detail: "The active TUI service does not provide the plan workflow.".to_string(),
        })
    }
    fn remove_installed(
        &mut self,
        _content_sha256: &str,
    ) -> Result<Option<InstalledGlass>, AppError> {
        Err(AppError {
            phase: LifecycleState::Stopped,
            summary: "Library removal is unavailable.".to_string(),
            detail: "The active TUI service does not provide package removal.".to_string(),
        })
    }
    fn start(&mut self, source: &Path) -> Result<RunId, AppError>;
    fn stop(&mut self, run: &RunId) -> Result<(), AppError>;
    fn poll_events(&mut self) -> Result<Vec<RuntimeEvent>, AppError>;
    fn write_serial(&mut self, run: &RunId, bytes: &[u8]) -> Result<(), AppError>;

    fn trust_policy_status(&mut self) -> TrustPolicyStatus {
        TrustPolicyStatus::default()
    }

    fn list_states(&mut self, _source: &Path) -> Result<StateSnapshot, AppError> {
        Err(state_api_unavailable("list"))
    }

    fn select_state(&mut self, _source: &Path, _id: &str) -> Result<StateSnapshot, AppError> {
        Err(state_api_unavailable("select"))
    }

    fn create_state(&mut self, _source: &Path, _name: &str) -> Result<StateSnapshot, AppError> {
        Err(state_api_unavailable("create"))
    }

    fn clone_state(
        &mut self,
        _source: &Path,
        _source_id: &str,
        _new_name: &str,
    ) -> Result<StateSnapshot, AppError> {
        Err(state_api_unavailable("clone"))
    }

    fn reset_state(&mut self, _source: &Path, _id: &str) -> Result<StateSnapshot, AppError> {
        Err(state_api_unavailable("reset"))
    }

    fn delete_state(&mut self, _source: &Path, _id: &str) -> Result<StateSnapshot, AppError> {
        Err(state_api_unavailable("delete"))
    }

    fn start_with_state(
        &mut self,
        source: &Path,
        _state_id: Option<&str>,
    ) -> Result<RunId, AppError> {
        self.start(source)
    }

    fn restart_with_state(
        &mut self,
        run: &RunId,
        source: &Path,
        state_id: Option<&str>,
    ) -> Result<RunId, AppError> {
        self.stop(run)?;
        self.start_with_state(source, state_id)
    }

    fn import_and_inspect(&mut self, source: &Path) -> Result<ImportCompletion, AppError> {
        crate::terminal::trace_tui("archive import started");
        let installed = self.import_archive(source)?;
        crate::terminal::trace_tui("archive import completed");
        let inspection = self.inspect_bundle(&installed.package_path);
        let outcome = match &inspection {
            InspectionOutcome::Ready(_) => "ready",
            InspectionOutcome::Blocked(_) => "blocked",
            InspectionOutcome::Failed(_) => "failed",
        };
        crate::terminal::trace_tui(format!(
            "imported bundle inspection completed: outcome={outcome}"
        ));
        Ok(ImportCompletion {
            installed,
            inspection,
        })
    }

    fn restart(&mut self, run: &RunId, source: &Path) -> Result<RunId, AppError> {
        self.stop(run)?;
        self.start(source)
    }
}

pub type SharedService = Arc<Mutex<Box<dyn TuiService>>>;

pub fn lock_service(
    service: &SharedService,
) -> Result<MutexGuard<'_, Box<dyn TuiService>>, AppError> {
    service.lock().map_err(|_| AppError {
        phase: LifecycleState::Failed,
        summary: "The application service is unavailable.".to_string(),
        detail: "The service lock was poisoned after an earlier failure.".to_string(),
    })
}

impl TuiService for AppService {
    fn installed_glasses(&mut self) -> Result<Vec<InstalledGlass>, AppError> {
        AppService::installed_glasses(self)
    }

    fn import_archive(&mut self, source: &Path) -> Result<InstalledGlass, AppError> {
        AppService::import_archive(self, source)
    }

    fn inspect_bundle(&mut self, source: &Path) -> InspectionOutcome {
        let summary = match AppService::inspect(self, source) {
            Ok(summary) => summary,
            Err(error) => return InspectionOutcome::Failed(failure_details(source, error)),
        };
        let report = match AppService::preflight(self, source) {
            Ok(report) => report,
            Err(error) if error.phase == LifecycleState::Ready => {
                return InspectionOutcome::Blocked(BlockedDetails {
                    path: Some(source.to_path_buf()),
                    reason: error.summary.clone(),
                    preflight: preflight_for_summary(&summary, Vec::new(), vec![error.detail]),
                });
            }
            Err(error) => return InspectionOutcome::Failed(failure_details(source, error)),
        };
        InspectionOutcome::Ready(preview_from_report(report))
    }

    fn validate_manifest(&mut self, source: &Path) -> Result<ManifestValidationReport, AppError> {
        AppService::validate_manifest_only(self, source)
    }

    fn plan_bundle(&mut self, source: &Path) -> Result<LaunchPlanDetails, AppError> {
        let (summary, command) = AppService::launch_plan(self, source)?;
        let preflight = match AppService::preflight(self, source) {
            Ok(report) => preflight_details(&report),
            Err(error) if error.phase == LifecycleState::Ready => preflight_for_summary(
                &summary,
                Vec::new(),
                vec![format!("{} {}", error.summary, error.detail)],
            ),
            Err(error) => return Err(error),
        };
        Ok(LaunchPlanDetails {
            preview: preview_from_summary(summary, preflight),
            command,
        })
    }

    fn remove_installed(
        &mut self,
        content_sha256: &str,
    ) -> Result<Option<InstalledGlass>, AppError> {
        AppService::remove_installed_glass(self, content_sha256)
    }

    fn start(&mut self, source: &Path) -> Result<RunId, AppError> {
        AppService::start(self, source, RunOptions::default())
    }

    fn stop(&mut self, run: &RunId) -> Result<(), AppError> {
        AppService::stop(self, run)
    }

    fn poll_events(&mut self) -> Result<Vec<RuntimeEvent>, AppError> {
        let mut events = Vec::new();
        for _ in 0..32 {
            match AppService::poll_event(self, Duration::ZERO)? {
                Some(event) => events.push(event),
                None => break,
            }
        }
        Ok(events)
    }

    fn write_serial(&mut self, run: &RunId, bytes: &[u8]) -> Result<(), AppError> {
        AppService::write_serial(self, run, bytes)
    }

    fn trust_policy_status(&mut self) -> TrustPolicyStatus {
        AppService::trust_policy_status(self)
    }

    fn list_states(&mut self, source: &Path) -> Result<StateSnapshot, AppError> {
        app_state_snapshot(self, source)
    }

    fn select_state(&mut self, source: &Path, id: &str) -> Result<StateSnapshot, AppError> {
        AppService::select_state(self, source, id)?;
        app_state_snapshot(self, source)
    }

    fn create_state(&mut self, source: &Path, name: &str) -> Result<StateSnapshot, AppError> {
        AppService::create_state(self, source, name)?;
        app_state_snapshot(self, source)
    }

    fn clone_state(
        &mut self,
        source: &Path,
        source_id: &str,
        new_name: &str,
    ) -> Result<StateSnapshot, AppError> {
        AppService::clone_state(self, source, source_id, new_name)?;
        app_state_snapshot(self, source)
    }

    fn reset_state(&mut self, source: &Path, id: &str) -> Result<StateSnapshot, AppError> {
        AppService::reset_state(self, source, id)?;
        app_state_snapshot(self, source)
    }

    fn delete_state(&mut self, source: &Path, id: &str) -> Result<StateSnapshot, AppError> {
        AppService::delete_state(self, source, id)?;
        app_state_snapshot(self, source)
    }

    fn start_with_state(
        &mut self,
        source: &Path,
        state_id: Option<&str>,
    ) -> Result<RunId, AppError> {
        AppService::start(
            self,
            source,
            RunOptions {
                state_id: state_id.map(str::to_string),
            },
        )
    }

    fn restart_with_state(
        &mut self,
        run: &RunId,
        source: &Path,
        state_id: Option<&str>,
    ) -> Result<RunId, AppError> {
        AppService::stop(self, run)?;
        self.start_with_state(source, state_id)
    }
}

fn state_api_unavailable(action: &str) -> AppError {
    AppError {
        phase: LifecycleState::Ready,
        summary: "Named state management is unavailable.".to_string(),
        detail: format!("The active TUI service does not provide the state {action} workflow."),
    }
}

fn app_state_snapshot(service: &mut AppService, source: &Path) -> Result<StateSnapshot, AppError> {
    let snapshot = AppService::state_snapshot(service, source)?;
    Ok(snapshot_with_default(
        snapshot.records,
        snapshot.locked,
        snapshot.selected_state_id,
    ))
}

fn snapshot_with_default(
    mut records: Vec<StateRecord>,
    locked: bool,
    selected_state_id: String,
) -> StateSnapshot {
    if !records.iter().any(|record| record.id == "default") {
        records.insert(
            0,
            StateRecord {
                id: "default".to_string(),
                name: "default".to_string(),
                image_path: PathBuf::new(),
                image_bytes: 0,
            },
        );
    }
    StateSnapshot {
        records,
        locked,
        selected_state_id,
    }
}

pub struct UiModel {
    pub state: UiState,
    pub installed_glasses: Vec<InstalledGlass>,
    pub library_filter: String,
    pub library_error: Option<String>,
    pub selected_path: Option<PathBuf>,
    pub run_id: Option<RunId>,
    pub console: String,
    pub console_paused: bool,
    pub console_scroll_offset: usize,
    pub console_truncated: bool,
    pub console_notice: Option<String>,
    pub developer_panel: DeveloperPanel,
    pub operations_panel_open: bool,
    pub state_records: Vec<StateRecord>,
    pub state_cursor: usize,
    pub selected_state_id: String,
    pub state_error: Option<String>,
    pub state_busy: bool,
    pub state_locked: bool,
    pub state_loaded: bool,
    pub trust_policy: TrustPolicyStatus,
    service: SharedService,
    next_generation: u64,
    active_operation: Option<OperationTicket>,
    last_preview: Option<GlassPreview>,
    ansi: AnsiStripper,
    utf8_tail: Vec<u8>,
    paused_console: Option<String>,
}

impl UiModel {
    pub fn with_service(service: impl TuiService + 'static, initial_state: UiState) -> Self {
        Self {
            state: initial_state,
            installed_glasses: Vec::new(),
            library_filter: String::new(),
            library_error: None,
            selected_path: None,
            run_id: None,
            console: String::new(),
            console_paused: false,
            console_scroll_offset: 0,
            console_truncated: false,
            console_notice: None,
            developer_panel: DeveloperPanel::Closed,
            operations_panel_open: false,
            state_records: Vec::new(),
            state_cursor: 0,
            selected_state_id: "default".to_string(),
            state_error: None,
            state_busy: false,
            state_locked: false,
            state_loaded: true,
            trust_policy: TrustPolicyStatus::default(),
            service: Arc::new(Mutex::new(Box::new(service))),
            next_generation: 0,
            active_operation: None,
            last_preview: None,
            ansi: AnsiStripper::default(),
            utf8_tail: Vec::new(),
            paused_console: None,
        }
    }

    pub fn shared_service(&self) -> SharedService {
        Arc::clone(&self.service)
    }

    pub fn focused_state(&self) -> Option<&StateRecord> {
        self.state_records.get(self.state_cursor)
    }

    pub fn move_state_cursor(&mut self, down: bool) {
        if self.state_records.is_empty() {
            self.state_cursor = 0;
        } else if down {
            self.state_cursor = (self.state_cursor + 1).min(self.state_records.len() - 1);
        } else {
            self.state_cursor = self.state_cursor.saturating_sub(1);
        }
    }

    pub fn begin_state_operation(&mut self) -> bool {
        if self.state_busy || !self.actions().state_manage {
            return false;
        }
        self.state_busy = true;
        self.state_error = None;
        true
    }

    pub fn finish_state_operation(&mut self, result: Result<StateSnapshot, AppError>) {
        self.state_busy = false;
        match result {
            Ok(snapshot) => {
                self.state_records = snapshot.records;
                self.state_locked = snapshot.locked;
                self.selected_state_id = snapshot.selected_state_id;
                self.state_loaded = true;
                self.state_cursor = self
                    .state_cursor
                    .min(self.state_records.len().saturating_sub(1));
                self.state_error = None;
            }
            Err(error) => {
                self.state_loaded = true;
                if error.detail.contains("glass state is currently in use") {
                    self.state_locked = true;
                }
                self.state_error = Some(error.to_string());
            }
        }
    }

    pub fn finish_trust_policy(&mut self, result: Result<TrustPolicyStatus, AppError>) {
        match result {
            Ok(status) => self.trust_policy = status,
            Err(error) => self.trust_policy.error = Some(error.to_string()),
        }
    }

    pub fn actions(&self) -> UiActions {
        let booting = matches!(self.state, UiState::Booting(_));
        let running = matches!(self.state, UiState::Running(_));
        let stopping = matches!(self.state, UiState::Stopping(_));
        let removing = matches!(self.state, UiState::Removing(_));
        let ready = matches!(
            &self.state,
            UiState::Ready(preview) if matches!(&preview.signature, SignatureStatus::Verified(_))
        );
        let has_run = self.run_id.is_some();
        let has_console = has_run || !self.console.is_empty();
        let busy = matches!(
            self.state,
            UiState::Loading
                | UiState::Importing(_)
                | UiState::Opening(_)
                | UiState::Validating
                | UiState::Planning
                | UiState::Removing(_)
                | UiState::Booting(_)
                | UiState::Stopping(_)
        );
        let bootable_plan = matches!(
            &self.state,
            UiState::Planned(plan)
                if plan.preview.preflight.blockers.is_empty()
                    && matches!(&plan.preview.signature, SignatureStatus::Verified(_))
        );
        let has_path = self.selected_path.is_some();
        let has_installed_selection = self.selected_installed().is_some();
        let state_mutation_allowed = has_path
            && !has_run
            && !booting
            && !stopping
            && !busy
            && !self.state_busy
            && !self.state_locked
            && self.state_loaded
            && self.state_error.is_none();
        let has_focused_state = self.focused_state().is_some();
        let focused_state_exists = self
            .focused_state()
            .is_some_and(|state| state.image_bytes > 0);
        UiActions {
            // A new open/inspect request supersedes in-flight file operations. The
            // generation ticket prevents their stale completions from replacing it.
            open: !has_run && !booting && !stopping && !removing && !self.state_busy,
            validate: !busy && !has_run && has_path,
            plan: !busy && !has_run && has_path,
            boot: (ready || bootable_plan)
                && !has_run
                && self.state_loaded
                && !self.state_locked
                && self.state_error.is_none(),
            stop: !stopping && (has_run || booting),
            restart: running && has_run,
            remove: !busy && !has_run && has_installed_selection,
            refresh: !stopping,
            reset_state: state_mutation_allowed && focused_state_exists,
            state_manage: !self.state_busy,
            state_create: state_mutation_allowed,
            state_clone: state_mutation_allowed && focused_state_exists,
            state_select: state_mutation_allowed && has_focused_state,
            state_delete: state_mutation_allowed && focused_state_exists,
            trust_details: true,
            console_input: running && has_run,
            console_pause: has_console,
            console_clear: !self.console.is_empty(),
            console_copy: !self.visible_console().is_empty(),
            developer: !busy && !has_run,
        }
    }

    pub fn open_developer(&mut self) {
        if self.actions().developer {
            self.developer_panel = DeveloperPanel::Menu;
        }
    }

    pub fn close_developer(&mut self) {
        if !matches!(&self.developer_panel, DeveloperPanel::Working(_)) {
            self.developer_panel = DeveloperPanel::Closed;
        }
    }

    pub fn begin_developer(&mut self, action: DeveloperAction) {
        self.developer_panel = DeveloperPanel::Working(action);
    }

    pub fn finish_developer(&mut self, result: Result<DeveloperResult, String>) {
        let action = match &self.developer_panel {
            DeveloperPanel::Working(action) => *action,
            _ => return,
        };
        self.developer_panel = match result {
            Ok(result) => DeveloperPanel::Complete(result),
            Err(detail) => DeveloperPanel::Failed { action, detail },
        };
    }

    pub fn developer_is_open(&self) -> bool {
        !matches!(&self.developer_panel, DeveloperPanel::Closed)
    }

    pub fn visible_console(&self) -> &str {
        self.paused_console.as_deref().unwrap_or(&self.console)
    }

    pub fn toggle_console_pause(&mut self) {
        if self.console_paused {
            self.console_paused = false;
            self.paused_console = None;
            self.console_scroll_offset = 0;
        } else {
            self.console_paused = true;
            self.paused_console = Some(self.console.clone());
            self.console_scroll_offset = 0;
        }
    }

    pub fn clear_console(&mut self) {
        self.console.clear();
        self.paused_console = self.console_paused.then(String::new);
        self.console_scroll_offset = 0;
        self.console_truncated = false;
        self.console_notice = None;
        self.ansi.reset();
        self.utf8_tail.clear();
    }

    pub fn scroll_console_up(&mut self, lines: usize) {
        if self.visible_console().is_empty() {
            return;
        }
        self.console_scroll_offset = self.console_scroll_offset.saturating_add(lines);
    }

    pub fn scroll_console_down(&mut self, lines: usize) {
        self.console_scroll_offset = self.console_scroll_offset.saturating_sub(lines);
    }

    pub fn selected_installed(&self) -> Option<&InstalledGlass> {
        let path = self.selected_path.as_ref()?;
        self.visible_installed()
            .into_iter()
            .find(|glass| &glass.package_path == path)
    }

    pub fn visible_installed(&self) -> Vec<&InstalledGlass> {
        let filter = self.library_filter.trim().to_lowercase();
        self.installed_glasses
            .iter()
            .filter(|glass| {
                filter.is_empty()
                    || [
                        glass.name.as_str(),
                        glass.identifier.as_str(),
                        glass.version.as_str(),
                        glass.publisher_name.as_str(),
                        glass.publisher_identity.as_str(),
                        glass.content_sha256.as_str(),
                    ]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&filter))
            })
            .collect()
    }

    pub fn set_library_filter(&mut self, filter: String) {
        self.library_filter = filter;
    }

    pub fn begin_library_refresh(&self) -> SharedService {
        self.shared_service()
    }

    pub fn finish_library_refresh(&mut self, result: Result<Vec<InstalledGlass>, AppError>) {
        match result {
            Ok(glasses) => {
                self.installed_glasses = glasses;
                self.library_error = None;
            }
            Err(error) => self.library_error = Some(error.to_string()),
        }
    }

    pub fn begin_inspection(&mut self, path: PathBuf) -> Option<OperationTicket> {
        if !self.actions().open {
            return None;
        }
        self.selected_path = Some(path.clone());
        self.reset_state_selection();
        self.state_records.clear();
        self.last_preview = None;
        Some(self.begin_operation(UiState::Loading))
    }

    pub fn begin_open(&mut self, path: PathBuf) -> Option<OperationTicket> {
        if !self.actions().open {
            return None;
        }
        self.selected_path = Some(path.clone());
        self.reset_state_selection();
        self.last_preview = None;
        Some(self.begin_operation(UiState::Opening(path)))
    }

    pub fn begin_validation(&mut self, path: PathBuf) -> Option<OperationTicket> {
        if !self.actions().validate {
            return None;
        }
        self.selected_path = Some(path);
        self.last_preview = None;
        Some(self.begin_operation(UiState::Validating))
    }

    pub fn finish_validation(
        &mut self,
        ticket: &OperationTicket,
        result: Result<ManifestValidationReport, AppError>,
    ) -> bool {
        if !self.finishable(ticket) {
            return false;
        }
        self.active_operation = None;
        self.state = match result {
            Ok(report) => UiState::ManifestValidated(report),
            Err(error) => UiState::Failed(failure_details(
                self.selected_path.as_deref().unwrap_or(Path::new("")),
                error,
            )),
        };
        true
    }

    pub fn begin_plan(&mut self, path: PathBuf) -> Option<OperationTicket> {
        if !self.actions().plan {
            return None;
        }
        self.selected_path = Some(path);
        self.last_preview = None;
        Some(self.begin_operation(UiState::Planning))
    }

    pub fn finish_plan(
        &mut self,
        ticket: &OperationTicket,
        result: Result<LaunchPlanDetails, AppError>,
    ) -> bool {
        if !self.finishable(ticket) {
            return false;
        }
        self.active_operation = None;
        match result {
            Ok(plan) => {
                self.selected_path = Some(plan.preview.path.clone());
                self.last_preview = Some(plan.preview.clone());
                self.state = UiState::Planned(plan);
            }
            Err(error) => {
                self.state = UiState::Failed(failure_details(
                    self.selected_path.as_deref().unwrap_or(Path::new("")),
                    error,
                ));
            }
        }
        true
    }

    pub fn begin_remove(&mut self) -> Option<(OperationTicket, String)> {
        if !self.actions().remove {
            return None;
        }
        let selected = self.selected_installed()?.clone();
        let details = RemovalDetails {
            content_sha256: selected.content_sha256.clone(),
            name: selected.name,
            version: selected.version,
            path: selected.package_path,
        };
        let digest = details.content_sha256.clone();
        Some((self.begin_operation(UiState::Removing(details)), digest))
    }

    pub fn finish_remove(
        &mut self,
        ticket: &OperationTicket,
        result: Result<Option<InstalledGlass>, AppError>,
    ) -> bool {
        if !self.finishable(ticket) {
            return false;
        }
        let details = match &self.state {
            UiState::Removing(details) => details.clone(),
            _ => return false,
        };
        self.active_operation = None;
        match result {
            Ok(removed) => {
                let (digest, path) = removed.map_or_else(
                    || (details.content_sha256, details.path),
                    |glass| (glass.content_sha256, glass.package_path),
                );
                self.installed_glasses
                    .retain(|glass| glass.content_sha256 != digest);
                self.library_error = None;
                if Some(&path) == self.selected_path.as_ref() {
                    self.selected_path = None;
                    self.last_preview = None;
                    self.state = UiState::Empty;
                } else {
                    self.restore_preview();
                }
            }
            Err(error) => {
                self.state = UiState::Failed(failure_details(
                    self.selected_path.as_deref().unwrap_or(Path::new("")),
                    error,
                ));
            }
        }
        true
    }

    pub fn finish_inspection(
        &mut self,
        ticket: &OperationTicket,
        result: InspectionOutcome,
    ) -> bool {
        if !self.finishable(ticket) {
            return false;
        }
        self.active_operation = None;
        self.apply_inspection(result);
        true
    }

    pub fn begin_import(&mut self, path: PathBuf) -> Option<OperationTicket> {
        if !self.actions().open {
            return None;
        }
        self.selected_path = Some(path.clone());
        self.reset_state_selection();
        self.last_preview = None;
        Some(self.begin_operation(UiState::Importing(path)))
    }

    pub fn finish_import(
        &mut self,
        ticket: &OperationTicket,
        result: Result<ImportCompletion, AppError>,
    ) -> bool {
        if !self.finishable(ticket) {
            return false;
        }
        self.active_operation = None;
        match result {
            Ok(completion) => {
                let installed = completion.installed;
                self.selected_path = Some(installed.package_path.clone());
                if let Some(existing) = self
                    .installed_glasses
                    .iter_mut()
                    .find(|item| item.content_sha256 == installed.content_sha256)
                {
                    *existing = installed;
                } else {
                    self.installed_glasses.push(installed);
                    self.installed_glasses.sort_by(|a, b| {
                        a.identifier
                            .cmp(&b.identifier)
                            .then(a.version.cmp(&b.version))
                    });
                }
                self.library_error = None;
                self.apply_inspection(completion.inspection);
            }
            Err(error) => {
                self.state = UiState::Failed(failure_details(
                    self.selected_path.as_deref().unwrap_or(Path::new("")),
                    error,
                ));
            }
        }
        true
    }

    pub fn begin_start(&mut self) -> Option<(OperationTicket, PathBuf)> {
        if !self.actions().boot {
            return None;
        }
        let path = self.selected_path.clone()?;
        let name = self
            .last_preview
            .as_ref()
            .map(|preview| preview.name.clone())
            .unwrap_or_else(|| "Selected glass".to_string());
        self.reset_console();
        let ticket = self.begin_operation(UiState::Booting(RunDetails {
            name,
            status: "Starting guest".to_string(),
            elapsed: String::new(),
        }));
        Some((ticket, path))
    }

    pub fn finish_start(
        &mut self,
        ticket: &OperationTicket,
        result: Result<RunId, AppError>,
    ) -> StartEffect {
        if !self.finishable(ticket) {
            return result.map_or(StartEffect::Ignored, StartEffect::StopOrphan);
        }
        self.active_operation = None;
        match result {
            Ok(run) => {
                self.run_id = Some(run);
                let name = self
                    .last_preview
                    .as_ref()
                    .map(|preview| preview.name.clone())
                    .unwrap_or_else(|| "Selected glass".to_string());
                self.state = UiState::Booting(RunDetails {
                    name,
                    status: "Booting; waiting for guest login prompt".to_string(),
                    elapsed: String::new(),
                });
                StartEffect::Applied
            }
            Err(error) => {
                self.run_id = None;
                self.state = UiState::Failed(failure_details(
                    self.selected_path.as_deref().unwrap_or(Path::new("")),
                    error,
                ));
                StartEffect::Applied
            }
        }
    }

    pub fn begin_stop(&mut self) -> Option<StopRequest> {
        if !self.actions().stop {
            return None;
        }
        if self.run_id.is_none() && matches!(self.state, UiState::Booting(_)) {
            self.cancel_active_operation();
            self.restore_preview();
            return Some(StopRequest::CancelStartingOperation);
        }
        let run = self.run_id.clone()?;
        let name = self
            .last_preview
            .as_ref()
            .map(|preview| preview.name.clone())
            .unwrap_or_else(|| "Selected glass".to_string());
        let ticket = self.begin_operation(UiState::Stopping(RunDetails {
            name,
            status: "Stopping guest".to_string(),
            elapsed: String::new(),
        }));
        Some(StopRequest::StopRun { ticket, run })
    }

    pub fn finish_stop(&mut self, ticket: &OperationTicket, result: Result<(), AppError>) -> bool {
        if !self.finishable(ticket) {
            return false;
        }
        self.active_operation = None;
        match result {
            Ok(()) => {
                self.run_id = None;
                self.restore_preview();
            }
            Err(error) => {
                self.state = UiState::Failed(failure_details(
                    self.selected_path.as_deref().unwrap_or(Path::new("")),
                    error,
                ));
            }
        }
        true
    }

    pub fn begin_restart(&mut self) -> Option<(OperationTicket, RunId, PathBuf)> {
        if !self.actions().restart {
            return None;
        }
        let run = self.run_id.clone()?;
        let path = self.selected_path.clone()?;
        let name = self
            .last_preview
            .as_ref()
            .map(|preview| preview.name.clone())
            .unwrap_or_else(|| "Selected glass".to_string());
        let ticket = self.begin_operation(UiState::Stopping(RunDetails {
            name,
            status: "Restarting guest".to_string(),
            elapsed: String::new(),
        }));
        Some((ticket, run, path))
    }

    pub fn finish_restart(
        &mut self,
        ticket: &OperationTicket,
        result: Result<RunId, AppError>,
    ) -> StartEffect {
        if !self.finishable(ticket) {
            return result.map_or(StartEffect::Ignored, StartEffect::StopOrphan);
        }
        self.active_operation = None;
        match result {
            Ok(run) => {
                self.run_id = Some(run);
                let name = self
                    .last_preview
                    .as_ref()
                    .map(|preview| preview.name.clone())
                    .unwrap_or_else(|| "Selected glass".to_string());
                self.reset_console();
                self.state = UiState::Booting(RunDetails {
                    name,
                    status: "Booting; waiting for guest login prompt".to_string(),
                    elapsed: String::new(),
                });
                StartEffect::Applied
            }
            Err(error) => {
                self.run_id = None;
                self.state = UiState::Failed(failure_details(
                    self.selected_path.as_deref().unwrap_or(Path::new("")),
                    error,
                ));
                StartEffect::Applied
            }
        }
    }

    pub fn apply_poll_result(&mut self, result: Result<Vec<RuntimeEvent>, AppError>) {
        let Some(current_run) = self.run_id.clone() else {
            return;
        };
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                self.state = UiState::Failed(failure_details(
                    self.selected_path.as_deref().unwrap_or(Path::new("")),
                    error,
                ));
                return;
            }
        };
        for event in events {
            match event {
                RuntimeEvent::StateChanged { run, state }
                    if run == current_run && self.run_id.as_ref() == Some(&run) =>
                {
                    match state {
                        LifecycleState::Starting => {
                            self.state = UiState::Booting(self.run_details("Starting guest"));
                        }
                        LifecycleState::Booting => {
                            self.state = UiState::Booting(
                                self.run_details("Booting; waiting for profile-ready evidence"),
                            );
                        }
                        LifecycleState::Running => {
                            // The service state alone is not sufficient evidence to
                            // show a guest as ready. Wait for its explicit signal.
                            self.state = UiState::Booting(self.run_details(
                                "Runtime is active; waiting for profile-ready evidence",
                            ));
                        }
                        LifecycleState::Stopping => {
                            self.state = UiState::Stopping(self.run_details("Stopping guest"));
                        }
                        LifecycleState::Stopped => {
                            self.run_id = None;
                            self.restore_preview();
                        }
                        LifecycleState::Failed => {
                            self.state = UiState::Failed(FailureDetails {
                                path: self.selected_path.clone(),
                                phase: "Guest".to_string(),
                                summary: "The guest runtime reported a failure.".to_string(),
                                detail: "The runtime entered the failed state without additional details.".to_string(),
                            });
                        }
                        LifecycleState::Validating
                        | LifecycleState::Verifying
                        | LifecycleState::Ready => {}
                    }
                }
                RuntimeEvent::SerialChunk {
                    run,
                    bytes,
                    truncated,
                } if run == current_run && self.run_id.as_ref() == Some(&run) => {
                    self.append_console(&bytes, truncated);
                }
                RuntimeEvent::GuestReady { run, evidence }
                    if run == current_run && self.run_id.as_ref() == Some(&run) =>
                {
                    let name = self
                        .last_preview
                        .as_ref()
                        .map(|preview| preview.name.clone())
                        .unwrap_or_else(|| "Selected glass".to_string());
                    self.state = UiState::Running(RunDetails {
                        name,
                        status: format!("Running · ready evidence: {evidence}"),
                        elapsed: String::new(),
                    });
                }
                RuntimeEvent::Exited {
                    run,
                    exit_code,
                    expected,
                } if run == current_run && self.run_id.as_ref() == Some(&run) => {
                    self.run_id = None;
                    if expected {
                        self.restore_preview();
                    } else {
                        self.state = UiState::Failed(FailureDetails {
                            path: self.selected_path.clone(),
                            phase: "Guest".to_string(),
                            summary: "QEMU exited unexpectedly.".to_string(),
                            detail: format!("Exit code: {exit_code:?}"),
                        });
                    }
                }
                RuntimeEvent::Failed {
                    run,
                    phase,
                    summary,
                    detail,
                } if run == current_run && self.run_id.as_ref() == Some(&run) => {
                    self.run_id = None;
                    self.state = UiState::Failed(FailureDetails {
                        path: self.selected_path.clone(),
                        phase: format!("{phase:?}"),
                        summary,
                        detail,
                    });
                }
                _ => {}
            }
        }
    }

    pub fn input_request(&self, text: &str) -> Option<(RunId, Vec<u8>)> {
        if !self.actions().console_input {
            return None;
        }
        let run = self.run_id.clone()?;
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\n');
        Some((run, bytes))
    }

    pub fn finish_input(&mut self, run: &RunId, result: Result<(), AppError>) {
        let Some(active_run) = self.run_id.as_ref() else {
            return;
        };
        if active_run != run {
            return;
        }
        if let Err(error) = result {
            self.state = UiState::Failed(failure_details(
                self.selected_path.as_deref().unwrap_or(Path::new("")),
                error,
            ));
        }
    }

    fn begin_operation(&mut self, state: UiState) -> OperationTicket {
        self.cancel_active_operation();
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let ticket = OperationTicket {
            generation: self.next_generation,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.state = state;
        self.active_operation = Some(ticket.clone());
        ticket
    }

    fn reset_state_selection(&mut self) {
        self.state_records.clear();
        self.state_cursor = 0;
        self.selected_state_id = "default".to_string();
        self.state_error = None;
        self.state_locked = false;
        self.state_loaded = false;
    }

    fn cancel_active_operation(&mut self) {
        if let Some(operation) = self.active_operation.take() {
            operation.cancelled.store(true, Ordering::Release);
        }
    }

    fn finishable(&self, ticket: &OperationTicket) -> bool {
        !ticket.is_cancelled()
            && self
                .active_operation
                .as_ref()
                .is_some_and(|active| active.generation == ticket.generation)
    }

    fn apply_inspection(&mut self, result: InspectionOutcome) {
        match result {
            InspectionOutcome::Ready(preview) => {
                self.selected_path = Some(preview.path.clone());
                self.last_preview = Some(preview.clone());
                self.state = UiState::Ready(preview);
            }
            InspectionOutcome::Blocked(blocked) => self.state = UiState::Blocked(blocked),
            InspectionOutcome::Failed(failure) => self.state = UiState::Failed(failure),
        }
    }

    fn restore_preview(&mut self) {
        if let Some(preview) = self.last_preview.clone() {
            self.selected_path = Some(preview.path.clone());
            self.state = UiState::Ready(preview);
        } else {
            self.state = UiState::Empty;
        }
    }

    fn run_details(&self, status: &str) -> RunDetails {
        RunDetails {
            name: self
                .last_preview
                .as_ref()
                .map(|preview| preview.name.clone())
                .unwrap_or_else(|| "Selected glass".to_string()),
            status: status.to_string(),
            elapsed: String::new(),
        }
    }

    fn reset_console(&mut self) {
        self.console.clear();
        self.console_paused = false;
        self.paused_console = None;
        self.console_scroll_offset = 0;
        self.console_truncated = false;
        self.console_notice = None;
        self.ansi.reset();
        self.utf8_tail.clear();
    }

    fn append_console(&mut self, bytes: &[u8], runtime_truncated: bool) {
        if self.console.is_empty() {
            self.console_scroll_offset = 0;
        }
        if runtime_truncated {
            self.ansi.reset();
            self.utf8_tail.clear();
        }
        let clean = self.ansi.push(bytes);
        let decoded = self.decode_console_utf8(&clean);
        let safe = sanitize_console_text(&decoded);
        if runtime_truncated {
            self.console_truncated = true;
            if !self.console.is_empty() && !self.console.ends_with('\n') {
                self.console.push('\n');
            }
            self.console
                .push_str("[serial output was truncated by the runtime buffer]\n");
        }
        self.console.push_str(&safe);
        if self.console.len() > MAX_CONSOLE_BYTES {
            let mut drain = self.console.len() - MAX_CONSOLE_BYTES;
            while !self.console.is_char_boundary(drain) {
                drain += 1;
            }
            self.console.drain(..drain);
            if !self.console.starts_with(CONSOLE_TRUNCATED_NOTICE) {
                self.console_truncated = true;
                let excess = self
                    .console
                    .len()
                    .saturating_add(CONSOLE_TRUNCATED_NOTICE.len())
                    .saturating_sub(MAX_CONSOLE_BYTES);
                let mut marker_drain = excess;
                while marker_drain < self.console.len()
                    && !self.console.is_char_boundary(marker_drain)
                {
                    marker_drain += 1;
                }
                self.console.drain(..marker_drain);
                self.console.insert_str(0, CONSOLE_TRUNCATED_NOTICE);
            }
        }
    }

    fn decode_console_utf8(&mut self, bytes: &[u8]) -> String {
        let mut pending = std::mem::take(&mut self.utf8_tail);
        pending.extend_from_slice(bytes);
        let mut decoded = String::with_capacity(pending.len());
        let mut remaining = pending.as_slice();
        loop {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    decoded.push_str(valid);
                    break;
                }
                Err(error) => {
                    let valid_len = error.valid_up_to();
                    decoded.push_str(
                        std::str::from_utf8(&remaining[..valid_len])
                            .expect("valid UTF-8 prefix is valid"),
                    );
                    match error.error_len() {
                        Some(invalid_len) => {
                            decoded.push('\u{fffd}');
                            remaining = &remaining[valid_len + invalid_len..];
                        }
                        None => {
                            self.utf8_tail.extend_from_slice(&remaining[valid_len..]);
                            break;
                        }
                    }
                }
            }
        }
        decoded
    }
}

fn sanitize_console_text(input: &str) -> String {
    let mut safe = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                safe.push('\n');
            }
            '\n' => safe.push('\n'),
            '\t' => safe.push_str("    "),
            control if control.is_control() => safe.push('\u{fffd}'),
            visible => safe.push(visible),
        }
    }
    safe
}

impl Default for UiModel {
    fn default() -> Self {
        let (service, state) = match local_trust_config_path() {
            Ok(path) => match AppService::from_trust_config(&path) {
                Ok(service) => (with_packaged_runtime_assets(service), UiState::Empty),
                Err(error) => {
                    let detail = format!("{} ({})", error, path.display());
                    (
                        AppService::deny_all().with_trust_load_error(Some(path), detail.clone()),
                        trust_config_failure(detail),
                    )
                }
            },
            Err(error) => (
                AppService::deny_all().with_trust_load_error(None, error.clone()),
                trust_config_failure(error),
            ),
        };
        Self::with_service(service, state)
    }
}

fn with_packaged_runtime_assets(service: AppService) -> AppService {
    let Some(runtime_version) = option_env!("ROOTGLASS_RUNTIME_VERSION") else {
        return service;
    };
    let resource_root = packaged_resource_root();
    let user_data_root = app_data_root();
    service.with_verified_runtime_assets(
        RuntimeAssetsResolver::new(resource_root, user_data_root),
        runtime_version,
    )
}

fn packaged_resource_root() -> PathBuf {
    let executable = std::env::current_exe().unwrap_or_default();
    #[cfg(target_os = "macos")]
    {
        if let Some(contents) = executable.parent().and_then(Path::parent) {
            return contents.join("Resources").join("rootglass");
        }
    }
    #[cfg(target_os = "linux")]
    {
        let installed = PathBuf::from("/usr/lib/rootglass");
        if installed.is_dir() {
            return installed;
        }
    }
    executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("rootglass")
}

fn app_data_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path).join("Rootglass");
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home)
                .join("AppData")
                .join("Local")
                .join("Rootglass");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Rootglass");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path.join("rootglass");
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("rootglass");
        }
    }
    std::env::temp_dir().join("rootglass")
}

fn failure_details(source: &Path, error: AppError) -> FailureDetails {
    FailureDetails {
        path: Some(source.to_path_buf()),
        phase: format!("{:?}", error.phase),
        summary: error.summary,
        detail: error.detail,
    }
}

fn preview_from_report(report: PreflightReport) -> GlassPreview {
    let preflight = preflight_details(&report);
    preview_from_summary(report.summary, preflight)
}

fn preview_from_summary(
    summary: rootglass_app::GlassSummary,
    preflight: PreflightDetails,
) -> GlassPreview {
    GlassPreview {
        path: summary.path,
        name: summary.name,
        identifier: summary.identifier,
        version: summary.version,
        description: summary.description,
        publisher: summary.publisher_name,
        signature: SignatureStatus::Verified(summary.publisher_identity),
        preflight,
    }
}

#[cfg(test)]
fn empty_preflight() -> PreflightDetails {
    PreflightDetails {
        architecture: "Unknown".to_string(),
        profile: "Unknown".to_string(),
        memory_mb: 0,
        cpus: 0,
        qemu: "Not checked".to_string(),
        acceleration: "Not checked".to_string(),
        runtime_assets: vec![],
        blockers: vec![],
    }
}

fn preflight_details(report: &PreflightReport) -> PreflightDetails {
    preflight_for_summary(&report.summary, report.readiness.clone(), Vec::new())
}

fn preflight_for_summary(
    summary: &rootglass_app::GlassSummary,
    runtime_assets: Vec<String>,
    blockers: Vec<String>,
) -> PreflightDetails {
    PreflightDetails {
        architecture: summary.architecture.clone(),
        profile: summary.profile.clone(),
        memory_mb: summary.memory_mb,
        cpus: summary.cpus,
        qemu: "Resolved by runtime preparation; see launch command".to_string(),
        acceleration: summary.acceleration.clone(),
        runtime_assets,
        blockers,
    }
}

fn trust_config_failure(detail: String) -> UiState {
    UiState::Failed(FailureDetails {
        path: None,
        phase: "Trust configuration".to_string(),
        summary: "Local publisher trust could not be loaded. Bundle verification is disabled."
            .to_string(),
        detail,
    })
}

fn local_trust_config_path() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| {
                "APPDATA is not set; cannot locate Rootglass trust configuration.".to_string()
            })?;
        return Ok(root.join("Rootglass").join("trust.json"));
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            "HOME is not set; cannot locate Rootglass trust configuration.".to_string()
        })?;
        return Ok(home
            .join("Library")
            .join("Application Support")
            .join("Rootglass")
            .join("trust.json"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            if config_home.is_absolute() {
                return Ok(config_home.join("rootglass").join("trust.json"));
            }
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| {
                "Neither an absolute XDG_CONFIG_HOME nor HOME is set; cannot locate Rootglass trust configuration."
                    .to_string()
            })?;
        Ok(home.join(".config").join("rootglass").join("trust.json"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Err("This platform has no configured Rootglass trust configuration directory.".to_string())
    }
}

#[derive(Default)]
struct AnsiStripper {
    state: AnsiState,
}

#[derive(Clone, Copy, Default)]
enum AnsiState {
    #[default]
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
    String,
    StringEscape,
}

impl AnsiStripper {
    fn reset(&mut self) {
        self.state = AnsiState::Text;
    }

    fn push(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len());
        for byte in input.iter().copied() {
            self.state = match self.state {
                AnsiState::Text if byte == 0x1b => AnsiState::Escape,
                AnsiState::Text => {
                    output.push(byte);
                    AnsiState::Text
                }
                AnsiState::Escape if byte == b'[' => AnsiState::Csi,
                AnsiState::Escape if byte == b']' => AnsiState::Osc,
                AnsiState::Escape if matches!(byte, b'P' | b'X' | b'^' | b'_') => AnsiState::String,
                AnsiState::Escape if (0x40..=0x5f).contains(&byte) => AnsiState::Text,
                AnsiState::Escape => AnsiState::Text,
                AnsiState::Csi if (0x40..=0x7e).contains(&byte) => AnsiState::Text,
                AnsiState::Csi if byte == 0x1b => AnsiState::Escape,
                AnsiState::Csi => AnsiState::Csi,
                AnsiState::Osc if byte == 0x07 => AnsiState::Text,
                AnsiState::Osc if byte == 0x1b => AnsiState::OscEscape,
                AnsiState::Osc => AnsiState::Osc,
                AnsiState::OscEscape if byte == b'\\' => AnsiState::Text,
                AnsiState::OscEscape if byte == 0x07 => AnsiState::Text,
                AnsiState::OscEscape if byte == 0x1b => AnsiState::OscEscape,
                AnsiState::OscEscape => AnsiState::Osc,
                AnsiState::String if byte == 0x1b => AnsiState::StringEscape,
                AnsiState::String => AnsiState::String,
                AnsiState::StringEscape if byte == b'\\' => AnsiState::Text,
                AnsiState::StringEscape if byte == 0x1b => AnsiState::StringEscape,
                AnsiState::StringEscape => AnsiState::String,
            };
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootglass_app::AppError;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeService {
        glasses: Vec<InstalledGlass>,
        events: VecDeque<RuntimeEvent>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl TuiService for FakeService {
        fn installed_glasses(&mut self) -> Result<Vec<InstalledGlass>, AppError> {
            self.calls.lock().unwrap().push("installed_glasses");
            Ok(self.glasses.clone())
        }

        fn import_archive(&mut self, source: &Path) -> Result<InstalledGlass, AppError> {
            self.calls.lock().unwrap().push("import_archive");
            Ok(installed(source))
        }

        fn inspect_bundle(&mut self, source: &Path) -> InspectionOutcome {
            self.calls.lock().unwrap().push("inspect_bundle");
            InspectionOutcome::Ready(preview(source))
        }

        fn start(&mut self, _source: &Path) -> Result<RunId, AppError> {
            self.calls.lock().unwrap().push("start");
            Ok(RunId(2))
        }

        fn stop(&mut self, _run: &RunId) -> Result<(), AppError> {
            self.calls.lock().unwrap().push("stop");
            Ok(())
        }

        fn poll_events(&mut self) -> Result<Vec<RuntimeEvent>, AppError> {
            self.calls.lock().unwrap().push("poll_events");
            Ok(self.events.drain(..).collect())
        }

        fn write_serial(&mut self, _run: &RunId, _bytes: &[u8]) -> Result<(), AppError> {
            self.calls.lock().unwrap().push("write_serial");
            Ok(())
        }
    }

    fn model() -> UiModel {
        UiModel::with_service(FakeService::default(), UiState::Empty)
    }

    fn model_with_calls() -> (UiModel, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let service = FakeService {
            calls: Arc::clone(&calls),
            ..FakeService::default()
        };
        (UiModel::with_service(service, UiState::Empty), calls)
    }

    fn preview(path: &Path) -> GlassPreview {
        GlassPreview {
            path: path.to_path_buf(),
            name: "Hello World".into(),
            identifier: "rootglass.hello".into(),
            version: "0.1.0".into(),
            description: "Fixture guest".into(),
            publisher: "Rootglass Examples".into(),
            signature: SignatureStatus::Verified("fixture-key".into()),
            preflight: empty_preflight(),
        }
    }

    fn installed(path: &Path) -> InstalledGlass {
        InstalledGlass {
            content_sha256: "abc123".into(),
            package_path: path.to_path_buf(),
            identifier: "rootglass.hello".into(),
            name: "Hello World".into(),
            version: "0.1.0".into(),
            publisher_name: "Rootglass Examples".into(),
            publisher_identity: "fixture-key".into(),
            key_id: "fixture".into(),
            original_source: PathBuf::from("fixture.glass"),
            imported_at_unix_seconds: 1,
        }
    }

    fn ready_model() -> UiModel {
        let mut model = model();
        let path = PathBuf::from("hello-world.glass");
        let ticket = model.begin_inspection(path.clone()).unwrap();
        assert!(model.finish_inspection(&ticket, InspectionOutcome::Ready(preview(&path))));
        model.finish_state_operation(Ok(StateSnapshot::default()));
        model
    }

    fn app_error() -> AppError {
        AppError {
            phase: LifecycleState::Failed,
            summary: "operation failed".into(),
            detail: "fake service error".into(),
        }
    }

    #[test]
    fn library_filter_is_case_insensitive_and_hides_remove_for_hidden_selection() {
        let mut model = model();
        let mut alpha = installed(Path::new("/library/alpha.glass"));
        alpha.content_sha256 = "alpha-digest".into();
        alpha.identifier = "org.example.alpha".into();
        alpha.name = "Alpha Guest".into();
        alpha.version = "1.2.3".into();
        let mut beta = installed(Path::new("/library/beta.glass"));
        beta.content_sha256 = "beta-digest".into();
        beta.identifier = "org.example.beta".into();
        beta.name = "Beta Guest".into();
        beta.publisher_name = "Beta Publisher".into();
        model.installed_glasses = vec![alpha.clone(), beta.clone()];
        model.selected_path = Some(alpha.package_path.clone());

        model.set_library_filter("BETA PUBLISHER".into());

        let visible = model.visible_installed();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].content_sha256, "beta-digest");
        assert!(model.selected_installed().is_none());
        assert!(!model.actions().remove);

        model.set_library_filter(String::new());
        assert_eq!(model.selected_installed(), Some(&alpha));
        assert!(model.actions().remove);
    }

    #[test]
    fn removal_clears_the_selected_release_and_handles_already_removed_entries() {
        let mut model = model();
        let selected = installed(Path::new("/library/hello.glass"));
        model.installed_glasses = vec![selected.clone()];
        model.selected_path = Some(selected.package_path.clone());

        let (ticket, digest) = model
            .begin_remove()
            .expect("selected release can be removed");
        assert_eq!(digest, selected.content_sha256);
        assert!(!model.actions().open);
        assert!(model.finish_remove(&ticket, Ok(Some(selected.clone()))));
        assert!(model.installed_glasses.is_empty());
        assert!(model.selected_path.is_none());
        assert!(matches!(model.state, UiState::Empty));

        model.installed_glasses = vec![selected.clone()];
        model.selected_path = Some(selected.package_path.clone());
        let (ticket, _) = model.begin_remove().expect("release can be retried");
        assert!(model.finish_remove(&ticket, Ok(None)));
        assert!(model.installed_glasses.is_empty());
        assert!(model.selected_path.is_none());
        assert!(matches!(model.state, UiState::Empty));
    }

    #[test]
    fn action_availability_matches_each_lifecycle_state() {
        let mut model = model();
        let empty = model.actions();
        assert!(empty.open);
        assert!(!empty.boot && !empty.stop && !empty.restart && !empty.reset_state);

        let loading = model.begin_inspection(PathBuf::from("one.glass")).unwrap();
        let actions = model.actions();
        assert!(actions.open);
        assert!(!actions.boot && !actions.stop && !actions.restart);

        model.finish_inspection(
            &loading,
            InspectionOutcome::Ready(preview(Path::new("one.glass"))),
        );
        assert!(
            !model.actions().boot,
            "boot waits for the saved state selection"
        );
        model.finish_state_operation(Ok(StateSnapshot::default()));
        let ready = model.actions();
        assert!(ready.open && ready.boot);
        assert!(!ready.stop && !ready.restart && !ready.reset_state);

        let blocked = model
            .begin_inspection(PathBuf::from("blocked.glass"))
            .unwrap();
        model.finish_inspection(
            &blocked,
            InspectionOutcome::Blocked(BlockedDetails {
                path: Some(PathBuf::from("blocked.glass")),
                reason: "missing runtime asset".into(),
                preflight: empty_preflight(),
            }),
        );
        let actions = model.actions();
        assert!(actions.open);
        assert!(!actions.boot && !actions.stop && !actions.restart);

        let failed = model.begin_inspection(PathBuf::from("bad.glass")).unwrap();
        model.finish_inspection(
            &failed,
            InspectionOutcome::Failed(FailureDetails {
                path: Some(PathBuf::from("bad.glass")),
                phase: "Verification".into(),
                summary: "Invalid signature".into(),
                detail: "The signed subject did not match.".into(),
            }),
        );
        let actions = model.actions();
        assert!(actions.open);
        assert!(!actions.boot && !actions.stop && !actions.restart);

        model = ready_model();
        let (_, _) = model.begin_start().expect("ready bundle can start");
        let booting = model.actions();
        assert!(!booting.boot && booting.stop && !booting.restart);
        assert!(!booting.open);

        assert!(matches!(
            model.begin_stop(),
            Some(StopRequest::CancelStartingOperation)
        ));
        assert!(model.actions().boot);

        model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![RuntimeEvent::GuestReady {
            run: RunId(2),
            evidence: "fixture login".into(),
        }]));
        let running = model.actions();
        assert!(!running.open && !running.boot);
        assert!(running.stop && running.restart && running.console_input);
        assert!(!running.reset_state);
        let (restart, _, _) = model.begin_restart().unwrap();
        let stopping = model.actions();
        assert!(!stopping.open && !stopping.boot && !stopping.stop && !stopping.restart);
        assert!(model.begin_stop().is_none());
        assert!(matches!(
            model.finish_restart(&restart, Ok(RunId(3))),
            StartEffect::Applied
        ));
        assert!(matches!(model.state, UiState::Booting(_)));
    }

    #[test]
    fn boot_remains_disabled_for_unverified_previews_and_plans() {
        let path = Path::new("untrusted.glass");
        let mut untrusted_preview = preview(path);
        untrusted_preview.signature = SignatureStatus::Untrusted("unknown publisher".into());
        let model =
            UiModel::with_service(FakeService::default(), UiState::Ready(untrusted_preview));
        assert!(!model.actions().boot);

        let mut invalid_plan = preview(path);
        invalid_plan.signature = SignatureStatus::Tampered("digest mismatch".into());
        let model = UiModel::with_service(
            FakeService::default(),
            UiState::Planned(LaunchPlanDetails {
                preview: invalid_plan,
                command: "qemu-system-x86_64".into(),
            }),
        );
        assert!(!model.actions().boot);
    }

    #[test]
    fn a_new_inspection_cancels_old_results_without_blocking_model_updates() {
        let mut model = model();
        let first = model.begin_inspection(PathBuf::from("slow.glass")).unwrap();
        let second_path = PathBuf::from("current.glass");
        let second = model.begin_inspection(second_path.clone()).unwrap();
        assert!(first.is_cancelled());
        assert!(!model.finish_inspection(
            &first,
            InspectionOutcome::Ready(preview(Path::new("slow.glass")))
        ));
        assert!(matches!(model.state, UiState::Loading));
        assert!(model.finish_inspection(&second, InspectionOutcome::Ready(preview(&second_path))));
        assert!(matches!(&model.state, UiState::Ready(item) if item.path == second_path));
    }

    #[test]
    fn fake_service_runs_behind_the_shared_boundary() {
        let (mut model, calls) = model_with_calls();
        let ticket = model
            .begin_inspection(PathBuf::from("hello-world.glass"))
            .unwrap();
        assert!(calls.lock().unwrap().is_empty());
        let service = model.shared_service();
        let result = lock_service(&service)
            .unwrap()
            .inspect_bundle(Path::new("hello-world.glass"));
        assert!(matches!(&result, InspectionOutcome::Ready(_)));
        assert_eq!(&*calls.lock().unwrap(), &["inspect_bundle"]);
        assert!(model.finish_inspection(&ticket, result));
    }

    #[test]
    fn stale_start_completion_stops_the_orphaned_process() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        assert!(matches!(
            model.begin_stop(),
            Some(StopRequest::CancelStartingOperation)
        ));
        assert!(ticket.is_cancelled());
        assert!(matches!(
            model.finish_start(&ticket, Ok(RunId(9))),
            StartEffect::StopOrphan(RunId(9))
        ));
        assert!(!model.actions().stop);
    }

    #[test]
    fn stale_run_events_do_not_mutate_the_current_session() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        assert!(matches!(
            model.finish_start(&ticket, Ok(RunId(2))),
            StartEffect::Applied
        ));
        model.apply_poll_result(Ok(vec![
            RuntimeEvent::GuestReady {
                run: RunId(1),
                evidence: "stale ready event".into(),
            },
            RuntimeEvent::SerialChunk {
                run: RunId(1),
                bytes: b"old console".to_vec(),
                truncated: false,
            },
        ]));
        assert!(matches!(model.state, UiState::Booting(_)));
        assert!(model.console.is_empty());
        assert_eq!(model.run_id, Some(RunId(2)));
    }

    #[test]
    fn console_sanitizes_split_control_sequences_and_preserves_split_utf8() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![
            RuntimeEvent::SerialChunk {
                run: RunId(2),
                bytes: b"hello\x1b[3".to_vec(),
                truncated: false,
            },
            RuntimeEvent::SerialChunk {
                run: RunId(2),
                bytes: b"1m world\x1b[0m\r\n\x1b]52;c;host clipboard contents".to_vec(),
                truncated: false,
            },
            RuntimeEvent::SerialChunk {
                run: RunId(2),
                bytes: b"\x07\x1bPignored device-control data".to_vec(),
                truncated: false,
            },
            RuntimeEvent::SerialChunk {
                run: RunId(2),
                bytes: b"\x1b\\done\x00\x07".to_vec(),
                truncated: false,
            },
            RuntimeEvent::SerialChunk {
                run: RunId(2),
                bytes: vec![0xe2, 0x82],
                truncated: false,
            },
            RuntimeEvent::SerialChunk {
                run: RunId(2),
                bytes: vec![0xac],
                truncated: false,
            },
            RuntimeEvent::SerialChunk {
                run: RunId(2),
                bytes: vec![0xff],
                truncated: false,
            },
        ]));
        assert_eq!(model.console, "hello world\ndone��€�");
        assert!(model
            .console
            .chars()
            .all(|character| !character.is_control() || character == '\n'));
    }

    #[test]
    fn runtime_truncation_resets_an_incomplete_escape_sequence() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
            run: RunId(2),
            bytes: b"\x1b]unfinished terminal string".to_vec(),
            truncated: false,
        }]));
        assert!(model.console.is_empty());

        model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
            run: RunId(2),
            bytes: b"recovered after dropped bytes".to_vec(),
            truncated: true,
        }]));
        assert!(model.console.ends_with("recovered after dropped bytes"));
    }

    #[test]
    fn console_reports_runtime_and_local_scrollback_truncation_after_output_floods() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        for _ in 0..160 {
            model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
                run: RunId(2),
                bytes: vec![b'x'; 1024],
                truncated: false,
            }]));
        }
        assert!(model.console.len() <= MAX_CONSOLE_BYTES);
        assert!(model.console_truncated);
        assert!(model.console.starts_with(CONSOLE_TRUNCATED_NOTICE));

        model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
            run: RunId(2),
            bytes: b"last chunk".to_vec(),
            truncated: true,
        }]));
        assert!(model
            .console
            .contains("[serial output was truncated by the runtime buffer]"));
        assert!(model.console.ends_with("last chunk"));
    }

    #[test]
    fn pause_clear_and_resume_keep_the_display_and_scrollback_bounded() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
            run: RunId(2),
            bytes: b"before pause\n".to_vec(),
            truncated: false,
        }]));

        model.toggle_console_pause();
        model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
            run: RunId(2),
            bytes: b"while paused\n".to_vec(),
            truncated: false,
        }]));
        assert_eq!(model.visible_console(), "before pause\n");
        assert!(model.console.contains("while paused"));

        model.scroll_console_up(10);
        model.clear_console();
        assert!(model.visible_console().is_empty());
        assert_eq!(model.console_scroll_offset, 0);
        model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
            run: RunId(2),
            bytes: b"after clear\n".to_vec(),
            truncated: false,
        }]));
        assert!(model.visible_console().is_empty());
        model.toggle_console_pause();
        assert_eq!(model.visible_console(), "after clear\n");
    }

    #[test]
    fn running_state_requires_and_displays_explicit_guest_ready_evidence() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![RuntimeEvent::StateChanged {
            run: RunId(2),
            state: LifecycleState::Running,
        }]));
        assert!(
            matches!(&model.state, UiState::Booting(details) if details.status.contains("waiting for profile-ready evidence"))
        );
        assert!(!model.actions().console_input);

        model.apply_poll_result(Ok(vec![RuntimeEvent::GuestReady {
            run: RunId(2),
            evidence: "rootglass-hello login prompt".into(),
        }]));
        assert!(
            matches!(&model.state, UiState::Running(details) if details.status.contains("rootglass-hello login prompt"))
        );
        assert!(model.actions().console_input);
    }

    #[test]
    fn input_errors_remain_visible_and_keep_stop_available() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![RuntimeEvent::GuestReady {
            run: RunId(2),
            evidence: "fixture login".into(),
        }]));

        model.finish_input(
            &RunId(2),
            Err(AppError {
                phase: LifecycleState::Running,
                summary: "Guest console input failed.".into(),
                detail: "serial pipe is closed".into(),
            }),
        );

        assert!(
            matches!(&model.state, UiState::Failed(failure) if failure.detail == "serial pipe is closed")
        );
        assert_eq!(model.run_id, Some(RunId(2)));
        assert!(model.actions().stop);
        assert!(!model.actions().console_input);
    }

    #[test]
    fn failed_and_unexpected_exit_events_preserve_runtime_details() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![RuntimeEvent::Failed {
            run: RunId(2),
            phase: LifecycleState::Booting,
            summary: "Kernel panic".into(),
            detail: "panic at init".into(),
        }]));
        assert!(
            matches!(&model.state, UiState::Failed(failure) if failure.summary == "Kernel panic" && failure.detail == "panic at init")
        );
        assert!(model.run_id.is_none());

        let mut exit_model = ready_model();
        let (ticket, _) = exit_model.begin_start().unwrap();
        exit_model.finish_start(&ticket, Ok(RunId(3)));
        exit_model.apply_poll_result(Ok(vec![RuntimeEvent::Exited {
            run: RunId(3),
            exit_code: Some(0),
            expected: false,
        }]));
        assert!(
            matches!(&exit_model.state, UiState::Failed(failure) if failure.summary.contains("unexpectedly") && failure.detail.contains("Some(0)"))
        );
        assert!(exit_model.run_id.is_none());
    }

    #[test]
    fn stopped_state_event_reconciles_the_active_run_and_ignores_later_events() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![
            RuntimeEvent::StateChanged {
                run: RunId(2),
                state: LifecycleState::Stopped,
            },
            RuntimeEvent::GuestReady {
                run: RunId(2),
                evidence: "late stale ready".into(),
            },
        ]));
        assert!(matches!(model.state, UiState::Ready(_)));
        assert!(model.run_id.is_none());
    }

    #[test]
    fn console_scroll_offset_is_clamped_when_the_buffer_is_cleared() {
        let mut model = ready_model();
        let (ticket, _) = model.begin_start().unwrap();
        model.finish_start(&ticket, Ok(RunId(2)));
        model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
            run: RunId(2),
            bytes: (0..30)
                .map(|line| format!("line {line}\n"))
                .collect::<String>()
                .into_bytes(),
            truncated: false,
        }]));
        model.scroll_console_up(10);
        assert_eq!(model.console_scroll_offset, 10);
        model.clear_console();
        assert!(model.console.len() <= MAX_CONSOLE_BYTES);
        assert_eq!(model.console_scroll_offset, 0);
    }

    #[test]
    fn import_result_is_ignored_if_a_newer_operation_started() {
        let mut model = model();
        let import = model.begin_import(PathBuf::from("old.glass")).unwrap();
        let current_path = PathBuf::from("new.glass");
        let current = model.begin_inspection(current_path.clone()).unwrap();
        let completion = ImportCompletion {
            installed: installed(Path::new("installed/old")),
            inspection: InspectionOutcome::Ready(preview(Path::new("installed/old"))),
        };
        assert!(!model.finish_import(&import, Ok(completion)));
        assert!(model.finish_inspection(&current, InspectionOutcome::Ready(preview(&current_path))));
        assert!(model.installed_glasses.is_empty());
        assert_eq!(model.selected_path, Some(current_path));
    }

    #[test]
    fn errors_remain_visible_and_actionable() {
        let mut model = model();
        let ticket = model.begin_inspection(PathBuf::from("bad.glass")).unwrap();
        assert!(model.finish_inspection(
            &ticket,
            InspectionOutcome::Failed(FailureDetails {
                path: Some(PathBuf::from("bad.glass")),
                phase: "Verification".into(),
                summary: app_error().summary,
                detail: app_error().detail,
            }),
        ));
        assert!(
            matches!(&model.state, UiState::Failed(error) if error.summary == "operation failed")
        );
        assert!(model.actions().open);
        assert!(!model.actions().boot);
    }
}
