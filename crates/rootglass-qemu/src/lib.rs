use rootglass_core::{
    HostAccelerator, HostCapabilities, HostPlatform, LaunchRequest, RootglassRuntime,
};
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
        let accelerator = Accelerator::from(request.accelerator);
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
        assert!(command_line.contains("-kernel builtin:aarch64-linux"));
        assert!(command_line.contains("readonly=on"));
        assert!(command_line.contains("rootfs.img"));
        assert!(command_line.contains("state.img"));
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
}
