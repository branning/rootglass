use rootglass_core::{HostCapabilities, RootglassRuntime};
use rootglass_manifest::Glass;
use rootglass_qemu::QemuLaunchPlan;
use std::env;
use std::path::PathBuf;
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
    println!("runtime: {} {}", glass.manifest.runtime.architecture, glass.manifest.runtime.profile);
    println!("rootfs: {}", glass.manifest.boot.rootfs);

    if let Some(overlay) = &glass.manifest.boot.writable_overlay {
        println!("writable overlay: {overlay}");
    } else {
        println!("writable overlay: none");
    }

    Ok(())
}

fn print_launch_plan(glass_root: PathBuf) -> Result<(), String> {
    let mut runtime = RootglassRuntime::new();
    let capabilities = HostCapabilities::detect_current();
    let request = runtime
        .prepare_launch_with_host(&glass_root, &capabilities)
        .map_err(|error| {
            runtime
                .status()
                .last_error
                .as_ref()
                .map(|user_error| format!("{}\n{}", user_error.summary, user_error.detail))
                .unwrap_or_else(|| error.to_string())
        })?;

    let plan = QemuLaunchPlan::for_launch_request(&request);
    println!("{}", plan.command_line());
    Ok(())
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  rootglass validate <path-to.glass>");
    eprintln!("  rootglass plan <path-to.glass>");
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
}
