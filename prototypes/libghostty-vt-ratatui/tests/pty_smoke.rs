use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libghostty_vt_ratatui_spike::{encode_bracketed_paste, GhosttyTerminal};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

struct PtyChild {
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    output: Receiver<io::Result<Vec<u8>>>,
    reader_thread: Option<JoinHandle<()>>,
}

impl PtyChild {
    fn spawn(executable: &Path) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
            })
            .map_err(io::Error::other)?;

        let mut command = CommandBuilder::new(executable.as_os_str());
        command.arg("--fixture-child");
        command.env_remove("NO_COLOR");
        command.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;

        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let writer = pair.master.take_writer().map_err(io::Error::other)?;
        let (output_tx, output) = mpsc::sync_channel(64);
        let reader_thread = thread::Builder::new()
            .name("rootglass-pty-smoke-reader".to_string())
            .spawn(move || {
                let mut buffer = [0; 4096];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(length) => {
                            if output_tx.send(Ok(buffer[..length].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            let _ = output_tx.send(Err(error));
                            break;
                        }
                    }
                }
            })?;

        Ok(Self {
            master: Some(pair.master),
            writer: Some(writer),
            child: Some(child),
            output,
            reader_thread: Some(reader_thread),
        })
    }

    fn wait_for(&self, parser: &mut GhosttyTerminal, needle: &str) -> io::Result<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let screen = parser.screen()?;
            if screen.contains(needle) {
                return Ok(screen);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("screen did not contain {needle:?}; last screen: {screen:?}"),
                ));
            }
            match self
                .output
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(Ok(bytes)) => parser.write(&bytes),
                Ok(Err(error)) => return Err(error),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "PTY child closed",
                    ));
                }
            }
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
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
                pixel_width: cols * 8,
                pixel_height: rows * 16,
            })
            .map_err(io::Error::other)
    }

    fn stop_reader(&mut self) -> io::Result<()> {
        if let Some(reader_thread) = self.reader_thread.take() {
            reader_thread
                .join()
                .map_err(|_| io::Error::other("PTY output reader panicked"))?;
        }
        Ok(())
    }

    fn wait(mut self) -> io::Result<()> {
        let status = self
            .child
            .as_mut()
            .expect("PTY child is present")
            .wait()
            .map_err(io::Error::other)?;
        self.child.take();
        self.writer.take();
        self.master.take();
        self.stop_reader()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "PTY child exited with status {status}"
            )))
        }
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.writer.take();
        self.master.take();
        let _ = self.stop_reader();
    }
}

#[test]
fn libghostty_vt_round_trips_ratatui_over_pty_headlessly() -> io::Result<()> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_libghostty-vt-ratatui-spike"));
    let mut child = PtyChild::spawn(&binary)?;
    let mut ghostty = GhosttyTerminal::new(80, 24)?;

    let screen = child.wait_for(&mut ghostty, "RATATUI-GHOSTTY-FIXTURE")?;
    assert!(screen.contains("LEFT:0"));
    assert!(ghostty.first_cell_is_styled()?);
    assert!(String::from_utf8_lossy(&ghostty.screen_vt()?).contains("\u{1b}["));

    // Ratatui's TestBackend exercises the same TUI rendering code without a display.
    let backend = TestBackend::new(80, 24);
    let mut headless = Terminal::new(backend)?;
    headless.draw(|frame| {
        frame.render_widget(
            Paragraph::new(Text::from(screen.as_str())),
            Rect::new(0, 0, 80, 24),
        );
    })?;
    let rendered = headless
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("RATATUI-GHOSTTY-FIXTURE"));

    // Copy a selection from the parsed terminal screen through Ghostty's public formatter.
    child.write_all(&ghostty.encode_left()?)?;
    let screen = child.wait_for(&mut ghostty, "LEFT:1")?;
    assert!(screen.contains("LEFT:1"));
    let copied = ghostty.copy_range(0, 1, 5, 1)?;
    assert_eq!(copied.trim(), "LEFT:1");

    // The PTY resize reaches Ratatui as a Crossterm event; keep libghostty's grid in sync.
    child.resize(90, 30)?;
    ghostty.resize(90, 30)?;
    let screen = child.wait_for(&mut ghostty, "SIZE:90x30")?;
    assert!(screen.contains("SIZE:90x30"));

    let pasted = encode_bracketed_paste("headless paste")?;
    child.write_all(&pasted)?;
    let screen = child.wait_for(&mut ghostty, "PASTE:headless paste")?;
    assert!(screen.contains("PASTE:headless paste"));

    // A key event encoded from terminal modes exits the fixture; wait proves PTY cleanup.
    child.write_all(&ghostty.encode_left()?)?;
    child.write_all(b"q")?;
    child.wait()
}
