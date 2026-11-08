use gpui_kit::base::StyledExt;
use gpui_kit::*;
use portable_pty::{native_pty_system, Child, MasterPty, PtySize};
use rootglass_terminal_vt::{encode_bracketed_paste, GhosttyTerminal};
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CELL_WIDTH: f32 = 8.0;
const CELL_HEIGHT: f32 = 16.0;
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const OUTPUT_CAPACITY: usize = 64;
const MAX_OUTPUT_CHUNKS_PER_TICK: usize = 32;
const DEFAULT_FOREGROUND: u32 = 0xd7deea;
const DEFAULT_BACKGROUND: u32 = 0x11131a;
const SELECTION_BACKGROUND: u32 = 0x43536b;
const TERMINAL_CONTEXT: &str = "RootglassEmbeddedTerminal";

pub struct EmbeddedTerminal {
    parser: Option<GhosttyTerminal>,
    pty: Option<TerminalPty>,
    screen: String,
    cells: Vec<Vec<TerminalCell>>,
    error: Option<String>,
    cols: u16,
    rows: u16,
    ready_reported: bool,
    typed_reported: bool,
    paste_reported: bool,
    ansi_reported: bool,
    cursor: CellPoint,
    selection: Option<Selection>,
    focus: FocusHandle,
    _bounds_subscription: Subscription,
    _poll_task: Task<()>,
}

impl EmbeddedTerminal {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        let (parser, pty, error) = match GhosttyTerminal::new(INITIAL_COLS, INITIAL_ROWS) {
            Ok(parser) => match TerminalPty::spawn() {
                Ok(pty) => (Some(parser), Some(pty), None),
                Err(error) => (Some(parser), None, Some(error.to_string())),
            },
            Err(error) => (None, None, Some(error.to_string())),
        };
        let background_executor = cx.background_executor().clone();
        let _bounds_subscription = cx.observe_window_bounds(window, |this, window, cx| {
            if let Err(error) = this.resize_to_window(window) {
                this.error = Some(format!("terminal resize failed: {error}"));
                cx.notify();
            }
        });
        let _poll_task = cx.spawn(async move |view, cx| loop {
            background_executor.timer(Duration::from_millis(16)).await;
            let Ok(active) = view.update(cx, |this, cx| {
                let active = match this.poll_pty() {
                    Ok(active) => active,
                    Err(error) => {
                        this.error = Some(format!("terminal session failed: {error}"));
                        this.screen = this.error.clone().unwrap_or_default();
                        false
                    }
                };
                cx.notify();
                active
            }) else {
                break;
            };
            if !active {
                break;
            }
        });

