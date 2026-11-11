#[cfg(feature = "embedded-terminal")]
#[path = "embedded_terminal.rs"]
mod embedded_terminal;
#[path = "failure_view.rs"]
mod failure_view;

use crate::ui_model::{
    lock_service, GlassPreview, RemovalDetails, SignatureStatus, StartEffect, StopRequest, UiModel,
    UiState,
};
use failure_view::RecoveryFailure;
use gpui_kit::actions;
use gpui_kit::base::StyledExt;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::text::{SelectionFormat, TextView};
use gpui_kit::component::*;
use gpui_kit::*;
use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use rootglass_app::registry::InstalledGlass;
use rootglass_app::state::StateRecord;
use rootglass_app::DiagnosticLogSnapshot;
use rootglass_app::ManifestValidationReport;
use rootglass_app::RuntimeEvent;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

actions!(rootglass, [OpenBundle, StopGlass, Quit]);
const ROOTGLASS_CONTEXT: &str = "RootglassWindow";

struct RootglassWindow {
    model: UiModel,
    input: Entity<InputState>,
    running_tab: RunningTab,
    qemu_diagnostics: String,
    qemu_diagnostics_error: Option<String>,
    qemu_diagnostics_truncated: bool,
    qemu_diagnostics_loading: bool,
    run_started_at: Option<Instant>,
    failure_guest_was_ready: bool,
    failure_diagnostics_open: bool,
    recovery_retrying: bool,
    recovery_preview_context: Option<GlassPreview>,
    pending_file_drop: Option<PathBuf>,
    file_drop_active: bool,
    #[cfg(feature = "embedded-terminal")]
    embedded_tui: Option<Entity<embedded_terminal::EmbeddedTerminal>>,
    _library_task: Task<()>,
    _poll_task: Task<()>,
    _input_subscription: Subscription,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RunningTab {
    #[default]
    Console,
    Overview,
    Logs,
}

enum ArchiveOpenResult {
    PublisherReview(rootglass_app::UntrustedPublisherReview),
    Imported(Result<crate::ui_model::ImportCompletion, rootglass_app::AppError>),
}

#[derive(Clone)]
struct RecoveryStateSnapshot {
    records: Vec<StateRecord>,
    cursor: usize,
    selected_state_id: String,
    error: Option<String>,
    busy: bool,
    locked: bool,
    loaded: bool,
}

impl RecoveryStateSnapshot {
    fn capture(model: &UiModel) -> Self {
        Self {
            records: model.state_records.clone(),
            cursor: model.state_cursor,
            selected_state_id: model.selected_state_id.clone(),
            error: model.state_error.clone(),
            busy: model.state_busy,
            locked: model.state_locked,
            loaded: model.state_loaded,
        }
    }

    fn restore(self, model: &mut UiModel) {
        model.state_records = self.records;
        model.state_cursor = self.cursor;
        model.selected_state_id = self.selected_state_id;
        model.state_error = self.error;
        model.state_busy = self.busy;
        model.state_locked = self.locked;
        model.state_loaded = self.loaded;
    }
}

impl Render for RootglassWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut actions = self.model.actions();
        actions.open &= !self.recovery_retrying;
        let root = div()
            .v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .key_context(ROOTGLASS_CONTEXT)
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                    let path = event
                        .dragged_item()
                        .downcast_ref::<ExternalPaths>()
                        .and_then(|paths| paths.paths().first())
                        .cloned();
                    if let Some(path) = path {
                        if this.pending_file_drop.as_ref() != Some(&path) {
                            this.pending_file_drop = Some(path);
                            this.file_drop_active = true;
                            desktop_smoke_event("ROOTGLASS_DESKTOP_DROP_ENTER");
                            cx.notify();
                        }
                    }
                }),
            )
            .on_file_drop_exit(cx.listener(|this, _, _, cx| {
                this.pending_file_drop = None;
                this.file_drop_active = false;
                cx.notify();
            }))
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.pending_file_drop = None;
                this.file_drop_active = false;
                if let Some(path) = paths.paths().first().cloned() {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_DROP_SUBMIT");
                    this.open_path(path, cx);
                }
                cx.notify();
            }))
            .on_action(cx.listener(Self::on_open_bundle))
            .on_action(cx.listener(Self::on_stop_shortcut))
            .on_action(cx.listener(Self::on_quit));

        #[cfg(feature = "embedded-terminal")]
        if let Some(terminal) = &self.embedded_tui {
            return root.child(terminal.clone());
        }

        let is_active_run = matches!(
            &self.model.state,
            UiState::Running(_) | UiState::Booting(_) | UiState::Stopping(_)
        );
        let recovery_failure = RecoveryFailure::from_ui_state(&self.model.state);
        let is_recovery_failure = recovery_failure.is_some();
        let main = if let UiState::PublisherReview(review) = &self.model.state {
            div()
                .flex_1()
                .child(publisher_review_view(review, &self.model.trust_policy, cx))
        } else if is_active_run {
            self.running_guest_view(actions, cx)
        } else if let Some(failure) = recovery_failure {
            let glass_name = self.recovery_glass_name(&failure);
            let preview = self
                .model
                .selected_preview()
                .or(self.recovery_preview_context.as_ref());
            let guest_was_ready = self.failure_guest_was_ready;
            let retry_available = failure
                .retry_path(self.model.selected_path.as_ref())
                .is_some();
            div()
                .v_flex()
                .flex_1()
                .min_h(px(0.0))
                .child(failure_view::content(
                    &failure,
                    &glass_name,
                    preview,
                    guest_was_ready,
                    self.failure_diagnostics_open,
                    cx,
                ))
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_3()
                        .child(
                            Button::new("retry-launch-checks")
                                .primary()
                                .label(if self.recovery_retrying {
                                    "CHECKING…"
                                } else {
                                    "RETRY CHECKS"
                                })
                                .disabled(self.recovery_retrying || !retry_available)
                                .on_click(cx.listener(|this, _, _, cx| this.retry_failure(cx))),
                        )
                        .child(
                            Button::new("toggle-launch-diagnostics")
                                .secondary()
                                .label(if self.failure_diagnostics_open {
                                    "HIDE DIAGNOSTICS"
                                } else {
                                    "VIEW DIAGNOSTICS"
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.failure_diagnostics_open = !this.failure_diagnostics_open;
                                    cx.notify();
                                })),
                        ),
                )
        } else {
            let mut main = div()
                .v_flex()
                .flex_1()
                .gap_2()
                .p_4()
                .bg(cx.theme().background)
                .overflow_scrollbar()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .v_flex()
                                .flex_1()
                                .gap_2()
                                .child(eyebrow("YOUR GLASSES", cx))
                                .child(page_title("Library", cx))
                                .child(muted_text(
                                    "Contained Linux worlds, ready when you are.",
                                    cx,
                                )),
                        )
                        .child(
                            Button::new("open-glass")
                                .primary()
                                .label("IMPORT GLASS")
                                .disabled(!actions.open)
                                .on_click(cx.listener(|this, _, _, cx| this.open_bundle(cx))),
                        ),
                )
                .child(library_view(
                    &self.model.state,
                    &self.model.installed_glasses,
                    self.model.selected_path.as_ref(),
                    self.model.library_error.as_deref(),
                    actions.open,
                    self.file_drop_active,
                    cx,
                ));
            if !matches!(&self.model.state, UiState::Empty) {
                main = main
                    .child(
                        div()
                            .v_flex()
                            .gap_2()
                            .p_4()
                            .bg(cx.theme().group_box)
                            .child(eyebrow("SELECTED GLASS", cx))
                            .child(details_view(&self.model.state).text_size(px(14.0))),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("boot-glass")
                                    .primary()
                                    .label("START GLASS")
                                    .disabled(!actions.boot)
                                    .on_click(cx.listener(|this, _, _, cx| this.start_guest(cx))),
                            )
                            .child(
                                Button::new("restart-glass")
                                    .label("RESTART")
                                    .disabled(!actions.restart)
                                    .on_click(cx.listener(|this, _, _, cx| this.restart_guest(cx))),
                            )
                            .child(
                                Button::new("stop-glass")
                                    .label("STOP GLASS")
                                    .disabled(!actions.stop)
                                    .on_click(cx.listener(|this, _, _, cx| this.stop_guest(cx))),
                            ),
                    )
                    .child(status_view(&self.model.state).p_2().bg(cx.theme().sidebar));
            }
            div().flex_1().child(main)
        };
        let mut layout = root.child(
            div()
                .h_flex()
                .flex_1()
                .min_h(px(0.0))
                .child(sidebar_view(cx))
                .child(main),
        );
        if !is_active_run && !is_recovery_failure && !self.model.console.is_empty() {
            layout = layout.child(
                div()
                    .v_flex()
                    .h(px(180.0))
                    .flex_shrink_0()
                    .gap_2()
                    .p_3()
                    .bg(cx.theme().group_box)
                    .child(eyebrow("GUEST CONSOLE", cx))
                    .child(
                        div()
                            .flex_1()
                            .overflow_y_scrollbar()
                            .child(self.model.console.clone()),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(Input::new(&self.input).disabled(!actions.console_input))
                            .child(muted_text("Press Enter to send", cx)),
                    ),
            );
        }
        layout.child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .p_2()
                .bg(cx.theme().status_bar)
                .child(eyebrow("LOCAL ONLY", cx))
                .child(muted_text(
                    "Your glass and saved data stay on this computer.",
                    cx,
                )),
        )
    }
}

