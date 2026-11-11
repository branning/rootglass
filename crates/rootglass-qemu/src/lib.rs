use rootglass_core::{HostAccelerator, LaunchRequest};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

pub const DEFAULT_QEMU_SYSTEM_AARCH64: &str = "qemu-system-aarch64";

#[derive(Debug, Error)]
pub enum QemuError {
    #[error("failed to start QEMU command '{command}': {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to create QMP endpoint: {0}")]
    QmpEndpoint(#[source] std::io::Error),

    #[error("failed to connect to QMP at {address}: {detail}")]
    QmpConnect { address: String, detail: String },

    #[error("{source}; QEMU {process_state}; startup diagnostics: {diagnostic_log}")]
    QmpStartup {
        #[source]
        source: Box<QemuError>,
        process_state: String,
        diagnostic_log: String,
    },

    #[error("QMP command failed: {0}")]
    QmpCommand(String),

    #[error("QEMU process I/O failed during {operation}: {source}")]
    ProcessIo {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accelerator {
    None,
    Hvf,
    Whpx,
    Kvm,
}

impl From<Option<HostAccelerator>> for Accelerator {
    fn from(accelerator: Option<HostAccelerator>) -> Self {
        match accelerator {
            Some(HostAccelerator::Hvf) => Self::Hvf,
            Some(HostAccelerator::Whpx) => Self::Whpx,
            Some(HostAccelerator::Kvm) => Self::Kvm,
            None => Self::None,
        }
    }
}

impl Accelerator {
    pub fn qemu_arg(self) -> Option<&'static str> {
        match self {
            Accelerator::None => None,
            Accelerator::Hvf => Some("hvf"),
            Accelerator::Whpx => Some("whpx"),
            Accelerator::Kvm => Some("kvm"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QemuBinary {
    pub path: PathBuf,
}

impl Default for QemuBinary {
    fn default() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_QEMU_SYSTEM_AARCH64),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QemuLaunchPlan {
    pub binary: QemuBinary,
    pub accelerator: Accelerator,
    pub args: Vec<String>,
}

impl QemuLaunchPlan {
    pub fn for_launch_request(request: &LaunchRequest) -> Self {
        let binary = request
            .qemu_binary
            .clone()
            .unwrap_or_else(|| QemuBinary::default().path);
        Self::for_launch_request_with_binary(request, binary)
    }

    /// Build a launch plan using a caller-resolved QEMU executable. The caller is
    /// responsible for selecting and, where required, verifying the binary.
    pub fn for_launch_request_with_binary(
        request: &LaunchRequest,
        binary_path: impl Into<PathBuf>,
    ) -> Self {
        let accelerator = Accelerator::from(request.accelerator);
        let binary = QemuBinary {
            path: binary_path.into(),
        };
        let mut args = Vec::new();
        if let Some(data_dir) = bundled_qemu_data_dir(&binary.path) {
            args.extend(["-L".to_string(), qemu_path_argument(&data_dir)]);
        }
        args.extend([
            "-machine".to_string(),
            "virt".to_string(),
            "-cpu".to_string(),
            "cortex-a57".to_string(),
            "-m".to_string(),
            request.memory_mb.to_string(),
            "-smp".to_string(),
            request.cpus.to_string(),
            "-kernel".to_string(),
            qemu_path_argument(&request.kernel),
        ]);

        if let Some(device_tree) = &request.device_tree {
            args.push("-dtb".to_string());
            args.push(qemu_path_argument(device_tree));
        }

        if let Some(initrd) = &request.initrd {
            args.push("-initrd".to_string());
            args.push(qemu_path_argument(initrd));
        }

        // On QEMU's aarch64 virt machine, the generated virtio-mmio block
        // devices appear in the guest in reverse of this append order. Add the
        // optional state disk first so the required rootfs lands at /dev/vda,
        // matching the example manifest's kernel args.
        if let Some(overlay) = &request.writable_overlay {
            args.push("-drive".to_string());
            args.push(format!(
                "if=none,file={},format=raw,id=state",
                qemu_path_argument(overlay)
            ));
            args.push("-device".to_string());
            args.push("virtio-blk-device,drive=state".to_string());
        }

        args.extend([
            "-drive".to_string(),
            format!(
                "if=none,file={},format=raw,readonly=on,id=rootfs",
                qemu_path_argument(&request.rootfs)
            ),
            "-device".to_string(),
            "virtio-blk-device,drive=rootfs".to_string(),
            "-nographic".to_string(),
            "-serial".to_string(),
            "mon:stdio".to_string(),
        ]);

        if let Some(accel) = accelerator.qemu_arg() {
            args.push("-accel".to_string());
            args.push(accel.to_string());
        }

        let mut kernel_args = request.kernel_args.clone();
        if request.writable_overlay.is_some()
            && !kernel_args
                .iter()
                .any(|arg| arg.starts_with("rootglass.state="))
        {
            kernel_args.push("rootglass.state=required".to_string());
        }
        if !kernel_args.is_empty() {
            args.push("-append".to_string());
            args.push(kernel_args.join(" "));
        }

        Self {
            binary,
            accelerator,
            args,
        }
    }

    pub fn command_line(&self) -> String {
        let mut parts = vec![shell_quote(&self.binary.path.display().to_string())];
        parts.extend(self.args.iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.binary.path);
        command.args(&self.args);
        configure_bundled_runtime(&mut command, &self.binary.path);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command
    }

    pub fn interactive_command(&self) -> Command {
        let mut command = Command::new(&self.binary.path);
        command.args(&self.args);
        configure_bundled_runtime(&mut command, &self.binary.path);
        command.stdin(Stdio::inherit());
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::inherit());
        command
    }

    pub fn spawn(&self) -> Result<Child, QemuError> {
        let command_line = self.command_line();
        self.command().spawn().map_err(|source| QemuError::Spawn {
            command: command_line,
            source,
        })
    }

    pub fn spawn_interactive(&self) -> Result<Child, QemuError> {
        let command_line = self.command_line();
        self.interactive_command()
            .spawn()
            .map_err(|source| QemuError::Spawn {
                command: command_line,
                source,
            })
    }

    /// Start QEMU with serial input/output on owned pipes and a QMP socket
    /// bound to an ephemeral IPv4 loopback port. The original interactive CLI
    /// path remains available through `spawn_interactive`. Each target OS
    /// still needs an actual QEMU host validation before it is advertised.
    pub fn spawn_supervised(&self) -> Result<QemuProcess, QemuError> {
        QemuProcess::spawn(self)
    }
}

fn qemu_path_argument(path: &Path) -> String {
    // Windows canonical paths may use the verbatim `\\?\` prefix. The
    // bundled Windows QEMU uses that same path in `-L` resource lookup and
    // fails to find ROM files, so pass ordinary paths when they fit.
    let path = path.to_string_lossy();
    if let Some(unc_path) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc_path}")
    } else if let Some(local_path) = path.strip_prefix(r"\\?\") {
        local_path.to_string()
    } else {
        path.into_owned()
    }
}

/// Maximum in-memory serial scrollback retained by a supervised run.
pub const SERIAL_BUFFER_LIMIT: usize = 1024 * 1024;
/// Maximum raw serial and stderr diagnostics retained on disk per run.
pub const DIAGNOSTIC_LOG_LIMIT: u64 = 10 * 1024 * 1024;
/// Maximum QEMU stderr output retained in memory for the desktop Logs tab.
pub const DIAGNOSTIC_STDERR_TAIL_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticTail {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialChunk {
    pub bytes: Vec<u8>,
    /// True when old bytes were dropped to keep the in-memory stream bounded.
    pub truncated: bool,
}

#[derive(Default)]
struct SerialBufferState {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    truncated: bool,
    closed: bool,
}

#[derive(Default)]
struct SerialBuffer {
    state: Mutex<SerialBufferState>,
    available: Condvar,
}

impl SerialBuffer {
    fn push(&self, bytes: Vec<u8>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if bytes.len() >= SERIAL_BUFFER_LIMIT {
            state.chunks.clear();
            state.bytes = 0;
            state
                .chunks
                .push_back(bytes[bytes.len().saturating_sub(SERIAL_BUFFER_LIMIT)..].to_vec());
            state.bytes = SERIAL_BUFFER_LIMIT;
            state.truncated = true;
        } else {
            while state.bytes + bytes.len() > SERIAL_BUFFER_LIMIT {
                if let Some(dropped) = state.chunks.pop_front() {
                    state.bytes = state.bytes.saturating_sub(dropped.len());
                    state.truncated = true;
                } else {
                    break;
                }
            }
            state.bytes += bytes.len();
            state.chunks.push_back(bytes);
        }
        self.available.notify_all();
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.available.notify_all();
        }
    }

    fn read(&self, timeout: Duration) -> Option<SerialChunk> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        if state.chunks.is_empty() && !state.closed && !timeout.is_zero() {
            let result = self.available.wait_timeout(state, timeout);
            let Ok((next, _)) = result else {
                return None;
            };
            state = next;
        }
        let bytes = state.chunks.pop_front()?;
        state.bytes = state.bytes.saturating_sub(bytes.len());
        let truncated = state.truncated;
        state.truncated = false;
        Some(SerialChunk { bytes, truncated })
    }
}

struct BoundedLog {
    file: File,
    written: u64,
}

impl BoundedLog {
    fn write(&mut self, bytes: &[u8]) {
        let available = DIAGNOSTIC_LOG_LIMIT.saturating_sub(self.written) as usize;
        if available == 0 {
            return;
        }
        let length = available.min(bytes.len());
        if self.file.write_all(&bytes[..length]).is_ok() {
            self.written += length as u64;
        }
    }
}

#[derive(Default)]
struct BoundedTail {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl BoundedTail {
    fn push(&mut self, bytes: &[u8]) {
        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(DIAGNOSTIC_STDERR_TAIL_LIMIT);
        if overflow > 0 {
            let drop_old = overflow.min(self.bytes.len());
            for _ in 0..drop_old {
                self.bytes.pop_front();
            }
            let drop_new = overflow - drop_old;
            self.bytes.extend(bytes[drop_new..].iter().copied());
            self.truncated = true;
        } else {
            self.bytes.extend(bytes.iter().copied());
        }
    }

    fn snapshot(&self) -> DiagnosticTail {
        DiagnosticTail {
            bytes: self.bytes.iter().copied().collect(),
            truncated: self.truncated,
        }
    }
}

struct QmpClient {
    writer: TcpStream,
}

impl QmpClient {
    fn connect(port: u16, timeout: Duration) -> Result<Self, QemuError> {
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let printable = address.to_string();
        let deadline = Instant::now() + timeout;
        let mut stream = loop {
            let socket_address = SocketAddr::V4(address);
            match TcpStream::connect_timeout(&socket_address, Duration::from_millis(250)) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    return Err(QemuError::QmpConnect {
                        address: printable,
                        detail: error.to_string(),
                    });
                }
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|source| QemuError::ProcessIo {
                operation: "setting QMP read timeout",
                source,
            })?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|source| QemuError::ProcessIo {
                operation: "setting QMP write timeout",
                source,
            })?;
        let reader_stream = stream.try_clone().map_err(|source| QemuError::ProcessIo {
            operation: "cloning QMP socket",
            source,
        })?;
        let mut reader = BufReader::new(reader_stream);
        read_qmp_line(&mut reader, deadline, &printable)?;
        stream
            .write_all(b"{\"execute\":\"qmp_capabilities\"}\r\n")
            .map_err(|source| QemuError::ProcessIo {
                operation: "negotiating QMP capabilities",
                source,
            })?;
        let response = read_qmp_line(&mut reader, deadline, &printable)?;
        if response.contains("\"error\"") {
            return Err(QemuError::QmpCommand(response));
        }
        Ok(Self { writer: stream })
    }

    fn execute(&mut self, command: &'static str) -> Result<(), QemuError> {
        let message = format!("{{\"execute\":\"{command}\"}}\r\n");
        self.writer
            .write_all(message.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|source| QemuError::ProcessIo {
                operation: "sending QMP command",
                source,
            })
    }
}

fn read_qmp_line(
    reader: &mut BufReader<TcpStream>,
    deadline: Instant,
    address: &str,
) -> Result<String, QemuError> {
    let mut line = String::new();
    loop {
        if Instant::now() >= deadline {
            return Err(QemuError::QmpConnect {
                address: address.to_string(),
                detail: "timed out waiting for QMP greeting/response".to_string(),
            });
        }
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(QemuError::QmpConnect {
                    address: address.to_string(),
                    detail: "QMP closed the connection during negotiation".to_string(),
                });
            }
            Ok(_) => return Ok(line),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(QemuError::ProcessIo {
                    operation: "reading QMP response",
                    source: error,
                });
            }
        }
    }
}

