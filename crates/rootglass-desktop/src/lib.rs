pub mod ui_model;

#[cfg(feature = "desktop-ui")]
mod gui;

#[cfg(feature = "desktop-ui")]
pub fn run_gui() {
    gui::run_gui();
}