impl RootglassWindow {
    fn running_guest_view(
        &mut self,
        actions: crate::ui_model::UiActions,
        cx: &mut Context<Self>,
    ) -> Div {
        let (name, status, phase) = match &self.model.state {
            UiState::Running(run) => (run.name.clone(), run.status.clone(), "RUNNING"),
            UiState::Booting(run) => (run.name.clone(), run.status.clone(), "BOOTING"),
            UiState::Stopping(run) => (run.name.clone(), run.status.clone(), "STOPPING"),
            _ => unreachable!("running view only renders for an active run"),
        };
        let installed = self.model.selected_installed().cloned();
        let preview = self.model.selected_preview().cloned();
        let version = installed
            .as_ref()
            .map(|glass| glass.version.as_str())
            .or_else(|| preview.as_ref().map(|glass| glass.version.as_str()))
            .unwrap_or("Unknown version");
        let publisher = installed
            .as_ref()
            .map(|glass| glass.publisher_name.as_str())
            .or_else(|| preview.as_ref().map(|glass| glass.publisher.as_str()))
            .unwrap_or("Unknown publisher");
        let elapsed = self.run_started_at.map_or_else(
            || "Elapsed: —".to_string(),
            |started| {
                let minutes = started.elapsed().as_secs() / 60;
                if minutes == 0 {
                    "Elapsed: <1 min".to_string()
                } else {
                    format!("Elapsed: {minutes} min")
                }
            },
        );
        let status_background = if phase == "RUNNING" {
            cx.theme().accent
        } else {
            cx.theme().group_box
        };
        let status_foreground = if phase == "RUNNING" {
            cx.theme().accent_foreground
        } else {
            cx.theme().muted_foreground
        };
        let mut content = div()
            .v_flex()
            .flex_1()
            .h_full()
            .min_h(px(0.0))
            .gap_4()
            .p_6()
            .bg(cx.theme().background)
            .child(
                div().h_flex().items_start().child(
                    div()
                        .v_flex()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_1()
                        .child(eyebrow(format!("LIBRARY / {}", name.to_uppercase()), cx))
                        .child(page_title(name.clone(), cx))
                        .child(muted_text(
                            format!("Version {version} · Publisher: {publisher}"),
                            cx,
                        )),
                ),
            )
            .child(
                div()
                    .h_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(
                        Button::new("running-restart")
                            .secondary()
                            .label("RESTART")
                            .disabled(!actions.restart)
                            .on_click(cx.listener(|this, _, _, cx| this.restart_guest(cx))),
                    )
                    .child(
                        Button::new("running-stop")
                            .danger()
                            .label("STOP")
                            .disabled(!actions.stop)
                            .on_click(cx.listener(|this, _, _, cx| this.stop_guest(cx))),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_3()
                    .p_3()
                    .bg(status_background)
                    .child(div().size(px(8.0)).rounded_full().bg(status_foreground))
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .gap_1()
                            .child(
                                div()
                                    .child(phase)
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_size(px(11.0))
                                    .text_color(status_foreground),
                            )
                            .child(div().child(status).text_size(px(13.0))),
                    )
                    .child(muted_text(elapsed, cx)),
            )
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("running-tab-console")
                            .ghost()
                            .label("Console")
                            .selected(self.running_tab == RunningTab::Console)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_running_tab(RunningTab::Console, cx)
                            })),
                    )
                    .child(
                        Button::new("running-tab-overview")
                            .ghost()
                            .label("Overview")
                            .selected(self.running_tab == RunningTab::Overview)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_running_tab(RunningTab::Overview, cx)
                            })),
                    )
                    .child(
                        Button::new("running-tab-logs")
                            .ghost()
                            .label("Logs")
                            .selected(self.running_tab == RunningTab::Logs)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_running_tab(RunningTab::Logs, cx)
                            })),
                    ),
            );

        match self.running_tab {
            RunningTab::Console => {
                let console = if self.model.visible_console().is_empty() {
                    "No serial output received yet.".to_string()
                } else {
                    fenced_text(self.model.visible_console())
                };
                content = content.child(
                    div()
                        .v_flex()
                        .h(px(0.0))
                        .flex_1()
                        .min_h(px(0.0))
                        .gap_3()
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .child(eyebrow("GUEST CONSOLE", cx))
                                .child(div().flex_1())
                                .child(muted_text("Live serial output", cx)),
                        )
                        .child(
                            div()
                                .h(px(0.0))
                                .flex_1()
                                .min_h(px(0.0))
                                .p_4()
                                .bg(cx.theme().group_box)
                                .overflow_y_scrollbar()
                                .child(
                                    TextView::markdown("guest-console-output", console)
                                        .selectable(true)
                                        .selection_format(SelectionFormat::Plain),
                                ),
                        )
                        .child(
                            div()
                                .h_flex()
                                .flex_shrink_0()
                                .items_center()
                                .gap_3()
                                .child(Input::new(&self.input).disabled(!actions.console_input))
                                .child(muted_text("Press Enter to send to the guest", cx)),
                        ),
                );
            }
            RunningTab::Overview => {
                let mut cards = div().v_flex().gap_3();
                if let Some(glass) = installed.as_ref() {
                    cards = cards
                        .child(section_heading("GLASS", cx))
                        .child(info_row("Identifier", &glass.identifier, cx))
                        .child(info_row("Version", &glass.version, cx))
                        .child(info_row("Publisher", &glass.publisher_name, cx))
                        .child(info_row("Content digest", &glass.content_sha256, cx));
                }
                if let Some(preview) = preview.as_ref() {
                    cards = cards
                        .child(section_heading("GUEST PROFILE", cx))
                        .child(info_row(
                            "Architecture",
                            &preview.preflight.architecture,
                            cx,
                        ))
                        .child(info_row(
                            "Memory / CPUs",
                            &format!(
                                "{} MiB / {} CPUs",
                                preview.preflight.memory_mb, preview.preflight.cpus
                            ),
                            cx,
                        ))
                        .child(info_row("QEMU", &preview.preflight.qemu, cx))
                        .child(info_row(
                            "Acceleration",
                            &preview.preflight.acceleration,
                            cx,
                        ));
                    if !preview.preflight.runtime_assets.is_empty() {
                        cards = cards.child(info_row(
                            "Runtime assets",
                            &preview.preflight.runtime_assets.join(", "),
                            cx,
                        ));
                    }
                }
                let state_name = self
                    .model
                    .state_records
                    .iter()
                    .find(|state| state.id == self.model.selected_state_id)
                    .map(|state| state.name.as_str())
                    .unwrap_or("Default state");
                cards = cards
                    .child(section_heading("SAVED DATA", cx))
                    .child(info_row(
                        "Selected state",
                        &format!("{state_name} ({})", self.model.selected_state_id),
                        cx,
                    ));
                content = content.child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .p_4()
                        .bg(cx.theme().group_box)
                        .overflow_y_scrollbar()
                        .child(cards),
                );
            }
            RunningTab::Logs => {
                let log_text = if self.qemu_diagnostics_loading {
                    "Loading QEMU diagnostics…".to_string()
                } else if let Some(error) = &self.qemu_diagnostics_error {
                    format!("Unable to load QEMU diagnostics.\n{error}")
                } else if self.qemu_diagnostics.is_empty() {
                    "No QEMU diagnostic output was captured for this run.".to_string()
                } else {
                    fenced_text(&self.qemu_diagnostics)
                };
                let mut log_panel = div()
                    .v_flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .gap_3()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_3()
                            .child(eyebrow("QEMU DIAGNOSTICS", cx))
                            .child(div().flex_1())
                            .child(
                                Button::new("refresh-qemu-diagnostics")
                                    .secondary()
                                    .label(if self.qemu_diagnostics_loading {
                                        "LOADING…"
                                    } else {
                                        "REFRESH"
                                    })
                                    .disabled(self.qemu_diagnostics_loading)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh_qemu_diagnostics(cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(200.0))
                            .p_4()
                            .bg(cx.theme().group_box)
                            .overflow_y_scrollbar()
                            .child(
                                TextView::markdown("qemu-diagnostics-output", log_text)
                                    .selectable(true)
                                    .selection_format(SelectionFormat::Plain),
                            ),
                    );
                if self.qemu_diagnostics_truncated {
                    log_panel = log_panel.child(muted_text(
                        "Earlier diagnostic output was dropped; showing the latest 64 KiB.",
                        cx,
                    ));
                }
                content = content.child(log_panel);
            }
        }

        content
    }

    fn set_running_tab(&mut self, tab: RunningTab, cx: &mut Context<Self>) {
        self.running_tab = tab;
        if tab == RunningTab::Logs {
            self.refresh_qemu_diagnostics(cx);
        }
        cx.notify();
    }

    fn refresh_qemu_diagnostics(&mut self, cx: &mut Context<Self>) {
        let Some(run) = self.model.run_id.clone() else {
            self.qemu_diagnostics_error = Some("There is no active QEMU run.".to_string());
            self.qemu_diagnostics.clear();
            self.qemu_diagnostics_truncated = false;
            self.qemu_diagnostics_loading = false;
            cx.notify();
            return;
        };
        if self.qemu_diagnostics_loading {
            return;
        }
        self.qemu_diagnostics_loading = true;
        self.qemu_diagnostics_error = None;
        let service = self.model.shared_service();
        let background_executor = cx.background_executor().clone();
        let run_for_diagnostics = run.clone();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let service_for_diagnostics = service.clone();
            let result = background_executor
                .spawn(async move {
                    lock_service(&service_for_diagnostics)
                        .and_then(|mut service| service.qemu_diagnostics(&run_for_diagnostics))
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.model.run_id.as_ref() != Some(&run) {
                    return;
                }
                match result {
                    Ok(snapshot) => view.apply_diagnostics_snapshot(snapshot),
                    Err(error) => {
                        view.qemu_diagnostics.clear();
                        view.qemu_diagnostics_truncated = false;
                        view.qemu_diagnostics_error = Some(error.to_string());
                    }
                }
                view.qemu_diagnostics_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_diagnostics_snapshot(&mut self, snapshot: DiagnosticLogSnapshot) {
        self.qemu_diagnostics = snapshot.text;
        self.qemu_diagnostics_truncated = snapshot.truncated;
        self.qemu_diagnostics_error = None;
    }
}

fn sidebar_view(cx: &App) -> Div {
    div()
        .v_flex()
        .w(px(240.0))
        .h_full()
        .flex_shrink_0()
        .gap_2()
        .p_4()
        .bg(cx.theme().sidebar)
        .text_color(cx.theme().sidebar_foreground)
        .child(
            div()
                .child("rootglass")
                .text_size(px(20.0))
                .font_weight(FontWeight::BOLD),
        )
        .child(eyebrow("LOCAL WORKSPACE", cx))
        .child(
            div()
                .p_2()
                .bg(cx.theme().sidebar_accent)
                .text_color(cx.theme().sidebar_accent_foreground)
                .child("Library"),
        )
        .child(muted_text("Activity", cx).p_2())
        .child(muted_text("Publishers", cx).p_2())
        .child(muted_text("Settings", cx).p_2())
        .child(div().flex_1())
        .child(eyebrow("ON THIS COMPUTER", cx))
        .child(muted_text("One glass at a time", cx))
        .child(muted_text(
            format!("{} · {}", std::env::consts::OS, std::env::consts::ARCH),
            cx,
        ))
}

fn section_heading(label: impl Into<String>, cx: &App) -> Div {
    div().pt_2().child(eyebrow(label, cx))
}

fn info_row(label: impl Into<String>, value: &str, cx: &App) -> Div {
    div()
        .h_flex()
        .items_start()
        .gap_4()
        .py_2()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(div().w(px(160.0)).child(eyebrow(label, cx)))
        .child(div().flex_1().child(value.to_string()).text_size(px(13.0)))
}

fn fenced_text(text: &str) -> String {
    let mut longest_run = 0;
    let mut current_run = 0;
    for character in text.chars() {
        if character == '`' {
            current_run += 1;
            longest_run = longest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    let fence = "`".repeat(longest_run.max(2) + 1);
    format!("{fence}text\n{text}\n{fence}")
}

fn eyebrow(label: impl Into<String>, cx: &App) -> Div {
    div()
        .child(label.into())
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(11.0))
        .text_color(cx.theme().muted_foreground)
}

fn page_title(title: impl Into<String>, cx: &App) -> Div {
    div()
        .child(title.into())
        .text_size(px(24.0))
        .text_color(cx.theme().foreground)
}

fn muted_text(text: impl Into<String>, cx: &App) -> Div {
    div()
        .child(text.into())
        .text_size(px(12.0))
        .text_color(cx.theme().muted_foreground)
}

fn install_rootglass_theme(cx: &mut App) {
    let dark_theme = {
        let registry = ThemeRegistry::global_mut(cx);
        registry
            .load_themes_from_str(include_str!("../themes/rootglass.json"))
            .expect("Rootglass theme configuration is valid");
        registry
            .themes()
            .get("Rootglass Dark")
            .cloned()
            .expect("Rootglass dark theme is registered")
    };
    Theme::global_mut(cx).dark_theme = dark_theme;
    Theme::change(ThemeMode::Dark, None, cx);
}

impl RootglassWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Type to guest console…"));
        let model = UiModel::default();
        log_desktop_smoke_trust_policy(&model);
        let service = model.shared_service();
        let background_executor = cx.background_executor().clone();
        let _library_task = cx.spawn(async move |view, cx| {
            let service_for_load = service.clone();
            let result = background_executor
                .spawn(async move {
                    lock_service(&service_for_load)
                        .and_then(|mut service| service.installed_glasses())
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                view.model.finish_library_refresh(result);
                let first_installed = if matches!(&view.model.state, UiState::Empty)
                    && view.model.selected_path.is_none()
                {
                    view.model
                        .installed_glasses
                        .first()
                        .map(|glass| glass.package_path.clone())
                } else {
                    None
                };
                if let Some(path) = first_installed {
                    view.select_bundle(path, cx);
                }
                cx.notify();
            });
        });

        let service = model.shared_service();
        let background_executor = cx.background_executor().clone();
        let serial_sentinel = std::env::var_os("ROOTGLASS_DESKTOP_SMOKE_SERIAL_SENTINEL")
            .map(|value| value.to_string_lossy().into_owned().into_bytes())
            .unwrap_or_default();
        let mut serial_sentinel_tail = Vec::new();
        let _poll_task = cx.spawn(async move |view, cx| loop {
            background_executor.timer(Duration::from_millis(100)).await;
            let service_for_poll = service.clone();
            let result = background_executor
                .spawn(async move {
                    lock_service(&service_for_poll).and_then(|mut service| service.poll_events())
                })
                .await;
            let guest_ready = result.as_ref().is_ok_and(|events| {
                events
                    .iter()
                    .any(|event| matches!(event, RuntimeEvent::GuestReady { .. }))
            });
            if let Ok(events) = &result {
                for event in events {
                    match event {
                        RuntimeEvent::GuestReady { .. } => {
                            desktop_smoke_event("ROOTGLASS_DESKTOP_GUEST_READY");
                        }
                        RuntimeEvent::SerialChunk { bytes, .. } => {
                            if !serial_sentinel.is_empty() {
                                let mut scan =
                                    Vec::with_capacity(serial_sentinel_tail.len() + bytes.len());
                                scan.extend_from_slice(&serial_sentinel_tail);
                                scan.extend_from_slice(bytes);
                                if scan.windows(serial_sentinel.len()).any(|window| {
                                    window.iter().zip(&serial_sentinel).all(|(byte, expected)| {
                                        byte.to_ascii_lowercase() == expected.to_ascii_lowercase()
                                    })
                                }) {
                                    desktop_smoke_event("ROOTGLASS_DESKTOP_SERIAL_ECHO");
                                }
                                let tail_len =
                                    serial_sentinel.len().saturating_sub(1).min(scan.len());
                                serial_sentinel_tail =
                                    scan[scan.len().saturating_sub(tail_len)..].to_vec();
                            }
                        }
                        _ => {}
                    }
                }
            }
            if view
                .update(cx, |view, cx| {
                    view.model.apply_poll_result(result);
                    if guest_ready {
                        view.failure_guest_was_ready = true;
                        if view.run_started_at.is_none()
                            && matches!(view.model.state, UiState::Running(_))
                        {
                            view.run_started_at = Some(Instant::now());
                        }
                    }
                    if !matches!(
                        view.model.state,
                        UiState::Running(_) | UiState::Booting(_) | UiState::Stopping(_)
                    ) {
                        view.run_started_at = None;
                        view.qemu_diagnostics_loading = false;
                    }
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        });

        let _input_subscription =
            cx.subscribe_in(&input, window, |view, state, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = state.read(cx).value().to_string();
                    if let Some((run, bytes)) = view.model.input_request(&text) {
                        desktop_smoke_event("ROOTGLASS_DESKTOP_SERIAL_INPUT_REQUEST");
                        let service = view.model.shared_service();
                        let background_executor = cx.background_executor().clone();
                        cx.spawn(async move |view, cx| {
                            let service_for_write = service.clone();
                            let run_for_write = run.clone();
                            let result = background_executor
                                .spawn(async move {
                                    lock_service(&service_for_write).and_then(|mut service| {
                                        service.write_serial(&run_for_write, &bytes)
                                    })
                                })
                                .await;
                            let _ = view.update(cx, |view, cx| {
                                desktop_smoke_event(if result.is_ok() {
                                    "ROOTGLASS_DESKTOP_SERIAL_INPUT_SENT"
                                } else {
                                    "ROOTGLASS_DESKTOP_SERIAL_INPUT_FAILED"
                                });
                                view.model.finish_input(&run, result);
                                cx.notify();
                            });
                        })
                        .detach();
                    }
                    state.update(cx, |state, cx| state.set_value("", window, cx));
                    cx.notify();
                }
            });

        desktop_smoke_event("ROOTGLASS_DESKTOP_WINDOW_READY");
        #[cfg(feature = "embedded-terminal")]
        let embedded_tui = std::env::var_os("ROOTGLASS_GPUI_SMOKE")
            .is_some()
            .then(|| cx.new(|cx| embedded_terminal::EmbeddedTerminal::new(window, cx)));

        Self {
            model,
            input,
            running_tab: RunningTab::Console,
            qemu_diagnostics: String::new(),
            qemu_diagnostics_error: None,
            qemu_diagnostics_truncated: false,
            qemu_diagnostics_loading: false,
            run_started_at: None,
            failure_guest_was_ready: false,
            failure_diagnostics_open: false,
            recovery_retrying: false,
            recovery_preview_context: None,
            pending_file_drop: None,
            file_drop_active: false,
            #[cfg(feature = "embedded-terminal")]
            embedded_tui,
            _library_task,
            _poll_task,
            _input_subscription,
        }
    }

    fn on_open_bundle(&mut self, _: &OpenBundle, _: &mut Window, cx: &mut Context<Self>) {
        self.open_bundle(cx);
    }

    fn on_stop_shortcut(&mut self, _: &StopGlass, _: &mut Window, cx: &mut Context<Self>) {
        self.stop_guest(cx);
    }

    fn on_quit(&mut self, _: &Quit, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    #[cfg(feature = "embedded-terminal")]
    fn open_embedded_tui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.embedded_tui.is_none() {
            self.embedded_tui =
                Some(cx.new(|cx| embedded_terminal::EmbeddedTerminal::new(window, cx)));
            cx.notify();
        }
    }

    #[cfg(feature = "embedded-terminal")]
    fn close_embedded_tui(&mut self, cx: &mut Context<Self>) {
        self.embedded_tui = None;
        cx.notify();
    }

    fn open_bundle(&mut self, cx: &mut Context<Self>) {
        if self.recovery_retrying || !self.model.actions().open {
            return;
        }
        let can_select_mixed_paths = cx.can_select_mixed_files_and_dirs();
        let prompt = if can_select_mixed_paths {
            "Select a Rootglass .glass archive or directory"
        } else {
            "Select a Rootglass .glass archive"
        };
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: can_select_mixed_paths,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        desktop_smoke_event("ROOTGLASS_DESKTOP_PICKER_OPEN");
        cx.spawn(async move |view, cx| {
            let selected = match selection.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_PICKER_CANCELLED");
                    None
                }
                Ok(Err(_)) | Err(_) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_PICKER_FAILED");
                    None
                }
            };
            let Some(path) = selected else { return };
            let _ = view.update(cx, |view, cx| view.open_path(path, cx));
        })
        .detach();
    }

    fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.recovery_retrying || !self.model.actions().open {
            desktop_smoke_event("ROOTGLASS_DESKTOP_OPEN_PATH_REJECTED");
            return;
        }
        self.failure_diagnostics_open = false;
        self.recovery_preview_context = None;
        self.run_started_at = None;
        self.failure_guest_was_ready = false;
        desktop_smoke_event("ROOTGLASS_DESKTOP_OPEN_PATH_ACCEPTED");
        let background_executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            let type_check_path = path.clone();
            let is_archive = background_executor
                .spawn(async move {
                    type_check_path.is_file()
                        && type_check_path
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("glass"))
                })
                .await;
            desktop_smoke_event(if is_archive {
                "ROOTGLASS_DESKTOP_OPEN_PATH_ARCHIVE"
            } else {
                "ROOTGLASS_DESKTOP_OPEN_PATH_DIRECTORY"
            });
            let ticket = match view.update(cx, |view, cx| {
                if view.recovery_retrying || !view.model.actions().open {
                    return None;
                }
                let ticket = if is_archive {
                    view.model.begin_import(path.clone())
                } else {
                    view.model.begin_inspection(path.clone())
                };
                cx.notify();
                ticket
            }) {
                Ok(Some(ticket)) => ticket,
                Err(_) => return,
                Ok(None) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_OPEN_PATH_NOT_STARTED");
                    return;
                }
            };
            let service = view.update(cx, |view, _| view.model.shared_service());
            let Ok(service) = service else { return };
            if is_archive {
                let source = path.clone();
                let service_for_import = service.clone();
                let result = background_executor
                    .spawn(async move {
                        let mut service = lock_service(&service_for_import)?;
                        match service.review_archive(&source)? {
                            Some(review) => Ok(ArchiveOpenResult::PublisherReview(review)),
                            None => Ok(ArchiveOpenResult::Imported(
                                service.import_and_inspect(&source),
                            )),
                        }
                    })
                    .await;
                let state_path = result.as_ref().ok().and_then(|outcome| match outcome {
                    ArchiveOpenResult::Imported(Ok(completion))
                        if matches!(
                            &completion.inspection,
                            crate::ui_model::InspectionOutcome::Ready(_)
                        ) =>
                    {
                        Some(completion.installed.package_path.clone())
                    }
                    _ => None,
                });
                let state_result = if let Some(path) = state_path {
                    let service_for_state = service.clone();
                    Some(
                        background_executor
                            .spawn(async move {
                                lock_service(&service_for_state)
                                    .and_then(|mut service| service.list_states(&path))
                            })
                            .await,
                    )
                } else {
                    None
                };
                let _ = view.update(cx, |view, cx| {
                    let applied = match result {
                        Ok(ArchiveOpenResult::PublisherReview(review)) => {
                            view.model.finish_inspection(
                                &ticket,
                                crate::ui_model::InspectionOutcome::PublisherReview(review),
                            )
                        }
                        Ok(ArchiveOpenResult::Imported(result)) => {
                            view.model.finish_import(&ticket, result)
                        }
                        Err(error) => view.model.finish_import(&ticket, Err(error)),
                    };
                    if applied {
                        view.log_desktop_smoke_inspection();
                        if let Some(state_result) = state_result {
                            let state_loaded = state_result.is_ok();
                            view.model.finish_state_operation(state_result);
                            desktop_smoke_event(if state_loaded {
                                "ROOTGLASS_DESKTOP_STATE_READY"
                            } else {
                                "ROOTGLASS_DESKTOP_STATE_FAILED"
                            });
                        }
                    }
                    cx.notify();
                });
            } else {
                desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_SERVICE_START");
                let service_for_inspect = service.clone();
                let source = path.clone();
                let result = background_executor
                    .spawn(async move {
                        match lock_service(&service_for_inspect) {
                            Ok(mut service) => service.inspect_bundle(&source),
                            Err(error) => crate::ui_model::InspectionOutcome::Failed(
                                crate::ui_model::FailureDetails {
                                    path: Some(source),
                                    phase: format!("{:?}", error.phase),
                                    summary: error.summary,
                                    detail: error.detail,
                                },
                            ),
                        }
                    })
                    .await;
                desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_SERVICE_DONE");
                desktop_smoke_event("ROOTGLASS_DESKTOP_STATE_LIST_START");
                let state_result = match &result {
                    crate::ui_model::InspectionOutcome::Ready(preview) => {
                        let path = preview.path.clone();
                        let service_for_state = service.clone();
                        let result = background_executor
                            .spawn(async move {
                                lock_service(&service_for_state)
                                    .and_then(|mut service| service.list_states(&path))
                            })
                            .await;
                        desktop_smoke_event("ROOTGLASS_DESKTOP_STATE_LIST_DONE");
                        Some(result)
                    }
                    _ => None,
                };
                desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECTION_UPDATE_START");
                let update_result = view.update(cx, |view, cx| {
                    let inspection_applied = view.model.finish_inspection(&ticket, result);
                    desktop_smoke_event(if inspection_applied {
                        "ROOTGLASS_DESKTOP_INSPECTION_APPLIED"
                    } else {
                        "ROOTGLASS_DESKTOP_INSPECTION_STALE"
                    });
                    if inspection_applied {
                        view.log_desktop_smoke_inspection();
                        if let Some(state_result) = state_result {
                            let state_loaded = state_result.is_ok();
                            view.model.finish_state_operation(state_result);
                            desktop_smoke_event(if state_loaded {
                                "ROOTGLASS_DESKTOP_STATE_READY"
                            } else {
                                "ROOTGLASS_DESKTOP_STATE_FAILED"
                            });
                        }
                    }
                    cx.notify();
                });
                desktop_smoke_event(if update_result.is_ok() {
                    "ROOTGLASS_DESKTOP_INSPECTION_UPDATE_DONE"
                } else {
                    "ROOTGLASS_DESKTOP_INSPECTION_UPDATE_FAILED"
                });
            }
        })
        .detach();
    }

    fn log_desktop_smoke_inspection(&self) {
        if std::env::var_os("ROOTGLASS_DESKTOP_SMOKE").is_none() {
            return;
        }
        match &self.model.state {
            UiState::PublisherReview(review) => {
                desktop_smoke_event("ROOTGLASS_DESKTOP_PUBLISHER_REVIEW");
                desktop_smoke_detail(format!(
                    "ROOTGLASS_DESKTOP_PUBLISHER_REVIEW key_id={} publisher_identity={}",
                    review.key_id, review.publisher_identity
                ));
            }
            UiState::Ready(preview) => match &preview.signature {
                SignatureStatus::Verified(_) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_VERIFIED");
                    desktop_smoke_event(if preview.preflight.blockers.is_empty() {
                        "ROOTGLASS_DESKTOP_PREFLIGHT_READY"
                    } else {
                        "ROOTGLASS_DESKTOP_PREFLIGHT_BLOCKED"
                    });
                }
                SignatureStatus::Untrusted(reason) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_UNTRUSTED");
                    desktop_smoke_detail(format!(
                        "ROOTGLASS_DESKTOP_SIGNATURE_UNTRUSTED reason={reason}"
                    ));
                }
                SignatureStatus::Tampered(_) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_TAMPERED")
                }
                SignatureStatus::Invalid(_) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_INVALID")
                }
            },
            UiState::Blocked(blocked) => {
                desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_BLOCKED");
                desktop_smoke_detail(format!(
                    "ROOTGLASS_DESKTOP_PREFLIGHT_BLOCKED summary={} detail={}",
                    blocked.reason,
                    blocked.preflight.blockers.join(" | ")
                ));
            }
            UiState::Failed(details) if details.detail.contains("unknown signing key") => {
                desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_UNTRUSTED");
                desktop_smoke_detail(format!(
                    "ROOTGLASS_DESKTOP_SIGNATURE_UNTRUSTED detail={}",
                    details.detail
                ));
            }
            UiState::Failed(details)
                if details.detail.contains("subject digest mismatch")
                    || details.detail.contains("signature verification failed") =>
            {
                desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_TAMPERED");
            }
            UiState::Failed(details) => {
                desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_FAILED");
                eprintln!(
                    "ROOTGLASS_DESKTOP_INSPECT_FAILURE phase={} summary={} detail={}",
                    details.phase, details.summary, details.detail
                );
            }
            _ => desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_UNEXPECTED_STATE"),
        }
    }

    fn select_bundle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.open_path(path, cx);
    }

    fn recovery_glass_name(&self, failure: &RecoveryFailure) -> String {
        if let Some(preview) = self
            .model
            .selected_preview()
            .or(self.recovery_preview_context.as_ref())
        {
            return preview.name.clone();
        }
        if let Some(glass) = self.model.selected_installed() {
            return glass.name.clone();
        }
        let path = failure.retry_path(self.model.selected_path.as_ref());
        let stem = path
            .as_deref()
            .and_then(|path| path.file_stem())
            .and_then(|stem| stem.to_str())
            .unwrap_or("this glass");
        stem.split(['-', '_', '.'])
            .filter(|word| !word.is_empty())
            .map(|word| {
                let mut characters = word.chars();
                characters.next().map_or_else(String::new, |first| {
                    first.to_uppercase().collect::<String>() + characters.as_str()
                })
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn retry_failure(&mut self, cx: &mut Context<Self>) {
        if self.recovery_retrying {
            return;
        }
        let Some(failure) = RecoveryFailure::from_ui_state(&self.model.state) else {
            return;
        };
        let Some(path) = failure.retry_path(self.model.selected_path.as_ref()) else {
            return;
        };
        let state_before = RecoveryStateSnapshot::capture(&self.model);
        let preview_before = self
            .model
            .selected_preview()
            .cloned()
            .or_else(|| self.recovery_preview_context.clone());
        self.recovery_retrying = true;
        desktop_smoke_event("ROOTGLASS_DESKTOP_RECOVERY_RETRY_REQUEST");
        let service = self.model.shared_service();
        let background_executor = cx.background_executor().clone();
        let path_for_check = path.clone();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let service_for_retry = service.clone();
            let retry_result = background_executor
                .spawn(async move {
                    match lock_service(&service_for_retry) {
                        Ok(mut service) => {
                            let outcome = service.inspect_bundle(&path_for_check);
                            let states =
                                if matches!(&outcome, crate::ui_model::InspectionOutcome::Ready(_))
                                {
                                    Some(service.list_states(&path_for_check))
                                } else {
                                    None
                                };
                            (outcome, states)
                        }
                        Err(error) => (
                            crate::ui_model::InspectionOutcome::Failed(
                                crate::ui_model::FailureDetails {
                                    path: Some(path_for_check.clone()),
                                    phase: format!("{:?}", error.phase),
                                    summary: error.summary,
                                    detail: error.detail,
                                },
                            ),
                            None,
                        ),
                    }
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                let ready = matches!(
                    &retry_result.0,
                    crate::ui_model::InspectionOutcome::Ready(_)
                );
                let verified_failure = matches!(
                    &retry_result.0,
                    crate::ui_model::InspectionOutcome::Blocked(_)
                        | crate::ui_model::InspectionOutcome::Failed(_)
                );
                if let Some(ticket) = view.model.begin_inspection(path) {
                    view.model.finish_inspection(&ticket, retry_result.0);
                    if let Some(states) = retry_result.1 {
                        view.model.finish_state_operation(states);
                    } else if verified_failure {
                        state_before.restore(&mut view.model);
                    }
                    if ready {
                        view.recovery_preview_context = None;
                        view.run_started_at = None;
                        view.failure_guest_was_ready = false;
                        view.failure_diagnostics_open = false;
                        desktop_smoke_event("ROOTGLASS_DESKTOP_RECOVERY_RETRY_READY");
                    } else if verified_failure {
                        view.recovery_preview_context = preview_before;
                        desktop_smoke_event(if matches!(&view.model.state, UiState::Blocked(_)) {
                            "ROOTGLASS_DESKTOP_RECOVERY_RETRY_BLOCKED"
                        } else {
                            "ROOTGLASS_DESKTOP_RECOVERY_RETRY_FAILED"
                        });
                    } else {
                        view.recovery_preview_context = None;
                        desktop_smoke_event("ROOTGLASS_DESKTOP_RECOVERY_RETRY_PUBLISHER_REVIEW");
                    }
                }
                view.recovery_retrying = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_publisher_review(&mut self, cx: &mut Context<Self>) {
        self.model.return_to_library();
        cx.notify();
    }

    fn start_guest(&mut self, cx: &mut Context<Self>) {
        self.run_started_at = None;
        self.failure_guest_was_ready = false;
        self.failure_diagnostics_open = false;
        self.recovery_retrying = false;
        self.recovery_preview_context = None;
        self.qemu_diagnostics.clear();
        self.qemu_diagnostics_error = None;
        self.qemu_diagnostics_truncated = false;
        self.qemu_diagnostics_loading = false;
        let Some((ticket, path)) = self.model.begin_start() else {
            return;
        };
        desktop_smoke_event("ROOTGLASS_DESKTOP_BOOT_REQUEST");
        let service = self.model.shared_service();
        let background_executor = cx.background_executor().clone();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let service_for_start = service.clone();
            let result = background_executor
                .spawn(async move {
                    lock_service(&service_for_start).and_then(|mut service| service.start(&path))
                })
                .await;
            let orphan = result.as_ref().ok().cloned();
            let boot_succeeded = result.is_ok();
            let effect = view.update(cx, |view, cx| {
                let effect = view.model.finish_start(&ticket, result);
                desktop_smoke_event(if boot_succeeded {
                    "ROOTGLASS_DESKTOP_BOOT_STARTED"
                } else {
                    "ROOTGLASS_DESKTOP_BOOT_FAILED"
                });
                cx.notify();
                effect
            });
            let stop = match effect {
                Ok(StartEffect::StopOrphan(run)) => Some(run),
                Err(_) => orphan,
                _ => None,
            };
            if let Some(run) = stop {
                let service_for_cleanup = service.clone();
                let _ = background_executor
                    .spawn(async move {
                        lock_service(&service_for_cleanup)
                            .and_then(|mut service| service.stop(&run))
                    })
                    .await;
            }
        })
        .detach();
    }

    fn stop_guest(&mut self, cx: &mut Context<Self>) {
        match self.model.begin_stop() {
            Some(StopRequest::CancelStartingOperation) => {
                desktop_smoke_event("ROOTGLASS_DESKTOP_BOOT_CANCELLED");
                cx.notify();
            }
            Some(StopRequest::StopRun { ticket, run }) => {
                desktop_smoke_event("ROOTGLASS_DESKTOP_STOP_REQUEST");
                let service = self.model.shared_service();
                let background_executor = cx.background_executor().clone();
                cx.notify();
                cx.spawn(async move |view, cx| {
                    let service_for_stop = service.clone();
                    let run_for_stop = run.clone();
                    let result = background_executor
                        .spawn(async move {
                            lock_service(&service_for_stop)
                                .and_then(|mut service| service.stop(&run_for_stop))
                        })
                        .await;
                    let _ = view.update(cx, |view, cx| {
                        desktop_smoke_event(if result.is_ok() {
                            "ROOTGLASS_DESKTOP_STOPPED"
                        } else {
                            "ROOTGLASS_DESKTOP_STOP_FAILED"
                        });
                        view.model.finish_stop(&ticket, result);
                        cx.notify();
                    });
                })
                .detach();
            }
            None => {}
        }
    }

    fn restart_guest(&mut self, cx: &mut Context<Self>) {
        self.run_started_at = None;
        self.failure_guest_was_ready = false;
        self.qemu_diagnostics.clear();
        self.qemu_diagnostics_error = None;
        self.qemu_diagnostics_truncated = false;
        self.qemu_diagnostics_loading = false;
        let Some((ticket, run, path)) = self.model.begin_restart() else {
            return;
        };
        desktop_smoke_event("ROOTGLASS_DESKTOP_RESTART_REQUEST");
        let service = self.model.shared_service();
        let background_executor = cx.background_executor().clone();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let service_for_restart = service.clone();
            let result = background_executor
                .spawn(async move {
                    lock_service(&service_for_restart)
                        .and_then(|mut service| service.restart(&run, &path))
                })
                .await;
            let orphan = result.as_ref().ok().cloned();
            let restart_succeeded = result.is_ok();
            let effect = view.update(cx, |view, cx| {
                let effect = view.model.finish_restart(&ticket, result);
                desktop_smoke_event(if restart_succeeded {
                    "ROOTGLASS_DESKTOP_RESTART_STARTED"
                } else {
                    "ROOTGLASS_DESKTOP_RESTART_FAILED"
                });
                cx.notify();
                effect
            });
            let stop = match effect {
                Ok(StartEffect::StopOrphan(run)) => Some(run),
                Err(_) => orphan,
                _ => None,
            };
            if let Some(run) = stop {
                let service_for_cleanup = service.clone();
                let _ = background_executor
                    .spawn(async move {
                        lock_service(&service_for_cleanup)
                            .and_then(|mut service| service.stop(&run))
                    })
                    .await;
            }
        })
        .detach();
    }
}

fn desktop_smoke_event(marker: &str) {
    if std::env::var_os("ROOTGLASS_DESKTOP_SMOKE").is_some() {
        eprintln!("{marker}");
        if let Some(path) = std::env::var_os("ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG") {
            if let Ok(mut log) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write;
                let _ = writeln!(log, "{marker}");
            }
        }
    }
}

