use crate::renderer::render_with_prompt;
use crate::{
    execute_developer_request, lock_service, DeveloperAction, DeveloperPanel, DeveloperRequest,
    FailureDetails, ImportCompletion, InspectionOutcome, OperationTicket, SharedService,
    StartEffect, StopRequest, UiModel, UiState,
};
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use rootglass_app::registry::InstalledGlass;
use rootglass_app::{AppError, RunId, RuntimeEvent};
use rootglass_core::LifecycleState;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Stdout, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TICK_RATE: Duration = Duration::from_millis(50);
const CONSOLE_QUERY_WAIT: Duration = Duration::from_millis(200);
const MAX_PENDING_SERVICE_REQUESTS: usize = 64;
const MAX_INPUT_BYTES: usize = 8_192;
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_POSITION_RESPONSE: &[u8] = b"\x1b[1;80R";

pub(crate) fn trace_tui(message: impl AsRef<str>) {
    static TRACE_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let Some(file) = TRACE_FILE.get_or_init(|| {
        let path = std::env::var_os("ROOTGLASS_TUI_TRACE_FILE")?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    }) else {
        return;
    };
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{}", message.as_ref());
    }
}

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
    worker.refresh_trust()?;
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
        worker.flush_expired_console_input()?;
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
                Event::Paste(text) => handle_paste(&mut input, &text),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if model.run_id.is_some() && !*poll_pending {
            trace_tui(format!(
                "queue poll for run {}",
                model.run_id.as_ref().map(|run| run.0).unwrap_or_default()
            ));
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
    StateCreateName,
    StateCloneName,
    ConfirmStateReset,
    ConfirmStateDelete,
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
    state_id: Option<String>,
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
            InputMode::StateCreateName => Some(format!(
                "New state name: {}  (Enter creates, Esc cancels)",
                self.text
            )),
            InputMode::StateCloneName => Some(format!(
                "Clone {} as: {}  (Enter clones, Esc cancels)",
                self.state_id.as_deref().unwrap_or("state"),
                self.text
            )),
            InputMode::ConfirmStateReset => Some(format!(
                "Reset state {} from the immutable template?  [y] confirm  [n/Esc] cancel",
                self.state_id.as_deref().unwrap_or("state")
            )),
            InputMode::ConfirmStateDelete => Some(format!(
                "Delete state {}?  [y] confirm  [n/Esc] cancel",
                self.state_id.as_deref().unwrap_or("state")
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
        self.state_id = None;
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
    if key.code == KeyCode::Char('b') {
        trace_tui(format!(
            "shortcut input: action=boot, mode={:?}, developer_panel_open={}, operations_panel_open={}, boot_enabled={}",
            input.mode,
            model.developer_is_open(),
            model.operations_panel_open,
            model.actions().boot,
        ));
    }
    if matches!(key.code, KeyCode::Char('v') | KeyCode::Char('p')) {
        let shortcut = match key.code {
            KeyCode::Char('v') => "validate",
            _ => "plan",
        };
        trace_tui(format!(
            "shortcut input: action={shortcut}, mode={:?}, operations_panel_open={}, validate_enabled={}, plan_enabled={}",
            input.mode,
            model.operations_panel_open,
            model.actions().validate,
            model.actions().plan,
        ));
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
        return Ok(true);
    }
    if key.code == KeyCode::Char('q')
        && (input.mode == InputMode::DeveloperMenu
            || (input.mode == InputMode::Normal && !model.operations_panel_open))
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
    if input.mode == InputMode::Normal && model.operations_panel_open {
        return handle_operations_panel_key(model, input, key, worker);
    }

    match input.mode {
        InputMode::OpenPath => handle_path_key(model, input, key, worker),
        InputMode::LibraryFilter => handle_filter_key(model, input, key),
        InputMode::Console => handle_console_key(model, input, key, worker),
        InputMode::ConfirmRemoval => handle_remove_confirmation_key(model, input, key, worker),
        InputMode::StateCreateName
        | InputMode::StateCloneName
        | InputMode::ConfirmStateReset
        | InputMode::ConfirmStateDelete => handle_state_input_key(model, input, key, worker),
        InputMode::DeveloperMenu => handle_developer_menu_key(model, input, key),
        InputMode::DeveloperSource
        | InputMode::DeveloperKeyFile
        | InputMode::DeveloperCreatedAt
        | InputMode::DeveloperOutput => handle_developer_form_key(model, input, key, worker),
        InputMode::Normal => handle_normal_key(model, input, key, worker),
    }
}

fn handle_paste(input: &mut InputBuffer, text: &str) {
    if matches!(
        input.mode,
        InputMode::Normal
            | InputMode::ConfirmRemoval
            | InputMode::ConfirmStateReset
            | InputMode::ConfirmStateDelete
            | InputMode::DeveloperMenu
    ) {
        return;
    }

    for character in text.chars().filter(|character| !character.is_control()) {
        input.push(character);
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
            trace_tui("developer panel closed");
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
            trace_tui("developer form opened: action=SignDirectory");
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
            trace_tui("developer form opened: action=PackageArchive");
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
                    let source = PathBuf::from(value);
                    trace_tui(format!(
                        "developer source accepted: directory={}, manifest_exists={}",
                        source.is_dir(),
                        source.join("rootglass.toml").is_file(),
                    ));
                    form.source = Some(source);
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
                worker.write_console_input(run, bytes)?;
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

fn handle_operations_panel_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('t') => model.operations_panel_open = false,
        KeyCode::Up => model.move_state_cursor(false),
        KeyCode::Down => model.move_state_cursor(true),
        KeyCode::Char('c') if model.actions().state_create => {
            trace_tui("state create prompt opened");
            input.text.clear();
            input.mode = InputMode::StateCreateName;
        }
        KeyCode::Char('l') if model.actions().state_clone => {
            if let Some(state) = model.focused_state() {
                input.text.clear();
                input.state_id = Some(state.id.clone());
                input.mode = InputMode::StateCloneName;
            }
        }
        KeyCode::Char('s') if model.actions().state_select => {
            if let Some(state) = model.focused_state() {
                let state_id = state.id.clone();
                if state_id != model.selected_state_id {
                    if let Some(path) = model.selected_path.clone() {
                        request_state_action(
                            model,
                            worker,
                            path,
                            StateOperation::Select(state_id),
                        )?;
                    }
                }
            }
        }
        KeyCode::Char('r') if model.actions().reset_state => {
            if let Some(state) = model.focused_state() {
                input.state_id = Some(state.id.clone());
                input.mode = InputMode::ConfirmStateReset;
            }
        }
        KeyCode::Char('d') if model.actions().state_delete => {
            if let Some(state) = model.focused_state() {
                input.state_id = Some(state.id.clone());
                input.mode = InputMode::ConfirmStateDelete;
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_state_input_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    match input.mode {
        InputMode::ConfirmStateReset | InputMode::ConfirmStateDelete => match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => input.clear(),
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let Some(state_id) = input.state_id.take() else {
                    input.clear();
                    return Ok(false);
                };
                let path = model.selected_path.clone();
                let operation = if input.mode == InputMode::ConfirmStateReset {
                    StateOperation::Reset(state_id.clone())
                } else {
                    StateOperation::Delete(state_id.clone())
                };
                input.clear();
                if let Some(path) = path {
                    request_state_action(model, worker, path, operation)?;
                }
            }
            _ => {}
        },
        InputMode::StateCreateName | InputMode::StateCloneName => match key.code {
            KeyCode::Esc => input.clear(),
            KeyCode::Enter => {
                let name = std::mem::take(&mut input.text);
                let operation = if input.mode == InputMode::StateCreateName {
                    StateOperation::Create(name.trim().to_string())
                } else {
                    let source_id = input.state_id.clone().unwrap_or_default();
                    StateOperation::Clone {
                        source_id,
                        new_name: name.trim().to_string(),
                    }
                };
                input.clear();
                if let Some(path) = model.selected_path.clone() {
                    request_state_action(model, worker, path, operation)?;
                }
            }
            KeyCode::Backspace => {
                input.text.pop();
            }
            KeyCode::Char(character) if accepts_text_key(key) => input.push(character),
            _ => {}
        },
        _ => {}
    }
    Ok(false)
}

fn request_state_action(
    model: &mut UiModel,
    worker: &mut ServiceWorker,
    path: PathBuf,
    operation: StateOperation,
) -> Result<(), String> {
    if !model.begin_state_operation() {
        return Ok(());
    }
    if let Err(error) = worker.state_action(path, operation) {
        model.state_busy = false;
        model.state_error = Some(error.clone());
        return Err(error);
    }
    Ok(())
}

fn refresh_state_selection(model: &mut UiModel, worker: &mut ServiceWorker) -> Result<(), String> {
    if matches!(
        model.state,
        UiState::Ready(_) | UiState::Blocked(_) | UiState::Planned(_)
    ) {
        if let Some(path) = model.selected_path.clone() {
            request_state_action(model, worker, path, StateOperation::Refresh)?;
        }
    }
    Ok(())
}

fn handle_normal_key(
    model: &mut UiModel,
    input: &mut InputBuffer,
    key: KeyEvent,
    worker: &mut ServiceWorker,
) -> Result<bool, String> {
    if matches!(model.state, UiState::PublisherReview(_))
        && matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('C')
        )
    {
        model.return_to_library();
        return Ok(false);
    }
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
        KeyCode::Char('t') => {
            model.operations_panel_open = true;
            worker.refresh_trust()?;
            if let Some(path) = model.selected_path.clone().filter(|_| {
                matches!(
                    model.state,
                    UiState::Ready(_) | UiState::Blocked(_) | UiState::Planned(_)
                )
            }) {
                request_state_action(model, worker, path, StateOperation::Refresh)?;
            }
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
    trace_tui(format!(
        "boot action dispatched: boot_enabled={}",
        model.actions().boot,
    ));
    if let Some((ticket, path)) = model.begin_start() {
        let state_id = model
            .state_loaded
            .then_some(model.selected_state_id.clone())
            .filter(|_| model.state_error.is_none());
        worker.start(ticket, path, state_id)?;
        trace_tui("queued start operation");
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
        let state_id = model
            .state_loaded
            .then_some(model.selected_state_id.clone())
            .filter(|_| model.state_error.is_none());
        worker.restart(ticket, run, path, state_id)?;
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
    RefreshTrust,
    StateAction(PathBuf, StateOperation),
    Open(OperationTicket, PathBuf),
    ValidateManifest(OperationTicket, PathBuf),
    PlanBundle(OperationTicket, PathBuf),
    RemoveInstalled(OperationTicket, String),
    Start(OperationTicket, PathBuf, Option<String>),
    Stop(OperationTicket, RunId),
    Restart(OperationTicket, RunId, PathBuf, Option<String>),
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
    Trust(Result<rootglass_app::TrustPolicyStatus, AppError>),
    States(
        PathBuf,
        StateOperation,
        Result<crate::StateSnapshot, AppError>,
    ),
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

#[derive(Clone, Debug)]
enum StateOperation {
    Refresh,
    Select(String),
    Create(String),
    Clone { source_id: String, new_name: String },
    Reset(String),
    Delete(String),
}

impl StateOperation {
    fn label(&self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::Select(_) => "select",
            Self::Create(_) => "create",
            Self::Clone { .. } => "clone",
            Self::Reset(_) => "reset",
            Self::Delete(_) => "delete",
        }
    }
}

struct ServiceWorker {
    requests: SyncSender<WorkerRequest>,
    completions: Receiver<WorkerCompletion>,
    thread: Option<JoinHandle<()>>,
    in_flight: usize,
    terminal_queries: TerminalQueryResponder,
    console_input: ConsoleInputGate,
}

#[derive(Default)]
struct TerminalQueryResponder {
    run: Option<RunId>,
    tail: Vec<u8>,
}

#[derive(Default)]
struct ConsoleInputGate {
    pending: Option<PendingConsoleInput>,
}

struct PendingConsoleInput {
    run: RunId,
    bytes: Vec<u8>,
    deadline: Instant,
}

impl ConsoleInputGate {
    fn begin(&mut self, run: RunId, bytes: Vec<u8>, now: Instant) -> Option<Vec<u8>> {
        if bytes.is_empty() {
            return None;
        }
        if let Some(pending) = self.pending.as_mut() {
            if pending.run == run {
                pending.bytes.extend_from_slice(&bytes);
                return None;
            }
        }

        let first_byte = bytes[0];
        let remainder = bytes[1..].to_vec();
        self.pending = (!remainder.is_empty()).then_some(PendingConsoleInput {
            run,
            bytes: remainder,
            deadline: now + CONSOLE_QUERY_WAIT,
        });
        Some(vec![first_byte])
    }

    fn release_after_query(&mut self, run: &RunId) -> Option<Vec<u8>> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| &pending.run == run)
        {
            self.pending.take().map(|pending| pending.bytes)
        } else {
            None
        }
    }

    fn take_expired(&mut self, now: Instant) -> Option<(RunId, Vec<u8>)> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            self.pending
                .take()
                .map(|pending| (pending.run, pending.bytes))
        } else {
            None
        }
    }

    fn cancel(&mut self, run: &RunId) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| &pending.run == run)
        {
            self.pending = None;
        }
    }
}