/// One owner controls each supervised QEMU process. Serial output is drained
/// continuously into a bounded ring and a bounded diagnostic file.
pub struct QemuProcess {
    child: Child,
    serial_input: Option<ChildStdin>,
    qmp: Option<QmpClient>,
    serial: Arc<SerialBuffer>,
    diagnostic_log: PathBuf,
    stderr_tail: Arc<Mutex<BoundedTail>>,
}

impl QemuProcess {
    fn spawn(plan: &QemuLaunchPlan) -> Result<Self, QemuError> {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(QemuError::QmpEndpoint)?;
        let port = listener
            .local_addr()
            .map_err(QemuError::QmpEndpoint)?
            .port();
        drop(listener);

        let command_line = plan.command_line();
        let mut command = Command::new(&plan.binary.path);
        command.args(supervised_args(&plan.args, port));
        configure_bundled_runtime(&mut command, &plan.binary.path);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|source| QemuError::Spawn {
            command: command_line,
            source,
        })?;
        let serial_input = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let Some(stdout) = stdout else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(QemuError::QmpCommand(
                "QEMU stdout pipe was not created".to_string(),
            ));
        };
        let Some(stderr) = stderr else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(QemuError::QmpCommand(
                "QEMU stderr pipe was not created".to_string(),
            ));
        };
        let (diagnostic_log, file) = match create_diagnostic_log() {
            Ok(result) => result,
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(QemuError::ProcessIo {
                    operation: "creating diagnostic log",
                    source,
                });
            }
        };
        let bounded_log = Arc::new(Mutex::new(BoundedLog { file, written: 0 }));
        let serial = Arc::new(SerialBuffer::default());
        let stderr_tail = Arc::new(Mutex::new(BoundedTail::default()));
        if let Err(error) = spawn_pipe_reader(
            stdout,
            Some(Arc::clone(&serial)),
            Arc::clone(&bounded_log),
            None,
        ) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        if let Err(error) = spawn_pipe_reader(
            stderr,
            None,
            Arc::clone(&bounded_log),
            Some(Arc::clone(&stderr_tail)),
        ) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }

        let qmp = match QmpClient::connect(port, Duration::from_secs(5)) {
            Ok(qmp) => qmp,
            Err(error) => {
                let process_state = match child.try_wait() {
                    Ok(Some(status)) => format!("exited with status {status:?}"),
                    Ok(None) => {
                        let _ = child.kill();
                        "was still running when QMP initialization failed".to_string()
                    }
                    Err(status_error) => {
                        let _ = child.kill();
                        format!("status could not be read ({status_error})")
                    }
                };
                let _ = child.wait();
                serial.close();
                return Err(QemuError::QmpStartup {
                    source: Box::new(error),
                    process_state,
                    diagnostic_log: diagnostic_log.display().to_string(),
                });
            }
        };
        Ok(Self {
            child,
            serial_input,
            qmp: Some(qmp),
            serial,
            diagnostic_log,
            stderr_tail,
        })
    }

    /// Read the next raw serial chunk, waiting for at most `timeout`.
    pub fn read_serial(&self, timeout: Duration) -> Option<SerialChunk> {
        self.serial.read(timeout)
    }

    /// Send bytes to the guest's serial console.
    pub fn write_serial(&mut self, bytes: &[u8]) -> Result<(), QemuError> {
        let input = self.serial_input.as_mut().ok_or_else(|| {
            QemuError::QmpCommand("guest serial input is already closed".to_string())
        })?;
        input
            .write_all(bytes)
            .and_then(|()| input.flush())
            .map_err(|source| QemuError::ProcessIo {
                operation: "writing guest serial input",
                source,
            })
    }

    /// Return the diagnostic log path for this run.
    pub fn diagnostic_log(&self) -> &std::path::Path {
        &self.diagnostic_log
    }

    /// Return a bounded snapshot of QEMU stderr, kept separate from guest serial output.
    pub fn stderr_tail(&self) -> Result<DiagnosticTail, QemuError> {
        self.stderr_tail
            .lock()
            .map(|tail| tail.snapshot())
            .map_err(|_| QemuError::ProcessIo {
                operation: "reading QEMU diagnostics",
                source: std::io::Error::other("diagnostic buffer lock was poisoned"),
            })
    }

    /// Poll the child without blocking the caller.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, QemuError> {
        self.child
            .try_wait()
            .map_err(|source| QemuError::ProcessIo {
                operation: "polling QEMU",
                source,
            })
    }

    /// Request guest shutdown, then QMP quit, and finally terminate/reap QEMU.
    /// All waits are bounded and repeated calls are safe after process exit.
    pub fn stop(
        &mut self,
        guest_shutdown_timeout: Duration,
        qmp_quit_timeout: Duration,
    ) -> Result<ExitStatus, QemuError> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }

        if let Some(qmp) = self.qmp.as_mut() {
            let _ = qmp.execute("system_powerdown");
        }
        if let Some(status) = self.wait_for_exit(guest_shutdown_timeout)? {
            return Ok(status);
        }

        if let Some(qmp) = self.qmp.as_mut() {
            let _ = qmp.execute("quit");
        }
        if let Some(status) = self.wait_for_exit(qmp_quit_timeout)? {
            return Ok(status);
        }

        self.child.kill().map_err(|source| QemuError::ProcessIo {
            operation: "terminating QEMU",
            source,
        })?;
        self.child.wait().map_err(|source| QemuError::ProcessIo {
            operation: "reaping QEMU",
            source,
        })
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, QemuError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for QemuProcess {
    fn drop(&mut self) {
        if self.try_wait().ok().flatten().is_none() {
            let _ = self.stop(Duration::from_secs(1), Duration::from_secs(1));
        }
    }
}