        Self {
            parser,
            pty,
            screen: if error.is_some() {
                String::new()
            } else {
                "Starting the Rootglass TUI…".to_string()
            },
            cells: blank_grid(INITIAL_COLS, INITIAL_ROWS),
            error,
            cols: INITIAL_COLS,
            rows: INITIAL_ROWS,
            ready_reported: false,
            typed_reported: false,
            paste_reported: false,
            ansi_reported: false,
            cursor: CellPoint { x: 0, y: 0 },
            selection: None,
            focus,
            _bounds_subscription,
            _poll_task,
        }
    }

    fn poll_pty(&mut self) -> io::Result<bool> {
        let (Some(parser), Some(pty)) = (self.parser.as_mut(), self.pty.as_mut()) else {
            return Ok(false);
        };
        for _ in 0..MAX_OUTPUT_CHUNKS_PER_TICK {
            match pty.try_read() {
                PtyRead::Bytes(bytes) => {
                    parser.write(&bytes);
                    let reply = parser.take_pty_output()?;
                    if !reply.is_empty() {
                        pty.write(&reply)?;
                    }
                }
                PtyRead::Empty => break,
                PtyRead::Closed => {
                    self.screen = "Rootglass TUI session exited.".to_string();
                    self.cells = blank_grid(self.cols, self.rows);
                    #[cfg(debug_assertions)]
                    if std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some() {
                        eprintln!("ROOTGLASS_EMBEDDED_TUI_EXITED");
                    }
                    return Ok(false);
                }
                PtyRead::Error(error) => return Err(error),
            }
        }

        self.screen = parser.screen()?;
        let vt = parser.screen_vt()?;
        self.cells = parse_vt_screen(&vt, self.cols, self.rows);
        if std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some() {
            if !self.ready_reported
                && self.screen.contains("Rootglass")
                && self.screen.contains("Headless TUI")
            {
                self.ready_reported = true;
                eprintln!("ROOTGLASS_EMBEDDED_TUI_READY");
            }
            if !self.typed_reported && self.screen.contains("Bundle path: headless-key") {
                self.typed_reported = true;
                eprintln!("ROOTGLASS_EMBEDDED_TUI_TYPED");
            }
            if !self.paste_reported && self.screen.contains("Bundle path: headless paste") {
                self.paste_reported = true;
                eprintln!("ROOTGLASS_EMBEDDED_TUI_PASTE");
            }
            if !self.ansi_reported
                && vt.windows(2).any(|bytes| bytes == b"\x1b[")
                && vt.contains(&b'm')
            {
                self.ansi_reported = true;
                eprintln!("ROOTGLASS_EMBEDDED_TUI_ANSI");
            }
        }
        Ok(true)
    }

    fn resize_to_window(&mut self, window: &Window) -> io::Result<()> {
        let bounds = window.bounds();
        let cols = ((bounds.size.width / px(CELL_WIDTH)).floor() as i32).clamp(40, 200) as u16;
        let rows = ((bounds.size.height / px(CELL_HEIGHT)).floor() as i32).clamp(12, 60) as u16;
        if (cols, rows) == (self.cols, self.rows) {
            return Ok(());
        }
        if let Some(pty) = &self.pty {
            pty.resize(cols, rows)?;
        }
        if let Some(parser) = &mut self.parser {
            parser.resize(cols, rows)?;
        }
        self.cols = cols;
        self.rows = rows;
        self.cells = blank_grid(cols, rows);
        if std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some() {
            eprintln!("ROOTGLASS_EMBEDDED_TUI_RESIZED {cols}x{rows}");
        }
        Ok(())
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut App) {
        let key = event.keystroke.key.as_str();
        let modifiers = &event.keystroke.modifiers;
        let platform_copy = modifiers.platform && !modifiers.shift && key.eq_ignore_ascii_case("c");
        let terminal_copy = modifiers.control && modifiers.shift && key.eq_ignore_ascii_case("c");
        if platform_copy || terminal_copy {
            let text = self.copy_text();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return;
        }

        let platform_paste = modifiers.platform && key.eq_ignore_ascii_case("v");
        let terminal_paste = modifiers.control && key.eq_ignore_ascii_case("v");
        if platform_paste || terminal_paste {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                if let Ok(bytes) = encode_bracketed_paste(&text) {
                    let _ = self.write_pty(&bytes);
                }
            }
            return;
        }

        if modifiers.shift && self.extend_selection(key) {
            if std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some() {
                eprintln!("ROOTGLASS_EMBEDDED_TUI_SELECTION");
            }
            return;
        }
        self.selection = None;

        let bytes = match key {
            "left" => Some(b"\x1b[D".to_vec()),
            "right" => Some(b"\x1b[C".to_vec()),
            "up" => Some(b"\x1b[A".to_vec()),
            "down" => Some(b"\x1b[B".to_vec()),
            "home" => Some(b"\x1b[H".to_vec()),
            "end" => Some(b"\x1b[F".to_vec()),
            "pageup" => Some(b"\x1b[5~".to_vec()),
            "pagedown" => Some(b"\x1b[6~".to_vec()),
            "delete" => Some(b"\x1b[3~".to_vec()),
            "enter" => Some(b"\r".to_vec()),
            "backspace" => Some(vec![0x7f]),
            "escape" => Some(vec![0x1b]),
            "tab" => Some(b"\t".to_vec()),
            _ if modifiers.control && key.eq_ignore_ascii_case("c") => Some(vec![0x03]),
            _ if modifiers.control => key
                .chars()
                .next()
                .filter(|character| character.is_ascii_alphabetic())
                .map(|character| vec![character.to_ascii_uppercase() as u8 - b'@']),
            _ => event
                .keystroke
                .key_char
                .as_ref()
                .map(|text| text.as_bytes().to_vec()),
        };
        if let Some(bytes) = bytes {
            let _ = self.write_pty(&bytes);
        }
    }

    fn write_pty(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pty
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "TUI session has exited"))?
            .write(bytes)
    }

    fn extend_selection(&mut self, key: &str) -> bool {
        let next = match key {
            "left" => CellPoint {
                x: self.cursor.x.saturating_sub(1),
                y: self.cursor.y,
            },
            "right" => CellPoint {
                x: self
                    .cursor
                    .x
                    .saturating_add(1)
                    .min(self.cols.saturating_sub(1)),
                y: self.cursor.y,
            },
            "up" => CellPoint {
                x: self.cursor.x,
                y: self.cursor.y.saturating_sub(1),
            },
            "down" => CellPoint {
                x: self.cursor.x,
                y: self
                    .cursor
                    .y
                    .saturating_add(1)
                    .min(self.rows.saturating_sub(1)),
            },
            _ => return false,
        };
        let anchor = self
            .selection
            .map(|selection| selection.anchor)
            .unwrap_or(self.cursor);
        self.cursor = next;
        self.selection = Some(Selection {
            anchor,
            head: self.cursor,
        });
        true
    }

    fn copy_text(&self) -> String {
        if let (Some(parser), Some(selection)) = (&self.parser, self.selection) {
            let (start, end) = selection.ordered();
            if let Ok(text) = parser.copy_range(start.x, start.y, end.x, end.y) {
                return text;
            }
        }
        self.screen.clone()
    }

    fn cell_is_selected(&self, x: u16, y: u16) -> bool {
        self.selection
            .is_some_and(|selection| selection.contains(CellPoint { x, y }))
    }
}