fn desktop_smoke_detail(detail: String) {
    if std::env::var_os("ROOTGLASS_DESKTOP_SMOKE").is_none() {
        return;
    }
    let detail = detail.replace('\n', " ").replace('\r', " ");
    eprintln!("{detail}");
    if let Some(path) = std::env::var_os("ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG") {
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = writeln!(log, "{detail}");
        }
    }
}

fn log_desktop_smoke_trust_policy(model: &UiModel) {
    if std::env::var_os("ROOTGLASS_DESKTOP_SMOKE").is_none() {
        return;
    }
    let expected_key_id = std::env::var("ROOTGLASS_DESKTOP_SMOKE_EXPECTED_TRUST_KEY_ID").ok();
    let shared_service = model.shared_service();
    match lock_service(&shared_service) {
        Ok(mut service) => {
            let status = service.trust_policy_status();
            let key_ids = status
                .trusted_publishers
                .iter()
                .map(|publisher| publisher.key_id.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let trusted_key_ids = if key_ids.is_empty() {
                "<none>".to_string()
            } else {
                key_ids
            };
            let expected_key_present = expected_key_id.as_deref().is_some_and(|expected| {
                status
                    .trusted_publishers
                    .iter()
                    .any(|publisher| publisher.key_id == expected)
            });
            desktop_smoke_detail(format!(
                "ROOTGLASS_DESKTOP_TRUST_POLICY path={} trusted_key_ids={} revoked_key_ids={} error={} expected_key_present={expected_key_present}",
                status
                    .config_path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string()),
                trusted_key_ids,
                if status.revoked_key_ids.is_empty() {
                    "<none>".to_string()
                } else {
                    status.revoked_key_ids.join(",")
                },
                status.error.as_deref().unwrap_or("<none>")
            ));
            desktop_smoke_event(if expected_key_present {
                "ROOTGLASS_DESKTOP_TRUST_POLICY_EXPECTED_KEY_PRESENT"
            } else {
                "ROOTGLASS_DESKTOP_TRUST_POLICY_EXPECTED_KEY_MISSING"
            });
        }
        Err(error) => desktop_smoke_detail(format!(
            "ROOTGLASS_DESKTOP_TRUST_POLICY unavailable={}",
            error.detail
        )),
    };
}