fn supervised_args(args: &[String], qmp_port: u16) -> Vec<String> {
    let mut supervised = Vec::with_capacity(args.len() + 8);
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-nographic" => index += 1,
            "-serial" => index = (index + 2).min(args.len()),
            "-monitor" | "-qmp" => index = (index + 2).min(args.len()),
            _ => {
                supervised.push(args[index].clone());
                index += 1;
            }
        }
    }
    supervised.extend([
        "-display".to_string(),
        "none".to_string(),
        "-serial".to_string(),
        "stdio".to_string(),
        "-monitor".to_string(),
        "none".to_string(),
        "-qmp".to_string(),
        format!("tcp:127.0.0.1:{qmp_port},server=on,wait=off"),
    ]);
    supervised
}

fn create_diagnostic_log() -> std::io::Result<(PathBuf, File)> {
    static NEXT_LOG_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let directory = std::env::temp_dir();
    for _ in 0..100 {
        let id = NEXT_LOG_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = directory.join(format!("rootglass-qemu-{}-{id}.log", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a unique diagnostic log path",
    ))
}

fn spawn_pipe_reader<R: Read + Send + 'static>(
    mut pipe: R,
    serial: Option<Arc<SerialBuffer>>,
    log: Arc<Mutex<BoundedLog>>,
    stderr_tail: Option<Arc<Mutex<BoundedTail>>>,
) -> Result<(), QemuError> {
    thread::Builder::new()
        .name("rootglass-qemu-output".to_string())
        .spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let bytes = &buffer[..count];
                        if let Some(serial) = &serial {
                            serial.push(bytes.to_vec());
                        }
                        if let Some(stderr_tail) = &stderr_tail {
                            if let Ok(mut tail) = stderr_tail.lock() {
                                tail.push(bytes);
                            }
                        }
                        if let Ok(mut log) = log.lock() {
                            log.write(bytes);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if let Some(serial) = serial {
                serial.close();
            }
        })
        .map(|_| ())
        .map_err(|source| QemuError::ProcessIo {
            operation: "starting output reader",
            source,
        })
}

