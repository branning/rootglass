fn main() {
    let exit_code = match rootglass_cli::run_with_frontends(
        std::env::args().skip(1).collect(),
        launch_gui,
        launch_tui,
    ) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    };
    std::process::exit(exit_code);
}

#[cfg(feature = "tui")]
fn launch_tui(initial_glass: Option<std::path::PathBuf>) -> Result<(), String> {
    rootglass_tui::run_terminal(initial_glass)
}

#[cfg(not(feature = "tui"))]
fn launch_tui(_initial_glass: Option<std::path::PathBuf>) -> Result<(), String> {
    Err(
        "Rootglass was built without TUI support; rebuild with `--features tui` to use `rootglass tui`."
            .to_string(),
    )
}

#[cfg(feature = "desktop-ui")]
fn launch_gui(initial_glass: Option<std::path::PathBuf>) -> Result<(), String> {
    rootglass_desktop::run_gui(initial_glass);
    Ok(())
}

#[cfg(not(feature = "desktop-ui"))]
fn launch_gui(_initial_glass: Option<std::path::PathBuf>) -> Result<(), String> {
    Err(
        "Rootglass was built without GUI support; rebuild with default features to use `rootglass gui`."
            .to_string(),
    )
}
