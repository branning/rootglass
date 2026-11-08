use std::process::ExitCode;

fn main() -> ExitCode {
    match rootglass_cli::run_with_frontends(
        std::env::args().skip(1).collect(),
        launch_gui,
        rootglass_tui::run_terminal,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "desktop-ui")]
fn launch_gui() -> Result<(), String> {
    rootglass_desktop::run_gui();
    Ok(())
}

#[cfg(not(feature = "desktop-ui"))]
fn launch_gui() -> Result<(), String> {
    Err(
        "Rootglass was built without GUI support; rebuild with default features to use `rootglass gui`."
            .to_string(),
    )
}