fn path_from_open_url(value: &str) -> Option<PathBuf> {
    url::Url::parse(value).ok()?.to_file_path().ok()
}

fn open_urls_in_view(app: &AsyncApp, view: &Entity<RootglassWindow>, urls: Vec<String>) {
    for url in urls {
        let Some(path) = path_from_open_url(&url) else {
            continue;
        };
        let app = app.clone();
        let view = view.clone();
        app.spawn(async move |cx| {
            let _ = view.update(cx, |view, cx| view.open_path(path, cx));
        })
        .detach();
        break;
    }
}

fn library_view(
    state: &UiState,
    installed_glasses: &[InstalledGlass],
    selected_path: Option<&PathBuf>,
    library_error: Option<&str>,
    can_select: bool,
    file_drop_active: bool,
    cx: &mut Context<RootglassWindow>,
) -> Div {
    let mut view = div().v_flex().gap_2().p_2().bg(cx.theme().table).child(
        div()
            .h_flex()
            .items_center()
            .gap_2()
            .p_2()
            .bg(cx.theme().table_head)
            .child(eyebrow("GLASS", cx).flex_1())
            .child(eyebrow("VERSION", cx).w(px(84.0)))
            .child(eyebrow("PUBLISHER", cx).w(px(160.0)))
            .child(eyebrow("STATUS", cx).w(px(100.0))),
    );
    if installed_glasses.is_empty() {
        view = view
            .child(div().p_3().child("No glasses are installed."))
            .child(muted_text("Open a signed .glass bundle to inspect it.", cx))
            .child(muted_text(
                if file_drop_active {
                    "Release the .glass bundle or folder to inspect it."
                } else {
                    "You can also drop a .glass bundle or folder here."
                },
                cx,
            ));
    } else {
        for glass in installed_glasses {
            let path = glass.package_path.clone();
            let selected = selected_path == Some(&glass.package_path);
            let background = if selected {
                cx.theme().accent
            } else {
                cx.theme().table
            };
            let foreground = if selected {
                cx.theme().accent_foreground
            } else {
                cx.theme().foreground
            };
            view = view.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .bg(background)
                    .text_color(foreground)
                    .child(
                        div().flex_1().child(
                            Button::new(format!("installed-{}", glass.content_sha256))
                                .ghost()
                                .label(glass.name.clone())
                                .disabled(!can_select)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_bundle(path.clone(), cx);
                                })),
                        ),
                    )
                    .child(div().w(px(84.0)).child(glass.version.clone()))
                    .child(div().w(px(160.0)).child(glass.publisher_name.clone()))
                    .child(
                        div()
                            .w(px(100.0))
                            .text_color(if selected {
                                cx.theme().accent_foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(if selected { "Selected" } else { "Installed" }),
                    ),
            );
        }
    }
    if let Some(error) = library_error {
        view = view.child(div().text_color(cx.theme().danger).child(format!(
            "The installed library could not be loaded: {error}"
        )));
    }
    match state {
        UiState::Opening(path) => {
            view = view.child(muted_text(
                format!("Opening {} in the background…", path.display()),
                cx,
            ));
        }
        UiState::Importing(path) => {
            view = view.child(muted_text(
                format!("Verifying and importing {}…", path.display()),
                cx,
            ));
        }
        UiState::Imported(glass) => {
            view = view
                .child(format!("Installed {} {}", glass.name, glass.version))
                .child(muted_text(format!("Publisher: {}", glass.publisher), cx))
                .child(muted_text(format!("Content digest: {}", glass.digest), cx))
                .child(muted_text(
                    format!("Installed at: {}", glass.path.display()),
                    cx,
                ));
        }
        _ => {}
    }
    view
}

