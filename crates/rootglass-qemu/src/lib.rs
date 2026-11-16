use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use rootglass_core::identity::ImageMasterKey;
use rootglass_core::{HostAccelerator, LaunchRequest};
use serde::Deserialize;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

pub const DEFAULT_QEMU_SYSTEM_AARCH64: &str = "qemu-system-aarch64";
const QMP_STARTUP_TIMEOUT: Duration = if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
    // The native Windows ARM64 QEMU bundle loads many MSYS2 DLLs before it
    // opens QMP; allow that cold startup to finish before reporting failure.
    Duration::from_secs(60)
} else {
    Duration::from_secs(5)
};

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

    /// Encrypted setup errors intentionally report only the phase. QMP error
    /// descriptions can echo request properties, so they must never reach logs
    /// or user-facing diagnostics.
    #[error("encrypted QEMU setup failed during {stage}")]
    EncryptedSetup { stage: &'static str },

    #[error(
        "encrypted QEMU setup failed during {stage}; QEMU termination could not be confirmed: {cleanup}"
    )]
    EncryptedCleanup {
        stage: &'static str,
        cleanup: String,
    },

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QemuOutputMode {
    Interactive,
    Headless,
}

impl QemuLaunchPlan {
    pub fn for_launch_request(request: &LaunchRequest) -> Self {
        let binary = request
            .qemu_binary
            .clone()
            .unwrap_or_else(|| QemuBinary::default().path);
        Self::for_launch_request_with_binary_and_output_mode(
            request,
            binary,
            QemuOutputMode::Interactive,
        )
    }

    /// Build a terminal-only plan that sends guest serial to stdio and disables
    /// QEMU's monitor so it cannot consume the user's stdin.
    pub fn for_headless_launch_request(request: &LaunchRequest) -> Self {
        let binary = request
            .qemu_binary
            .clone()
            .unwrap_or_else(|| QemuBinary::default().path);
        Self::for_launch_request_with_binary_and_output_mode(
            request,
            binary,
            QemuOutputMode::Headless,
        )
    }

    /// Build a launch plan using a caller-resolved QEMU executable. The caller is
    /// responsible for selecting and, where required, verifying the binary.
    pub fn for_launch_request_with_binary(
        request: &LaunchRequest,
        binary_path: impl Into<PathBuf>,
    ) -> Self {
        Self::for_launch_request_with_binary_and_output_mode(
            request,
            binary_path,
            QemuOutputMode::Interactive,
        )
    }

    /// Build a headless launch plan using a caller-resolved QEMU executable.
    pub fn for_headless_launch_request_with_binary(
        request: &LaunchRequest,
        binary_path: impl Into<PathBuf>,
    ) -> Self {
        Self::for_launch_request_with_binary_and_output_mode(
            request,
            binary_path,
            QemuOutputMode::Headless,
        )
    }

    /// Build the paused portion of an encrypted launch. The rootfs is attached
    /// later through QMP after Rootglass has installed the zeroizing secret
    /// object. Encrypted runs do not yet support writable overlays.
    pub fn for_encrypted_launch_request(request: &LaunchRequest) -> Result<Self, QemuError> {
        if request.writable_overlay.is_some() {
            return Err(QemuError::EncryptedSetup {
                stage: "validating encrypted launch options",
            });
        }

        let mut plan = Self::for_launch_request_with_binary_and_output_mode(
            request,
            request
                .qemu_binary
                .clone()
                .unwrap_or_else(|| QemuBinary::default().path),
            QemuOutputMode::Headless,
        );
        let mut filtered = Vec::with_capacity(plan.args.len());
        let mut index = 0;
        let mut removed_rootfs = false;
        while index < plan.args.len() {
            match plan.args[index].as_str() {
                "-drive"
                    if plan
                        .args
                        .get(index + 1)
                        .is_some_and(|value| value.ends_with(",id=rootfs")) =>
                {
                    index = (index + 2).min(plan.args.len());
                    removed_rootfs = true;
                }
                "-device"
                    if plan
                        .args
                        .get(index + 1)
                        .is_some_and(|value| value == "virtio-blk-device,drive=rootfs") =>
                {
                    index = (index + 2).min(plan.args.len());
                }
                _ => {
                    filtered.push(plan.args[index].clone());
                    index += 1;
                }
            }
        }
        if !removed_rootfs {
            return Err(QemuError::EncryptedSetup {
                stage: "building encrypted launch options",
            });
        }
        filtered.push("-S".to_string());
        plan.args = filtered;
        Ok(plan)
    }