impl TerminalQueryResponder {
    fn cursor_position_queries(&mut self, run: &RunId, bytes: &[u8]) -> usize {
        if self.run.as_ref() != Some(run) {
            self.run = Some(run.clone());
            self.tail.clear();
        }

        let mut combined = std::mem::take(&mut self.tail);
        combined.extend_from_slice(bytes);
        let count = combined
            .windows(CURSOR_POSITION_QUERY.len())
            .filter(|window| *window == CURSOR_POSITION_QUERY)
            .count();
        let tail_start = combined
            .len()
            .saturating_sub(CURSOR_POSITION_QUERY.len() - 1);
        self.tail.extend_from_slice(&combined[tail_start..]);
        count
    }
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
            terminal_queries: TerminalQueryResponder::default(),
            console_input: ConsoleInputGate::default(),
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

    fn refresh_trust(&mut self) -> Result<(), String> {
        self.send(WorkerRequest::RefreshTrust)
    }

    fn state_action(&mut self, path: PathBuf, operation: StateOperation) -> Result<(), String> {
        self.send(WorkerRequest::StateAction(path, operation))
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

    fn start(
        &mut self,
        ticket: OperationTicket,
        path: PathBuf,
        state_id: Option<String>,
    ) -> Result<(), String> {
        self.send(WorkerRequest::Start(ticket, path, state_id))
    }

    fn stop(&mut self, ticket: OperationTicket, run: RunId) -> Result<(), String> {
        self.send(WorkerRequest::Stop(ticket, run))
    }

    fn restart(
        &mut self,
        ticket: OperationTicket,
        run: RunId,
        path: PathBuf,
        state_id: Option<String>,
    ) -> Result<(), String> {
        self.send(WorkerRequest::Restart(ticket, run, path, state_id))
    }

    fn poll_events(&mut self) -> Result<(), String> {
        self.send(WorkerRequest::PollEvents)
    }

    fn write_serial(&mut self, run: RunId, bytes: Vec<u8>) -> Result<(), String> {
        self.send(WorkerRequest::WriteSerial(run, bytes))
    }

    fn write_console_input(&mut self, run: RunId, bytes: Vec<u8>) -> Result<(), String> {
        if let Some(first_byte) = self.console_input.begin(run.clone(), bytes, Instant::now()) {
            if let Err(error) = self.write_serial(run.clone(), first_byte) {
                self.console_input.cancel(&run);
                return Err(error);
            }
        }
        Ok(())
    }

    fn flush_expired_console_input(&mut self) -> Result<(), String> {
        self.flush_expired_console_input_at(Instant::now())
    }

    fn flush_expired_console_input_at(&mut self, now: Instant) -> Result<(), String> {
        if let Some((run, bytes)) = self.console_input.take_expired(now) {
            trace_tui("released buffered console input after cursor-query timeout");
            self.write_serial(run, bytes)?;
        }
        Ok(())
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
            WorkerCompletion::Trust(result) => model.finish_trust_policy(result),
            WorkerCompletion::States(path, operation, result) => {
                let successful = result.is_ok();
                let applied = model.selected_path.as_ref() == Some(&path);
                if applied {
                    model.finish_state_operation(result);
                }
                trace_tui(format!(
                    "state completion: operation={}, successful={successful}, applied={applied}",
                    operation.label()
                ));
            }
            WorkerCompletion::Opened(_, OpenedBundle::Cancelled) => {
                trace_tui("open completion: cancelled");
            }
            WorkerCompletion::Opened(ticket, OpenedBundle::Inspected(result)) => {
                let ready = matches!(&*result, InspectionOutcome::Ready(_));
                let applied = model.finish_inspection(&ticket, *result);
                trace_tui(format!(
                    "open completion: inspected_ready={ready}, applied={applied}"
                ));
                if applied {
                    refresh_state_selection(model, self)?;
                }
            }
            WorkerCompletion::Opened(ticket, OpenedBundle::Imported(result)) => {
                let imported = result.is_ok();
                let applied = model.finish_import(&ticket, *result);
                trace_tui(format!(
                    "open completion: imported={imported}, applied={applied}"
                ));
                if !applied && imported {
                    self.refresh_library()?;
                } else if applied {
                    refresh_state_selection(model, self)?;
                }
            }
            WorkerCompletion::Validated(ticket, result) => {
                let successful = result.is_ok();
                let applied = model.finish_validation(&ticket, result);
                trace_tui(format!(
                    "validate completion: successful={successful}, applied={applied}"
                ));
            }
            WorkerCompletion::Planned(ticket, result) => {
                let successful = result.is_ok();
                let applied = model.finish_plan(&ticket, result);
                trace_tui(format!(
                    "plan completion: successful={successful}, applied={applied}"
                ));
            }
            WorkerCompletion::Removed(ticket, result) => {
                model.finish_remove(&ticket, result);
            }
            WorkerCompletion::Started(ticket, result) => {
                let result_ok = result.is_ok();
                let effect = model.finish_start(&ticket, result);
                trace_tui(format!(
                    "start completion: result_ok={result_ok}, applied={}",
                    matches!(&effect, StartEffect::Applied)
                ));
                if let StartEffect::StopOrphan(run) = effect {
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
                if let Ok(events) = &result {
                    let mut response_run = None;
                    let mut responses = Vec::new();
                    for event in events {
                        if let RuntimeEvent::SerialChunk { run, bytes, .. } = event {
                            if model.run_id.as_ref() != Some(run) {
                                continue;
                            }
                            let query_count =
                                self.terminal_queries.cursor_position_queries(run, bytes);
                            if query_count > 0 {
                                response_run = Some(run.clone());
                                for _ in 0..query_count {
                                    responses.extend_from_slice(CURSOR_POSITION_RESPONSE);
                                }
                            }
                        }
                    }
                    if let Some(run) = response_run {
                        trace_tui(format!(
                            "answered {} guest terminal cursor-position query/queries",
                            responses.len() / CURSOR_POSITION_RESPONSE.len()
                        ));
                        let pending = self.console_input.release_after_query(&run);
                        self.write_serial(run.clone(), responses)?;
                        if let Some(bytes) = pending {
                            trace_tui(
                                "released buffered console input after cursor-query response",
                            );
                            self.write_serial(run, bytes)?;
                        }
                    }
                }
                match &result {
                    Ok(events) => {
                        let serial = events
                            .iter()
                            .filter(|event| matches!(event, RuntimeEvent::SerialChunk { .. }))
                            .count();
                        let serial_bytes = events
                            .iter()
                            .map(|event| match event {
                                RuntimeEvent::SerialChunk { bytes, .. } => bytes.len(),
                                _ => 0,
                            })
                            .sum::<usize>();
                        let ready = events
                            .iter()
                            .filter(|event| matches!(event, RuntimeEvent::GuestReady { .. }))
                            .count();
                        trace_tui(format!(
                            "poll completion: events={}, serial={serial}, serial_bytes={serial_bytes}, guest_ready={ready}",
                            events.len(),
                        ));
                    }
                    Err(_) => trace_tui("poll completion: application error"),
                }
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
            WorkerRequest::RefreshTrust => {
                Some(WorkerCompletion::Trust(with_service(&service, |service| {
                    Ok(service.trust_policy_status())
                })))
            }
            WorkerRequest::StateAction(path, operation) => {
                trace_tui(format!(
                    "state worker started: operation={}",
                    operation.label()
                ));
                let result = with_service(&service, |service| match &operation {
                    StateOperation::Refresh => service.list_states(&path),
                    StateOperation::Select(id) => service.select_state(&path, id),
                    StateOperation::Create(name) => service.create_state(&path, name),
                    StateOperation::Clone {
                        source_id,
                        new_name,
                    } => service.clone_state(&path, source_id, new_name),
                    StateOperation::Reset(id) => service.reset_state(&path, id),
                    StateOperation::Delete(id) => service.delete_state(&path, id),
                });
                trace_tui(format!(
                    "state worker returned: operation={}, outcome={}",
                    operation.label(),
                    if result.is_ok() { "ok" } else { "error" }
                ));
                Some(WorkerCompletion::States(path, operation, result))
            }
            WorkerRequest::Open(ticket, path) => {
                trace_tui(format!(
                    "open worker started: path_is_directory={}",
                    path.is_dir()
                ));
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
                    match with_service(&service, |service| service.review_archive(&path)) {
                        Ok(Some(review)) => OpenedBundle::Inspected(Box::new(
                            InspectionOutcome::PublisherReview(review),
                        )),
                        Ok(None) => {
                            OpenedBundle::Imported(Box::new(with_service(&service, |service| {
                                service.import_and_inspect(&path)
                            })))
                        }
                        Err(error) => OpenedBundle::Inspected(Box::new(InspectionOutcome::Failed(
                            failure_from(&path, error),
                        ))),
                    }
                };
                trace_tui("open worker returned");
                Some(WorkerCompletion::Opened(ticket, opened))
            }
            WorkerRequest::ValidateManifest(ticket, path) => {
                trace_tui("validate worker started");
                let result = with_service(&service, |service| service.validate_manifest(&path));
                trace_tui(format!(
                    "validate worker returned: {}",
                    if result.is_ok() { "ok" } else { "error" }
                ));
                Some(WorkerCompletion::Validated(ticket, result))
            }
            WorkerRequest::PlanBundle(ticket, path) => {
                trace_tui("plan worker started");
                let result = with_service(&service, |service| service.plan_bundle(&path));
                trace_tui(format!(
                    "plan worker returned: {}",
                    if result.is_ok() { "ok" } else { "error" }
                ));
                Some(WorkerCompletion::Planned(ticket, result))
            }
            WorkerRequest::RemoveInstalled(ticket, digest) => Some(WorkerCompletion::Removed(
                ticket,
                with_service(&service, |service| service.remove_installed(&digest)),
            )),
            WorkerRequest::Start(ticket, path, state_id) => {
                trace_tui("service start request began");
                let result = if ticket.is_cancelled() {
                    Err(AppError {
                        phase: LifecycleState::Starting,
                        summary: "The start operation was canceled.".to_string(),
                        detail: "The TUI canceled this operation before guest startup.".to_string(),
                    })
                } else {
                    with_service(&service, |service| {
                        service.start_with_state(&path, state_id.as_deref())
                    })
                };
                trace_tui(format!(
                    "service start request returned: {}",
                    if result.is_ok() { "ok" } else { "error" }
                ));
                Some(WorkerCompletion::Started(ticket, result))
            }
            WorkerRequest::Stop(ticket, run) => Some(WorkerCompletion::Stopped(
                ticket,
                with_service(&service, |service| service.stop(&run)),
            )),
            WorkerRequest::Restart(ticket, run, path, state_id) => {
                Some(WorkerCompletion::Restarted(
                    ticket,
                    with_service(&service, |service| {
                        service.restart_with_state(&run, &path, state_id.as_deref())
                    }),
                ))
            }
            WorkerRequest::PollEvents => {
                let result = with_service(&service, |service| service.poll_events());
                match &result {
                    Ok(events) => {
                        let serial = events
                            .iter()
                            .filter(|event| matches!(event, RuntimeEvent::SerialChunk { .. }))
                            .count();
                        let serial_bytes = events
                            .iter()
                            .map(|event| match event {
                                RuntimeEvent::SerialChunk { bytes, .. } => bytes.len(),
                                _ => 0,
                            })
                            .sum::<usize>();
                        let ready = events
                            .iter()
                            .filter(|event| matches!(event, RuntimeEvent::GuestReady { .. }))
                            .count();
                        trace_tui(format!(
                            "service poll returned: events={}, serial={serial}, serial_bytes={serial_bytes}, guest_ready={ready}",
                            events.len(),
                        ));
                    }
                    Err(_) => trace_tui("service poll returned an application error"),
                }
                Some(WorkerCompletion::Polled(result))
            }
            WorkerRequest::WriteSerial(run, bytes) => Some(WorkerCompletion::Written(
                run.clone(),
                with_service(&service, |service| service.write_serial(&run, &bytes)),
            )),
            WorkerRequest::Developer(request) => {
                let action = request.action;
                trace_tui(format!(
                    "developer worker started: action={action:?}, source_is_directory={}, manifest_exists={}, key_file_exists={}, output_requested={}",
                    request.source.is_dir(),
                    request.source.join("rootglass.toml").is_file(),
                    request.key_file.is_file(),
                    request.output.is_some(),
                ));
                let result = execute_developer_request(request);
                trace_tui(format!(
                    "developer worker returned: action={action:?}, successful={}",
                    result.is_ok(),
                ));
                Some(WorkerCompletion::Developer(result))
            }
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
    fn enable_bracketed_paste(&mut self) -> io::Result<()>;
    fn disable_bracketed_paste(&mut self) -> io::Result<()>;
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

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        execute!(self.stdout, EnableBracketedPaste)
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        execute!(self.stdout, DisableBracketedPaste)
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
    bracketed_paste: bool,
    alternate_screen: bool,
    cursor_hidden: bool,
}

impl<O: TerminalOps> TerminalRestore<O> {
    fn enter(operations: O) -> io::Result<Self> {
        let mut restore = Self {
            operations,
            raw_mode: false,
            bracketed_paste: false,
            alternate_screen: false,
            cursor_hidden: false,
        };
        restore.raw_mode = true;
        restore.operations.enable_raw_mode()?;
        restore.bracketed_paste = true;
        restore.operations.enable_bracketed_paste()?;
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
        if self.bracketed_paste {
            keep_first_error(&mut first_error, self.operations.disable_bracketed_paste());
        }
        if self.alternate_screen {
            keep_first_error(&mut first_error, self.operations.leave_alternate_screen());
        }
        if self.raw_mode {
            keep_first_error(&mut first_error, self.operations.disable_raw_mode());
        }
        self.cursor_hidden = false;
        self.bracketed_paste = false;
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
    use crate::{
        GlassPreview, PreflightDetails, SignatureStatus, StateSnapshot, TuiService, UiState,
    };
    use rootglass_app::registry::InstalledGlass;
    use rootglass_app::state::StateRecord;
    use rootglass_app::{
        AppError, AppService, RunId, RuntimeEvent, TrustPolicyStatus, TrustedPublisherStatus,
    };
    use rootglass_core::runtime_assets::RuntimeAssetsResolver;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;
    use std::fs;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn terminal_query_responder_handles_split_repeated_and_new_run_queries() {
        let mut responder = TerminalQueryResponder::default();
        let first_run = RunId(7);

        assert_eq!(
            responder.cursor_position_queries(&first_run, b"prompt\x1b["),
            0
        );
        assert_eq!(
            responder.cursor_position_queries(&first_run, b"6n next\x1b[6n"),
            2
        );

        assert_eq!(
            responder.cursor_position_queries(&first_run, b"still\x1b["),
            0
        );
        let second_run = RunId(8);
        assert_eq!(responder.cursor_position_queries(&second_run, b"6n"), 0);
        assert_eq!(
            responder.cursor_position_queries(&second_run, b"\x1b[6n"),
            1
        );
    }

    #[test]
    fn console_input_gate_waits_for_query_reply_then_releases_remaining_bytes() {
        let now = Instant::now();
        let run = RunId(7);
        let mut gate = ConsoleInputGate::default();

        assert_eq!(
            gate.begin(run.clone(), b"echo READY\n".to_vec(), now),
            Some(b"e".to_vec())
        );
        assert_eq!(
            gate.release_after_query(&RunId(8)),
            None,
            "a query from another run must not release pending input"
        );
        assert_eq!(
            gate.release_after_query(&run),
            Some(b"cho READY\n".to_vec())
        );
        assert_eq!(
            gate.take_expired(now + CONSOLE_QUERY_WAIT),
            None,
            "query response releases the input exactly once"
        );
    }

    #[test]
    fn console_input_gate_flushes_after_query_timeout_and_appends_same_run_input() {
        let now = Instant::now();
        let run = RunId(7);
        let mut gate = ConsoleInputGate::default();

        assert_eq!(
            gate.begin(run.clone(), b"echo ".to_vec(), now),
            Some(b"e".to_vec())
        );
        assert_eq!(gate.begin(run.clone(), b"READY\n".to_vec(), now), None);
        assert_eq!(
            gate.take_expired(now + CONSOLE_QUERY_WAIT),
            Some((run, b"cho READY\n".to_vec()))
        );
    }

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

        fn enable_bracketed_paste(&mut self) -> io::Result<()> {
            self.record("paste-on")
        }

        fn disable_bracketed_paste(&mut self) -> io::Result<()> {
            self.record("paste-off")
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
                "paste-on",
                "alternate-on",
                "cursor-hide",
                "cursor-show",
                "paste-off",
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
            &events.borrow()[4..],
            ["cursor-show", "paste-off", "alternate-off", "raw-off"]
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
                "paste-on",
                "alternate-on",
                "cursor-show",
                "paste-off",
                "alternate-off",
                "raw-off"
            ]
        );
    }

    #[derive(Clone, Default)]
    struct FakeService {
        calls: Arc<Mutex<Vec<String>>>,
        states_used: Arc<Mutex<Vec<String>>>,
        glasses: Vec<InstalledGlass>,
        states: Vec<StateRecord>,
        selected_state_id: String,
        fail_state_ops: bool,
        trust_status: TrustPolicyStatus,
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

        fn trust_policy_status(&mut self) -> TrustPolicyStatus {
            self.trust_status.clone()
        }

        fn list_states(&mut self, _source: &Path) -> Result<StateSnapshot, AppError> {
            self.state_result()
        }

        fn select_state(&mut self, _source: &Path, id: &str) -> Result<StateSnapshot, AppError> {
            self.check_state_operation()?;
            if id != "default" && !self.states.iter().any(|state| state.id == id) {
                return Err(state_not_found(id));
            }
            self.calls
                .lock()
                .unwrap()
                .push(format!("select-state:{id}"));
            self.selected_state_id = id.to_string();
            self.state_result()
        }

        fn create_state(&mut self, _source: &Path, name: &str) -> Result<StateSnapshot, AppError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create-state:{name}"));
            self.check_state_operation()?;
            self.states.push(fake_state(name));
            self.state_result()
        }

        fn clone_state(
            &mut self,
            _source: &Path,
            source_id: &str,
            new_name: &str,
        ) -> Result<StateSnapshot, AppError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("clone-state:{source_id}:{new_name}"));
            self.check_state_operation()?;
            self.states.push(fake_state(new_name));
            self.state_result()
        }