fn details_view(state: &UiState) -> Div {
    match state {
        UiState::Empty => div()
            .v_flex()
            .child("No bundle selected")
            .child("Publisher, verification, and preflight details appear after inspection.")
            .child("Boot is unavailable until a bundle passes verification and preflight."),
        UiState::Opening(path) => div()
            .v_flex()
            .child("Opening bundle in the background…")
            .child(path.display().to_string()),
        UiState::Loading => div()
            .v_flex()
            .child("Inspecting bundle…")
            .child("Verification and preflight are pending."),
        UiState::Importing(_) => div()
            .v_flex()
            .child("Verifying archive and importing package…"),
        UiState::Validating => div()
            .v_flex()
            .child("Checking manifest fields only…")
            .child("Signature and publisher trust are not checked by Validate."),
        UiState::ManifestValidated(report) => manifest_validation_view(report),
        UiState::Planning => div()
            .v_flex()
            .child("Building verified launch plan…")
            .child("Checking signature, publisher trust, and runtime preflight."),
        UiState::Planned(plan) => {
            preview_details_view("Verified launch plan", &plan.preview, Some(&plan.command))
        }
        UiState::Removing(details) => removal_details_view(details),
        UiState::Imported(glass) => div()
            .v_flex()
            .child(format!("{} {} installed", glass.name, glass.version))
            .child(format!("Trusted publisher: {}", glass.publisher))
            .child("The archive was verified and copied into the local package store."),
        UiState::Ready(glass) => preview_details_view("Ready", glass, None),
        UiState::PublisherReview(review) => div()
            .v_flex()
            .child("Publisher is not trusted · Signature not verified")
            .child(format!(
                "Declared publisher · unverified: {}",
                review.publisher_name
            ))
            .child(format!(
                "Declared identity · unverified: {}",
                review.publisher_identity
            ))
            .child(format!("Signing key ID · unverified: {}", review.key_id))
            .child("Import and boot remain blocked until this key is trusted."),
        UiState::Blocked(blocked) => {
            let mut view = div()
                .v_flex()
                .child("Launch blocked")
                .child(blocked.reason.clone());
            if let Some(path) = &blocked.path {
                view = view.child(format!("Selected path: {}", path.display()));
            }
            view = view.child(format!(
                "Guest: {} / {} · {} MiB · {} CPU(s)",
                blocked.preflight.architecture,
                blocked.preflight.profile,
                blocked.preflight.memory_mb,
                blocked.preflight.cpus
            ));
            view = view.child(format!("QEMU: {}", blocked.preflight.qemu));
            view = view.child(format!("Acceleration: {}", blocked.preflight.acceleration));
            for asset in &blocked.preflight.runtime_assets {
                view = view.child(format!("Runtime asset: {asset}"));
            }
            for blocker in &blocked.preflight.blockers {
                view = view.child(format!("Blocked: {blocker}"));
            }
            view
        }
        UiState::Failed(failure) => div()
            .v_flex()
            .child(format!("{} failed", failure.phase))
            .child(failure.summary.clone())
            .child(failure.detail.clone()),
        UiState::Booting(run) => div()
            .v_flex()
            .child("Booting")
            .child(format!("Status: {}", run.status)),
        UiState::Stopping(run) => div()
            .v_flex()
            .child("Stopping")
            .child(format!("Status: {}", run.status)),
        UiState::Running(run) => div()
            .v_flex()
            .child(format!("Running: {}", run.name))
            .child(format!("Status: {}", run.status))
            .child(format!("Elapsed: {}", run.elapsed)),
    }
}

