use rootglass_app::registry::InstalledGlass;
use rootglass_app::{AppError, AppService, PreflightReport, RunId, RunOptions, RuntimeEvent};
use rootglass_core::LifecycleState;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const MAX_CONSOLE_BYTES: usize = 80_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiState {
    Empty,
    Loading,
    Importing(PathBuf),
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
    pub boot: bool,
    pub stop: bool,
    pub restart: bool,
    /// State management is intentionally disabled until the named-state UI is implemented.
    pub reset_state: bool,
    pub console_input: bool,
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
    fn start(&mut self, source: &Path) -> Result<RunId, AppError>;
    fn stop(&mut self, run: &RunId) -> Result<(), AppError>;
    fn poll_events(&mut self) -> Result<Vec<RuntimeEvent>, AppError>;
    fn write_serial(&mut self, run: &RunId, bytes: &[u8]) -> Result<(), AppError>;

    fn import_and_inspect(&mut self, source: &Path) -> Result<ImportCompletion, AppError> {
        let installed = self.import_archive(source)?;
        let inspection = self.inspect_bundle(&installed.package_path);
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
                    reason: error.summary,
                    preflight: PreflightDetails {
                        blockers: vec![error.detail],
                        ..empty_preflight()
                    },
                });
            }
            Err(error) => return InspectionOutcome::Failed(failure_details(source, error)),
        };
        InspectionOutcome::Ready(preview_from_report(source, summary.publisher_name, report))
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
}

pub struct UiModel {
    pub state: UiState,
    pub installed_glasses: Vec<InstalledGlass>,
    pub library_error: Option<String>,
    pub selected_path: Option<PathBuf>,
    pub run_id: Option<RunId>,
    pub console: String,
    service: SharedService,
    next_generation: u64,
    active_operation: Option<OperationTicket>,
    last_preview: Option<GlassPreview>,
    ansi: AnsiStripper,
}

impl UiModel {
    pub fn with_service(service: impl TuiService + 'static, initial_state: UiState) -> Self {
        Self {
            state: initial_state,
            installed_glasses: Vec::new(),
            library_error: None,
            selected_path: None,
            run_id: None,
            console: String::new(),
            service: Arc::new(Mutex::new(Box::new(service))),
            next_generation: 0,
            active_operation: None,
            last_preview: None,
            ansi: AnsiStripper::default(),
        }
    }

    pub fn shared_service(&self) -> SharedService {
        Arc::clone(&self.service)
    }

    pub fn actions(&self) -> UiActions {
        let booting = matches!(self.state, UiState::Booting(_));
        let running = matches!(self.state, UiState::Running(_));
        let stopping = matches!(self.state, UiState::Stopping(_));
        let ready = matches!(self.state, UiState::Ready(_));
        let has_run = self.run_id.is_some();
        UiActions {
            open: !stopping && !booting && !has_run,
            boot: ready && !has_run,
            stop: !stopping && (has_run || booting),
            restart: running && has_run,
            reset_state: false,
            console_input: running && has_run,
        }
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
        Some(self.begin_operation(UiState::Loading))
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
        self.console.clear();
        self.ansi.reset();
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
                self.console.clear();
                self.ansi.reset();
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
                RuntimeEvent::SerialChunk { run, bytes, .. } if run == current_run => {
                    self.append_console(&bytes);
                }
                RuntimeEvent::GuestReady { run, .. } if run == current_run => {
                    let name = self
                        .last_preview
                        .as_ref()
                        .map(|preview| preview.name.clone())
                        .unwrap_or_else(|| "Selected glass".to_string());
                    self.state = UiState::Running(RunDetails {
                        name,
                        status: "Running".to_string(),
                        elapsed: String::new(),
                    });
                }
                RuntimeEvent::Exited {
                    run,
                    exit_code,
                    expected,
                } if run == current_run => {
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
                } if run == current_run => {
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

    fn append_console(&mut self, bytes: &[u8]) {
        let clean = self.ansi.push(bytes);
        self.console.push_str(&String::from_utf8_lossy(&clean));
        if self.console.len() > MAX_CONSOLE_BYTES {
            let mut drain = self.console.len() - MAX_CONSOLE_BYTES;
            while !self.console.is_char_boundary(drain) {
                drain += 1;
            }
            self.console.drain(..drain);
        }
    }
}

impl Default for UiModel {
    fn default() -> Self {
        let (service, state) = match local_trust_config_path() {
            Ok(path) => match AppService::from_trust_config(&path) {
                Ok(service) => (service, UiState::Empty),
                Err(error) => (
                    AppService::deny_all(),
                    trust_config_failure(format!("{} ({})", error, path.display())),
                ),
            },
            Err(error) => (AppService::deny_all(), trust_config_failure(error)),
        };
        Self::with_service(service, state)
    }
}

fn failure_details(source: &Path, error: AppError) -> FailureDetails {
    FailureDetails {
        path: Some(source.to_path_buf()),
        phase: format!("{:?}", error.phase),
        summary: error.summary,
        detail: error.detail,
    }
}

fn preview_from_report(source: &Path, publisher: String, report: PreflightReport) -> GlassPreview {
    let preflight = preflight_details(&report);
    let summary = report.summary;
    GlassPreview {
        path: source.to_path_buf(),
        name: summary.name,
        identifier: summary.identifier,
        version: summary.version,
        description: summary.description,
        publisher,
        signature: SignatureStatus::Verified(summary.publisher_identity),
        preflight,
    }
}

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
    PreflightDetails {
        architecture: report.summary.architecture.clone(),
        profile: report.summary.profile.clone(),
        memory_mb: report.summary.memory_mb,
        cpus: report.summary.cpus,
        qemu: "Resolved by the launch plan; executable check pending".to_string(),
        acceleration: "Selected by host preflight; capability detail pending".to_string(),
        runtime_assets: report.readiness.clone(),
        blockers: vec![],
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
                AnsiState::Escape if (0x40..=0x5f).contains(&byte) => AnsiState::Text,
                AnsiState::Escape => AnsiState::Text,
                AnsiState::Csi if (0x40..=0x7e).contains(&byte) => AnsiState::Text,
                AnsiState::Csi => AnsiState::Csi,
                AnsiState::Osc if byte == 0x07 => AnsiState::Text,
                AnsiState::Osc if byte == 0x1b => AnsiState::OscEscape,
                AnsiState::Osc => AnsiState::Osc,
                AnsiState::OscEscape if byte == b'\\' => AnsiState::Text,
                AnsiState::OscEscape if byte == 0x07 => AnsiState::Text,
                AnsiState::OscEscape if byte == 0x1b => AnsiState::OscEscape,
                AnsiState::OscEscape => AnsiState::Osc,
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
    fn console_strips_split_ansi_sequences_preserves_lines_and_is_bounded() {
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
                bytes: b"1m world\x1b[0m\r\n".to_vec(),
                truncated: false,
            },
        ]));
        assert_eq!(model.console, "hello world\r\n");
        model.apply_poll_result(Ok(vec![RuntimeEvent::SerialChunk {
            run: RunId(2),
            bytes: vec![b'x'; MAX_CONSOLE_BYTES + 10],
            truncated: false,
        }]));
        assert!(model.console.len() <= MAX_CONSOLE_BYTES);
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