    fn for_launch_request_with_binary_and_output_mode(
        request: &LaunchRequest,
        binary_path: impl Into<PathBuf>,
        output_mode: QemuOutputMode,
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
        ]);

        // Schema-1 bundles with `mode = "user"` rely on QEMU's existing user-network default.
        // Preserve that behavior; schema-2 network settings are built in the user-network task.
        if request.network_mode == "none" {
            args.extend(["-nic".to_string(), "none".to_string()]);
        }

        match output_mode {
            QemuOutputMode::Interactive => args.extend([
                "-nographic".to_string(),
                "-serial".to_string(),
                "mon:stdio".to_string(),
            ]),
            QemuOutputMode::Headless => args.extend([
                "-display".to_string(),
                "none".to_string(),
                "-serial".to_string(),
                "stdio".to_string(),
                "-monitor".to_string(),
                "none".to_string(),
            ]),
        }

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
        let executable = qemu_executable_path(&self.binary.path);
        let mut parts = vec![shell_quote(&executable.display().to_string())];
        parts.extend(self.args.iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }

    pub fn command(&self) -> Command {
        let binary = qemu_executable_path(&self.binary.path);
        let mut command = Command::new(&binary);
        command.args(&self.args);
        configure_bundled_runtime(&mut command, &binary);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command
    }

    pub fn interactive_command(&self) -> Command {
        let binary = qemu_executable_path(&self.binary.path);
        let mut command = Command::new(&binary);
        command.args(&self.args);
        configure_bundled_runtime(&mut command, &binary);
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

    /// Start paused, copy exactly one verified encrypted extent into a
    /// private read-only temporary image, configure its QEMU LUKS node over
    /// private QMP, and resume only after each setup command succeeds.
    pub fn spawn_encrypted_supervised<R: Read>(
        &self,
        artifact: R,
        artifact_length: u64,
        image_master_key: ImageMasterKey,
    ) -> Result<QemuProcess, QemuError> {
        QemuProcess::spawn_encrypted(self, artifact, artifact_length, image_master_key)
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

fn qemu_executable_path(path: &Path) -> PathBuf {
    // QEMU's Windows runtime does not consistently handle the extended-length
    // `\\?\` prefix in its own executable path. Use the same ordinary path
    // form for process creation that we already use for QEMU arguments.
    PathBuf::from(qemu_path_argument(path))
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

enum QmpControl {
    Tcp(QmpClient),
    Private(PrivateQmpClient),
}

impl QmpControl {
    fn execute(&mut self, command: &'static str) -> Result<(), QemuError> {
        match self {
            Self::Tcp(qmp) => qmp.execute(command),
            Self::Private(qmp) => qmp.execute_without_wait(command),
        }
    }

    fn configure_encrypted_rootfs(
        &mut self,
        image_path: &Path,
        image_master_key: ImageMasterKey,
    ) -> Result<(), QemuError> {
        match self {
            Self::Tcp(_) => Err(QemuError::EncryptedSetup {
                stage: "opening private QMP",
            }),
            Self::Private(qmp) => qmp.configure_encrypted_rootfs(image_path, image_master_key),
        }
    }
}

#[cfg(unix)]
enum PrivateQmpTransport {
    Unix(UnixStream),
}

#[cfg(windows)]
enum PrivateQmpTransport {
    NamedPipe(NamedPipeStream),
}

#[cfg(unix)]
impl Read for PrivateQmpTransport {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buffer),
        }
    }
}

#[cfg(windows)]
impl Read for PrivateQmpTransport {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::NamedPipe(stream) => stream.read(buffer),
        }
    }
}

#[cfg(unix)]
impl Write for PrivateQmpTransport {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
        }
    }
}

#[cfg(windows)]
impl Write for PrivateQmpTransport {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::NamedPipe(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::NamedPipe(stream) => stream.flush(),
        }
    }
}

#[derive(Deserialize)]
struct PrivateQmpReply {
    #[serde(rename = "QMP")]
    greeting: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    event: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    id: Option<u64>,
    #[serde(rename = "return", default)]
    result: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    error: Option<serde::de::IgnoredAny>,
}

struct PrivateQmpClient {
    stream: PrivateQmpTransport,
    next_id: u64,
}

impl PrivateQmpClient {
    fn connect(stream: PrivateQmpTransport, timeout: Duration) -> Result<Self, QemuError> {
        let deadline = Instant::now() + timeout;
        let mut client = Self { stream, next_id: 1 };
        let greeting = client.read_reply(deadline, "reading QMP greeting")?;
        if greeting.greeting.is_none() {
            return Err(QemuError::EncryptedSetup {
                stage: "validating QMP greeting",
            });
        }
        client.execute_value("qmp_capabilities", serde_json::json!({}), "negotiating QMP")?;
        Ok(client)
    }

    fn execute_without_wait(&mut self, command: &'static str) -> Result<(), QemuError> {
        let request = serde_json::json!({ "execute": command });
        let bytes = serde_json::to_vec(&request).map_err(|_| QemuError::EncryptedSetup {
            stage: "encoding QMP shutdown command",
        })?;
        self.stream
            .write_all(&bytes)
            .and_then(|()| self.stream.write_all(b"\r\n"))
            .and_then(|()| self.stream.flush())
            .map_err(|_| QemuError::EncryptedSetup {
                stage: "sending QMP shutdown command",
            })
    }