fn publisher_review_view(
    review: &rootglass_app::UntrustedPublisherReview,
    trust_policy: &rootglass_app::TrustPolicyStatus,
    cx: &mut Context<RootglassWindow>,
) -> impl IntoElement {
    let archive_name = review
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Selected archive");
    let mut trust_guidance = div()
        .v_flex()
        .gap_2()
        .p_4()
        .bg(cx.theme().group_box)
        .child(eyebrow("LOCAL TRUST CONFIGURATION", cx))
        .child(muted_text(
            trust_policy
                .config_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "No trust configuration is available.".to_string()),
            cx,
        ))
        .child(muted_text(
            "Add and verify this signing key through the documented trust setup, then restart Rootglass and open the archive again.",
            cx,
        ));
    if let Some(error) = &trust_policy.error {
        trust_guidance =
            trust_guidance.child(div().text_color(cx.theme().danger).child(error.clone()));
    }

    div()
        .v_flex()
        .flex_1()
        .gap_3()
        .p_5()
        .bg(cx.theme().background)
        .overflow_scrollbar()
        .child(eyebrow("PUBLISHER REVIEW", cx))
        .child(page_title("Review this glass", cx))
        .child(muted_text(
            "Check the publisher claims and signing key before deciding whether to trust this bundle.",
            cx,
        ))
        .child(
            div()
                .v_flex()
                .gap_2()
                .p_4()
                .bg(cx.theme().group_box)
                .child(eyebrow("GLASS BUNDLE", cx))
                .child(format!("{} · {}", review.name, review.version))
                .child(muted_text(format!("File: {archive_name}"), cx))
                .child(muted_text(format!("Identifier: {}", review.identifier), cx))
                .child(muted_text(format!("Archive path: {}", review.path.display()), cx))
                .child(muted_text(
                    review
                        .description
                        .clone()
                        .unwrap_or_else(|| "No description provided.".to_string()),
                    cx,
                )),
        )
        .child(
            div()
                .v_flex()
                .gap_2()
                .p_4()
                .bg(cx.theme().group_box)
                .child(eyebrow("SIGNATURE AND PUBLISHER", cx))
                .child("Signature not verified · signing key is not trusted")
                .child(format!("Declared publisher · unverified: {}", review.publisher_name))
                .child(format!(
                    "Declared identity · unverified: {}",
                    review.publisher_identity
                ))
                .child(format!("Signing key ID · unverified: {}", review.key_id)),
        )
        .child(
            div()
                .v_flex()
                .gap_2()
                .p_4()
                .bg(cx.theme().danger)
                .text_color(cx.theme().danger_foreground)
                .child(eyebrow("THIS PUBLISHER IS NOT TRUSTED", cx))
                .child("Rootglass does not have this key's public key, so it cannot verify the bundle signature.")
                .child("Publisher name and identity come from the unverified archive. Confirm the key ID through a trusted channel before changing local trust."),
        )
        .child(trust_guidance)
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(
                    Button::new("cancel-publisher-review")
                        .label("CANCEL")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_publisher_review(cx)
                        })),
                )
                .child(
                    Button::new("review-publisher")
                        .primary()
                        .label("REVIEW PUBLISHER")
                        .disabled(true),
                ),
        )
        .child(muted_text(
            "Import and boot are blocked until this key is trusted. Restart Rootglass after updating trust, then open this archive again.",
            cx,
        ))
        .child(
            status_view(&UiState::PublisherReview(review.clone()))
                .p_2()
                .bg(cx.theme().sidebar),
        )
}

