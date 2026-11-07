use crate::renderer::render_with_prompt;
use crate::{
    execute_developer_request, lock_service, DeveloperAction, DeveloperPanel, DeveloperRequest,
    FailureDetails, ImportCompletion, InspectionOutcome, OperationTicket, SharedService,
    StartEffect, StopRequest, UiModel, UiState,
};
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use rootglass_app::registry::InstalledGlass;
use rootglass_app::{AppError, RunId, RuntimeEvent};
use rootglass_core::LifecycleState;
use std::io::{self, IsTerminal, Stdout, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const TICK_RATE: Duration = Duration::from_millis(50);
const MAX_PENDING_SERVICE_REQUESTS: usize = 64;
const MAX_INPUT_BYTES: usize = 8_192;

/// Run the same Rootglass TUI session in a real terminal or a full-duplex PTY.
/// The controller and renderer remain independent of this terminal adapter.
pub fn run_terminal(initial_path: Option<PathBuf>) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "rootglass tui requires an interactive terminal on stdin and stdout; run it in a terminal or attach a full-duplex PTY.".to_string(),
        );
    }

    let mut model = UiModel::default();
    let mut restore = TerminalRestore::enter(CrosstermTerminalOps::new())
        .map_err(|error| format!("could not initialize terminal: {error}"))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(|error| format!("could not create the Ratatui terminal: {error}"))?;

    let loop_result = panic::catch_unwind(AssertUnwindSafe(|| {
        run_session(&mut terminal, &mut model, initial_path)
    }));
    let cleanup_result = stop_active_guest(&mut model);
    drop(terminal);
    let restore_result = restore.restore();

    match loop_result {
        Ok(result) => combine_cleanup(result, cleanup_result, restore_result),
        Err(payload) => {
            let _ = cleanup_result;
            let _ = restore_result;
            panic::resume_unwind(payload)
        }
    }
}

