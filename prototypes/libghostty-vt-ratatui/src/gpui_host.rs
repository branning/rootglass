use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gpui_kit::*;
use libghostty_vt_ratatui_spike::{encode_bracketed_paste, GhosttyTerminal};
use portable_pty::{native_pty_system, Child, MasterPty, PtySize};

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const CELL_WIDTH: f32 = 8.0;
const CELL_HEIGHT: f32 = 16.0;
const OUTPUT_CAPACITY: usize = 64;

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

        let mut command =
            portable_pty::CommandBuilder::new(std::env::current_exe().map_err(io::Error::other)?);
        command.arg("--fixture-child");
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
            .name("rootglass-gpui-pty-reader".to_string())
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

    fn try_read(&self) -> Result<Vec<u8>, TryReadError> {
        let Some(output) = &self.output else {
            return Err(TryReadError::Closed);
        };
        match output.try_recv() {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(error)) => Err(TryReadError::Io(error)),
            Err(mpsc::TryRecvError::Empty) => Err(TryReadError::Empty),
            Err(mpsc::TryRecvError::Disconnected) => Err(TryReadError::Closed),
        }
    }
}

enum TryReadError {
    Empty,
    Closed,
    Io(io::Error),
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

impl Drop for TerminalPty {
    fn drop(&mut self) {
        // Wake a reader blocked while sending into a full bounded channel.
        self.output.take();
        self.writer.take();
        self.master.take();
        let mut reaped = false;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            reaped = child.wait().is_ok();
        }
        let mut reader_joined = false;
        if let Some(reader) = self.reader.take() {
            reader_joined = reader.join().is_ok();
        }
        if std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some() {
            if reaped && reader_joined {
                eprintln!("ROOTGLASS_GPUI_PTY_CLEANED");
            } else {
                eprintln!("ROOTGLASS_GPUI_PTY_CLEANUP_FAILED");
            }
        }
    }
}

struct TerminalWindow {
    parser: GhosttyTerminal,
    pty: TerminalPty,
    screen: String,
    ready_reported: bool,
    typed_reported: bool,
    pasted_reported: bool,
    last_fixture_size: Option<String>,
    focus: FocusHandle,
    _bounds_subscription: Subscription,
    _poll_task: Task<()>,
}

impl TerminalWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let parser = GhosttyTerminal::new(INITIAL_COLS, INITIAL_ROWS)
            .expect("create libghostty-vt terminal");
        let pty = TerminalPty::spawn().expect("spawn Ratatui fixture in PTY");
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        let _bounds_subscription = cx.observe_window_bounds(window, |this, window, cx| {
            if let Err(error) = this.resize_to_window(window) {
                this.screen = format!("PTY resize failed: {error}");
                cx.notify();
            }
        });
        let smoke_mode = std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some();
        let smoke_deadline = Instant::now() + Duration::from_secs(30);
        let background_executor = cx.background_executor().clone();
        let _poll_task = cx.spawn(async move |view, cx| loop {
            background_executor.timer(Duration::from_millis(16)).await;
            if smoke_mode && Instant::now() >= smoke_deadline {
                let _ = cx.update(|cx| cx.quit());
                break;
            }
            let _ = view.update(cx, |this, cx| {
                if let Err(error) = this.poll_pty() {
                    this.screen = format!("PTY read failed: {error}");
                }
                cx.notify();
            });
        });

        Self {
            parser,
            pty,
            screen: String::new(),
            ready_reported: false,
            typed_reported: false,
            pasted_reported: false,
            last_fixture_size: None,
            focus,
            _bounds_subscription,
            _poll_task,
        }
    }

    fn poll_pty(&mut self) -> io::Result<()> {
        loop {
            match self.pty.try_read() {
                Ok(bytes) => {
                    self.parser.write(&bytes);
                    let reply = self.parser.take_pty_output()?;
                    if !reply.is_empty() {
                        self.pty.write(&reply)?;
                    }
                }
                Err(TryReadError::Empty | TryReadError::Closed) => break,
                Err(TryReadError::Io(error)) => return Err(error),
            }
        }
        self.screen = self.parser.screen()?;
        if !self.ready_reported && self.screen.contains("RATATUI-GHOSTTY-FIXTURE") {
            self.ready_reported = true;
            if std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some() {
                eprintln!("ROOTGLASS_GPUI_HOST_READY");
            }
        }
        if std::env::var_os("ROOTGLASS_GPUI_SMOKE").is_some() {
            if !self.typed_reported && self.screen.contains("INPUT:headless-key") {
                self.typed_reported = true;
                eprintln!("ROOTGLASS_GPUI_TYPED_READY");
            }
            if !self.pasted_reported && self.screen.contains("INPUT:headless paste") {
                self.pasted_reported = true;
                eprintln!("ROOTGLASS_GPUI_PASTE_READY");
            }
            if let Some(size) = self.screen.lines().find_map(|line| {
                line.trim_start()
                    .strip_prefix("SIZE:")
                    .map(str::to_owned)
            }) {
                if self
                    .last_fixture_size
                    .as_deref()
                    .is_some_and(|last_size| last_size != size.as_str())
                {
                    eprintln!("ROOTGLASS_GPUI_RESIZE_READY {size}");
                }
                self.last_fixture_size = Some(size);
            }
        }
        Ok(())
    }

    fn resize_to_window(&mut self, window: &Window) -> io::Result<()> {
        let bounds = window.bounds();
        let cols = ((bounds.size.width / px(CELL_WIDTH)).floor() as i32).clamp(40, 200) as u16;
        let rows = ((bounds.size.height / px(CELL_HEIGHT)).floor() as i32).clamp(12, 60) as u16;
        self.pty.resize(cols, rows)?;
        self.parser.resize(cols, rows)?;
        Ok(())
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut App) {
        let key = event.keystroke.key.as_str();
        let modifiers = &event.keystroke.modifiers;

        if (modifiers.control || modifiers.platform)
            && modifiers.shift
            && key.eq_ignore_ascii_case("c")
        {
            if let Ok(text) = self.parser.screen() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            return;
        }

        if (modifiers.control || modifiers.platform) && key.eq_ignore_ascii_case("v") {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                if let Ok(encoded) = encode_bracketed_paste(&text) {
                    let _ = self.pty.write(&encoded);
                }
            }
            return;
        }

        let bytes = match key {
            "left" => self.parser.encode_left().ok(),
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
            let _ = self.pty.write(&bytes);
        }
    }
}

impl Render for TerminalWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("terminal-surface")
            .size_full()
            .p_3()
            .bg(rgb(0x11131a))
            .text_color(rgb(0xd7deea))
            .font_family("monospace")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, _, cx| this.handle_key(event, cx)))
            .child(self.screen.clone())
    }
}

pub fn run() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| TerminalWindow::new(window, cx))
            })
            .expect("open GPUI libghostty-vt prototype");
        });
}
