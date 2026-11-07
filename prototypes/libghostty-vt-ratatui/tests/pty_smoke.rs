#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use libc::{c_char, c_int, pid_t, winsize};
use libghostty_vt_ratatui_spike::{encode_bracketed_paste, write_all, GhosttyTerminal};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

#[link(name = "util")]
unsafe extern "C" {
    fn forkpty(
        master: *mut c_int,
        name: *mut c_char,
        termp: *const libc::termios,
        winp: *const winsize,
    ) -> pid_t;
}

struct PtyChild {
    fd: RawFd,
    pid: pid_t,
}

impl PtyChild {
    fn spawn(executable: &Path) -> io::Result<Self> {
        let mut fd = -1;
        let initial_size = winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pid = unsafe {
            forkpty(
                &mut fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &initial_size,
            )
        };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            let no_color = CString::new("NO_COLOR").unwrap();
            let term = CString::new("TERM").unwrap();
            let term_value = CString::new("xterm-256color").unwrap();
            unsafe {
                libc::unsetenv(no_color.as_ptr());
                libc::setenv(term.as_ptr(), term_value.as_ptr(), 1);
            }
            let executable = CString::new(executable.as_os_str().as_encoded_bytes()).unwrap();
            let arg0 = executable.clone();
            let arg1 = CString::new("--fixture-child").unwrap();
            unsafe {
                libc::execl(
                    executable.as_ptr(),
                    arg0.as_ptr(),
                    arg1.as_ptr(),
                    std::ptr::null::<c_char>(),
                );
                libc::_exit(127);
            }
        }
        Ok(Self { fd, pid })
    }

    fn wait_for(&self, parser: &mut GhosttyTerminal, needle: &str) -> io::Result<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = [0u8; 4096];
        loop {
            let screen = parser.screen()?;
            if screen.contains(needle) {
                return Ok(screen);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("screen did not contain {needle:?}; last screen: {screen:?}"),
                ));
            }
            let mut pollfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pollfd, 1, 100) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }
            let read = unsafe { libc::read(self.fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "PTY child closed",
                ));
            }
            parser.write(&bytes[..read as usize]);
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let size = winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: cols * 8,
            ws_ypixel: rows * 16,
        };
        if unsafe { libc::ioctl(self.fd, libc::TIOCSWINSZ, &size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn wait(mut self) -> io::Result<()> {
        let mut status = 0;
        if unsafe { libc::waitpid(self.pid, &mut status, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.pid = -1;
        unsafe { libc::close(self.fd) };
        self.fd = -1;
        if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "PTY child exited with wait status {status}"
            )))
        }
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
        if self.pid > 0 {
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
                libc::waitpid(self.pid, std::ptr::null_mut(), 0);
            }
        }
    }
}

#[test]
fn libghostty_vt_round_trips_ratatui_over_pty_headlessly() -> io::Result<()> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_libghostty-vt-ratatui-spike"));
    let child = PtyChild::spawn(&binary)?;
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
    write_all(child.fd, &ghostty.encode_left()?)?;
    let screen = child.wait_for(&mut ghostty, "LEFT:1")?;
    assert!(screen.contains("LEFT:1"));
    let copied = ghostty.copy_range(0, 1, 5, 1)?;
    assert_eq!(copied.trim(), "LEFT:1");

    // TIOCSWINSZ reaches Ratatui as a Crossterm resize event; keep libghostty's grid in sync.
    child.resize(90, 30)?;
    ghostty.resize(90, 30)?;
    let screen = child.wait_for(&mut ghostty, "SIZE:90x30")?;
    assert!(screen.contains("SIZE:90x30"));

    let pasted = encode_bracketed_paste("headless paste")?;
    write_all(child.fd, &pasted)?;
    let screen = child.wait_for(&mut ghostty, "PASTE:headless paste")?;
    assert!(screen.contains("PASTE:headless paste"));

    // A key event encoded from terminal modes exits the fixture; wait proves PTY cleanup.
    write_all(child.fd, &ghostty.encode_left()?)?;
    // Send q as the fixture's shutdown key through the PTY input path.
    write_all(child.fd, b"q")?;
    child.wait()
}