        fn reset_state(&mut self, _source: &Path, id: &str) -> Result<StateSnapshot, AppError> {
            self.calls.lock().unwrap().push(format!("reset-state:{id}"));
            self.check_state_operation()?;
            self.state_result()
        }

        fn delete_state(&mut self, _source: &Path, id: &str) -> Result<StateSnapshot, AppError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("delete-state:{id}"));
            self.check_state_operation()?;
            self.states.retain(|state| state.id != id);
            if self.selected_state_id == id {
                self.selected_state_id = "default".to_string();
            }
            self.state_result()
        }

        fn start_with_state(
            &mut self,
            source: &Path,
            state_id: Option<&str>,
        ) -> Result<RunId, AppError> {
            self.states_used
                .lock()
                .unwrap()
                .push(state_id.unwrap_or("stored").to_string());
            self.start(source)
        }
    }

    impl FakeService {
        fn check_state_operation(&self) -> Result<(), AppError> {
            if self.fail_state_ops {
                Err(AppError {
                    phase: rootglass_core::LifecycleState::Ready,
                    summary: "State change was denied.".to_string(),
                    detail: "glass state is currently in use".to_string(),
                })
            } else {
                Ok(())
            }
        }

        fn state_result(&self) -> Result<StateSnapshot, AppError> {
            self.check_state_operation()?;
            let mut records = self.states.clone();
            if !records.iter().any(|state| state.id == "default") {
                records.insert(0, fake_state("default"));
            }
            Ok(StateSnapshot {
                records,
                locked: false,
                selected_state_id: if self.selected_state_id.is_empty() {
                    "default".to_string()
                } else {
                    self.selected_state_id.clone()
                },
            })
        }
    }

    fn fake_state(name: &str) -> StateRecord {
        StateRecord {
            id: name.to_string(),
            name: name.to_string(),
            image_path: PathBuf::from(format!("/states/{name}.img")),
            image_bytes: 32,
        }
    }

    fn state_not_found(id: &str) -> AppError {
        AppError {
            phase: rootglass_core::LifecycleState::Ready,
            summary: "The selected writable state could not be prepared.".to_string(),
            detail: format!("state {id} does not exist"),
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
        let service = FakeService {
            states: vec![fake_state("saved")],
            selected_state_id: "saved".to_string(),
            ..FakeService::default()
        };
        let mut model = UiModel::with_service(service, UiState::Empty);
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        open_path(&mut model, directory.clone(), &mut worker).expect("directory open is queued");
        assert!(matches!(model.state, UiState::Opening(_)));
        worker
            .drain_until_idle(&mut model)
            .expect("directory inspection completes");
        assert!(matches!(model.state, UiState::Ready(_)));
        assert!(model.state_loaded);
        assert_eq!(model.selected_state_id, "saved");

        open_path(&mut model, archive.clone(), &mut worker).expect("archive open is queued");
        assert!(matches!(model.state, UiState::Opening(_)));
        worker
            .drain_until_idle(&mut model)
            .expect("archive import and inspection complete");
        assert!(matches!(model.state, UiState::Ready(_)));
        assert!(model.state_loaded);
        assert_eq!(model.selected_state_id, "saved");
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
            state_id: None,
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
        assert_eq!(*calls.lock().unwrap(), ["serial:i"]);

        worker
            .flush_expired_console_input_at(Instant::now() + CONSOLE_QUERY_WAIT)
            .expect("input gate expires");
        worker
            .drain_until_idle(&mut model)
            .expect("remaining serial input finishes");

        assert_eq!(*calls.lock().unwrap(), ["serial:i", "serial:d\n"]);
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
            state_id: None,
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
            state_id: None,
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
            states_used: Arc::new(Mutex::new(Vec::new())),
            glasses: Vec::new(),
            states: Vec::new(),
            selected_state_id: "default".to_string(),
            fail_state_ops: false,
            trust_status: TrustPolicyStatus::default(),
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
    fn state_reset_requires_confirmation_and_lock_conflicts_disable_state_actions() {
        let path = PathBuf::from("example.glass");
        let service = FakeService {
            states: vec![fake_state("default")],
            ..FakeService::default()
        };
        let calls = Arc::clone(&service.calls);
        let mut model = UiModel::with_service(service, UiState::Ready(preview(path.clone())));
        model.selected_path = Some(path);
        model.state_records = vec![fake_state("default")];
        model.operations_panel_open = true;
        let mut input = InputBuffer::default();
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("reset opens confirmation");
        assert_eq!(input.mode, InputMode::ConfirmStateReset);
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("cancel reset");
        assert!(calls.lock().unwrap().is_empty());

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("reset opens a second confirmation");
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("confirmed reset is scheduled");
        worker
            .drain_until_idle(&mut model)
            .expect("reset operation finishes");
        assert_eq!(*calls.lock().unwrap(), ["reset-state:default"]);

        let service = FakeService {
            states: vec![fake_state("default")],
            fail_state_ops: true,
            ..FakeService::default()
        };
        let path = PathBuf::from("example.glass");
        let mut locked_model =
            UiModel::with_service(service, UiState::Ready(preview(path.clone())));
        locked_model.selected_path = Some(path);
        locked_model.state_records = vec![fake_state("default")];
        locked_model.operations_panel_open = true;
        let mut locked_input = InputBuffer::default();
        let mut locked_worker =
            ServiceWorker::spawn(locked_model.shared_service()).expect("worker starts");
        handle_key(
            &mut locked_model,
            &mut locked_input,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut locked_worker,
        )
        .expect("locked reset opens confirmation before lock status refresh");
        handle_key(
            &mut locked_model,
            &mut locked_input,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut locked_worker,
        )
        .expect("conflicting reset is sent to the service safety lock");
        locked_worker
            .drain_until_idle(&mut locked_model)
            .expect("lock conflict is reported");
        assert!(locked_model.state_locked);
        assert!(locked_model
            .state_error
            .as_deref()
            .is_some_and(|error| error.contains("currently in use")));
        assert!(!locked_model.actions().state_create);
        assert!(!locked_model.actions().state_delete);

        locked_model.run_id = Some(RunId(7));
        locked_model.state = UiState::Running(crate::RunDetails {
            name: "Example".to_string(),
            status: "Running".to_string(),
            elapsed: String::new(),
        });
        assert!(!locked_model.actions().state_select);
        assert!(!locked_model.actions().reset_state);
        assert!(!locked_model.actions().state_delete);
    }

    #[test]
    fn selected_state_is_used_for_boot_and_preserved_by_guest_restart() {
        let path = PathBuf::from("example.glass");
        let service = FakeService {
            states: vec![fake_state("default"), fake_state("persistent")],
            ..FakeService::default()
        };
        let used = Arc::clone(&service.states_used);
        let mut model = UiModel::with_service(service, UiState::Ready(preview(path.clone())));
        model.selected_path = Some(path);
        model.state_records = vec![fake_state("default"), fake_state("persistent")];
        model.state_cursor = 1;
        model.operations_panel_open = true;
        let mut input = InputBuffer::default();
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("selected state is validated by the service");
        worker
            .drain_until_idle(&mut model)
            .expect("state selection finishes");
        assert_eq!(model.selected_state_id, "persistent");

        model.operations_panel_open = false;
        start_guest(&mut model, &mut worker).expect("boot is scheduled with selected state");
        worker
            .drain_until_idle(&mut model)
            .expect("boot operation finishes");
        model.apply_poll_result(Ok(vec![RuntimeEvent::GuestReady {
            run: RunId(7),
            evidence: "fixture login prompt".to_string(),
        }]));
        restart_guest(&mut model, &mut worker).expect("restart reuses selected state");
        worker
            .drain_until_idle(&mut model)
            .expect("restart operation finishes");

        assert_eq!(*used.lock().unwrap(), ["persistent", "persistent"]);
    }

    #[test]
    fn state_panel_creates_clones_selects_and_confirms_deletion() {
        let path = PathBuf::from("example.glass");
        let service = FakeService {
            states: vec![fake_state("default"), fake_state("source")],
            ..FakeService::default()
        };
        let calls = Arc::clone(&service.calls);
        let mut model = UiModel::with_service(service, UiState::Ready(preview(path.clone())));
        model.selected_path = Some(path);
        model.state_records = vec![fake_state("default"), fake_state("source")];
        model.operations_panel_open = true;
        let mut input = InputBuffer::default();
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("create opens a name prompt");
        assert_eq!(input.mode, InputMode::StateCreateName);
        for character in "new-state".chars() {
            handle_key(
                &mut model,
                &mut input,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut worker,
            )
            .expect("state name is accepted");
        }
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut worker,
        )
        .expect("new state is created");
        worker
            .drain_until_idle(&mut model)
            .expect("create operation finishes");
        assert!(model
            .state_records
            .iter()
            .any(|state| state.id == "new-state"));

        model.state_cursor = model
            .state_records
            .iter()
            .position(|state| state.id == "source")
            .unwrap();
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("clone opens a name prompt");
        assert_eq!(input.mode, InputMode::StateCloneName);
        for character in "copy".chars() {
            handle_key(
                &mut model,
                &mut input,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut worker,
            )
            .expect("clone name is accepted");
        }
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut worker,
        )
        .expect("state is cloned");
        worker
            .drain_until_idle(&mut model)
            .expect("clone operation finishes");
        let copy_index = model
            .state_records
            .iter()
            .position(|state| state.id == "copy")
            .unwrap();
        model.state_cursor = copy_index;
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("cloned state is selected");
        worker
            .drain_until_idle(&mut model)
            .expect("selection operation finishes");
        assert_eq!(model.selected_state_id, "copy");

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("delete opens a confirmation");
        assert_eq!(input.mode, InputMode::ConfirmStateDelete);
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("delete can be canceled");
        assert!(!calls
            .lock()
            .unwrap()
            .contains(&"delete-state:copy".to_string()));

        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("delete opens confirmation again");
        handle_key(
            &mut model,
            &mut input,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &mut worker,
        )
        .expect("confirmed delete is sent");
        worker
            .drain_until_idle(&mut model)
            .expect("delete operation finishes");
        assert!(!model.state_records.iter().any(|state| state.id == "copy"));
        assert_eq!(model.selected_state_id, "default");
    }

    #[test]
    fn trust_policy_errors_and_revoked_keys_reach_the_operations_panel_model() {
        let status = TrustPolicyStatus {
            config_path: Some(PathBuf::from("/config/rootglass/trust.json")),
            trusted_publishers: vec![TrustedPublisherStatus {
                name: "Trusted Publisher".to_string(),
                identity: "publisher.example/rootglass".to_string(),
                key_id: "ed25519:trusted".to_string(),
            }],
            revoked_key_ids: vec!["ed25519:revoked".to_string()],
            error: Some("unsupported trust configuration schema".to_string()),
        };
        let service = FakeService {
            trust_status: status.clone(),
            ..FakeService::default()
        };
        let mut model = UiModel::with_service(service, UiState::Empty);
        let mut worker = ServiceWorker::spawn(model.shared_service()).expect("worker starts");
        worker.refresh_trust().expect("trust status is queued");
        worker
            .drain_until_idle(&mut model)
            .expect("trust status finishes");
        assert_eq!(model.trust_policy, status);
    }

    #[test]
    fn non_interactive_stdio_returns_actionable_error() {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            let error = run_terminal(None).expect_err("captured test stdio is not a terminal");
            assert!(error.contains("requires an interactive terminal"));
            assert!(error.contains("full-duplex PTY"));
        }
    }

    #[test]
    fn bracketed_paste_fills_text_prompts_without_triggering_actions() {
        let mut input = InputBuffer {
            mode: InputMode::OpenPath,
            ..InputBuffer::default()
        };

        handle_paste(&mut input, "hello\nworld\t!");

        assert_eq!(input.text, "helloworld!");
        assert_eq!(input.mode, InputMode::OpenPath);

        input.mode = InputMode::Normal;
        input.text.clear();
        handle_paste(&mut input, "q");
        assert!(input.text.is_empty());
        assert_eq!(input.mode, InputMode::Normal);
    }
}
