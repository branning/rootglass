pub mod ui_model;

#[cfg(feature = "desktop-ui")]
mod gui;

#[cfg(feature = "desktop-ui")]
pub fn run_gui(initial_glass: Option<std::path::PathBuf>) {
    gui::run_gui(initial_glass);
}