fn status_view(state: &UiState) -> Div {
    match state {
        UiState::Empty => div().child("Status: Stopped · Console is inactive."),
        UiState::Opening(_) => div().child("Status: Opening bundle in the background."),
        UiState::Loading => div().child("Status: Loading · Console is inactive."),
        UiState::Importing(_) => div().child("Status: Verifying archive · Console is inactive."),
        UiState::Validating => div().child("Status: Validating manifest fields."),
        UiState::ManifestValidated(_) => div().child("Status: Manifest valid · Not verified."),
        UiState::Planning => div().child("Status: Building verified launch plan."),
        UiState::Planned(plan) if plan.preview.preflight.blockers.is_empty() => {
            div().child("Status: Launch plan ready.")
        }
        UiState::Planned(_) => div().child("Status: Launch plan blocked by preflight."),
        UiState::Removing(_) => div().child("Status: Removing installed release."),
        UiState::Imported(_) => div().child("Status: Installed · Stopped."),
        UiState::Ready(_) => div().child("Status: Ready · Stopped."),
        UiState::PublisherReview(_) => {
            div().child("Status: Publisher not trusted · Import and boot are blocked.")
        }
        UiState::Blocked(_) => div().child("Status: Blocked · Stopped."),
        UiState::Failed(error) => div().child(format!("Status: Failed · {}", error.summary)),
        UiState::Booting(run) => div().child(format!("Status: {}", run.status)),
        UiState::Stopping(run) => div().child(format!("Status: {}", run.status)),
        UiState::Running(run) => div().child(format!("Status: {}", run.status)),
    }
}