    fn execute_value(
        &mut self,
        command: &'static str,
        arguments: serde_json::Value,
        stage: &'static str,
    ) -> Result<(), QemuError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let request = Zeroizing::new(
            serde_json::to_vec(&serde_json::json!({
                "execute": command,
                "arguments": arguments,
                "id": id,
            }))
            .map_err(|_| QemuError::EncryptedSetup { stage })?,
        );
        self.execute_request(&request, id, stage)
    }

    fn add_image_master_key(&mut self, key: &ImageMasterKey) -> Result<(), QemuError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let mut encoded_key = Zeroizing::new([0u8; 44]);
        BASE64_STANDARD
            .encode_slice(key.as_bytes(), encoded_key.as_mut())
            .map_err(|_| QemuError::EncryptedSetup {
                stage: "encoding QEMU secret",
            })?;

        // Construct the secret-bearing request without an intermediate String
        // or JSON Value. Both encoded and serialized key bytes are zeroized.
        let mut request = Zeroizing::new(Vec::with_capacity(192));
        request.extend_from_slice(
            br#"{"execute":"object-add","arguments":{"qom-type":"secret","id":"rootglass-imk","props":{"format":"base64","data":""#,
        );
        request.extend_from_slice(&encoded_key[..]);
        request.extend_from_slice(b"\"}},\"id\":");
        request.extend_from_slice(id.to_string().as_bytes());
        request.extend_from_slice(b"}");
        encoded_key.zeroize();
        self.execute_request(&request, id, "adding QEMU secret")
    }

    fn configure_encrypted_rootfs(
        &mut self,
        image_path: &Path,
        image_master_key: ImageMasterKey,
    ) -> Result<(), QemuError> {
        let filename = image_path.to_string_lossy().into_owned();
        self.add_image_master_key(&image_master_key)?;
        self.execute_value(
            "blockdev-add",
            serde_json::json!({
                "driver": "file",
                "filename": filename,
                "node-name": "rootglass-encrypted-file",
                "read-only": true,
            }),
            "adding read-only encrypted file node",
        )?;
        self.execute_value(
            "blockdev-add",
            serde_json::json!({
                "driver": "raw",
                "file": "rootglass-encrypted-file",
                "node-name": "rootglass-encrypted-raw",
                "read-only": true,
            }),
            "adding read-only raw disk node",
        )?;
        self.execute_value(
            "blockdev-add",
            serde_json::json!({
                "driver": "luks",
                "file": "rootglass-encrypted-raw",
                "node-name": "rootglass-encrypted-rootfs",
                "key-secret": "rootglass-imk",
                "read-only": true,
            }),
            "opening encrypted rootfs",
        )?;
        self.execute_value(
            "device_add",
            serde_json::json!({
                "driver": "virtio-blk-device",
                "drive": "rootglass-encrypted-rootfs",
                "id": "rootglass-encrypted-rootfs-device",
            }),
            "attaching encrypted rootfs",
        )?;
        self.execute_value("cont", serde_json::json!({}), "resuming encrypted guest")?;
        drop(image_master_key);
        Ok(())
    }

    fn execute_request(
        &mut self,
        request: &[u8],
        id: u64,
        stage: &'static str,
    ) -> Result<(), QemuError> {
        self.stream
            .write_all(request)
            .and_then(|()| self.stream.write_all(b"\r\n"))
            .and_then(|()| self.stream.flush())
            .map_err(|_| QemuError::EncryptedSetup { stage })?;
        let deadline = Instant::now() + QMP_STARTUP_TIMEOUT;
        loop {
            let response = self.read_reply(deadline, stage)?;
            if response.event.is_some() || response.id != Some(id) {
                continue;
            }
            if response.error.is_some() || response.result.is_none() {
                return Err(QemuError::EncryptedSetup { stage });
            }
            return Ok(());
        }
    }

    fn read_reply(
        &mut self,
        deadline: Instant,
        stage: &'static str,
    ) -> Result<PrivateQmpReply, QemuError> {
        let mut line = Zeroizing::new(Vec::with_capacity(512));
        loop {
            if Instant::now() >= deadline || line.len() >= 64 * 1024 {
                return Err(QemuError::EncryptedSetup { stage });
            }
            let mut byte = [0u8; 1];
            match self.stream.read(&mut byte) {
                Ok(0) => return Err(QemuError::EncryptedSetup { stage }),
                Ok(_) => {
                    let is_newline = byte[0] == b'\n';
                    line.push(byte[0]);
                    byte.zeroize();
                    if is_newline {
                        return serde_json::from_slice(&line)
                            .map_err(|_| QemuError::EncryptedSetup { stage });
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return Err(QemuError::EncryptedSetup { stage }),
            }
        }
    }
}

struct EncryptedTempDir {
    path: PathBuf,
}

impl EncryptedTempDir {
    fn create_in(parent: &Path) -> Result<Self, QemuError> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..100 {
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = parent.join(format!("rootglass-qmp-{}-{id}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(QemuError::ProcessIo {
                        operation: "creating private encrypted QEMU directory",
                        source,
                    });
                }
            }
        }
        Err(QemuError::EncryptedSetup {
            stage: "reserving private QEMU directory",
        })
    }

    fn image_path(&self) -> PathBuf {
        self.path.join("encrypted-rootfs.img")
    }
}

impl Drop for EncryptedTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn materialize_encrypted_image<R: Read>(
    directory: &EncryptedTempDir,
    artifact: R,
    artifact_length: u64,
) -> Result<PathBuf, QemuError> {
    if artifact_length == 0 {
        return Err(QemuError::EncryptedSetup {
            stage: "validating encrypted artifact length",
        });
    }
    let path = directory.image_path();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|_| QemuError::EncryptedSetup {
        stage: "creating private encrypted image",
    })?;
    let mut limited = artifact.take(artifact_length);
    let copied = std::io::copy(&mut limited, &mut file).map_err(|_| QemuError::EncryptedSetup {
        stage: "copying verified encrypted extent",
    })?;
    if copied != artifact_length {
        return Err(QemuError::EncryptedSetup {
            stage: "validating encrypted extent length",
        });
    }
    file.sync_all().map_err(|_| QemuError::EncryptedSetup {
        stage: "syncing private encrypted image",
    })?;
    drop(file);

    let mut permissions = fs::metadata(&path)
        .map_err(|_| QemuError::EncryptedSetup {
            stage: "protecting private encrypted image",
        })?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o400);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).map_err(|_| QemuError::EncryptedSetup {
        stage: "protecting private encrypted image",
    })?;
    Ok(path)
}