fn bundled_runtime_root(binary: &Path) -> Option<PathBuf> {
    let root = binary.parent()?.parent()?;
    root.is_dir().then(|| root.to_path_buf())
}

fn bundled_qemu_data_dir(binary: &Path) -> Option<PathBuf> {
    let data_dir = bundled_runtime_root(binary)?.join("share").join("qemu");
    data_dir.is_dir().then_some(data_dir)
}

fn configure_bundled_runtime(command: &mut Command, binary: &Path) {
    let Some(runtime_root) = bundled_runtime_root(binary) else {
        return;
    };

    let library_dir = runtime_root.join("lib");
    if library_dir.is_dir() {
        let module_dir = library_dir.join("qemu");
        if module_dir.is_dir() {
            command.env("QEMU_MODULE_DIR", module_dir);
        }
        #[cfg(target_os = "linux")]
        prepend_command_path(command, "LD_LIBRARY_PATH", &library_dir);
        #[cfg(target_os = "macos")]
        prepend_command_path(command, "DYLD_LIBRARY_PATH", &library_dir);
        #[cfg(target_os = "macos")]
        prepend_command_path(command, "DYLD_FALLBACK_LIBRARY_PATH", &library_dir);
    }

    #[cfg(target_os = "windows")]
    if let Some(binary_dir) = binary.parent() {
        prepend_command_path(command, "PATH", binary_dir);
    }
}

