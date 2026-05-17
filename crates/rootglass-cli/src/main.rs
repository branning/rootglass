use rootglass_core::{HostCapabilities, LaunchRequest, RootglassRuntime};
use rootglass_manifest::Glass;
use rootglass_qemu::QemuLaunchPlan;
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let Some(command) = args.first().map(String::as_str) else {
        print_usage();
        return Err("missing command".to_string());
    };

    match command {
        "validate" => {
            let glass_root = required_path_arg(&args, "validate")?;
            validate_glass(glass_root)
        }
        "plan" => {
            let glass_root = required_path_arg(&args, "plan")?;
            print_launch_plan(glass_root)
        }
        "boot" => {
            let glass_root = required_path_arg(&args, "boot")?;
            boot_glass(glass_root)
        }
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            Err(format!("unknown command '{other}'"))
        }
    }
}

fn required_path_arg(args: &[String], command: &str) -> Result<PathBuf, String> {
    args.get(1)
        .map(PathBuf::from)
        .ok_or_else(|| format!("usage: rootglass {command} <path-to.glass>"))
}

fn validate_glass(glass_root: PathBuf) -> Result<(), String> {
    let glass = Glass::load(&glass_root).map_err(|error| error.to_string())?;
    glass
        .validate_manifest_only()
        .map_err(|error| error.to_string())?;

    println!("valid manifest: {}", glass.manifest.name);
    println!("identifier: {}", glass.manifest.identifier);
    println!(
        "runtime: {} {}",
        glass.manifest.runtime.architecture, glass.manifest.runtime.profile
    );
    println!("rootfs: {}", glass.manifest.boot.rootfs);

    if let Some(overlay) = &glass.manifest.boot.writable_overlay {
        println!("writable overlay: {overlay}");
    } else {
        println!("writable overlay: none");
    }

    Ok(())
}

fn print_launch_plan(glass_root: PathBuf) -> Result<(), String> {
    let request = prepare_launch(&glass_root)?;
    print_verification(&request);

    let plan = QemuLaunchPlan::for_launch_request(&request);
    println!("{}", plan.command_line());
    Ok(())
}

fn boot_glass(glass_root: PathBuf) -> Result<(), String> {
    let request = prepare_launch(&glass_root)?;
    validate_boot_assets(&request)?;
    print_verification(&request);

    let plan = QemuLaunchPlan::for_launch_request(&request);
    eprintln!("launching: {}", plan.command_line());
    let mut child = plan
        .spawn_interactive()
        .map_err(|error| error.to_string())?;
    let status = child.wait().map_err(|error| error.to_string())?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("QEMU exited with status {status}"))
    }
}

fn prepare_launch(glass_root: &Path) -> Result<LaunchRequest, String> {
    let mut runtime = RootglassRuntime::new();
    let capabilities = HostCapabilities::detect_current();
    runtime
        .prepare_launch_with_host(glass_root, &capabilities)
        .map_err(|error| {
            runtime
                .status()
                .last_error
                .as_ref()
                .map(|user_error| format!("{}\n{}", user_error.summary, user_error.detail))
                .unwrap_or_else(|| error.to_string())
        })
}

fn validate_boot_assets(request: &LaunchRequest) -> Result<(), String> {
    require_file("kernel", &request.kernel)?;
    if let Some(initrd) = &request.initrd {
        require_file("initrd", initrd)?;
    }
    if let Some(device_tree) = &request.device_tree {
        require_file("device tree", device_tree)?;
    }
    require_file("rootfs", &request.rootfs)?;
    if let Some(writable_overlay) = &request.writable_overlay {
        require_file("writable overlay", writable_overlay)?;
    }
    Ok(())
}

fn require_file(label: &str, path: &Path) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("missing {label} asset: {}", path.display()))
    }
}

fn print_verification(request: &LaunchRequest) {
    println!(
        "verified publisher: {} ({})",
        request.signature.publisher_name, request.signature.publisher_identity
    );
    println!("signature key: {}", request.signature.key_id);
    println!("signed subjects: {}", request.signature.subject_count);
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  rootglass validate <path-to.glass>");
    eprintln!("  rootglass plan <path-to.glass>");
    eprintln!("  rootglass boot <path-to.glass>");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_sample_manifest() {
        run(vec![
            "validate".to_string(),
            "../../examples/hello-world.glass".to_string(),
        ])
        .expect("sample manifest validates");
    }

    #[test]
    fn plan_prints_for_sample_manifest() {
        run(vec![
            "plan".to_string(),
            "../../examples/hello-world.glass".to_string(),
        ])
        .expect("sample manifest produces a launch plan");
    }

    #[test]
    fn boot_fails_before_qemu_when_assets_are_missing() {
        let error = run(vec![
            "boot".to_string(),
            "../../examples/hello-world.glass".to_string(),
        ])
        .expect_err("boot should fail before QEMU without generated assets");

        assert!(error.contains("missing kernel asset"));
    }
}