#[cfg(unix)]
struct PrivateQmpEndpoint {
    listener: UnixListener,
    socket_path: PathBuf,
}

#[cfg(unix)]
impl PrivateQmpEndpoint {
    fn bind(directory: &EncryptedTempDir) -> Result<Self, QemuError> {
        use std::os::unix::fs::PermissionsExt;

        let socket_path = directory.path.join("q");
        if socket_path.to_string_lossy().contains(',') {
            return Err(QemuError::EncryptedSetup {
                stage: "validating private QMP socket path",
            });
        }
        let listener = UnixListener::bind(&socket_path).map_err(QemuError::QmpEndpoint)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .map_err(QemuError::QmpEndpoint)?;
        listener
            .set_nonblocking(true)
            .map_err(QemuError::QmpEndpoint)?;
        Ok(Self {
            listener,
            socket_path,
        })
    }

    fn qemu_argument(&self) -> String {
        format!("unix:{},server=off,wait=off", self.socket_path.display())
    }

    fn connect(self, child: &mut Child, timeout: Duration) -> Result<PrivateQmpClient, QemuError> {
        let deadline = Instant::now() + timeout;
        let stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if child
                        .try_wait()
                        .map_err(|_| QemuError::EncryptedSetup {
                            stage: "checking QEMU during private QMP startup",
                        })?
                        .is_some()
                    {
                        return Err(QemuError::EncryptedSetup {
                            stage: "waiting for private QMP connection",
                        });
                    }
                    if Instant::now() >= deadline {
                        return Err(QemuError::EncryptedSetup {
                            stage: "waiting for private QMP connection",
                        });
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    return Err(QemuError::EncryptedSetup {
                        stage: "accepting private QMP connection",
                    });
                }
            }
        };
        let _ = fs::remove_file(&self.socket_path);
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|_| QemuError::EncryptedSetup {
                stage: "setting private QMP read timeout",
            })?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|_| QemuError::EncryptedSetup {
                stage: "setting private QMP write timeout",
            })?;
        PrivateQmpClient::connect(PrivateQmpTransport::Unix(stream), timeout)
    }
}

#[cfg(unix)]
impl Drop for PrivateQmpEndpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

#[cfg(windows)]
struct NamedPipeStream {
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl Read for NamedPipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        use std::os::windows::io::AsRawHandle;

        if buffer.is_empty() {
            return Ok(0);
        }
        let mut read = 0u32;
        let count = buffer.len().min(u32::MAX as usize) as u32;
        let succeeded = unsafe {
            win32::ReadFile(
                self.handle.as_raw_handle() as win32::Handle,
                buffer.as_mut_ptr().cast(),
                count,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if succeeded != 0 {
            return Ok(read as usize);
        }
        match unsafe { win32::GetLastError() } {
            win32::ERROR_NO_DATA => Err(std::io::ErrorKind::WouldBlock.into()),
            win32::ERROR_BROKEN_PIPE | win32::ERROR_PIPE_NOT_CONNECTED => Ok(0),
            code => Err(std::io::Error::from_raw_os_error(code as i32)),
        }
    }
}

#[cfg(windows)]
impl Write for NamedPipeStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        use std::os::windows::io::AsRawHandle;

