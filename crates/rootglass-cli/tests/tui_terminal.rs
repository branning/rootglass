#![cfg(unix)]

use std::ffi::CStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn tui_restores_terminal_modes_and_leaves_the_alternate_screen_on_q() {
    assert_tui_restores_terminal(b"q");
}

#[test]
fn tui_restores_terminal_modes_on_ctrl_c() {
    assert_tui_restores_terminal(b"\x03");
}

fn assert_tui_restores_terminal(quit_key: &[u8]) {
    let (mut master, slave) = open_pty().expect("create a pseudo-terminal");
    let original_modes = terminal_modes(slave.as_raw_fd()).expect("read initial terminal modes");
    let probe_slave = slave
        .try_clone()
        .expect("clone terminal for mode inspection");
    let root = temporary_root();
    fs::create_dir_all(&root).expect("create isolated user directories");

    let mut child = Command::new(env!("CARGO_BIN_EXE_rootglass"))
        .arg("tui")
        .stdin(Stdio::from(
            slave.try_clone().expect("clone terminal stdin"),
        ))
        .stdout(Stdio::from(
            slave.try_clone().expect("clone terminal stdout"),
        ))
        .stderr(Stdio::from(
            slave.try_clone().expect("clone terminal stderr"),
        ))
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .spawn()
        .expect("spawn the TUI on a full-duplex PTY");
    drop(slave);

    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !output
        .windows(b"Rootglass".len())
        .any(|w| w == b"Rootglass")
    {
        read_available(&mut master, &mut output).expect("read TUI output");
        if let Some(status) = child.try_wait().expect("check TUI process") {
            panic!(
                "TUI exited before rendering, status {status}; output: {}",
                String::from_utf8_lossy(&output)
            );
        }
        assert!(
            Instant::now() < deadline,
            "TUI did not render its first frame; output: {}",
            String::from_utf8_lossy(&output)
        );
        thread::sleep(Duration::from_millis(10));
    }

    master
        .write_all(quit_key)
        .expect("send quit key through the PTY");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        read_available(&mut master, &mut output).expect("read terminal cleanup output");
        if let Some(status) = child.try_wait().expect("check TUI shutdown") {
            break status;
        }
        assert!(Instant::now() < deadline, "TUI did not exit after quit key");
        thread::sleep(Duration::from_millis(10));
    };
    read_available(&mut master, &mut output).expect("drain terminal output");

    assert!(status.success(), "TUI exit status: {status}");
    assert!(
        output
            .windows(b"\x1b[?1049l".len())
            .any(|w| w == b"\x1b[?1049l"),
        "TUI did not leave the alternate screen"
    );
    assert!(
        output
            .windows(b"\x1b[?25h".len())
            .any(|w| w == b"\x1b[?25h"),
        "TUI did not restore the cursor"
    );
    assert_eq!(
        terminal_modes(probe_slave.as_raw_fd()).expect("read restored terminal modes"),
        original_modes,
        "TUI did not restore the PTY terminal modes"
    );

    fs::remove_dir_all(root).expect("remove isolated user directories");
}

#[test]
fn tui_reports_non_interactive_stdio_without_entering_terminal_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_rootglass"))
        .arg("tui")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run TUI with redirected standard streams");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires an interactive terminal"));
    assert!(stderr.contains("full-duplex PTY"));
}

fn open_pty() -> io::Result<(fs::File, fs::File)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { fs::File::from_raw_fd(master) };
    if unsafe { libc::grantpt(master.as_raw_fd()) } < 0
        || unsafe { libc::unlockpt(master.as_raw_fd()) } < 0
    {
        return Err(io::Error::last_os_error());
    }

    let slave_name = unsafe { libc::ptsname(master.as_raw_fd()) };
    if slave_name.is_null() {
        return Err(io::Error::last_os_error());
    }
    let slave_name = unsafe { CStr::from_ptr(slave_name) };
    let slave = unsafe { libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { fs::File::from_raw_fd(slave) };

    let size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

#[derive(Debug, PartialEq, Eq)]
struct TerminalModes {
    input: libc::tcflag_t,
    output: libc::tcflag_t,
    control: libc::tcflag_t,
    local: libc::tcflag_t,
    control_chars: Vec<libc::cc_t>,
}

fn terminal_modes(fd: libc::c_int) -> io::Result<TerminalModes> {
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let settings = unsafe { settings.assume_init() };
    Ok(TerminalModes {
        input: settings.c_iflag,
        output: settings.c_oflag,
        control: settings.c_cflag,
        local: settings.c_lflag,
        control_chars: settings.c_cc.to_vec(),
    })
}

fn read_available(reader: &mut impl Read, output: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(length) => output.extend_from_slice(&buffer[..length]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn temporary_root() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("rootglass-tui-{}-{nonce}", std::process::id()))
}
