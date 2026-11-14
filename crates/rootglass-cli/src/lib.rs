use rootglass_app::{AppService, GlassSummary, RuntimeEvent};
use rootglass_manifest::Glass;
use rootglass_signature::{
    load_signing_key, package_glass_directory, package_glass_directory_at, sign_glass_directory,
    sign_glass_directory_at, SigningReport,
};
use signal_hook::low_level;
use signal_hook::{consts, flag, SigId};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeadlessRunOptions {
    glass_path: PathBuf,
    trust_path: Option<PathBuf>,
    shutdown_timeout: Duration,
}

const DEFAULT_HEADLESS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
const HEADLESS_QMP_QUIT_TIMEOUT: Duration = Duration::from_secs(2);
const HEADLESS_RUN_USAGE: &str =
    "usage: rootglass run --headless <path-to.glass> [--trust <file>] [--shutdown-timeout <seconds>]";

pub fn run_with_frontends(
    args: Vec<String>,
    launch_gui: impl FnOnce(Option<PathBuf>) -> Result<(), String>,
    launch_tui: impl FnOnce(Option<PathBuf>) -> Result<(), String>,
) -> Result<i32, String> {
    run_with_frontends_and_headless(args, launch_gui, launch_tui, run_headless_glass)
}

fn run_with_frontends_and_headless(
    args: Vec<String>,
    launch_gui: impl FnOnce(Option<PathBuf>) -> Result<(), String>,
    launch_tui: impl FnOnce(Option<PathBuf>) -> Result<(), String>,
    run_headless: impl FnOnce(HeadlessRunOptions) -> Result<i32, String>,
) -> Result<i32, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return launch_gui(None).map(|()| 0);
    };

    match command {
        "gui" => {
            if args.len() > 2 {
                return Err("usage: rootglass gui [path-to.glass]".to_string());
            }
            launch_gui(args.get(1).map(PathBuf::from)).map(|()| 0)
        }
        "--rootglass-tui-session" => {
            if args.len() != 1 {
                return Err("invalid internal Rootglass TUI session arguments".to_string());
            }
            launch_tui(None).map(|()| 0)
        }
        "validate" => {
            let glass_root = required_path_arg(&args, "validate")?;
            validate_glass(glass_root).map(|()| 0)
        }
        "plan" => {
            let glass_root = required_path_arg(&args, "plan")?;
            print_launch_plan(glass_root).map(|()| 0)
        }
        "boot" => {
            let glass_root = required_path_arg(&args, "boot")?;
            boot_glass(glass_root).map(|()| 0)
        }
        "run" => {
            let options = parse_headless_run_args(&args)?;
            run_headless(options)
        }
        "tui" => {
            if args.len() > 2 {
                return Err("usage: rootglass tui [path-to.glass]".to_string());
            }
            launch_tui(args.get(1).map(PathBuf::from)).map(|()| 0)
        }
        "sign" => sign_command(&args).map(|()| 0),
        "package" => package_command(&args).map(|()| 0),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(0)
        }
        other if args.len() == 1 && is_glass_path(other) => {
            launch_gui(Some(PathBuf::from(other))).map(|()| 0)
        }
        other => {
            print_usage();
            Err(format!("unknown command '{other}'"))
        }
    }
}

fn parse_headless_run_args(args: &[String]) -> Result<HeadlessRunOptions, String> {
    if args.first().map(String::as_str) != Some("run")
        || args.get(1).map(String::as_str) != Some("--headless")
    {
        return Err(HEADLESS_RUN_USAGE.to_string());
    }

    let mut glass_path = None;
    let mut trust_path = None;
    let mut shutdown_timeout = None;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--trust" => {
                if trust_path.is_some() {
                    return Err("--trust may be specified only once".to_string());
                }
                let Some(path) = args.get(index + 1) else {
                    return Err("--trust requires a configuration file path".to_string());
                };
                if path.is_empty() || path.starts_with('-') {
                    return Err("--trust requires a configuration file path".to_string());
                }
                trust_path = Some(PathBuf::from(path));
                index += 2;
            }
            "--shutdown-timeout" => {
                if shutdown_timeout.is_some() {
                    return Err("--shutdown-timeout may be specified only once".to_string());
                }
                let Some(seconds) = args.get(index + 1) else {
                    return Err("--shutdown-timeout requires a number of seconds".to_string());
                };
                if seconds.starts_with('-') {
                    return Err("--shutdown-timeout must be zero or greater".to_string());
                }
                let seconds = seconds
                    .parse::<u64>()
                    .map_err(|_| "--shutdown-timeout requires a number of seconds".to_string())?;
                let timeout = Duration::from_secs(seconds);
                if std::time::Instant::now().checked_add(timeout).is_none() {
                    return Err("--shutdown-timeout is too large".to_string());
                }
                shutdown_timeout = Some(timeout);
                index += 2;
            }
            option if option.starts_with('-') => {
                return Err(format!("unexpected run option '{option}'"));
            }
            path => {
                if glass_path.is_some() {
                    return Err(HEADLESS_RUN_USAGE.to_string());
                }
                glass_path = Some(PathBuf::from(path));
                index += 1;
            }
        }
    }

    let glass_path = glass_path.ok_or_else(|| HEADLESS_RUN_USAGE.to_string())?;
    Ok(HeadlessRunOptions {
        glass_path,
        trust_path,
        shutdown_timeout: shutdown_timeout.unwrap_or(DEFAULT_HEADLESS_SHUTDOWN_TIMEOUT),
    })
}