        if buffer.is_empty() {
            return Ok(0);
        }
        let mut written = 0u32;
        let count = buffer.len().min(u32::MAX as usize) as u32;
        let succeeded = unsafe {
            win32::WriteFile(
                self.handle.as_raw_handle() as win32::Handle,
                buffer.as_ptr().cast(),
                count,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if succeeded != 0 {
            return Ok(written as usize);
        }
        match unsafe { win32::GetLastError() } {
            win32::ERROR_NO_DATA => Err(std::io::ErrorKind::WouldBlock.into()),
            win32::ERROR_BROKEN_PIPE | win32::ERROR_PIPE_NOT_CONNECTED => {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            code => Err(std::io::Error::from_raw_os_error(code as i32)),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
mod win32 {
    use std::ffi::c_void;

    pub type Handle = *mut c_void;
    pub const ERROR_NO_DATA: u32 = 232;
    pub const ERROR_BROKEN_PIPE: u32 = 109;
    pub const ERROR_PIPE_NOT_CONNECTED: u32 = 233;
    pub const ERROR_PIPE_CONNECTED: u32 = 535;
    pub const ERROR_PIPE_LISTENING: u32 = 536;

    pub const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    pub const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    pub const PIPE_TYPE_BYTE: u32 = 0;
    pub const PIPE_READMODE_BYTE: u32 = 0;
    pub const PIPE_NOWAIT: u32 = 1;
    pub const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
    pub const SDDL_REVISION_1: u32 = 1;

    #[repr(C)]
    pub struct SecurityAttributes {
        pub length: u32,
        pub descriptor: *mut c_void,
        pub inherit_handle: i32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout: u32,
            security_attributes: *const SecurityAttributes,
        ) -> Handle;
        pub fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        pub fn GetLastError() -> u32;
        pub fn LocalFree(memory: Handle) -> Handle;
        pub fn ReadFile(
            file: Handle,
            buffer: *mut c_void,
            bytes_to_read: u32,
            bytes_read: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        pub fn WriteFile(
            file: Handle,
            buffer: *const c_void,
            bytes_to_write: u32,
            bytes_written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_string: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            descriptor_size: *mut u32,
        ) -> i32;
    }
}

#[cfg(windows)]
struct PrivateQmpEndpoint {
    name: String,
    stream: Option<NamedPipeStream>,
}

#[cfg(windows)]
impl PrivateQmpEndpoint {
    fn bind(_directory: &EncryptedTempDir) -> Result<Self, QemuError> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\rootglass-qmp-{}-{id}", std::process::id());
        let wide_name = name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let sddl = "D:P(A;;GA;;;OW)"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        let converted = unsafe {
            win32::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                win32::SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(QemuError::QmpEndpoint(std::io::Error::last_os_error()));
        }
        let attributes = win32::SecurityAttributes {
            length: std::mem::size_of::<win32::SecurityAttributes>() as u32,
            descriptor,
            inherit_handle: 0,
        };
        let raw_handle = unsafe {
            win32::CreateNamedPipeW(
                wide_name.as_ptr(),
                win32::PIPE_ACCESS_DUPLEX | win32::FILE_FLAG_FIRST_PIPE_INSTANCE,
                win32::PIPE_TYPE_BYTE
                    | win32::PIPE_READMODE_BYTE
                    | win32::PIPE_NOWAIT
                    | win32::PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                &attributes,
            )
        };
        unsafe {
            win32::LocalFree(descriptor);
        }
        if raw_handle == (-1isize) as win32::Handle || raw_handle.is_null() {
            return Err(QemuError::QmpEndpoint(std::io::Error::last_os_error()));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw_handle.cast()) };
        Ok(Self {
            name,
            stream: Some(NamedPipeStream { handle }),
        })
    }

    fn qemu_argument(&self) -> String {
        format!("pipe:{},server=off,wait=off", self.name)
    }

    fn connect(
        mut self,
        child: &mut Child,
        timeout: Duration,
    ) -> Result<PrivateQmpClient, QemuError> {
        use std::os::windows::io::AsRawHandle;

        let stream = self.stream.as_mut().expect("named pipe server exists");
        let handle = stream.handle.as_raw_handle() as win32::Handle;
        let deadline = Instant::now() + timeout;
        loop {
            let connected = unsafe { win32::ConnectNamedPipe(handle, std::ptr::null_mut()) };
            if connected != 0 {
                break;
            }
            let code = unsafe { win32::GetLastError() };
            if code == win32::ERROR_PIPE_CONNECTED {
                break;
            }
            if code != win32::ERROR_PIPE_LISTENING {
                return Err(QemuError::EncryptedSetup {
                    stage: "accepting private QMP connection",
                });
            }
            if child
                .try_wait()
                .map_err(|_| QemuError::EncryptedSetup {
                    stage: "checking QEMU during private QMP startup",
                })?
                .is_some()
                || Instant::now() >= deadline
            {
                return Err(QemuError::EncryptedSetup {
                    stage: "waiting for private QMP connection",
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
        let stream = self.stream.take().expect("connected pipe server exists");
        PrivateQmpClient::connect(PrivateQmpTransport::NamedPipe(stream), timeout)
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
    qmp: Option<QmpControl>,
    serial: Arc<SerialBuffer>,
    diagnostic_log: PathBuf,
    stderr_tail: Arc<Mutex<BoundedTail>>,
    encrypted_temp_dir: Option<EncryptedTempDir>,
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

        Self::spawn_with_connector(
            plan,
            supervised_args(&plan.args, port),
            move |_| QmpClient::connect(port, QMP_STARTUP_TIMEOUT).map(QmpControl::Tcp),
            None,
        )
    }

    fn spawn_encrypted<R: Read>(
        plan: &QemuLaunchPlan,
        artifact: R,
        artifact_length: u64,
        image_master_key: ImageMasterKey,
    ) -> Result<Self, QemuError> {
        Self::spawn_encrypted_with_parent(
            plan,
            artifact,
            artifact_length,
            image_master_key,
            &std::env::temp_dir(),
        )
    }

    fn spawn_encrypted_with_parent<R: Read>(
        plan: &QemuLaunchPlan,
        artifact: R,
        artifact_length: u64,
        image_master_key: ImageMasterKey,
        temp_parent: &Path,
    ) -> Result<Self, QemuError> {
        if !plan.args.iter().any(|argument| argument == "-S") {
            return Err(QemuError::EncryptedSetup {
                stage: "validating paused launch options",
            });
        }
        let encrypted_temp_dir = EncryptedTempDir::create_in(temp_parent)?;
        let image_path =
            materialize_encrypted_image(&encrypted_temp_dir, artifact, artifact_length)?;
        let endpoint = PrivateQmpEndpoint::bind(&encrypted_temp_dir)?;
        let qmp_argument = endpoint.qemu_argument();
        let args = supervised_args_with_qmp(&plan.args, qmp_argument);
        let mut process = Self::spawn_with_connector(
            plan,
            args,
            move |child| {
                endpoint
                    .connect(child, QMP_STARTUP_TIMEOUT)
                    .map(QmpControl::Private)
            },
            Some(encrypted_temp_dir),
        )?;
        let setup_result = process
            .qmp
            .as_mut()
            .ok_or(QemuError::EncryptedSetup {
                stage: "initializing encrypted QMP",
            })?
            .configure_encrypted_rootfs(&image_path, image_master_key);
        if let Err(error) = setup_result {
            if let Err(cleanup) = process.abort_and_reap() {
                let stage = match &error {
                    QemuError::EncryptedSetup { stage } => *stage,
                    _ => "encrypted QMP setup",
                };
                return Err(QemuError::EncryptedCleanup {
                    stage,
                    cleanup: cleanup.to_string(),
                });
            }
            return Err(error);
        }
        Ok(process)
    }

    fn spawn_with_connector(
        plan: &QemuLaunchPlan,
        args: Vec<String>,
        connect_qmp: impl FnOnce(&mut Child) -> Result<QmpControl, QemuError>,
        encrypted_temp_dir: Option<EncryptedTempDir>,
    ) -> Result<Self, QemuError> {
        let binary = qemu_executable_path(&plan.binary.path);
        let command_line = command_line(&binary, &args);

        let mut command = Command::new(&binary);
        command.args(&args);
        configure_bundled_runtime(&mut command, &binary);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Keep terminal SIGINT directed at the supervising CLI. It can
            // then request guest shutdown through QMP before terminating QEMU.
            command.process_group(0);
        }
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

        let qmp = match connect_qmp(&mut child) {
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
            encrypted_temp_dir,
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
        let status = self
            .child
            .try_wait()
            .map_err(|source| QemuError::ProcessIo {
                operation: "polling QEMU",
                source,
            })?;
        if status.is_some() {
            self.cleanup_after_exit();
        }
        Ok(status)
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
        let status = self.child.wait().map_err(|source| QemuError::ProcessIo {
            operation: "reaping QEMU",
            source,
        })?;
        self.cleanup_after_exit();
        Ok(status)
    }

    fn abort_and_reap(&mut self) -> Result<(), QemuError> {
        if let Err(kill_error) = self.child.kill() {
            let exited = self
                .child
                .try_wait()
                .map_err(|source| QemuError::ProcessIo {
                    operation: "checking QEMU after failed termination",
                    source,
                })?
                .is_some();
            if !exited {
                return Err(QemuError::ProcessIo {
                    operation: "terminating QEMU after encrypted setup failure",
                    source: kill_error,
                });
            }
        }
        self.child.wait().map_err(|source| QemuError::ProcessIo {
            operation: "reaping QEMU after encrypted setup failure",
            source,
        })?;
        self.cleanup_after_exit();
        Ok(())
    }

    fn cleanup_after_exit(&mut self) {
        self.serial_input.take();
        self.qmp.take();
        self.encrypted_temp_dir.take();
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
    supervised_args_with_qmp(args, format!("tcp:127.0.0.1:{qmp_port},server=on,wait=off"))
}

fn supervised_args_with_qmp(args: &[String], endpoint: String) -> Vec<String> {
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
        endpoint,
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

    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    configure_windows_arm64_msys2_environment(command);
}

#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
fn configure_windows_arm64_msys2_environment(command: &mut Command) {
    // QEMU from the MSYS2 CLANGARM64 runtime hangs before QMP startup unless
    // its child environment matches the one established by the MSYS2 login shell.
    command
        .env("MSYSTEM", "CLANGARM64")
        .env("MINGW_PREFIX", "/clangarm64")
        .env("MSYSTEM_PREFIX", "/clangarm64")
        .env("MINGW_CHOST", "aarch64-w64-mingw32")
        .env("MSYSTEM_CARCH", "aarch64")
        .env("MSYSTEM_CHOST", "aarch64-w64-mingw32")
        .env("MSYS2_PATH_TYPE", "inherit")
        .env_remove("CHERE_INVOKING");

    let username = std::env::var("USERNAME")
        .ok()
        .filter(|username| !username.is_empty())
        .or_else(|| {
            std::env::var_os("USERPROFILE").and_then(|profile| {
                Path::new(&profile)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        });
    if let Some(username) = username {
        command.env("HOME", format!("/home/{username}"));
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

fn command_line(binary: &Path, args: &[String]) -> String {
    let mut parts = vec![shell_quote(&binary.display().to_string())];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
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
        assert_eq!(request.network_mode, "user");
        assert!(!plan
            .args
            .windows(2)
            .any(|args| args[0] == "-nic" && args[1] == "none"));
    }

    #[test]
    fn builds_headless_qemu_command_without_a_stdio_monitor() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");

        let plan = QemuLaunchPlan::for_headless_launch_request(&request);

        assert!(plan
            .args
            .windows(2)
            .any(|args| args == ["-display", "none"]));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args == ["-serial", "stdio"]));
        assert!(plan
            .args
            .windows(2)
            .any(|args| args == ["-monitor", "none"]));
        assert!(!plan.args.iter().any(|arg| arg == "-nographic"));
        assert!(!plan.args.iter().any(|arg| arg == "mon:stdio"));
    }

    #[test]
    fn disables_qemu_network_when_manifest_declares_none() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let mut request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");
        request.network_mode = "none".to_string();

        let plan = QemuLaunchPlan::for_launch_request(&request);

        assert!(plan
            .args
            .windows(2)
            .any(|args| args[0] == "-nic" && args[1] == "none"));
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

    #[test]
    fn encrypted_launch_starts_paused_without_a_plain_rootfs_argument() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let mut request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");
        request.writable_overlay = None;

        let plan = QemuLaunchPlan::for_encrypted_launch_request(&request)
            .expect("encrypted plan can be built");

        assert!(plan.args.iter().any(|argument| argument == "-S"));
        assert!(!plan.args.iter().any(|argument| argument == "-drive"));
        assert!(!plan
            .args
            .iter()
            .any(|argument| argument.contains("rootfs.img")));
        assert!(!plan
            .args
            .iter()
            .any(|argument| argument == "virtio-blk-device,drive=rootfs"));
    }

    #[test]
    fn encrypted_launch_rejects_a_writable_overlay() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");

        let error = QemuLaunchPlan::for_encrypted_launch_request(&request)
            .expect_err("encrypted launch does not allow a writable overlay");

        assert!(error.to_string().contains("encrypted QEMU setup failed"));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod supervisor_tests {
    use super::*;
    use rootglass_core::{HostCapabilities, HostPlatform, RootglassRuntime};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

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
        static NEXT_FAKE_QEMU_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let root = loop {
            let id = NEXT_FAKE_QEMU_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = std::env::temp_dir()
                .join(format!("rootglass-fake-qemu-{}-{id}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fake QEMU fixture directory: {error}"),
            }
        };
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

    struct FakePrivateQemu {
        root: PathBuf,
        binary: PathBuf,
        trace: PathBuf,
        pid_file: PathBuf,
    }

    impl Drop for FakePrivateQemu {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fake_private_qemu() -> FakePrivateQemu {
        static NEXT_PRIVATE_QEMU_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let root = loop {
            let id = NEXT_PRIVATE_QEMU_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = std::env::temp_dir().join(format!(
                "rootglass-private-qemu-{}-{id}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fake private QEMU fixture directory: {error}"),
            }
        };
        let binary = root.join("fake-private-qemu.py");
        let trace = root.join("qmp-trace");
        let pid_file = root.join("qemu.pid");
        let script = r##"#!/usr/bin/env python3
import json
import os
import socket
import sys
import time

args = sys.argv[1:]
def option(name):
    return args[args.index(name) + 1]

trace = option("--rootglass-trace")
pid_file = option("--rootglass-pidfile")
fail_at = option("--rootglass-fail-at")
timeout_at = option("--rootglass-timeout-at")
open(pid_file, "w").write(str(os.getpid()))
endpoint = option("-qmp")
socket_path = endpoint[len("unix:"):].split(",server=", 1)[0]
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(socket_path)
with client:
    reader = client.makefile("rb")
    client.sendall(b'{"QMP":{"version":{},"capabilities":[]}}\r\n')
    command_counts = {}
    for line in reader:
        request = json.loads(line)
        command = request.get("execute", "")
        arguments = request.get("arguments", {})
        command_counts[command] = command_counts.get(command, 0) + 1
        if command == "object-add":
            props = arguments.get("props", {})
            valid = props.get("format") == "base64" and len(props.get("data", "")) == 44
            trace_line = "object-add:secret-present" if valid else "object-add:secret-missing"
        else:
            trace_line = command
        with open(trace, "a") as output:
            output.write(trace_line + "\n")
        stage = command + ("-" + str(command_counts[command]) if command == "blockdev-add" else "")
        if command == timeout_at:
            time.sleep(30)
            continue
        if command == fail_at or stage == fail_at:
            description = arguments.get("props", {}).get("data", "injected QMP failure")
            response = {"error": {"class": "GenericError", "desc": description}, "id": request.get("id")}
        else:
            response = {"return": {}, "id": request.get("id")}
        client.sendall(json.dumps(response).encode() + b"\r\n")
        if command == "system_powerdown":
            break
"##;
        fs::write(&binary, script).unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();
        FakePrivateQemu {
            root,
            binary,
            trace,
            pid_file,
        }
    }

    fn private_plan(fake: &FakePrivateQemu, fail_at: &str, timeout_at: &str) -> QemuLaunchPlan {
        let mut plan = plan(&fake.binary);
        plan.args.extend([
            "--rootglass-trace".to_string(),
            fake.trace.display().to_string(),
            "--rootglass-pidfile".to_string(),
            fake.pid_file.display().to_string(),
            "--rootglass-fail-at".to_string(),
            fail_at.to_string(),
            "--rootglass-timeout-at".to_string(),
            timeout_at.to_string(),
            "-S".to_string(),
        ]);
        plan
    }

    fn test_image_master_key() -> ImageMasterKey {
        ImageMasterKey::new(Zeroizing::new([0x5a; 32]))
    }

    fn assert_private_qemu_reaped(fake: &FakePrivateQemu) {
        let pid: u32 = fs::read_to_string(&fake.pid_file)
            .expect("fake QEMU records its process id")
            .parse()
            .expect("fake QEMU process id is numeric");
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "encrypted setup must terminate and reap fake QEMU"
        );
        assert!(fs::read_dir(&fake.root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("rootglass-qmp-")));
    }

    #[test]
    fn encrypted_qmp_delivers_secret_in_order_then_reaps_and_cleans() {
        let fake = fake_private_qemu();
        let plan = private_plan(&fake, "", "");
        let key_text = BASE64_STANDARD.encode([0x5a; 32]);
        assert!(!plan.command_line().contains(&key_text));
        let artifact = b"verified ciphertext extent";
        let mut process = QemuProcess::spawn_encrypted_with_parent(
            &plan,
            std::io::Cursor::new(artifact),
            artifact.len() as u64,
            test_image_master_key(),
            &fake.root,
        )
        .expect("private QMP setup succeeds");

        let session_dir = process
            .encrypted_temp_dir
            .as_ref()
            .expect("encrypted launch retains its private temporary directory")
            .path
            .clone();
        let image = session_dir.join("encrypted-rootfs.img");
        assert_eq!(fs::read(&image).unwrap(), artifact);
        assert_eq!(
            fs::metadata(&session_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(!session_dir.join("q").exists());
        assert_eq!(
            fs::metadata(&image).unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(
            fs::read_to_string(&fake.trace)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            vec![
                "qmp_capabilities".to_string(),
                "object-add:secret-present".to_string(),
                "blockdev-add".to_string(),
                "blockdev-add".to_string(),
                "blockdev-add".to_string(),
                "device_add".to_string(),
                "cont".to_string(),
            ]
        );
        assert!(!fs::read(process.diagnostic_log())
            .unwrap()
            .windows(key_text.len())
            .any(|window| window == key_text.as_bytes()));

        let status = process
            .stop(Duration::from_secs(1), Duration::from_secs(1))
            .expect("fake guest stops cleanly");
        assert!(status.success());
        assert!(!session_dir.exists());
        assert_private_qemu_reaped(&fake);
    }

    #[test]
    fn encrypted_qmp_failures_never_echo_secret_and_clean_up_at_each_setup_stage() {
        for stage in [
            "qmp_capabilities",
            "object-add",
            "blockdev-add-1",
            "blockdev-add-2",
            "blockdev-add-3",
            "device_add",
            "cont",
        ] {
            let fake = fake_private_qemu();
            let plan = private_plan(&fake, stage, "");
            let key_text = BASE64_STANDARD.encode([0x5a; 32]);
            let artifact = b"verified ciphertext extent";
            let error = QemuProcess::spawn_encrypted_with_parent(
                &plan,
                std::io::Cursor::new(artifact),
                artifact.len() as u64,
                test_image_master_key(),
                &fake.root,
            )
            .err()
            .expect("injected QMP error stops encrypted setup");

            assert!(
                !error.to_string().contains(&key_text),
                "QMP error at {stage} must redact the secret"
            );
            assert_private_qemu_reaped(&fake);
        }
    }

    #[test]
    fn encrypted_qmp_timeout_kills_qemu_and_removes_temporary_extent() {
        let fake = fake_private_qemu();
        let plan = private_plan(&fake, "", "object-add");
        let artifact = b"verified ciphertext extent";
        let error = QemuProcess::spawn_encrypted_with_parent(
            &plan,
            std::io::Cursor::new(artifact),
            artifact.len() as u64,
            test_image_master_key(),
            &fake.root,
        )
        .err()
        .expect("QMP response timeout aborts encrypted startup");

        assert!(error.to_string().contains("adding QEMU secret"));
        assert_private_qemu_reaped(&fake);
    }

    #[test]
    fn short_encrypted_extent_is_removed_before_qemu_starts() {
        let fake = fake_private_qemu();
        let plan = private_plan(&fake, "", "");
        let error = QemuProcess::spawn_encrypted_with_parent(
            &plan,
            std::io::Cursor::new(b"short"),
            12,
            test_image_master_key(),
            &fake.root,
        )
        .err()
        .expect("short verified extent is rejected");

        assert!(error.to_string().contains("encrypted QEMU setup failed"));
        assert!(!fake.pid_file.exists());
        assert!(fs::read_dir(&fake.root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("rootglass-qmp-")));
    }

    fn fake_qemu_exit(code: i32) -> FakeQemu {
        static NEXT_FAKE_QEMU_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let root = loop {
            let id = NEXT_FAKE_QEMU_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = std::env::temp_dir().join(format!(
                "rootglass-fake-qemu-exit-{}-{id}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fake QEMU fixture directory: {error}"),
            }
        };
        let binary = root.join("fake-qemu.sh");
        fs::write(&binary, format!("#!/bin/sh\nexit {code}\n")).unwrap();
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
    fn supervised_qemu_uses_a_separate_process_group() {
        let fake = fake_qemu("hold");
        let process = plan(&fake.binary).spawn_supervised().unwrap();
        let stat = fs::read_to_string(format!("/proc/{}/stat", process.child.id()))
            .expect("Linux reports the supervised child process group");
        let fields = stat
            .rsplit_once(')')
            .expect("process stat has a command name")
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        assert_eq!(
            fields[2].parse::<u32>().unwrap(),
            process.child.id(),
            "the child process group must be separate so terminal SIGINT reaches the CLI"
        );
        drop(process);
    }

    #[test]
    fn headless_qemu_exit_status_is_preserved() {
        let mut runtime = RootglassRuntime::new();
        let capabilities = HostCapabilities::without_acceleration(HostPlatform::Other);
        let request = runtime
            .prepare_launch_with_host("../../examples/hello-world.glass", &capabilities)
            .expect("sample glass produces launch request");

        for expected in [0, 3] {
            let fake = fake_qemu_exit(expected);
            let plan =
                QemuLaunchPlan::for_headless_launch_request_with_binary(&request, &fake.binary);
            let status = plan
                .spawn_interactive()
                .expect("fake QEMU starts")
                .wait()
                .expect("QEMU status is available");

            assert_eq!(status.code(), Some(expected));
        }
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