fn run_session(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    model: &mut UiModel,
    initial_path: Option<PathBuf>,
) -> Result<(), String> {
    let mut worker = ServiceWorker::spawn(model.shared_service())?;
    let mut poll_pending = false;
    worker.refresh_library()?;
    if let Some(path) = initial_path {
        open_path(model, path, &mut worker)?;
    }

    let session_result = panic::catch_unwind(AssertUnwindSafe(|| {
        run_event_loop(terminal, model, &mut worker, &mut poll_pending)
    }));
    let cleanup_result = worker.shutdown_session(model, &mut poll_pending);
    drop(worker);
    match session_result {
        Ok(result) => combine_cleanup(result, cleanup_result, Ok(())),
        Err(payload) => {
            let _ = cleanup_result;
            panic::resume_unwind(payload)
        }
    }
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    model: &mut UiModel,
    worker: &mut ServiceWorker,
    poll_pending: &mut bool,
) -> Result<(), String> {
    let mut input = InputBuffer::default();
    loop {
        worker.drain_ready(model, poll_pending)?;
        let prompt = input.prompt();
        terminal
            .draw(|frame| render_with_prompt(frame, model, prompt.as_deref()))
            .map_err(|error| format!("could not render the TUI: {error}"))?;

        if event::poll(TICK_RATE).map_err(|error| format!("terminal input failed: {error}"))? {
            match event::read().map_err(|error| format!("terminal input failed: {error}"))? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(model, &mut input, key, worker)? {
                        break;
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if model.run_id.is_some() && !*poll_pending {
            worker.poll_events()?;
            *poll_pending = true;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum InputMode {
    #[default]
    Normal,
    OpenPath,
    LibraryFilter,
    Console,
    ConfirmRemoval,
    DeveloperMenu,
    DeveloperSource,
    DeveloperKeyFile,
    DeveloperCreatedAt,
    DeveloperOutput,
}

#[derive(Default)]
struct InputBuffer {
    mode: InputMode,
    text: String,
    developer_form: Option<DeveloperForm>,
}

struct DeveloperForm {
    action: DeveloperAction,
    source: Option<PathBuf>,
    key_file: Option<PathBuf>,
    created_at: Option<String>,
}

impl InputBuffer {
    fn prompt(&self) -> Option<String> {
        match self.mode {
            InputMode::Normal => None,
            InputMode::OpenPath => Some(format!(
                "Bundle path: {}  (Enter opens, Esc cancels)",
                self.text
            )),
            InputMode::LibraryFilter => Some(format!(
                "Library filter: {}  (Enter applies, Esc cancels)",
                self.text
            )),
            InputMode::Console => Some(format!(
                "Guest input: {}  (Enter sends line, Esc returns to controls, Ctrl-C cancels/interrupts)",
                self.text
            )),
            InputMode::ConfirmRemoval => Some(format!(
                "Remove {}?  [y] confirm  [n/Esc] cancel",
                self.text
            )),
            InputMode::DeveloperMenu => Some(
                "Developer signing/package: [s] sign directory  [p] create .glass ZIP  [Esc] back"
                    .to_string(),
            ),
            InputMode::DeveloperSource => Some(format!(
                ".glass directory path: {}  (Enter continues, Esc cancels)",
                self.text
            )),
            InputMode::DeveloperKeyFile => Some(format!(
                "External secret-file path (key bytes stay in the worker): {}  (Enter continues, Esc cancels)",
                self.text
            )),
            InputMode::DeveloperCreatedAt => Some(format!(
                "Deterministic created-at RFC3339 (blank uses current time): {}  (Enter continues, Esc cancels)",
                self.text
            )),
            InputMode::DeveloperOutput => Some(format!(
                "Output canonical .glass path: {}  (Enter packages, Esc cancels)",
                self.text
            )),
        }
    }

    fn clear(&mut self) {
        self.mode = InputMode::Normal;
        self.text.clear();
        self.developer_form = None;
    }

    fn push(&mut self, character: char) {
        if self.text.len() + character.len_utf8() <= MAX_INPUT_BYTES {
            self.text.push(character);
        }
    }
}

fn handle_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
        return Ok(true);
    }
    if key.code == KeyCode::Char('q')
        && matches!(input.mode, InputMode::Normal | InputMode::DeveloperMenu)
    {
        return Ok(true);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('c')
        && input.mode != InputMode::Console
    {
        return Ok(true);
    }

    if input.mode == InputMode::Normal && model.developer_is_open() {
        return handle_developer_panel_key(model, input, key);
    }

    match input.mode {
        InputMode::OpenPath => handle_path_key(model, input, key, worker),
        InputMode::LibraryFilter => handle_filter_key(model, input, key),
        InputMode::Console => handle_console_key(model, input, key, worker),
        InputMode::ConfirmRemoval => handle_remove_confirmation_key(model, input, key, worker),
        InputMode::DeveloperMenu => handle_developer_menu_key(model, input, key),
        InputMode::DeveloperSource
        | InputMode::DeveloperKeyFile
        | InputMode::DeveloperCreatedAt
        | InputMode::DeveloperOutput => handle_developer_form_key(model, input, key, worker),
        InputMode::Normal => handle_normal_key(model, input, key, worker),
    }
}

fn handle_developer_panel_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
) -> Result<bool, String> {
    if matches!(model.developer_panel, DeveloperPanel::Working(_)) {
        return Ok(false);
    }
    match key.code {
        KeyCode::Esc => {
            model.close_developer();
        }
        KeyCode::Char('s') | KeyCode::Char('p') => {
            model.developer_panel = DeveloperPanel::Menu;
            input.mode = InputMode::DeveloperMenu;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_developer_menu_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
) -> Result<bool, String> {
    match key.code {
        KeyCode::Esc => {
            model.close_developer();
            input.clear();
        }
        KeyCode::Char('s') => {
            input.developer_form = Some(DeveloperForm {
                action: DeveloperAction::SignDirectory,
                source: None,
                key_file: None,
                created_at: None,
            });
            input.text.clear();
            input.mode = InputMode::DeveloperSource;
        }
        KeyCode::Char('p') => {
            input.developer_form = Some(DeveloperForm {
                action: DeveloperAction::PackageArchive,
                source: None,
                key_file: None,
                created_at: None,
            });
            input.text.clear();
            input.mode = InputMode::DeveloperSource;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_developer_form_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    if key.code == KeyCode::Esc {
        input.text.clear();
        input.developer_form = None;
        input.mode = InputMode::DeveloperMenu;
        return Ok(false);
    }

    match key.code {
        KeyCode::Enter => {
            let value = std::mem::take(&mut input.text);
            let form = input
                .developer_form
                .as_mut()
                .ok_or_else(|| "the developer workflow form was lost".to_string())?;
            match input.mode {
                InputMode::DeveloperSource if !value.trim().is_empty() => {
                    form.source = Some(PathBuf::from(value));
                    input.mode = InputMode::DeveloperKeyFile;
                }
                InputMode::DeveloperKeyFile if !value.trim().is_empty() => {
                    form.key_file = Some(PathBuf::from(value));
                    input.mode = InputMode::DeveloperCreatedAt;
                }
                InputMode::DeveloperCreatedAt => {
                    form.created_at = (!value.trim().is_empty()).then(|| value.trim().to_string());
                    if form.action == DeveloperAction::PackageArchive {
                        input.mode = InputMode::DeveloperOutput;
                    } else {
                        submit_developer(model, input, worker)?;
                    }
                }
                InputMode::DeveloperOutput if !value.trim().is_empty() => {
                    let output = PathBuf::from(value);
                    let request = developer_request(input, Some(output))?;
                    worker.developer(request)?;
                    let action = input
                        .developer_form
                        .as_ref()
                        .expect("request came from a developer form")
                        .action;
                    model.begin_developer(action);
                    input.clear();
                }
                _ => input.text = value,
            }
        }
        KeyCode::Backspace => {
            input.text.pop();
        }
        KeyCode::Char(character) if accepts_text_key(key) => input.push(character),
        _ => {}
    }
    Ok(false)
}

fn submit_developer(
    model: &mut UiModel,
    input: &mut InputBuffer,
    worker: &mut ServiceWorker,
) -> Result<(), String> {
    let request = developer_request(input, None)?;
    worker.developer(request)?;
    let action = input
        .developer_form
        .as_ref()
        .expect("request came from a developer form")
        .action;
    model.begin_developer(action);
    input.clear();
    Ok(())
}

fn developer_request(
    input: &InputBuffer,
    output: Option<PathBuf>,
) -> Result<DeveloperRequest, String> {
    let form = input
        .developer_form
        .as_ref()
        .ok_or_else(|| "the developer workflow form was lost".to_string())?;
    Ok(DeveloperRequest {
        action: form.action,
        source: form
            .source
            .clone()
            .ok_or_else(|| "a .glass directory path is required".to_string())?,
        key_file: form
            .key_file
            .clone()
            .ok_or_else(|| "an external signing-key file path is required".to_string())?,
        output,
        created_at: form.created_at.clone(),
    })
}

fn handle_path_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    match key.code {
        KeyCode::Esc => input.clear(),
        KeyCode::Enter => {
            let path = std::mem::take(&mut input.text);
            input.clear();
            if !path.trim().is_empty() {
                open_path(model, PathBuf::from(path), worker)?;
            }
        }
        KeyCode::Backspace => {
            input.text.pop();
        }
        KeyCode::Char(character) if accepts_text_key(key) => input.push(character),
        _ => {}
    }
    Ok(false)
}

fn handle_console_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if input.text.is_empty() {
            if let Some(run) = model
                .run_id
                .clone()
                .filter(|_| model.actions().console_input)
            {
                worker.write_serial(run, vec![0x03])?;
            }
        } else {
            input.text.clear();
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc => input.clear(),
        KeyCode::Enter => {
            let text = std::mem::take(&mut input.text);
            input.clear();
            if let Some((run, bytes)) = model.input_request(&text) {
                worker.write_serial(run, bytes)?;
            }
        }
        KeyCode::Backspace => {
            input.text.pop();
        }
        KeyCode::Char(character) if accepts_text_key(key) => input.push(character),
        _ => {}
    }
    Ok(false)
}

fn handle_filter_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
) -> Result<bool, String> {
    match key.code {
        KeyCode::Esc => input.clear(),
        KeyCode::Enter => {
            model.set_library_filter(std::mem::take(&mut input.text));
            input.clear();
        }
        KeyCode::Backspace => {
            input.text.pop();
        }
        KeyCode::Char(character) if accepts_text_key(key) => input.push(character),
        _ => {}
    }
    Ok(false)
}

fn handle_remove_confirmation_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => input.clear(),
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            input.clear();
            remove_selected(model, worker)?;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_normal_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('o') if can_open(model) => input.mode = InputMode::OpenPath,
        KeyCode::Char('v') if model.actions().validate => validate_selected(model, worker)?,
        KeyCode::Char('p') if model.actions().plan => plan_selected(model, worker)?,
        KeyCode::Char('b') if model.actions().boot => start_guest(model, worker)?,
        KeyCode::Char('s') if model.actions().stop => stop_guest(model, worker)?,
        KeyCode::Char('r') if model.actions().restart => restart_guest(model, worker)?,
        KeyCode::Char('d') if model.actions().remove => {
            if let Some(glass) = model.selected_installed() {
                input.text = format!("{} {}", glass.name, glass.version);
                input.mode = InputMode::ConfirmRemoval;
            }
        }
        KeyCode::Char('/') => {
            input.text = model.library_filter.clone();
            input.mode = InputMode::LibraryFilter;
        }
        KeyCode::Char('f') if model.actions().refresh => worker.refresh_library()?,
        KeyCode::Char('D') if model.actions().developer => {
            model.open_developer();
            input.mode = InputMode::DeveloperMenu;
        }
        KeyCode::Char('P') if model.actions().console_pause => model.toggle_console_pause(),
        KeyCode::Char('c') if model.actions().console_clear => model.clear_console(),
        KeyCode::Char('y') if model.actions().console_copy => request_console_copy(model),
        KeyCode::Enter if model.actions().console_input => input.mode = InputMode::Console,
        KeyCode::Up => select_installed(model, false, worker)?,
        KeyCode::Down => select_installed(model, true, worker)?,
        KeyCode::PageUp if model.actions().console_pause => model.scroll_console_up(10),
        KeyCode::PageDown if model.actions().console_pause => model.scroll_console_down(10),
        _ => {}
    }
    Ok(false)
}

fn can_open(model: &UiModel) -> bool {
    model.actions().open
}

fn accepts_text_key(key: KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn request_console_copy(model: &mut UiModel) {
    model.console_notice = Some(match copy_console_to_clipboard(model.visible_console()) {
        Ok((bytes, truncated)) if truncated => {
            format!("copy request sent for the last {bytes} bytes; host policy may restrict it")
        }
        Ok((bytes, _)) => {
            format!("copy request sent for {bytes} bytes; host policy may restrict it")
        }
        Err(error) => format!("clipboard request failed: {error}"),
    });
}

fn copy_console_to_clipboard(console: &str) -> Result<(usize, bool), String> {
    const MAX_COPY_BYTES: usize = 60_000;
    let mut start = console.len().saturating_sub(MAX_COPY_BYTES);
    while !console.is_char_boundary(start) {
        start += 1;
    }
    let contents = &console[start..];
    let encoded = encode_base64(contents.as_bytes());
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| error.to_string())?;
    Ok((contents.len(), start > 0))
}

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn select_installed(
    model: &mut UiModel,
    down: bool,
    worker: &mut ServiceWorker,
) -> Result<(), String> {
    let visible = model.visible_installed();
    if visible.is_empty() || !can_open(model) {
        return Ok(());
    }
    let current = model
        .selected_path
        .as_ref()
        .and_then(|path| visible.iter().position(|glass| &glass.package_path == path));
    let index = match (current, down) {
        (Some(index), true) => (index + 1).min(visible.len() - 1),
        (Some(index), false) => index.saturating_sub(1),
        (None, true) => 0,
        (None, false) => visible.len() - 1,
    };
    open_path(model, visible[index].package_path.clone(), worker)
}

fn open_path(model: &mut UiModel, path: PathBuf, worker: &mut ServiceWorker) -> Result<(), String> {
    if let Some(ticket) = model.begin_open(path.clone()) {
        worker.open(ticket, path)?;
    }
    Ok(())
}

fn start_guest(model: &mut UiModel, worker: &mut ServiceWorker) -> Result<(), String> {
    if let Some((ticket, path)) = model.begin_start() {
        worker.start(ticket, path)?;
    }
    Ok(())
}

fn validate_selected(model: &mut UiModel, worker: &mut ServiceWorker) -> Result<(), String> {
    let Some(path) = model.selected_path.clone() else {
        return Ok(());
    };
    if let Some(ticket) = model.begin_validation(path.clone()) {
        worker.validate_manifest(ticket, path)?;
    }
    Ok(())
}

fn plan_selected(model: &mut UiModel, worker: &mut ServiceWorker) -> Result<(), String> {
    let Some(path) = model.selected_path.clone() else {
        return Ok(());
    };
    if let Some(ticket) = model.begin_plan(path.clone()) {
        worker.plan_bundle(ticket, path)?;
    }
    Ok(())
}

fn remove_selected(model: &mut UiModel, worker: &mut ServiceWorker) -> Result<(), String> {
    if let Some((ticket, digest)) = model.begin_remove() {
        worker.remove_installed(ticket, digest)?;
    }
    Ok(())
}

fn stop_guest(model: &mut UiModel, worker: &mut ServiceWorker) -> Result<(), String> {
    if let Some(StopRequest::StopRun { ticket, run }) = model.begin_stop() {
        worker.stop(ticket, run)?;
    }
    Ok(())
}

fn restart_guest(model: &mut UiModel, worker: &mut ServiceWorker) -> Result<(), String> {
    if let Some((ticket, run, path)) = model.begin_restart() {
        worker.restart(ticket, run, path)?;
    }
    Ok(())
}

fn stop_active_guest(model: &mut UiModel) -> Result<(), String> {
    let Some(run) = model.run_id.clone() else {
        return Ok(());
    };
    let service = model.shared_service();
    let result = match lock_service(&service) {
        Ok(mut service) => service.stop(&run),
        Err(error) => Err(error),
    };
    result.map_err(|error| error.to_string())
}

enum WorkerRequest {
    RefreshLibrary,
    Open(OperationTicket, PathBuf),
    ValidateManifest(OperationTicket, PathBuf),
    PlanBundle(OperationTicket, PathBuf),
    RemoveInstalled(OperationTicket, String),
    Start(OperationTicket, PathBuf),
    Stop(OperationTicket, RunId),
    Restart(OperationTicket, RunId, PathBuf),
    PollEvents,
    WriteSerial(RunId, Vec<u8>),
    Developer(DeveloperRequest),
    StopOrphan(RunId),
    Shutdown,
}

enum OpenedBundle {
    Cancelled,
    Inspected(Box<InspectionOutcome>),
    Imported(Box<Result<ImportCompletion, AppError>>),
}

enum WorkerCompletion {
    Library(Result<Vec<InstalledGlass>, AppError>),
    Opened(OperationTicket, OpenedBundle),
    Validated(
        OperationTicket,
        Result<rootglass_app::ManifestValidationReport, AppError>,
    ),
    Planned(OperationTicket, Result<crate::LaunchPlanDetails, AppError>),
    Removed(OperationTicket, Result<Option<InstalledGlass>, AppError>),
    Started(OperationTicket, Result<RunId, AppError>),
    Stopped(OperationTicket, Result<(), AppError>),
    Restarted(OperationTicket, Result<RunId, AppError>),
    Polled(Result<Vec<RuntimeEvent>, AppError>),
    Written(RunId, Result<(), AppError>),
    Developer(Result<crate::DeveloperResult, String>),
    OrphanStopped(RunId, Result<(), AppError>),
}

struct ServiceWorker {
    requests: SyncSender<WorkerRequest>,
    completions: Receiver<WorkerCompletion>,
    thread: Option<JoinHandle<()>>,
    in_flight: usize,
}

impl ServiceWorker {
    fn spawn(service: SharedService) -> Result<Self, String> {
        let (request_tx, request_rx) = mpsc::sync_channel(MAX_PENDING_SERVICE_REQUESTS);
        let (completion_tx, completion_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("rootglass-tui-service".to_string())
            .spawn(move || service_worker_loop(service, request_rx, completion_tx))
            .map_err(|error| format!("could not start the TUI service worker: {error}"))?;
        Ok(Self {
            requests: request_tx,
            completions: completion_rx,
            thread: Some(thread),
            in_flight: 0,
        })
    }

    fn send(&mut self, request: WorkerRequest) -> Result<(), String> {
        if self.in_flight >= MAX_PENDING_SERVICE_REQUESTS {
            return Err("the TUI service request queue is full; wait for current work".to_string());
        }
        self.requests
            .send(request)
            .map_err(|_| "the TUI service worker stopped unexpectedly".to_string())?;
        self.in_flight += 1;
        Ok(())
    }

    fn refresh_library(&mut self) -> Result<(), String> {
        self.send(WorkerRequest::RefreshLibrary)
    }

    fn open(&mut self, ticket: OperationTicket, path: PathBuf) -> Result<(), String> {
        self.send(WorkerRequest::Open(ticket, path))
    }

    fn validate_manifest(&mut self, ticket: OperationTicket, path: PathBuf) -> Result<(), String> {
        self.send(WorkerRequest::ValidateManifest(ticket, path))
    }

    fn plan_bundle(&mut self, ticket: OperationTicket, path: PathBuf) -> Result<(), String> {
        self.send(WorkerRequest::PlanBundle(ticket, path))
    }

    fn remove_installed(&mut self, ticket: OperationTicket, digest: String) -> Result<(), String> {
        self.send(WorkerRequest::RemoveInstalled(ticket, digest))
    }

    fn start(&mut self, ticket: OperationTicket, path: PathBuf) -> Result<(), String> {
        self.send(WorkerRequest::Start(ticket, path))
    }

    fn stop(&mut self, ticket: OperationTicket, run: RunId) -> Result<(), String> {
        self.send(WorkerRequest::Stop(ticket, run))
    }

    fn restart(
        &mut self,
        ticket: OperationTicket,
        run: RunId,
        path: PathBuf,
    ) -> Result<(), String> {
        self.send(WorkerRequest::Restart(ticket, run, path))
    }

    fn poll_events(&mut self) -> Result<(), String> {
        self.send(WorkerRequest::PollEvents)
    }

    fn write_serial(&mut self, run: RunId, bytes: Vec<u8>) -> Result<(), String> {
        self.send(WorkerRequest::WriteSerial(run, bytes))
    }

    fn developer(&mut self, request: DeveloperRequest) -> Result<(), String> {
        self.send(WorkerRequest::Developer(request))
    }

    fn stop_orphan(&mut self, run: RunId) -> Result<(), String> {
        self.send(WorkerRequest::StopOrphan(run))
    }

    fn drain_ready(&mut self, model: &mut UiModel, poll_pending: &mut bool) -> Result<(), String> {
        loop {
            match self.completions.try_recv() {
                Ok(completion) => {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    let _ = self.apply_completion(model, poll_pending, completion)?;
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err("the TUI service worker stopped unexpectedly".to_string());
                }
            }
        }
    }

    #[cfg(test)]
    fn drain_until_idle(&mut self, model: &mut UiModel) -> Result<(), String> {
        let mut poll_pending = false;
        while self.in_flight > 0 {
            let completion = self
                .completions
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| format!("waiting for the TUI service worker failed: {error}"))?;
            self.in_flight -= 1;
            let _ = self.apply_completion(model, &mut poll_pending, completion)?;
        }
        Ok(())
    }

    fn shutdown_session(
        &mut self,
        model: &mut UiModel,
        poll_pending: &mut bool,
    ) -> Result<(), String> {
        let mut stop_attempted = Vec::new();
        let mut cleanup_errors = Vec::new();
        loop {
            if let Some(run) = model.run_id.clone() {
                if !stop_attempted.contains(&run) {
                    if let Some(StopRequest::StopRun { ticket, run }) = model.begin_stop() {
                        stop_attempted.push(run.clone());
                        self.stop(ticket, run)?;
                    }
                }
            } else if matches!(model.state, UiState::Booting(_)) {
                let _ = model.begin_stop();
            }

            let run_is_stopped = match model.run_id.as_ref() {
                None => true,
                Some(run) => stop_attempted.contains(run),
            };
            if self.in_flight == 0 && run_is_stopped {
                break;
            }

            match self.completions.recv_timeout(Duration::from_secs(10)) {
                Ok(completion) => {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    if let Some(error) = self.apply_completion(model, poll_pending, completion)? {
                        cleanup_errors.push(error);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    cleanup_errors.push(
                        "timed out waiting for outstanding TUI operations to finish".to_string(),
                    );
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    cleanup_errors.push("the TUI service worker stopped unexpectedly".to_string());
                    break;
                }
            }
        }

        if let Some(run) = &model.run_id {
            cleanup_errors.push(format!("guest run {} may still be active", run.0));
        }
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(cleanup_errors.join("; "))
        }
    }

    fn apply_completion(
        &mut self,
        model: &mut UiModel,
        poll_pending: &mut bool,
        completion: WorkerCompletion,
    ) -> Result<Option<String>, String> {
        match completion {
            WorkerCompletion::Library(result) => model.finish_library_refresh(result),
            WorkerCompletion::Opened(_, OpenedBundle::Cancelled) => {}
            WorkerCompletion::Opened(ticket, OpenedBundle::Inspected(result)) => {
                model.finish_inspection(&ticket, *result);
            }
            WorkerCompletion::Opened(ticket, OpenedBundle::Imported(result)) => {
                let imported = result.is_ok();
                if !model.finish_import(&ticket, *result) && imported {
                    self.refresh_library()?;
                }
            }
            WorkerCompletion::Validated(ticket, result) => {
                model.finish_validation(&ticket, result);
            }
            WorkerCompletion::Planned(ticket, result) => {
                model.finish_plan(&ticket, result);
            }
            WorkerCompletion::Removed(ticket, result) => {
                model.finish_remove(&ticket, result);
            }
            WorkerCompletion::Started(ticket, result) => {
                if let StartEffect::StopOrphan(run) = model.finish_start(&ticket, result) {
                    if let Err(error) = self.stop_orphan(run.clone()) {
                        model.run_id = Some(run);
                        return Err(error);
                    }
                }
            }
            WorkerCompletion::Stopped(ticket, result) => {
                let error = result.as_ref().err().map(ToString::to_string);
                model.finish_stop(&ticket, result);
                return Ok(error);
            }
            WorkerCompletion::Restarted(ticket, result) => {
                if let StartEffect::StopOrphan(run) = model.finish_restart(&ticket, result) {
                    if let Err(error) = self.stop_orphan(run.clone()) {
                        model.run_id = Some(run);
                        return Err(error);
                    }
                }
            }
            WorkerCompletion::Polled(result) => {
                *poll_pending = false;
                model.apply_poll_result(result);
            }
            WorkerCompletion::Written(run, result) => model.finish_input(&run, result),
            WorkerCompletion::Developer(result) => model.finish_developer(result),
            WorkerCompletion::OrphanStopped(run, result) => {
                let error = result.as_ref().err().map(ToString::to_string);
                if let Some(detail) = error.clone() {
                    model.run_id = Some(run);
                    model.state = UiState::Failed(FailureDetails {
                        path: model.selected_path.clone(),
                        phase: "Cleanup".to_string(),
                        summary: "A canceled guest start could not be stopped.".to_string(),
                        detail,
                    });
                }
                return Ok(error);
            }
        }
        Ok(None)
    }
}

impl Drop for ServiceWorker {
    fn drop(&mut self) {
        let _ = self.requests.send(WorkerRequest::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn service_worker_loop(
    service: SharedService,
    requests: Receiver<WorkerRequest>,
    completions: Sender<WorkerCompletion>,
) {
    while let Ok(request) = requests.recv() {
        let completion = match request {
            WorkerRequest::RefreshLibrary => Some(WorkerCompletion::Library(with_service(
                &service,
                |service| service.installed_glasses(),
            ))),
            WorkerRequest::Open(ticket, path) => {
                let opened = if ticket.is_cancelled() {
                    OpenedBundle::Cancelled
                } else if path.is_dir() {
                    let result =
                        with_service(&service, |service| Ok(service.inspect_bundle(&path)))
                            .unwrap_or_else(|error| {
                                InspectionOutcome::Failed(failure_from(&path, error))
                            });
                    OpenedBundle::Inspected(Box::new(result))
                } else {
                    OpenedBundle::Imported(Box::new(with_service(&service, |service| {
                        service.import_and_inspect(&path)
                    })))
                };
                Some(WorkerCompletion::Opened(ticket, opened))
            }
            WorkerRequest::ValidateManifest(ticket, path) => Some(WorkerCompletion::Validated(
                ticket,
                with_service(&service, |service| service.validate_manifest(&path)),
            )),
            WorkerRequest::PlanBundle(ticket, path) => Some(WorkerCompletion::Planned(
                ticket,
                with_service(&service, |service| service.plan_bundle(&path)),
            )),
            WorkerRequest::RemoveInstalled(ticket, digest) => Some(WorkerCompletion::Removed(
                ticket,
                with_service(&service, |service| service.remove_installed(&digest)),
            )),
            WorkerRequest::Start(ticket, path) => {
                let result = if ticket.is_cancelled() {
                    Err(AppError {
                        phase: LifecycleState::Starting,
                        summary: "The start operation was canceled.".to_string(),
                        detail: "The TUI canceled this operation before guest startup.".to_string(),
                    })
                } else {
                    with_service(&service, |service| service.start(&path))
                };
                Some(WorkerCompletion::Started(ticket, result))
            }
            WorkerRequest::Stop(ticket, run) => Some(WorkerCompletion::Stopped(
                ticket,
                with_service(&service, |service| service.stop(&run)),
            )),
            WorkerRequest::Restart(ticket, run, path) => Some(WorkerCompletion::Restarted(
                ticket,
                with_service(&service, |service| service.restart(&run, &path)),
            )),
            WorkerRequest::PollEvents => Some(WorkerCompletion::Polled(with_service(
                &service,
                |service| service.poll_events(),
            ))),
            WorkerRequest::WriteSerial(run, bytes) => Some(WorkerCompletion::Written(
                run.clone(),
                with_service(&service, |service| service.write_serial(&run, &bytes)),
            )),
            WorkerRequest::Developer(request) => Some(WorkerCompletion::Developer(
                execute_developer_request(request),
            )),
            WorkerRequest::StopOrphan(run) => Some(WorkerCompletion::OrphanStopped(
                run.clone(),
                with_service(&service, |service| service.stop(&run)),
            )),
            WorkerRequest::Shutdown => break,
        };
        if let Some(completion) = completion {
            if completions.send(completion).is_err() {
                break;
            }
        }
    }
}

fn with_service<T>(
    service: &SharedService,
    operation: impl FnOnce(&mut dyn crate::TuiService) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let mut service = lock_service(service)?;
    operation(service.as_mut())
}

fn failure_from(path: &Path, error: AppError) -> FailureDetails {
    FailureDetails {
        path: Some(path.to_path_buf()),
        phase: format!("{:?}", error.phase),
        summary: error.summary,
        detail: error.detail,
    }
}

fn combine_cleanup(
    main: Result<(), String>,
    guest: Result<(), String>,
    terminal: io::Result<()>,
) -> Result<(), String> {
    let terminal = terminal.map_err(|error| format!("could not restore terminal: {error}"));
    let errors = [main.err(), guest.err(), terminal.err()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; cleanup: "))
    }
}

trait TerminalOps {
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    fn disable_raw_mode(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn hide_cursor(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
}

struct CrosstermTerminalOps {
    stdout: Stdout,
}

impl CrosstermTerminalOps {
    fn new() -> Self {
        Self {
            stdout: io::stdout(),
        }
    }
}

impl TerminalOps for CrosstermTerminalOps {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        terminal::disable_raw_mode()
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        execute!(self.stdout, EnterAlternateScreen)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(self.stdout, LeaveAlternateScreen)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        execute!(self.stdout, crossterm::cursor::Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(self.stdout, Show)
    }
}

struct TerminalRestore<O: TerminalOps> {
    operations: O,
    raw_mode: bool,
    alternate_screen: bool,
    cursor_hidden: bool,
}

impl<O: TerminalOps> TerminalRestore<O> {
    fn enter(operations: O) -> io::Result<Self> {
        let mut restore = Self {
            operations,
            raw_mode: false,
            alternate_screen: false,
            cursor_hidden: false,
        };
        restore.raw_mode = true;
        restore.operations.enable_raw_mode()?;
        restore.alternate_screen = true;
        restore.cursor_hidden = true;
        restore.operations.enter_alternate_screen()?;
        restore.operations.hide_cursor()?;
        Ok(restore)
    }

    fn restore(&mut self) -> io::Result<()> {
        let mut first_error = None;
        if self.cursor_hidden || self.alternate_screen {
            keep_first_error(&mut first_error, self.operations.show_cursor());
        }
        if self.alternate_screen {
            keep_first_error(&mut first_error, self.operations.leave_alternate_screen());
        }
        if self.raw_mode {
            keep_first_error(&mut first_error, self.operations.disable_raw_mode());
        }
        self.cursor_hidden = false;
        self.alternate_screen = false;
        self.raw_mode = false;
        first_error.map_or(Ok(()), Err)
    }
}

impl<O: TerminalOps> Drop for TerminalRestore<O> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn keep_first_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GlassPreview, PreflightDetails, SignatureStatus, TuiService, UiState};
    use rootglass_app::registry::InstalledGlass;
    use rootglass_app::{AppError, AppService, RunId, RuntimeEvent};
    use rootglass_core::runtime_assets::RuntimeAssetsResolver;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;
    use std::fs;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone)]
    struct FakeTerminalOps {
        events: Rc<RefCell<Vec<&'static str>>>,
        fail_at: Option<&'static str>,
    }

    impl FakeTerminalOps {
        fn record(&self, event: &'static str) -> io::Result<()> {
            self.events.borrow_mut().push(event);
            if self.fail_at == Some(event) {
                Err(io::Error::other(format!("failed at {event}")))
            } else {
                Ok(())
            }
        }
    }

    impl TerminalOps for FakeTerminalOps {
        fn enable_raw_mode(&mut self) -> io::Result<()> {
            self.record("raw-on")
        }

        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.record("raw-off")
        }

        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.record("alternate-on")
        }

        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.record("alternate-off")
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.record("cursor-hide")
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.record("cursor-show")
        }
    }

    #[test]
    fn terminal_modes_are_restored_on_normal_drop() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let guard = TerminalRestore::enter(FakeTerminalOps {
            events: Rc::clone(&events),
            fail_at: None,
        })
        .expect("terminal initializes");

        drop(guard);

        assert_eq!(
            *events.borrow(),
            [
                "raw-on",
                "alternate-on",
                "cursor-hide",
                "cursor-show",
                "alternate-off",
                "raw-off"
            ]
        );
    }

    #[test]
    fn terminal_modes_are_restored_during_unwind() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let guard = TerminalRestore::enter(FakeTerminalOps {
            events: Rc::clone(&events),
            fail_at: None,
        })
        .expect("terminal initializes");

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = guard;
            panic!("simulated session panic");
        }));

        assert!(result.is_err());
        assert_eq!(
            &events.borrow()[3..],
            ["cursor-show", "alternate-off", "raw-off"]
        );
    }

    #[test]
    fn partial_terminal_initialization_restores_raw_mode() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let result = TerminalRestore::enter(FakeTerminalOps {
            events: Rc::clone(&events),
            fail_at: Some("alternate-on"),
        });

        assert!(result.is_err());
        assert_eq!(
            *events.borrow(),
            [
                "raw-on",
                "alternate-on",
                "cursor-show",
                "alternate-off",
                "raw-off"
            ]
        );
    }

    #[derive(Clone, Default)]
    struct FakeService {
        calls: Arc<Mutex<Vec<String>>>,
        glasses: Vec<InstalledGlass>,
        import_entered: Option<Arc<Barrier>>,
        import_release: Option<Arc<Barrier>>,
        start_entered: Option<Arc<Barrier>>,
        start_release: Option<Arc<Barrier>>,
    }

    impl TuiService for FakeService {
        fn installed_glasses(&mut self) -> Result<Vec<InstalledGlass>, AppError> {
            Ok(self.glasses.clone())
        }

        fn remove_installed(
            &mut self,
            content_sha256: &str,
        ) -> Result<Option<InstalledGlass>, AppError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove:{content_sha256}"));
            Ok(self
                .glasses
                .iter()
                .find(|glass| glass.content_sha256 == content_sha256)
                .cloned())
        }

        fn import_archive(&mut self, source: &Path) -> Result<InstalledGlass, AppError> {
            if let Some(import_entered) = &self.import_entered {
                import_entered.wait();
            }
            if let Some(import_release) = &self.import_release {
                import_release.wait();
            }
            let installed = installed(source);
            self.glasses.push(installed.clone());
            Ok(installed)
        }

        fn inspect_bundle(&mut self, source: &Path) -> InspectionOutcome {
            InspectionOutcome::Ready(preview(source.to_path_buf()))
        }

        fn start(&mut self, _source: &Path) -> Result<RunId, AppError> {
            self.calls.lock().unwrap().push("start".to_string());
            if let Some(start_entered) = &self.start_entered {
                start_entered.wait();
            }
            if let Some(start_release) = &self.start_release {
                start_release.wait();
            }
            Ok(RunId(7))
        }

        fn stop(&mut self, _run: &RunId) -> Result<(), AppError> {
            self.calls.lock().unwrap().push("stop".to_string());
            Ok(())
        }

        fn poll_events(&mut self) -> Result<Vec<RuntimeEvent>, AppError> {
            Ok(Vec::new())
        }

        fn write_serial(&mut self, _run: &RunId, bytes: &[u8]) -> Result<(), AppError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("serial:{}", String::from_utf8_lossy(bytes)));
            Ok(())
        }
    }

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rootglass-tui-workflows-{}-{nonce}",
            std::process::id()
        ))
    }

    #[cfg(unix)]
    fn signed_test_bundle(root: &Path) -> PathBuf {
        use ed25519_dalek::SigningKey;

        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello-world.glass");
        let bundle = root.join("hello-world.glass");
        fs::create_dir_all(bundle.join("metadata")).unwrap();
        fs::create_dir_all(bundle.join("signatures")).unwrap();
        for name in ["README.md", "metadata/README.md"] {
            fs::copy(example.join(name), bundle.join(name)).unwrap();
        }
        let manifest = fs::read_to_string(example.join("rootglass.toml"))
            .unwrap()
            .replace("writable_overlay = \"state.img\"\n", "");
        fs::write(bundle.join("rootglass.toml"), manifest).unwrap();
        fs::write(bundle.join("rootfs.img"), b"synthetic test root filesystem").unwrap();
        let seed: [u8; 32] =
            Sha256::digest(b"Rootglass public development fixture signing key v1").into();
        rootglass_signature::sign_glass_directory_at(
            &bundle,
            &SigningKey::from_bytes(&seed),
            "2026-11-07T00:00:00Z",
        )
        .unwrap();
        bundle
    }

    #[cfg(unix)]
    fn fixture_trust_store() -> rootglass_signature::TrustStore {
        use ed25519_dalek::SigningKey;
        use rootglass_signature::TrustedPublisher;

        let seed: [u8; 32] =
            Sha256::digest(b"Rootglass public development fixture signing key v1").into();
        let signing_key = SigningKey::from_bytes(&seed);
        let key_id = Sha256::digest(signing_key.verifying_key().to_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        rootglass_signature::TrustStore::new([TrustedPublisher {
            name: "Rootglass Examples".into(),
            identity: "rootglass.dev/examples".into(),
            key_id: format!("ed25519:{key_id}"),
            public_key_base64url: "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw".into(),
        }])
    }

    fn installed(path: &Path) -> InstalledGlass {
        InstalledGlass {
            content_sha256: "abc123".into(),
            package_path: path.to_path_buf(),
            identifier: "org.example.hello".into(),
            name: "Hello Guest".into(),
            version: "1.0.0".into(),
            publisher_name: "Example Publisher".into(),
            publisher_identity: "example.test".into(),
            key_id: "fixture".into(),
            original_source: PathBuf::from("hello.glass"),
            imported_at_unix_seconds: 1,
        }
    }

    #[cfg(unix)]
    fn install_spawn_probe(root: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let resource_root = root.join("resources");
        let version_root = resource_root.join("runtime-assets/tui-workflow-test");
        fs::create_dir_all(&version_root).unwrap();
        fs::write(version_root.join("kernel"), b"test kernel").unwrap();
        fs::write(version_root.join("initramfs"), b"test initramfs").unwrap();
        let qemu = version_root.join("qemu");
        fs::write(
            &qemu,
            b"#!/bin/sh\ntouch \"$(dirname \"$0\")/spawned\"\nexit 0\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&qemu).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&qemu, permissions).unwrap();

        let digest = |path: &Path| {
            Sha256::digest(fs::read(path).unwrap())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let manifest = json!({
            "schema_version": 1,
            "runtime_version": "tui-workflow-test",
            "assets": [
                {"kind": "kernel", "path": "kernel", "sha256": digest(&version_root.join("kernel"))},
                {"kind": "initramfs", "path": "initramfs", "sha256": digest(&version_root.join("initramfs"))},
                {"kind": "qemu", "path": "qemu", "sha256": digest(&qemu)}
            ]
        });
        fs::write(
            version_root.join("assets.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        (resource_root, qemu.with_file_name("spawned"))
    }

    #[cfg(unix)]
    #[test]
    fn real_app_service_separates_validation_from_plan_and_blocks_untrusted_start() {
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        let (resource_root, spawn_marker) = install_spawn_probe(&root);
        let source = signed_test_bundle(&root);
        let runtime = || RuntimeAssetsResolver::new(&resource_root, root.join("runtime-data"));

        // The fixture key is used only in this test to exercise a successful plan.
        let trusted = AppService::with_trust_store(fixture_trust_store())
            .with_library_root(root.join("trusted-library"))
            .with_state_root(root.join("trusted-state"))
            .with_verified_runtime_assets(runtime(), "tui-workflow-test");
        let mut trusted_model = UiModel::with_service(trusted, UiState::Empty);
        trusted_model.selected_path = Some(source.clone());
        let mut trusted_worker =
            ServiceWorker::spawn(trusted_model.shared_service()).expect("worker starts");
        let mut trusted_input = InputBuffer::default();
        handle_key(
            &mut trusted_model,
            &mut trusted_input,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
            &mut trusted_worker,
        )
        .expect("plan key is handled");
        trusted_worker
            .drain_until_idle(&mut trusted_model)
            .expect("trusted plan completes");
        assert!(
            matches!(trusted_model.state, UiState::Planned(_)),
            "trusted plan state: {:?}",
            trusted_model.state
        );
        assert!(trusted_model.actions().boot);

        let denied = AppService::deny_all()
            .with_library_root(root.join("denied-library"))
            .with_state_root(root.join("denied-state"))
            .with_verified_runtime_assets(runtime(), "tui-workflow-test");
        let mut denied_model = UiModel::with_service(denied, UiState::Empty);
        denied_model.selected_path = Some(source.clone());
        let shared_service = denied_model.shared_service();
        let mut denied_worker =
            ServiceWorker::spawn(shared_service.clone()).expect("worker starts");
        let mut denied_input = InputBuffer::default();
        handle_key(
            &mut denied_model,
            &mut denied_input,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            &mut denied_worker,
        )
        .expect("validate key is handled");
        denied_worker
            .drain_until_idle(&mut denied_model)
            .expect("manifest validation completes");
        assert!(matches!(denied_model.state, UiState::ManifestValidated(_)));
        assert!(!denied_model.actions().boot);

        handle_key(
            &mut denied_model,
            &mut denied_input,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
            &mut denied_worker,
        )
        .expect("plan key is handled");
        denied_worker
            .drain_until_idle(&mut denied_model)
            .expect("untrusted plan is rejected");
        assert!(matches!(
            &denied_model.state,
            UiState::Failed(failure) if failure.detail.contains("unknown signing key")
        ));
        assert!(!denied_model.actions().boot);

        let start_error = lock_service(&shared_service)
            .unwrap()
            .start(&source)
            .expect_err("deny-all service rejects the fixture before spawn");
        assert!(start_error.detail.contains("unknown signing key"));
        assert!(
            !spawn_marker.exists(),
            "untrusted input reached the fake QEMU spawn"
        );

        let invalid_source = signed_test_bundle(&root.join("invalid-case"));
        fs::write(invalid_source.join("rootglass.toml"), b"not valid TOML").unwrap();
        let mut invalid_service = AppService::with_trust_store(fixture_trust_store())
            .with_library_root(root.join("invalid-library"))
            .with_state_root(root.join("invalid-state"))
            .with_verified_runtime_assets(runtime(), "tui-workflow-test");
        let invalid_error = invalid_service
            .start(&invalid_source, rootglass_app::RunOptions::default())
            .expect_err("invalid manifest fails before spawn");
        assert!(!invalid_error.detail.is_empty());
        assert!(
            !spawn_marker.exists(),
            "invalid input reached the fake QEMU spawn"
        );

        let tampered_source = signed_test_bundle(&root.join("tampered-case"));
        fs::write(
            tampered_source.join("rootfs.img"),
            b"tampered after signing",
        )
        .unwrap();
        let mut tampered_service = AppService::with_trust_store(fixture_trust_store())
            .with_library_root(root.join("tampered-library"))
            .with_state_root(root.join("tampered-state"))
            .with_verified_runtime_assets(runtime(), "tui-workflow-test");
        let tampered_error = tampered_service
            .start(&tampered_source, rootglass_app::RunOptions::default())
            .expect_err("tampered input fails before spawn");
        assert!(tampered_error.detail.contains("subject digest mismatch"));
        assert!(
            !spawn_marker.exists(),
            "tampered input reached the fake QEMU spawn"
        );

        drop(trusted_worker);
        drop(denied_worker);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn library_removal_requires_confirmation_and_runs_on_the_worker() {
        let release_path = PathBuf::from("/library/hello.glass");
        let release = installed(&release_path);
        let service = FakeService {
            glasses: vec![release.clone()],
            ..FakeService::default()
        };
        let calls = Arc::clone(&service.calls);
        let mut model = UiModel::with_service(service, UiState::Empty);
        model.installed_glasses = vec![release.clone()];
        model.selected_path = Some(release_path);
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");
        let mut input = InputBuffer::default();

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("remove key opens confirmation");
        assert_eq!(input.mode, InputMode::ConfirmRemoval);
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("negative answer cancels removal");
        assert!(calls.lock().unwrap().is_empty());

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("remove key opens confirmation again");
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("positive answer schedules removal");
        worker
            .drain_until_idle(&mut model)
            .expect("removal service call completes");

        assert_eq!(
            *calls.lock().unwrap(),
            [format!("remove:{}", release.content_sha256)]
        );
        assert!(model.installed_glasses.is_empty());
        assert!(model.selected_path.is_none());
    }

    #[test]
    fn stale_import_completion_refreshes_library_without_replacing_selection() {
        let root = temp_root();
        let imported_path = root.join("old.glass");
        let selected_path = root.join("current.glass");
        fs::create_dir_all(&selected_path).unwrap();
        fs::write(&imported_path, b"archive fixture placeholder").unwrap();
        let import_entered = Arc::new(Barrier::new(2));
        let import_release = Arc::new(Barrier::new(2));
        let service = FakeService {
            import_entered: Some(Arc::clone(&import_entered)),
            import_release: Some(Arc::clone(&import_release)),
            ..FakeService::default()
        };
        let mut model = UiModel::with_service(service, UiState::Empty);
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        open_path(&mut model, imported_path.clone(), &mut worker).expect("import is scheduled");
        import_entered.wait();
        open_path(&mut model, selected_path.clone(), &mut worker)
            .expect("current selection is scheduled");
        import_release.wait();
        worker
            .drain_until_idle(&mut model)
            .expect("import, inspection, and follow-up refresh complete");

        assert_eq!(model.selected_path, Some(selected_path.clone()));
        assert!(matches!(
            &model.state,
            UiState::Ready(preview) if preview.path == selected_path
        ));
        assert_eq!(model.installed_glasses.len(), 1);
        assert_eq!(model.installed_glasses[0].package_path, imported_path);
        drop(worker);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn directory_and_archive_type_checks_run_on_the_service_worker() {
        let root = temp_root();
        let directory = root.join("directory.glass");
        let archive = root.join("archive.glass");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&archive, b"zip fixture placeholder").unwrap();
        let mut model = UiModel::with_service(FakeService::default(), UiState::Empty);
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        open_path(&mut model, directory.clone(), &mut worker).expect("directory open is queued");
        assert!(matches!(model.state, UiState::Opening(_)));
        worker
            .drain_until_idle(&mut model)
            .expect("directory inspection completes");
        assert!(matches!(model.state, UiState::Ready(_)));

        open_path(&mut model, archive.clone(), &mut worker).expect("archive open is queued");
        assert!(matches!(model.state, UiState::Opening(_)));
        worker
            .drain_until_idle(&mut model)
            .expect("archive import and inspection complete");
        assert!(matches!(model.state, UiState::Ready(_)));
        assert_eq!(model.installed_glasses.len(), 1);
        assert_eq!(model.installed_glasses[0].package_path, archive);
        drop(worker);
        let _ = fs::remove_dir_all(root);
    }

    fn preview(path: PathBuf) -> GlassPreview {
        GlassPreview {
            path,
            name: "Example".to_string(),
            identifier: "example.test".to_string(),
            version: "1.0.0".to_string(),
            description: "A deterministic fixture".to_string(),
            publisher: "Rootglass tests".to_string(),
            signature: SignatureStatus::Verified("fixture".to_string()),
            preflight: PreflightDetails {
                architecture: "aarch64".to_string(),
                profile: "virt".to_string(),
                memory_mb: 512,
                cpus: 1,
                qemu: "qemu-system-aarch64".to_string(),
                acceleration: "TCG".to_string(),
                runtime_assets: Vec::new(),
                blockers: Vec::new(),
            },
        }
    }

    #[test]
    fn boot_and_stop_keys_call_the_shared_service() {
        let service = FakeService::default();
        let calls = Arc::clone(&service.calls);
        let path = PathBuf::from("example.glass");
        let mut model = UiModel::with_service(service, UiState::Ready(preview(path.clone())));
        model.selected_path = Some(path);
        let mut input = InputBuffer::default();
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        assert!(!handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("boot key is handled"));
        worker
            .drain_until_idle(&mut model)
            .expect("start operation finishes");
        assert!(matches!(model.state, UiState::Booting(_)));

        assert!(!handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("stop key is handled"));
        worker
            .drain_until_idle(&mut model)
            .expect("stop operation finishes");
        assert_eq!(*calls.lock().unwrap(), ["start", "stop"]);
    }

    #[test]
    fn console_prompt_sends_line_to_the_active_guest() {
        let service = FakeService::default();
        let calls = Arc::clone(&service.calls);
        let mut model = UiModel::with_service(
            service,
            UiState::Running(crate::RunDetails {
                name: "Example".to_string(),
                status: "Running".to_string(),
                elapsed: String::new(),
            }),
        );
        model.run_id = Some(RunId(7));
        let mut input = InputBuffer {
            mode: InputMode::Console,
            text: String::new(),
            developer_form: None,
        };
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("console text is accepted");
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("console text is accepted");
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut worker,
        )
        .expect("console line is sent");
        worker
            .drain_until_idle(&mut model)
            .expect("serial write finishes");

        assert_eq!(*calls.lock().unwrap(), ["serial:id\n"]);
        assert_eq!(input.mode, InputMode::Normal);
    }

    #[test]
    fn developer_signing_flow_reads_key_path_and_keeps_key_bytes_out_of_ui_state() {
        let temp = temp_root();
        fs::create_dir_all(&temp).expect("create test directory");
        let key_file = temp.join("publisher.seed");
        let private_material = b"PRIVATE-KEY-MATERIAL-MUST-NEVER-REACH-THE-UI";
        fs::write(&key_file, private_material).expect("write external test key");
        let source = temp.join("missing.glass");

        let mut model = UiModel::with_service(FakeService::default(), UiState::Empty);
        let mut input = InputBuffer::default();
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");
        let mut press = |key: KeyEvent| {
            handle_key(&mut model, &mut input, key, &mut worker).expect("developer key is handled");
        };

        press(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE));
        press(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        for character in source.to_string_lossy().chars() {
            press(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        press(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for character in key_file.to_string_lossy().chars() {
            press(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        press(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        press(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        drop(press);
        assert_eq!(input.mode, InputMode::Normal);
        assert!(matches!(model.developer_panel, DeveloperPanel::Working(_)));
        worker
            .drain_until_idle(&mut model)
            .expect("developer workflow finishes on the worker");

        let panel = format!("{:?}", model.developer_panel);
        assert!(panel.contains("failed to read manifest"));
        assert!(!panel.contains(std::str::from_utf8(private_material).unwrap()));
        drop(worker);
        fs::remove_dir_all(temp).expect("remove test directory");
    }

    #[test]
    fn ctrl_c_quits_outside_console_and_ctrl_q_quits_from_any_mode() {
        let mut model = UiModel::with_service(FakeService::default(), UiState::Empty);
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");
        let mut input = InputBuffer {
            mode: InputMode::OpenPath,
            text: "unfinished".to_string(),
            developer_form: None,
        };

        assert!(handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut worker,
        )
        .expect("Ctrl-C is handled as a clean quit"));
        assert_eq!(input.text, "unfinished");

        input.mode = InputMode::Console;
        assert!(handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &mut worker,
        )
        .expect("Ctrl-Q quits from console mode"));

        model.open_developer();
        input.mode = InputMode::DeveloperMenu;
        assert!(handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("q quits from the developer menu"));
    }

    #[test]
    fn ctrl_c_interrupts_a_running_guest_from_console_mode() {
        let service = FakeService::default();
        let calls = Arc::clone(&service.calls);
        let mut model = UiModel::with_service(
            service,
            UiState::Running(crate::RunDetails {
                name: "Example".to_string(),
                status: "Running".to_string(),
                elapsed: String::new(),
            }),
        );
        model.run_id = Some(RunId(7));
        let mut input = InputBuffer {
            mode: InputMode::Console,
            text: String::new(),
            developer_form: None,
        };
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        assert!(!handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut worker,
        )
        .expect("Ctrl-C is sent to the guest"));
        worker
            .drain_until_idle(&mut model)
            .expect("interrupt write finishes");

        assert_eq!(*calls.lock().unwrap(), ["serial:\u{3}"]);
        assert_eq!(input.mode, InputMode::Console);
    }

    #[test]
    fn base64_clipboard_encoding_matches_standard_vectors() {
        assert_eq!(encode_base64(b"Rootglass"), "Um9vdGdsYXNz");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
    }

    #[test]
    fn session_cleanup_stops_a_guest_started_by_the_fake_service() {
        let service = FakeService::default();
        let calls = Arc::clone(&service.calls);
        let path = PathBuf::from("example.glass");
        let mut model = UiModel::with_service(service, UiState::Ready(preview(path.clone())));
        model.selected_path = Some(path);
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");
        start_guest(&mut model, &mut worker).expect("start is scheduled");
        worker
            .drain_until_idle(&mut model)
            .expect("start operation finishes");

        let mut poll_pending = false;
        worker
            .shutdown_session(&mut model, &mut poll_pending)
            .expect("shutdown stops active guest");

        assert_eq!(*calls.lock().unwrap(), ["start", "stop"]);
        assert!(model.run_id.is_none());
    }

    #[test]
    fn cancel_during_start_stops_the_guest_if_start_finishes_late() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let start_entered = Arc::new(Barrier::new(2));
        let start_release = Arc::new(Barrier::new(2));
        let service = FakeService {
            calls: Arc::clone(&calls),
            glasses: Vec::new(),
            import_entered: None,
            import_release: None,
            start_entered: Some(Arc::clone(&start_entered)),
            start_release: Some(Arc::clone(&start_release)),
        };
        let path = PathBuf::from("example.glass");
        let mut model = UiModel::with_service(service, UiState::Ready(preview(path.clone())));
        model.selected_path = Some(path);
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");
        let mut input = InputBuffer::default();
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("start key is accepted");

        start_entered.wait();
        stop_guest(&mut model, &mut worker).expect("start can be canceled");
        start_release.wait();
        let mut poll_pending = false;
        worker
            .shutdown_session(&mut model, &mut poll_pending)
            .expect("late guest start is stopped");

        assert_eq!(*calls.lock().unwrap(), ["start", "stop"]);
        assert!(model.run_id.is_none());
    }

    #[test]
    fn non_interactive_stdio_returns_actionable_error() {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            let error = run_terminal(None).expect_err("captured test stdio is not a terminal");
            assert!(error.contains("requires an interactive terminal"));
            assert!(error.contains("full-duplex PTY"));
        }
    }
}