fn manifest_validation_view(report: &ManifestValidationReport) -> Div {
    let mut view = div()
        .v_flex()
        .child("Manifest valid · Signature and publisher trust are not checked.")
        .child(format!("Name: {}", report.name))
        .child(format!("Identifier: {}", report.identifier))
        .child(format!("Version: {}", report.version))
        .child(format!(
            "Declared publisher: {}",
            report.declared_publisher_name
        ))
        .child(format!(
            "Declared identity: {}",
            report.declared_publisher_identity
        ))
        .child(format!(
            "Runtime: {} / {} · {} MiB · {} CPU(s)",
            report.architecture, report.profile, report.memory_mb, report.cpus
        ))
        .child(format!("Rootfs: {}", report.rootfs.display()))
        .child(format!("Network: {}", report.network_mode))
        .child(format!("Path: {}", report.path.display()));
    if let Some(description) = &report.description {
        view = view.child(format!("Description: {description}"));
    }
    view
}

fn preview_details_view(title: &str, glass: &GlassPreview, command: Option<&str>) -> Div {
    let signature = match &glass.signature {
        SignatureStatus::Verified(publisher) => format!("Verified: {publisher}"),
        SignatureStatus::Untrusted(publisher) => format!("Untrusted: {publisher}"),
        SignatureStatus::Invalid(reason) => format!("Invalid: {reason}"),
        SignatureStatus::Tampered(reason) => format!("Tampered: {reason}"),
    };
    let mut view = div()
        .v_flex()
        .child(title.to_string())
        .child(format!("{}  {}", glass.name, glass.version))
        .child(format!("Identifier: {}", glass.identifier))
        .child(glass.description.clone())
        .child(format!("Publisher: {}", glass.publisher))
        .child(format!("Signature: {signature}"))
        .child(format!("Path: {}", glass.path.display()))
        .child(format!(
            "Guest: {} / {} · {} MiB · {} CPU(s)",
            glass.preflight.architecture,
            glass.preflight.profile,
            glass.preflight.memory_mb,
            glass.preflight.cpus
        ))
        .child(format!("QEMU: {}", glass.preflight.qemu))
        .child(format!("Acceleration: {}", glass.preflight.acceleration));
    for asset in &glass.preflight.runtime_assets {
        view = view.child(format!("Runtime asset: {asset}"));
    }
    for blocker in &glass.preflight.blockers {
        view = view.child(format!("Blocked: {blocker}"));
    }
    if let Some(command) = command {
        view = view
            .child("Resolved launch command")
            .child(command.to_string());
    }
    view
}

fn removal_details_view(details: &RemovalDetails) -> Div {
    div()
        .v_flex()
        .child("Removing installed release…")
        .child(format!("Release: {} {}", details.name, details.version))
        .child(format!("SHA-256: {}", details.content_sha256))
        .child(format!("Path: {}", details.path.display()))
        .child("Immutable files remain until library garbage collection.")
}

pub fn run_gui(initial_glass: Option<PathBuf>) {
    let open_url_target: Rc<RefCell<Option<(AsyncApp, Entity<RootglassWindow>)>>> =
        Rc::new(RefCell::new(None));
    let pending_open_urls: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let application = gpui_kit::application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .with_assets(gpui_kit::assets::Assets);
    {
        let open_url_target = open_url_target.clone();
        let pending_open_urls = pending_open_urls.clone();
        application.on_open_urls(move |urls| {
            desktop_smoke_event("ROOTGLASS_DESKTOP_OPEN_URLS_RECEIVED");
            let target = open_url_target.borrow().clone();
            if let Some((app, view)) = target {
                open_urls_in_view(&app, &view, urls);
            } else {
                pending_open_urls.borrow_mut().extend(urls);
            }
        });
    }
    application.run(move |cx| {
        gpui_kit::init(cx);
        install_rootglass_theme(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-o", OpenBundle, Some(ROOTGLASS_CONTEXT)),
            KeyBinding::new("cmd-.", StopGlass, Some(ROOTGLASS_CONTEXT)),
            KeyBinding::new("cmd-q", Quit, Some(ROOTGLASS_CONTEXT)),
            KeyBinding::new("cmd-w", Quit, Some(ROOTGLASS_CONTEXT)),
            KeyBinding::new("alt-f4", Quit, Some(ROOTGLASS_CONTEXT)),
        ]);
        let initial_glass = initial_glass.clone();
        let (_, view) = gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            let initial_glass = initial_glass.clone();
            cx.new(move |cx| {
                let mut view = RootglassWindow::new(window, cx);
                if let Some(path) = initial_glass {
                    view.open_path(path, cx);
                }
                view
            })
        })
        .expect("failed to open Rootglass window");
        let app = cx.to_async();
        *open_url_target.borrow_mut() = Some((app.clone(), view.clone()));
        let pending = std::mem::take(&mut *pending_open_urls.borrow_mut());
        open_urls_in_view(&app, &view, pending);
    });

    #[cfg(feature = "embedded-terminal")]
    embedded_terminal::wait_for_cleanup();
}
