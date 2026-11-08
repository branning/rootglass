use rootglass_app::{AppService, GlassSummary};
use rootglass_manifest::Glass;
use rootglass_signature::{
    load_signing_key, package_glass_directory, package_glass_directory_at, sign_glass_directory,
    sign_glass_directory_at, SigningReport,
};
use std::path::PathBuf;

pub fn run_with_frontends(
    args: Vec<String>,
    launch_gui: impl FnOnce() -> Result<(), String>,
    launch_tui: impl FnOnce(Option<PathBuf>) -> Result<(), String>,
) -> Result<(), String> {
    let Some(command) = args.first().map(String::as_str) else {
        return launch_gui();
    };

    match command {
        "gui" => {
            if args.len() != 1 {
                return Err("usage: rootglass gui".to_string());
            }
            launch_gui()
        }
        "--rootglass-tui-session" => {
            if args.len() != 1 {
                return Err("invalid internal Rootglass TUI session arguments".to_string());
            }
            launch_tui(None)
        }
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
        "tui" => {
            if args.len() > 2 {
                return Err("usage: rootglass tui [path-to.glass]".to_string());
            }
            launch_tui(args.get(1).map(PathBuf::from))
        }
        "sign" => sign_command(&args),
        "package" => package_command(&args),
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

fn sign_command(args: &[String]) -> Result<(), String> {
    let directory = args.get(1).ok_or_else(|| {
        "usage: rootglass sign <glass-directory> --key <secret-file> [--created-at <RFC3339>]"
            .to_string()
    })?;
    let (key_path, created_at) = signing_options(args, 2)?;
    let key = load_signing_key(key_path).map_err(|error| error.to_string())?;
    let report = match created_at {
        Some(timestamp) => sign_glass_directory_at(directory, &key, timestamp),
        None => sign_glass_directory(directory, &key),
    }
    .map_err(|error| error.to_string())?;
    print_signing_report(&report);
    Ok(())
}

fn package_command(args: &[String]) -> Result<(), String> {
    let directory = args
        .get(1)
        .ok_or_else(|| "usage: rootglass package <glass-directory> <output.glass> --key <secret-file> [--created-at <RFC3339>]".to_string())?;
    let output = args.get(2).ok_or_else(|| {
        "usage: rootglass package <glass-directory> <output.glass> --key <secret-file> [--created-at <RFC3339>]"
            .to_string()
    })?;
    let (key_path, created_at) = signing_options(args, 3)?;
    let key = load_signing_key(key_path).map_err(|error| error.to_string())?;
    let report = match created_at {
        Some(timestamp) => package_glass_directory_at(directory, output, &key, timestamp),
        None => package_glass_directory(directory, output, &key),
    }
    .map_err(|error| error.to_string())?;
    print_signing_report(&report);
    println!("created package: {}", output);
    Ok(())
}

fn signing_options(args: &[String], start: usize) -> Result<(&str, Option<&str>), String> {
    let mut key = None;
    let mut created_at = None;
    let mut index = start;
    while index < args.len() {
        match args[index].as_str() {
            "--key" => {
                index += 1;
                key = args.get(index).map(String::as_str);
                if key.is_none() {
                    return Err("--key requires a secret-file path".to_string());
                }
            }
            "--created-at" => {
                index += 1;
                created_at = args.get(index).map(String::as_str);
                if created_at.is_none() {
                    return Err("--created-at requires an RFC3339 timestamp".to_string());
                }
            }
            other => return Err(format!("unexpected signing option '{other}'")),
        }
        index += 1;
    }
    let key = key.ok_or_else(|| "--key <secret-file> is required".to_string())?;
    Ok((key, created_at))
}

fn print_signing_report(report: &SigningReport) {
    println!(
        "signed publisher: {} ({})",
        report.publisher_name, report.publisher_identity
    );
    println!("signature key: {}", report.key_id);
    println!("signed subjects: {}", report.subject_count);
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

fn resolve_glass_root(service: &AppService, glass_path: PathBuf) -> Result<PathBuf, String> {
    if glass_path.is_file() {
        service
            .import_archive(&glass_path)
            .map(|installed| installed.package_path)
            .map_err(|error| error.to_string())
    } else {
        Ok(glass_path)
    }
}

fn print_launch_plan(glass_path: PathBuf) -> Result<(), String> {
    let mut service = AppService::new();
    let glass_root = resolve_glass_root(&service, glass_path)?;
    let (summary, command) = service
        .launch_plan(&glass_root)
        .map_err(|error| error.to_string())?;
    print_verification(&summary);
    println!("{command}");
    Ok(())
}

fn boot_glass(glass_path: PathBuf) -> Result<(), String> {
    let mut service = AppService::new();
    let glass_root = resolve_glass_root(&service, glass_path)?;
    let report = service
        .preflight(&glass_root)
        .map_err(|error| error.to_string())?;
    print_verification(&report.summary);
    eprintln!(
        "launching: {}",
        service
            .launch_plan(&glass_root)
            .map_err(|error| error.to_string())?
            .1
    );
    service
        .boot_interactive(&glass_root)
        .map_err(|error| error.to_string())
}

fn print_verification(summary: &GlassSummary) {
    println!(
        "verified publisher: {} ({})",
        summary.publisher_name, summary.publisher_identity
    );
    println!("signature key: {}", summary.key_id);
    println!("signed subjects: {}", summary.signed_subject_count);
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  rootglass [gui]");
    eprintln!("  rootglass tui [path-to.glass]");
    eprintln!("  rootglass validate <path-to.glass>");
    eprintln!("  rootglass plan <path-to.glass>");
    eprintln!("  rootglass boot <path-to.glass>");
    eprintln!("  rootglass sign <glass-directory> --key <secret-file> [--created-at <RFC3339>]");
    eprintln!("  rootglass package <glass-directory> <output.glass> --key <secret-file> [--created-at <RFC3339>]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_dispatch_passes_optional_bundle_path_to_the_session() {
        let mut received = None;
        run_with_frontends(
            vec!["tui".to_string(), "example.glass".to_string()],
            || panic!("the GUI should not launch for the TUI command"),
            |path| {
                received = path;
                Ok(())
            },
        )
        .expect("TUI command dispatch succeeds");
        assert_eq!(received, Some(PathBuf::from("example.glass")));
    }

    #[test]
    fn tui_dispatch_accepts_no_bundle_path() {
        let mut launched = false;
        run_with_frontends(
            vec!["tui".to_string()],
            || panic!("the GUI should not launch for the TUI command"),
            |path| {
                assert_eq!(path, None);
                launched = true;
                Ok(())
            },
        )
        .expect("TUI command dispatch succeeds");
        assert!(launched);
    }

    #[test]
    fn tui_dispatch_rejects_multiple_bundle_paths_before_launch() {
        let mut launched = false;
        let error = run_with_frontends(
            vec![
                "tui".to_string(),
                "one.glass".to_string(),
                "two.glass".to_string(),
            ],
            || panic!("the GUI should not launch for the TUI command"),
            |_| {
                launched = true;
                Ok(())
            },
        )
        .expect_err("the TUI accepts at most one bundle path");
        assert!(error.contains("usage: rootglass tui [path-to.glass]"));
        assert!(!launched);
    }

    #[test]
    fn validate_accepts_sample_manifest() {
        run_cli(vec![
            "validate".to_string(),
            "../../examples/hello-world.glass".to_string(),
        ])
        .expect("sample manifest validates");
    }

    #[test]
    fn plan_prints_for_sample_manifest() {
        run_cli(vec![
            "plan".to_string(),
            "../../examples/hello-world.glass".to_string(),
        ])
        .expect("sample manifest produces a launch plan");
    }

    #[test]
    fn boot_fails_before_qemu_when_assets_are_missing() {
        let error = run_cli(vec![
            "boot".to_string(),
            "../../examples/hello-world.glass".to_string(),
        ])
        .expect_err("boot should fail before QEMU without generated assets");

        assert!(error.contains("missing kernel asset"));
    }

    fn run_cli(args: Vec<String>) -> Result<(), String> {
        run_with_frontends(
            args,
            || Err("GUI dispatch is unavailable in a CLI unit test".to_string()),
            rootglass_tui::run_terminal,
        )
    }
}
