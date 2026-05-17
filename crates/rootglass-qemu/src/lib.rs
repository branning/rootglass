use rootglass_core::LaunchRequest;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accelerator {
    None,
    Hvf,
    Whpx,
    Kvm,
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
    pub fn for_launch_request(request: &LaunchRequest, accelerator: Accelerator) -> Self {
        let binary = QemuBinary::default();
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
            request.kernel.clone(),
            "-dtb".to_string(),
            request.device_tree.clone(),
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
        ];

        if let Some(accel) = accelerator.qemu_arg() {
            args.push("-accel".to_string());
            args.push(accel.to_string());
        }

        if let Some(overlay) = &request.writable_overlay {
            args.push("-drive".to_string());
            args.push(format!(
                "if=none,file={},format=raw,id=state",
                overlay.display()
            ));
            args.push("-device".to_string());
            args.push("virtio-blk-device,drive=state".to_string());
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
        let mut parts = vec![self.binary.path.display().to_string()];
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

    pub fn spawn(&self) -> Result<Child, QemuError> {
        let command_line = self.command_line();
        self.command().spawn().map_err(|source| QemuError::Spawn {
            command: command_line,
            source,
        })
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
    use rootglass_core::RootglassRuntime;

    #[test]
    fn builds_qemu_command_for_sample_launch_request() {
        let mut runtime = RootglassRuntime::new();
        let request = runtime
            .prepare_launch("../../examples/hello-world.glass")
            .expect("sample glass produces launch request");

        let plan = QemuLaunchPlan::for_launch_request(&request, Accelerator::None);
        let command_line = plan.command_line();

        assert!(command_line.contains(DEFAULT_QEMU_SYSTEM_AARCH64));
        assert!(command_line.contains("-machine virt"));
        assert!(command_line.contains("-cpu cortex-a57"));
        assert!(command_line.contains("-kernel builtin:aarch64-linux"));
        assert!(command_line.contains("readonly=on"));
        assert!(command_line.contains("rootfs.img"));
        assert!(command_line.contains("state.img"));
        assert!(command_line.contains("console=ttyAMA0"));
    }

    #[test]
    fn includes_acceleration_when_requested() {
        let mut runtime = RootglassRuntime::new();
        let request = runtime
            .prepare_launch("../../examples/hello-world.glass")
            .expect("sample glass produces launch request");

        let plan = QemuLaunchPlan::for_launch_request(&request, Accelerator::Kvm);
        assert!(plan.command_line().contains("-accel kvm"));
    }
}