impl Render for EmbeddedTerminal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut surface = div()
            .id("rootglass-embedded-terminal")
            .size_full()
            .p_2()
            .bg(rgb(DEFAULT_BACKGROUND))
            .text_color(rgb(DEFAULT_FOREGROUND))
            .font_family("monospace")
            .key_context(TERMINAL_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, _, cx| this.handle_key(event, cx)));

        if let Some(error) = &self.error {
            surface = surface.child(format!("Embedded terminal unavailable: {error}"));
        } else {
            for (y, row) in self.cells.iter().enumerate() {
                let mut row_element = div().h_flex().font_family("monospace");
                let mut run = String::new();
                let mut run_style = None;
                for (x, cell) in row.iter().enumerate() {
                    let selected = self.cell_is_selected(x as u16, y as u16);
                    let (foreground, background) = if cell.style.inverse {
                        (cell.style.background, cell.style.foreground)
                    } else {
                        (cell.style.foreground, cell.style.background)
                    };
                    let background = if selected {
                        SELECTION_BACKGROUND
                    } else {
                        background
                    };
                    let style = (foreground, background, selected);
                    if run_style.is_some_and(|active| active != style) {
                        row_element = row_element.child(styled_run(&run, run_style.unwrap()));
                        run.clear();
                    }
                    run_style = Some(style);
                    run.push(cell.character);
                }
                if !run.is_empty() {
                    row_element = row_element.child(styled_run(&run, run_style.unwrap()));
                }
                surface = surface.child(row_element);
            }
        }

        div()
            .v_flex()
            .size_full()
            .child(surface)
            .child("Shift+Arrow selects · Ctrl/Cmd+Shift+C copies · Ctrl/Cmd+V pastes")
    }
}

fn styled_run(text: &str, style: (u32, u32, bool)) -> Div {
    let (foreground, background, _) = style;
    div()
        .text_color(rgb(foreground))
        .bg(rgb(background))
        .child(text.to_string())
}

struct TerminalPty {
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    output: Option<Receiver<io::Result<Vec<u8>>>>,
    reader: Option<JoinHandle<()>>,
}