fn prepend_command_path(command: &mut Command, variable: &str, path: &Path) {
    let mut paths = vec![path.to_path_buf()];
    if let Some(existing) = std::env::var_os(variable) {
        paths.extend(std::env::split_paths(&existing));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        command.env(variable, joined);
    }
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_=./:,".contains(c))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootglass_core::{HostCapabilities, HostPlatform, RootglassRuntime};

    #[test]
    fn builds_qemu_command_for_sample_launch_request() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");

        let plan = QemuLaunchPlan::for_launch_request(&request);
        let command_line = plan.command_line();

        assert_eq!(plan.accelerator, Accelerator::None);
        assert!(command_line.contains(DEFAULT_QEMU_SYSTEM_AARCH64));
        assert!(command_line.contains("-machine virt"));
        assert!(command_line.contains("-cpu cortex-a57"));
        assert!(command_line.contains("-kernel runtime-assets/alpine-aarch64/vmlinuz-virt"));
        assert!(!command_line.contains("-dtb"));
        assert!(command_line.contains("-initrd runtime-assets/alpine-aarch64/initramfs-virt"));
        assert!(command_line.contains("readonly=on"));
        assert!(command_line.contains("rootfs.img"));
        assert!(command_line.contains("state.img"));
        assert!(command_line.find("id=state").unwrap() < command_line.find("id=rootfs").unwrap());
        assert!(command_line.contains("console=ttyAMA0"));
        assert!(command_line.contains("rootglass.state=required"));
    }

    #[test]
    fn includes_acceleration_selected_by_core() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities {
            platform: HostPlatform::Linux,
            hvf_available: false,
            whpx_available: false,
            kvm_available: true,
        };
        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");

        let plan = QemuLaunchPlan::for_launch_request(&request);
        assert_eq!(plan.accelerator, Accelerator::Kvm);
        assert!(plan.command_line().contains("-accel kvm"));
    }

    #[test]
    fn strips_windows_verbatim_prefix_from_paths_passed_to_qemu() {
        assert_eq!(
            qemu_path_argument(Path::new(r"\\?\C:\Rootglass\runtime\share\qemu")),
            r"C:\Rootglass\runtime\share\qemu"
        );
        assert_eq!(
            qemu_path_argument(Path::new(r"\\?\UNC\server\share\kernel")),
            r"\\server\share\kernel"
        );
        assert_eq!(
            qemu_path_argument(Path::new(r"C:\Rootglass\runtime\boot\kernel")),
            r"C:\Rootglass\runtime\boot\kernel"
        );
    }

    #[test]
    fn uses_explicit_qemu_binary_when_supplied() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let mut request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");

        let binary = PathBuf::from("/opt/rootglass runtime/bin/qemu-system-aarch64");
        request.qemu_binary = Some(binary.clone());
        let plan = QemuLaunchPlan::for_launch_request(&request);

        assert_eq!(plan.binary.path, binary);
        assert!(plan
            .command_line()
            .starts_with("'/opt/rootglass runtime/bin/qemu-system-aarch64' "));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod supervisor_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FakeQemu {
        root: PathBuf,
        binary: PathBuf,
    }

    impl Drop for FakeQemu {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fake_qemu(mode: &str) -> FakeQemu {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rootglass-fake-qemu-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let binary = root.join("fake-qemu.py");
        let script = r##"#!/usr/bin/env python3
import os
import socket
import sys

MODE = "__MODE__"
args = sys.argv[1:]
endpoint = args[args.index("-qmp") + 1]
port = int(endpoint.rsplit(":", 1)[1].split(",", 1)[0])
server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", port))
server.listen(1)
client, _ = server.accept()
server.close()
with client:
    reader = client.makefile("rb")
    client.sendall(b'{"QMP":{"version":{},"capabilities":[]}}\r\n')
    if not reader.readline():
        sys.exit(2)
    client.sendall(b'{"return":{}}\r\n')
    print("FAKE_QEMU_READY", flush=True)
    if MODE == "diagnostics":
        print("FAKE_QEMU_DIAGNOSTIC", file=sys.stderr, flush=True)
        sys.exit(0)
    if MODE == "eof":
        sys.exit(0)
    if MODE == "crash":
        os._exit(23)
    if MODE == "flood":
        sys.stdout.buffer.write(b"x" * (12 * 1024 * 1024))
        sys.stdout.flush()
        sys.exit(0)
    for line in reader:
        if MODE == "stop" and b'"system_powerdown"' in line:
            print("FAKE_POWERDOWN", flush=True)
            break
        if MODE == "quit" and b'"quit"' in line:
            break
"##
        .replace("__MODE__", mode);
        fs::write(&binary, script).unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();
        FakeQemu { root, binary }
    }

    fn plan(binary: &Path) -> QemuLaunchPlan {
        QemuLaunchPlan {
            binary: QemuBinary {
                path: binary.to_path_buf(),
            },
            accelerator: Accelerator::None,
            args: vec![
                "-machine".to_string(),
                "virt".to_string(),
                "-nographic".to_string(),
                "-serial".to_string(),
                "mon:stdio".to_string(),
            ],
        }
    }

    fn wait_for_exit(process: &mut QemuProcess, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = process.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "fake QEMU did not exit in time");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn supervised_spawn_reports_missing_binary() {
        let missing =
            std::env::temp_dir().join(format!("rootglass-missing-qemu-{}", std::process::id()));
        let error = plan(&missing)
            .spawn_supervised()
            .err()
            .expect("missing QEMU binary must fail to spawn");
        assert!(matches!(error, QemuError::Spawn { .. }));
    }

    #[test]
    fn supervised_stop_is_idempotent_and_uses_guest_powerdown() {
        let fake = fake_qemu("stop");
        let mut process = plan(&fake.binary).spawn_supervised().unwrap();
        let ready = process.read_serial(Duration::from_secs(2)).unwrap();
        assert!(String::from_utf8_lossy(&ready.bytes).contains("FAKE_QEMU_READY"));

        let first = process
            .stop(Duration::from_secs(1), Duration::from_secs(1))
            .unwrap();
        let second = process
            .stop(Duration::from_millis(1), Duration::from_millis(1))
            .unwrap();
        assert!(first.success());
        assert_eq!(first, second);
    }

    #[test]
    fn supervised_stop_uses_qmp_quit_then_kills_and_reaps_on_timeout() {
        let fake = fake_qemu("quit");
        let mut process = plan(&fake.binary).spawn_supervised().unwrap();
        let _ = process.read_serial(Duration::from_secs(2)).unwrap();
        let status = process
            .stop(Duration::from_millis(20), Duration::from_secs(1))
            .unwrap();
        assert!(status.success());
        assert!(process.try_wait().unwrap().is_some());

        let holding = fake_qemu("hold");
        let mut process = plan(&holding.binary).spawn_supervised().unwrap();
        let _ = process.read_serial(Duration::from_secs(2)).unwrap();
        let status = process
            .stop(Duration::from_millis(20), Duration::from_millis(20))
            .unwrap();
        assert!(!status.success(), "forced termination must be visible");
        assert!(
            process.try_wait().unwrap().is_some(),
            "child must be reaped"
        );
    }

    #[test]
    fn dropping_supervised_process_cancels_and_reaps_child() {
        let fake = fake_qemu("hold");
        let process = plan(&fake.binary).spawn_supervised().unwrap();
        let _ = process.read_serial(Duration::from_secs(2)).unwrap();
        let pid = process.child.id();
        drop(process);
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "dropped process must be terminated and reaped"
        );
    }

    #[test]
    fn serial_eof_and_unexpected_exit_are_observable() {
        let eof = fake_qemu("eof");
        let mut process = plan(&eof.binary).spawn_supervised().unwrap();
        let ready = process.read_serial(Duration::from_secs(2)).unwrap();
        assert!(String::from_utf8_lossy(&ready.bytes).contains("FAKE_QEMU_READY"));
        assert!(process.read_serial(Duration::from_secs(2)).is_none());
        assert!(wait_for_exit(&mut process, Duration::from_secs(2)).success());

        let crash = fake_qemu("crash");
        let mut process = plan(&crash.binary).spawn_supervised().unwrap();
        let _ = process.read_serial(Duration::from_secs(2)).unwrap();
        assert_eq!(
            wait_for_exit(&mut process, Duration::from_secs(2)).code(),
            Some(23)
        );
    }

    #[test]
    fn serial_flood_and_diagnostic_log_remain_bounded() {
        let fake = fake_qemu("flood");
        let mut process = plan(&fake.binary).spawn_supervised().unwrap();
        assert!(wait_for_exit(&mut process, Duration::from_secs(5)).success());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !process.serial.state.lock().unwrap().closed && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(process.serial.state.lock().unwrap().closed);

        let mut serial_bytes = 0usize;
        let mut truncated = false;
        while let Some(chunk) = process.read_serial(Duration::ZERO) {
            serial_bytes += chunk.bytes.len();
            truncated |= chunk.truncated;
        }
        assert!(serial_bytes <= SERIAL_BUFFER_LIMIT);
        assert!(truncated, "dropping old serial output must be reported");
        assert_eq!(
            fs::metadata(process.diagnostic_log()).unwrap().len(),
            DIAGNOSTIC_LOG_LIMIT
        );
    }

    #[test]
    fn qemu_stderr_tail_contains_host_diagnostics_without_serial_output() {
        let fake = fake_qemu("diagnostics");
        let process = plan(&fake.binary).spawn_supervised().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let tail = loop {
            let tail = process.stderr_tail().unwrap();
            if !tail.bytes.is_empty() {
                break tail;
            }
            assert!(Instant::now() < deadline, "QEMU stderr was not collected");
            thread::sleep(Duration::from_millis(10));
        };
        let text = String::from_utf8_lossy(&tail.bytes);
        assert!(text.contains("FAKE_QEMU_DIAGNOSTIC"));
        assert!(!text.contains("FAKE_QEMU_READY"));
        assert!(!tail.truncated);
    }

    #[test]
    fn bounded_stderr_tail_retains_the_latest_bytes_and_reports_truncation() {
        let mut tail = BoundedTail::default();
        tail.push(b"older output\n");
        let latest = vec![b'x'; DIAGNOSTIC_STDERR_TAIL_LIMIT];
        tail.push(&latest);

        let snapshot = tail.snapshot();
        assert_eq!(snapshot.bytes, latest);
        assert!(snapshot.truncated);
    }

    #[test]
    fn bounded_stderr_tail_at_capacity_is_not_marked_truncated() {
        let mut tail = BoundedTail::default();
        tail.push(&vec![b'x'; DIAGNOSTIC_STDERR_TAIL_LIMIT]);

        assert_eq!(tail.snapshot().bytes.len(), DIAGNOSTIC_STDERR_TAIL_LIMIT);
        assert!(!tail.snapshot().truncated);
    }
}
