use rootglass_core::{HostAccelerator, LaunchRequest};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::PathBuf;
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
        Self::for_launch_request_with_binary(request, QemuBinary::default().path)
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
        let mut args = vec![
            "-machine".to_string(),
            "virt".to_string(),
            "-cpu".to_string(),
            "cortex-a57".to_string(),
            "-m".to_string(),
            request.memory_mb.to_string(),
            "-smp".to_string(),
            request.cpus.to_string(),
            "-kernel".to_string(),
            request.kernel.display().to_string(),
        ];

        if let Some(device_tree) = &request.device_tree {
            args.push("-dtb".to_string());
            args.push(device_tree.display().to_string());
        }

        if let Some(initrd) = &request.initrd {
            args.push("-initrd".to_string());
            args.push(initrd.display().to_string());
        }

        // On QEMU's aarch64 virt machine, the generated virtio-mmio block
        // devices appear in the guest in reverse of this append order. Add the
        // optional state disk first so the required rootfs lands at /dev/vda,
        // matching the example manifest's kernel args.
        if let Some(overlay) = &request.writable_overlay {
            args.push("-drive".to_string());
            args.push(format!(
                "if=none,file={},format=raw,id=state",
                overlay.display()
            ));
            args.push("-device".to_string());
            args.push("virtio-blk-device,drive=state".to_string());
        }

        args.extend([
            "-drive".to_string(),
            format!(
                "if=none,file={},format=raw,readonly=on,id=rootfs",
                request.rootfs.display()
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

        if !request.kernel_args.is_empty() {
            args.push("-append".to_string());
            args.push(request.kernel_args.join(" "));
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
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command
    }

    pub fn interactive_command(&self) -> Command {
        let mut command = Command::new(&self.binary.path);
        command.args(&self.args);
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

/// Maximum in-memory serial scrollback retained by a supervised run.
pub const SERIAL_BUFFER_LIMIT: usize = 1024 * 1024;
/// Maximum raw serial and stderr diagnostics retained on disk per run.
pub const DIAGNOSTIC_LOG_LIMIT: u64 = 10 * 1024 * 1024;

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
        if let Err(error) =
            spawn_pipe_reader(stdout, Some(Arc::clone(&serial)), Arc::clone(&bounded_log))
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        if let Err(error) = spawn_pipe_reader(stderr, None, Arc::clone(&bounded_log)) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }

        let qmp = match QmpClient::connect(port, Duration::from_secs(5)) {
            Ok(qmp) => qmp,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                serial.close();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            serial_input,
            qmp: Some(qmp),
            serial,
            diagnostic_log,
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
    fn uses_explicit_qemu_binary_when_supplied() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");

        let binary = PathBuf::from("/opt/rootglass runtime/bin/qemu-system-aarch64");
        let plan = QemuLaunchPlan::for_launch_request_with_binary(&request, binary.clone());

        assert_eq!(plan.binary.path, binary);
        assert!(plan
            .command_line()
            .starts_with("'/opt/rootglass runtime/bin/qemu-system-aarch64' "));
        assert!(!plan.command_line().contains(DEFAULT_QEMU_SYSTEM_AARCH64));
    }
}