impl TerminalPty {
    fn spawn() -> io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: INITIAL_ROWS,
                cols: INITIAL_COLS,
                pixel_width: INITIAL_COLS * CELL_WIDTH as u16,
                pixel_height: INITIAL_ROWS * CELL_HEIGHT as u16,
            })
            .map_err(io::Error::other)?;
        let executable = std::env::current_exe()?;
        let mut command = portable_pty::CommandBuilder::new(executable);
        command.arg("--rootglass-tui-session");
        command.env("TERM", "xterm-256color");
        command.env_remove("NO_COLOR");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let writer = pair.master.take_writer().map_err(io::Error::other)?;
        let (output_tx, output) = mpsc::sync_channel(OUTPUT_CAPACITY);
        let reader = thread::Builder::new()
            .name("rootglass-embedded-tui-reader".to_string())
            .spawn(move || read_pty(&mut reader, output_tx))?;

        Ok(Self {
            master: Some(pair.master),
            writer: Some(writer),
            child: Some(child),
            output: Some(output),
            reader: Some(reader),
        })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY writer is closed"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY master is closed"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: cols.saturating_mul(CELL_WIDTH as u16),
                pixel_height: rows.saturating_mul(CELL_HEIGHT as u16),
            })
            .map_err(io::Error::other)
    }

    fn try_read(&self) -> PtyRead {
        let Some(output) = &self.output else {
            return PtyRead::Closed;
        };
        match output.try_recv() {
            Ok(Ok(bytes)) => PtyRead::Bytes(bytes),
            Ok(Err(error)) => PtyRead::Error(error),
            Err(mpsc::TryRecvError::Empty) => PtyRead::Empty,
            Err(mpsc::TryRecvError::Disconnected) => PtyRead::Closed,
        }
    }
}

impl Drop for TerminalPty {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.write_all(&[0x11]);
            let _ = writer.flush();
        }
        self.output.take();
        let master = self.master.take();
        let writer = self.writer.take();
        let child = self.child.take();
        let reader = self.reader.take();
        let smoke = std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some();
        let cleanup = thread::spawn(move || {
            let mut graceful = false;
            if let Some(mut child) = child {
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    match child.try_wait() {
                        Ok(Some(_)) => {
                            graceful = true;
                            break;
                        }
                        Ok(None) => thread::sleep(Duration::from_millis(25)),
                        Err(_) => break,
                    }
                }
                if !graceful {
                    let _ = child.kill();
                }
                let reaped = child.wait().is_ok();
                drop(writer);
                drop(master);
                let reader_joined = match reader {
                    Some(reader) => reader.join().is_ok(),
                    None => true,
                };
                if smoke {
                    if graceful && reaped && reader_joined {
                        eprintln!("ROOTGLASS_EMBEDDED_TUI_CLEANED");
                    } else {
                        eprintln!("ROOTGLASS_EMBEDDED_TUI_CLEANUP_FAILED");
                    }
                }
            } else {
                drop(writer);
                drop(master);
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
            }
        });
        if let Ok(mut handles) = cleanup_handles().lock() {
            handles.push(cleanup);
        }
    }
}

fn cleanup_handles() -> &'static Mutex<Vec<JoinHandle<()>>> {
    static CLEANUP_HANDLES: OnceLock<Mutex<Vec<JoinHandle<()>>>> = OnceLock::new();
    CLEANUP_HANDLES.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn wait_for_cleanup() {
    let handles = cleanup_handles()
        .lock()
        .map(|mut handles| std::mem::take(&mut *handles))
        .unwrap_or_default();
    for handle in handles {
        let _ = handle.join();
    }
}

enum PtyRead {
    Bytes(Vec<u8>),
    Empty,
    Closed,
    Error(io::Error),
}