fn is_glass_path(value: &str) -> bool {
    PathBuf::from(value)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("glass"))
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
    let mut service = AppService::new().with_packaged_runtime_assets();
    let glass_root = resolve_glass_root(&service, glass_path)?;
    let (summary, command) = service
        .launch_plan(&glass_root)
        .map_err(|error| error.to_string())?;
    print_verification(&summary);
    println!("{command}");
    Ok(())
}

fn boot_glass(glass_path: PathBuf) -> Result<(), String> {
    let mut service = AppService::new().with_packaged_runtime_assets();
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

fn run_headless_glass(options: HeadlessRunOptions) -> Result<i32, String> {
    let mut service = headless_service(options.trust_path)?;
    let glass_root = resolve_glass_root(&service, options.glass_path)?;
    let signals = ShutdownSignals::install()?;
    let run = service
        .start_headless(&glass_root)
        .map_err(|error| error.to_string())?;
    run_headless_session(&mut service, &run, options.shutdown_timeout, &signals)
}

struct ShutdownSignals {
    pending: Arc<AtomicBool>,
    registrations: Vec<SigId>,
}

impl ShutdownSignals {
    fn install() -> Result<Self, String> {
        let pending = Arc::new(AtomicBool::new(false));
        let mut registrations = Vec::new();
        for signal in [consts::SIGINT, consts::SIGTERM] {
            match flag::register(signal, Arc::clone(&pending)) {
                Ok(registration) => registrations.push(registration),
                Err(error) => {
                    for registration in registrations {
                        low_level::unregister(registration);
                    }
                    return Err(format!(
                        "failed to install shutdown signal handler: {error}"
                    ));
                }
            }
        }
        Ok(Self {
            pending,
            registrations,
        })
    }

    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            low_level::unregister(registration);
        }
    }
}

fn run_headless_session(
    service: &mut AppService,
    run: &rootglass_app::RunId,
    shutdown_timeout: Duration,
    signals: &ShutdownSignals,
) -> Result<i32, String> {
    let input = spawn_console_input_reader();
    let mut input_open = true;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    loop {
        if signals.is_pending() {
            let status = service
                .stop_with_timeouts(run, shutdown_timeout, HEADLESS_QMP_QUIT_TIMEOUT)
                .map_err(|error| error.to_string())?;
            return status.code().ok_or_else(|| {
                format!("QEMU was terminated without an exit code during shutdown: {status}")
            });
        }

        if input_open {
            loop {
                match input.try_recv() {
                    Ok(Ok(bytes)) => {
                        if let Err(error) = service.write_serial(run, &bytes) {
                            let _ = service.stop_with_timeouts(
                                run,
                                Duration::from_secs(1),
                                Duration::from_secs(1),
                            );
                            return Err(error.to_string());
                        }
                    }
                    Ok(Err(error)) => {
                        let _ = service.stop_with_timeouts(
                            run,
                            Duration::from_secs(1),
                            Duration::from_secs(1),
                        );
                        return Err(format!("guest console input failed: {error}"));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        input_open = false;
                        break;
                    }
                }
            }
        }

        match service
            .poll_event(Duration::from_millis(50))
            .map_err(|error| error.to_string())?
        {
            Some(RuntimeEvent::SerialChunk { bytes, .. }) => {
                if let Err(error) = output.write_all(&bytes).and_then(|()| output.flush()) {
                    let _ = service.stop_with_timeouts(
                        run,
                        Duration::from_secs(1),
                        Duration::from_secs(1),
                    );
                    return Err(format!("guest console output failed: {error}"));
                }
            }
            Some(RuntimeEvent::Exited { exit_code, .. }) => {
                return exit_code.ok_or_else(|| "QEMU terminated without an exit code".to_string())
            }
            Some(RuntimeEvent::Failed {
                summary, detail, ..
            }) => {
                return Err(format!("{summary}\n{detail}"));
            }
            Some(RuntimeEvent::StateChanged { .. } | RuntimeEvent::GuestReady { .. }) | None => {}
        }
    }
}