fn read_pty(reader: &mut dyn Read, output: SyncSender<io::Result<Vec<u8>>>) {
    let mut buffer = [0; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(length) => {
                if output.send(Ok(buffer[..length].to_vec())).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = output.send(Err(error));
                break;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellStyle {
    foreground: u32,
    background: u32,
    inverse: bool,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            foreground: DEFAULT_FOREGROUND,
            background: DEFAULT_BACKGROUND,
            inverse: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalCell {
    character: char,
    style: CellStyle,
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            character: ' ',
            style: CellStyle::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellPoint {
    x: u16,
    y: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: CellPoint,
    head: CellPoint,
}

impl Selection {
    fn ordered(self) -> (CellPoint, CellPoint) {
        if (self.anchor.y, self.anchor.x) <= (self.head.y, self.head.x) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn contains(self, point: CellPoint) -> bool {
        let (start, end) = self.ordered();
        (point.y, point.x) >= (start.y, start.x) && (point.y, point.x) <= (end.y, end.x)
    }
}

fn blank_grid(cols: u16, rows: u16) -> Vec<Vec<TerminalCell>> {
    vec![vec![TerminalCell::default(); cols as usize]; rows as usize]
}

fn parse_vt_screen(input: &[u8], cols: u16, rows: u16) -> Vec<Vec<TerminalCell>> {
    let mut grid = blank_grid(cols.max(1), rows.max(1));
    let (mut x, mut y) = (0usize, 0usize);
    let (width, height) = (grid[0].len(), grid.len());
    let mut style = CellStyle::default();
    let screen = String::from_utf8_lossy(input);
    let mut characters = screen.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\x1b' => match characters.next() {
                Some('[') => {
                    let mut parameters = String::new();
                    for final_byte in characters.by_ref() {
                        if ('@'..='~').contains(&final_byte) {
                            apply_csi(
                                &mut grid,
                                &mut style,
                                &mut x,
                                &mut y,
                                &parameters,
                                final_byte,
                            );
                            break;
                        }
                        parameters.push(final_byte);
                    }
                }
                Some(']') => {
                    let mut previous_escape = false;
                    for byte in characters.by_ref() {
                        if byte == '\x07' || (previous_escape && byte == '\\') {
                            break;
                        }
                        previous_escape = byte == '\x1b';
                    }
                }
                Some(_) | None => {}
            },
            '\r' => x = 0,
            '\n' => y = y.saturating_add(1).min(height.saturating_sub(1)),
            '\t' => {
                let next_tab = ((x / 8) + 1) * 8;
                while x < next_tab.min(width) {
                    put_cell(&mut grid, x, y, ' ', style);
                    x += 1;
                }
            }
            character if !character.is_control() => {
                if x < width && y < height {
                    put_cell(&mut grid, x, y, character, style);
                }
                x = x.saturating_add(1).min(width);
            }
            _ => {}
        }
    }
    grid
}

fn apply_csi(
    grid: &mut [Vec<TerminalCell>],
    style: &mut CellStyle,
    x: &mut usize,
    y: &mut usize,
    parameters: &str,
    final_byte: char,
) {
    let parameters = parameters.trim_start_matches('?');
    let values = parameters
        .split(';')
        .map(|value| value.parse::<u16>().unwrap_or(0))
        .collect::<Vec<_>>();
    let width = grid.first().map_or(1, Vec::len);
    let height = grid.len().max(1);
    match final_byte {
        'm' => apply_sgr(style, &values),
        'H' | 'f' => {
            *y = values.first().copied().unwrap_or(1).saturating_sub(1) as usize;
            *x = values.get(1).copied().unwrap_or(1).saturating_sub(1) as usize;
            *y = (*y).min(height - 1);
            *x = (*x).min(width - 1);
        }
        'A' => *y = (*y).saturating_sub(values.first().copied().unwrap_or(1) as usize),
        'B' => {
            *y = (*y)
                .saturating_add(values.first().copied().unwrap_or(1) as usize)
                .min(height - 1)
        }
        'C' => {
            *x = (*x)
                .saturating_add(values.first().copied().unwrap_or(1) as usize)
                .min(width - 1)
        }
        'D' => *x = (*x).saturating_sub(values.first().copied().unwrap_or(1) as usize),
        'J' if values.first().copied().unwrap_or(0) == 2 => {
            for row in grid {
                row.fill(TerminalCell::default());
            }
            *x = 0;
            *y = 0;
        }
        'K' => {
            if let Some(row) = grid.get_mut(*y) {
                let start = match values.first().copied().unwrap_or(0) {
                    1 => 0,
                    2 => 0,
                    _ => (*x).min(width),
                };
                let end = if values.first().copied() == Some(1) {
                    (*x).min(width.saturating_sub(1))
                } else {
                    width.saturating_sub(1)
                };
                for cell in row.iter_mut().take(end.saturating_add(1)).skip(start) {
                    *cell = TerminalCell::default();
                }
            }
        }
        _ => {}
    }
}

fn apply_sgr(style: &mut CellStyle, values: &[u16]) {
    let values = if values.is_empty() { &[0][..] } else { values };
    let mut index = 0;
    while index < values.len() {
        match values[index] {
            0 => *style = CellStyle::default(),
            7 => style.inverse = true,
            27 => style.inverse = false,
            30..=37 => style.foreground = ansi_color(values[index] - 30, false),
            39 => style.foreground = DEFAULT_FOREGROUND,
            40..=47 => style.background = ansi_color(values[index] - 40, false),
            49 => style.background = DEFAULT_BACKGROUND,
            90..=97 => style.foreground = ansi_color(values[index] - 90, true),
            100..=107 => style.background = ansi_color(values[index] - 100, true),
            38 | 48 if values.get(index + 1) == Some(&5) => {
                if let Some(color) = values.get(index + 2).copied() {
                    set_extended_color(style, values[index] == 38, indexed_color(color));
                }
                index += 2;
            }
            38 | 48 if values.get(index + 1) == Some(&2) => {
                if let (Some(red), Some(green), Some(blue)) = (
                    values.get(index + 2),
                    values.get(index + 3),
                    values.get(index + 4),
                ) {
                    set_extended_color(
                        style,
                        values[index] == 38,
                        color_rgb(*red as u8, *green as u8, *blue as u8),
                    );
                }
                index += 4;
            }
            _ => {}
        }
        index += 1;
    }
}

fn set_extended_color(style: &mut CellStyle, foreground: bool, color: u32) {
    if foreground {
        style.foreground = color;
    } else {
        style.background = color;
    }
}

fn ansi_color(index: u16, bright: bool) -> u32 {
    const NORMAL: [u32; 8] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5,
    ];
    const BRIGHT: [u32; 8] = [
        0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    if bright {
        BRIGHT[index as usize]
    } else {
        NORMAL[index as usize]
    }
}

fn indexed_color(index: u16) -> u32 {
    const SYSTEM: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
        0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    match index {
        0..=15 => SYSTEM[index as usize],
        16..=231 => {
            let color = index - 16;
            let channel = |value: u16| if value == 0 { 0 } else { 55 + value * 40 };
            color_rgb(
                channel(color / 36) as u8,
                channel((color / 6) % 6) as u8,
                channel(color % 6) as u8,
            )
        }
        232..=255 => {
            let gray = (8 + (index - 232) * 10) as u8;
            color_rgb(gray, gray, gray)
        }
        _ => DEFAULT_FOREGROUND,
    }
}

fn color_rgb(red: u8, green: u8, blue: u8) -> u32 {
    (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
}

fn put_cell(grid: &mut [Vec<TerminalCell>], x: usize, y: usize, character: char, style: CellStyle) {
    if let Some(cell) = grid.get_mut(y).and_then(|row| row.get_mut(x)) {
        *cell = TerminalCell { character, style };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_grid_applies_ansi_foreground_and_truecolor_background() {
        let grid = parse_vt_screen(b"\x1b[31mR\x1b[48;2;1;2;3mG", 8, 1);
        assert_eq!(grid[0][0].character, 'R');
        assert_eq!(grid[0][0].style.foreground, ansi_color(1, false));
        assert_eq!(grid[0][1].character, 'G');
        assert_eq!(grid[0][1].style.background, color_rgb(1, 2, 3));
    }

    #[test]
    fn selection_is_inclusive_and_direction_independent() {
        let selection = Selection {
            anchor: CellPoint { x: 5, y: 1 },
            head: CellPoint { x: 2, y: 1 },
        };
        assert!(selection.contains(CellPoint { x: 2, y: 1 }));
        assert!(selection.contains(CellPoint { x: 5, y: 1 }));
        assert!(!selection.contains(CellPoint { x: 1, y: 1 }));
        assert_eq!(
            selection.ordered(),
            (CellPoint { x: 2, y: 1 }, CellPoint { x: 5, y: 1 })
        );
    }
}