fn spawn_console_input_reader() -> mpsc::Receiver<Result<Vec<u8>, String>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        loop {
            let mut bytes = vec![0; 4096];
            match input.read(&mut bytes) {
                Ok(0) => break,
                Ok(read) => {
                    bytes.truncate(read);
                    if sender.send(Ok(bytes)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    receiver
}

fn headless_service(trust_path: Option<PathBuf>) -> Result<AppService, String> {
    let service = match trust_path {
        Some(path) => AppService::from_trust_config(&path)
            .map_err(|error| format!("failed to load trust configuration: {error}"))?,
        None => AppService::new(),
    };
    Ok(service.with_packaged_runtime_assets())
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
    eprintln!("  rootglass [gui [path-to.glass]]");
    eprintln!("  rootglass <path-to.glass>");
    eprintln!("  rootglass tui [path-to.glass]");
    eprintln!("  rootglass validate <path-to.glass>");
    eprintln!("  rootglass plan <path-to.glass>");
    eprintln!("  rootglass boot <path-to.glass>");
    eprintln!("  {HEADLESS_RUN_USAGE}");
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
            |_| panic!("the GUI should not launch for the TUI command"),
            |path| {
                received = path;
                Ok(())
            },
        )
        .expect("TUI command dispatch succeeds");
        assert_eq!(received, Some(PathBuf::from("example.glass")));
    }

    #[test]
    fn headless_run_dispatch_returns_qemu_exit_code() {
        let mut received = None;
        let exit_code = run_with_frontends_and_headless(
            vec![
                "run".to_string(),
                "--headless".to_string(),
                "--trust".to_string(),
                "trust.json".to_string(),
                "--shutdown-timeout".to_string(),
                "17".to_string(),
                "example.glass".to_string(),
            ],
            |_| panic!("the GUI should not launch for a headless run"),
            |_| panic!("the TUI should not launch for a headless run"),
            |options| {
                received = Some(options);
                Ok(3)
            },
        )
        .expect("headless run returns the QEMU status");

        assert_eq!(
            received,
            Some(HeadlessRunOptions {
                glass_path: PathBuf::from("example.glass"),
                trust_path: Some(PathBuf::from("trust.json")),
                shutdown_timeout: Duration::from_secs(17),
            })
        );
        assert_eq!(exit_code, 3);
    }

    #[test]
    fn headless_run_rejects_malformed_arguments_before_launch() {
        let mut launched = false;
        let error = run_with_frontends_and_headless(
            vec![
                "run".to_string(),
                "--headless".to_string(),
                "example.glass".to_string(),
                "--trust".to_string(),
            ],
            |_| panic!("the GUI should not launch for a headless run"),
            |_| panic!("the TUI should not launch for a headless run"),
            |_| {
                launched = true;
                Ok(0)
            },
        )
        .expect_err("headless run must require a trust configuration path");

        assert!(error.contains("--trust requires a configuration file path"));
        assert!(!launched);
    }

    #[test]
    fn headless_run_uses_sixty_second_shutdown_timeout_by_default() {
        let options = parse_headless_run_args(&[
            "run".to_string(),
            "--headless".to_string(),
            "example.glass".to_string(),
        ])
        .expect("headless arguments parse");
        assert_eq!(options.shutdown_timeout, DEFAULT_HEADLESS_SHUTDOWN_TIMEOUT);
    }

    #[test]
    fn headless_run_rejects_invalid_shutdown_timeout() {
        for timeout in ["-1", "many", "18446744073709551615"] {
            let error = parse_headless_run_args(&[
                "run".to_string(),
                "--headless".to_string(),
                "example.glass".to_string(),
                "--shutdown-timeout".to_string(),
                timeout.to_string(),
            ])
            .expect_err("invalid shutdown timeout must be rejected");
            assert!(error.contains("--shutdown-timeout"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn sigint_and_sigterm_are_registered_as_graceful_shutdown_requests() {
        for signal in [consts::SIGINT, consts::SIGTERM] {
            let signals = ShutdownSignals::install().expect("signal handlers install");
            assert!(!signals.is_pending());
            low_level::raise(signal).expect("test signal is raised");
            assert!(
                signals.is_pending(),
                "signal {signal} must request shutdown"
            );
        }
    }

    #[test]
    fn headless_run_uses_configured_publisher_trust() {
        let root = temp_root();
        let glass_path = root.join("configured-publisher.glass");
        std::fs::create_dir(&glass_path).expect("bundle directory is created");
        let manifest = include_str!("../../../examples/hello-world.glass/rootglass.toml")
            .replace("Rootglass Examples", "CLI Test Publisher")
            .replace("rootglass.dev/examples", "rootglass.test/cli");
        std::fs::write(glass_path.join("rootglass.toml"), manifest)
            .expect("test manifest is written");

        let signing_key_path = root.join("test-signing-seed");
        std::fs::write(&signing_key_path, [42_u8; 32]).expect("test seed is written");
        let signing_key =
            rootglass_signature::load_signing_key(&signing_key_path).expect("test seed loads");
        let report = rootglass_signature::sign_glass_directory_at(
            &glass_path,
            &signing_key,
            "2026-11-14T00:00:00Z",
        )
        .expect("test bundle is signed");
        let public_key = base64url_no_pad(&signing_key.verifying_key().to_bytes());
        let trust_path = root.join("trust.json");
        let trust_config = format!(
            r#"{{"schemaVersion":1,"trustedPublishers":[{{"name":"{}","identity":"{}","keyId":"{}","publicKeyBase64url":"{}"}}],"revokedKeyIds":[]}}"#,
            report.publisher_name, report.publisher_identity, report.key_id, public_key
        );
        std::fs::write(&trust_path, trust_config).expect("trust configuration is written");

        let error = run_with_frontends_and_headless(
            vec![
                "run".to_string(),
                "--headless".to_string(),
                glass_path.display().to_string(),
                "--trust".to_string(),
                trust_path.display().to_string(),
            ],
            |_| panic!("the GUI should not launch for a headless run"),
            |_| panic!("the TUI should not launch for a headless run"),
            run_headless_glass,
        )
        .expect_err("the trusted bundle should stop at the intentionally missing runtime asset");

        assert!(error.contains("missing kernel asset") || error.contains("missing rootfs asset"));
        assert!(!error.to_lowercase().contains("unknown signing key"));
        std::fs::remove_dir_all(root).expect("temporary test files are removed");
    }

    #[test]
    fn malformed_headless_trust_config_fails_before_opening_the_bundle() {
        let root = temp_root();
        let trust_path = root.join("malformed-trust.json");
        std::fs::write(&trust_path, b"not json").expect("malformed trust config is written");

        let error = run_with_frontends_and_headless(
            vec![
                "run".to_string(),
                "--headless".to_string(),
                root.join("missing.glass").display().to_string(),
                "--trust".to_string(),
                trust_path.display().to_string(),
            ],
            |_| panic!("the GUI should not launch for a headless run"),
            |_| panic!("the TUI should not launch for a headless run"),
            run_headless_glass,
        )
        .expect_err("malformed trust configuration must fail closed");

        assert!(error.contains("invalid trust configuration JSON"));
        std::fs::remove_dir_all(root).expect("temporary test files are removed");
    }

    #[test]
    fn tui_dispatch_accepts_no_bundle_path() {
        let mut launched = false;
        run_with_frontends(
            vec!["tui".to_string()],
            |_| panic!("the GUI should not launch for the TUI command"),
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
            |_| panic!("the GUI should not launch for the TUI command"),
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

    fn run_cli(args: Vec<String>) -> Result<i32, String> {
        run_with_frontends(
            args,
            |_| Err("GUI dispatch is unavailable in a CLI unit test".to_string()),
            rootglass_tui::run_terminal,
        )
    }

    fn temp_root() -> PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("rootglass-cli-trust-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temporary test directory is created");
        root
    }

    fn base64url_no_pad(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut encoded = String::new();
        let mut index = 0;
        while index < bytes.len() {
            let first = bytes[index] as u32;
            let second = bytes.get(index + 1).copied().unwrap_or(0) as u32;
            let third = bytes.get(index + 2).copied().unwrap_or(0) as u32;
            encoded.push(ALPHABET[(first >> 2) as usize] as char);
            encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            if index + 1 < bytes.len() {
                encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
            }
            if index + 2 < bytes.len() {
                encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
            }
            index += 3;
        }
        encoded
    }

    #[test]
    fn file_association_dispatches_glass_path_to_gui() {
        let mut received = None;
        run_with_frontends(
            vec!["/tmp/hello world.GLASS".to_string()],
            |path| {
                received = path;
                Ok(())
            },
            |_| panic!("the TUI should not launch for a .glass association"),
        )
        .expect("a .glass path launches the GUI");
        assert_eq!(received, Some(PathBuf::from("/tmp/hello world.GLASS")));
    }

    #[test]
    fn gui_command_accepts_a_path_from_the_file_association() {
        let mut received = None;
        run_with_frontends(
            vec!["gui".to_string(), "hello.glass".to_string()],
            |path| {
                received = path;
                Ok(())
            },
            |_| panic!("the TUI should not launch for the GUI command"),
        )
        .expect("gui with a bundle path launches successfully");
        assert_eq!(received, Some(PathBuf::from("hello.glass")));
    }
}
