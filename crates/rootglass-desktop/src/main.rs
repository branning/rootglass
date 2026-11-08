#[cfg(feature = "embedded-terminal")]
mod embedded_terminal;

use gpui_kit::actions;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::*;
use rootglass_app::registry::InstalledGlass;
use rootglass_app::ManifestValidationReport;
use rootglass_app::RuntimeEvent;
use rootglass_desktop::ui_model::{
    lock_service, GlassPreview, RemovalDetails, SignatureStatus, StartEffect, StopRequest, UiModel,
    UiState,
};
use std::path::PathBuf;
use std::time::Duration;

actions!(rootglass, [OpenBundle, StopGlass]);
const ROOTGLASS_CONTEXT: &str = "RootglassWindow";

struct RootglassWindow {
    model: UiModel,
    input: Entity<InputState>,
    pending_file_drop: Option<PathBuf>,
    file_drop_active: bool,
    #[cfg(feature = "embedded-terminal")]
    embedded_tui: Option<Entity<embedded_terminal::EmbeddedTerminal>>,
    _library_task: Task<()>,
    _poll_task: Task<()>,
    _input_subscription: Subscription,
}

impl Render for RootglassWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = self.model.actions();
        let mut toolbar = div()
            .h_flex()
            .items_center()
            .gap_2()
            .child("Rootglass")
            .child(
                Button::new("open-glass")
                    .primary()
                    .label("Open a .glass bundle…")
                    .disabled(!actions.open)
                    .on_click(cx.listener(|this, _, _, cx| this.open_bundle(cx))),
            )
            .child(
                Button::new("boot-glass")
                    .label("Boot")
                    .disabled(!actions.boot)
                    .on_click(cx.listener(|this, _, _, cx| this.start_guest(cx))),
            )
            .child(
                Button::new("restart-glass")
                    .label("Restart")
                    .disabled(!actions.restart)
                    .on_click(cx.listener(|this, _, _, cx| this.restart_guest(cx))),
            )
            .child(
                Button::new("stop-glass")
                    .label("Stop")
                    .disabled(!actions.stop)
                    .on_click(cx.listener(|this, _, _, cx| this.stop_guest(cx))),
            );

        #[cfg(feature = "embedded-terminal")]
        {
            let terminal_open = self.embedded_tui.is_some();
            toolbar = toolbar.child(
                Button::new("embedded-tui")
                    .label(if terminal_open {
                        "Close TUI"
                    } else {
                        "Open TUI"
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.embedded_tui.is_some() {
                            this.close_embedded_tui(cx);
                        } else {
                            this.open_embedded_tui(window, cx);
                        }
                    })),
            );
        }

        let root = div()
            .v_flex()
            .size_full()
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
            .child(toolbar);

        #[cfg(feature = "embedded-terminal")]
        if let Some(terminal) = &self.embedded_tui {
            return root.child(terminal.clone());
        }

        root.child(
            div()
                .h_flex()
                .size_full()
                .gap_2()
                .child(library_view(
                    &self.model.state,
                    &self.model.installed_glasses,
                    self.model.library_error.as_deref(),
                    actions.open,
                    self.file_drop_active,
                    cx,
                ))
                .child(details_view(&self.model.state)),
        )
        .child(status_view(&self.model.state))
        .child(
            div()
                .v_flex()
                .child("Serial console")
                .child(self.model.console.clone()),
        )
        .child(
            div()
                .h_flex()
                .child(Input::new(&self.input).disabled(!actions.console_input))
                .child("Press Enter to send"),
        )
    }
}

impl RootglassWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Type to guest console…"));
        let model = UiModel::default();
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
                cx.notify();
            });
        });

        let service = model.shared_service();
        let background_executor = cx.background_executor().clone();
        let _poll_task = cx.spawn(async move |view, cx| loop {
            background_executor.timer(Duration::from_millis(100)).await;
            let service_for_poll = service.clone();
            let result = background_executor
                .spawn(async move {
                    lock_service(&service_for_poll).and_then(|mut service| service.poll_events())
                })
                .await;
            if let Ok(events) = &result {
                for event in events {
                    match event {
                        RuntimeEvent::GuestReady { .. } => {
                            desktop_smoke_event("ROOTGLASS_DESKTOP_GUEST_READY");
                        }
                        RuntimeEvent::SerialChunk { bytes, .. } => {
                            if let Some(sentinel) =
                                std::env::var_os("ROOTGLASS_DESKTOP_SMOKE_SERIAL_SENTINEL")
                            {
                                let sentinel = sentinel.to_string_lossy();
                                if !sentinel.is_empty()
                                    && bytes
                                        .windows(sentinel.len())
                                        .any(|window| window == sentinel.as_bytes())
                                {
                                    desktop_smoke_event("ROOTGLASS_DESKTOP_SERIAL_ECHO");
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if view
                .update(cx, |view, cx| {
                    view.model.apply_poll_result(result);
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
        if !self.model.actions().open {
            return;
        }
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Select a Rootglass .glass archive or directory".into()),
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
        if !self.model.actions().open {
            desktop_smoke_event("ROOTGLASS_DESKTOP_OPEN_PATH_REJECTED");
            return;
        }
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
                if !view.model.actions().open {
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
                        lock_service(&service_for_import)
                            .and_then(|mut service| service.import_and_inspect(&source))
                    })
                    .await;
                let state_path = result.as_ref().ok().and_then(|completion| {
                    matches!(
                        &completion.inspection,
                        rootglass_desktop::ui_model::InspectionOutcome::Ready(_)
                    )
                    .then(|| completion.installed.package_path.clone())
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
                    if view.model.finish_import(&ticket, result) {
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
                            Err(error) => rootglass_desktop::ui_model::InspectionOutcome::Failed(
                                rootglass_desktop::ui_model::FailureDetails {
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
                let state_result = match &result {
                    rootglass_desktop::ui_model::InspectionOutcome::Ready(preview) => {
                        let path = preview.path.clone();
                        let service_for_state = service.clone();
                        Some(
                            background_executor
                                .spawn(async move {
                                    lock_service(&service_for_state)
                                        .and_then(|mut service| service.list_states(&path))
                                })
                                .await,
                        )
                    }
                    _ => None,
                };
                let _ = view.update(cx, |view, cx| {
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
            }
        })
        .detach();
    }

    fn log_desktop_smoke_inspection(&self) {
        if std::env::var_os("ROOTGLASS_DESKTOP_SMOKE").is_none() {
            return;
        }
        match &self.model.state {
            UiState::Ready(preview) => match &preview.signature {
                SignatureStatus::Verified(_) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_VERIFIED");
                    desktop_smoke_event(if preview.preflight.blockers.is_empty() {
                        "ROOTGLASS_DESKTOP_PREFLIGHT_READY"
                    } else {
                        "ROOTGLASS_DESKTOP_PREFLIGHT_BLOCKED"
                    });
                }
                SignatureStatus::Untrusted(_) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_UNTRUSTED")
                }
                SignatureStatus::Tampered(_) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_TAMPERED")
                }
                SignatureStatus::Invalid(_) => {
                    desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_INVALID")
                }
            },
            UiState::Blocked(_) => desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_BLOCKED"),
            UiState::Failed(details) if details.detail.contains("unknown signing key") => {
                desktop_smoke_event("ROOTGLASS_DESKTOP_INSPECT_UNTRUSTED");
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

    fn start_guest(&mut self, cx: &mut Context<Self>) {
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
    }
}

fn library_view(
    state: &UiState,
    installed_glasses: &[InstalledGlass],
    library_error: Option<&str>,
    can_select: bool,
    file_drop_active: bool,
    cx: &mut Context<RootglassWindow>,
) -> Div {
    let mut view = div().v_flex().child("Library");
    if installed_glasses.is_empty() {
        view = view
            .child("No glasses are installed.")
            .child("Open an existing .glass bundle to inspect it.")
            .child(if file_drop_active {
                "Release the .glass bundle or folder to inspect it."
            } else {
                "Drop a .glass bundle or folder here to inspect it."
            });
    } else {
        for glass in installed_glasses {
            let path = glass.package_path.clone();
            view = view
                .child(
                    Button::new(format!("installed-{}", glass.content_sha256))
                        .label(format!("{} {}", glass.name, glass.version))
                        .disabled(!can_select)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_bundle(path.clone(), cx);
                        })),
                )
                .child(format!("Publisher: {}", glass.publisher_name))
                .child(format!("Installed at: {}", glass.package_path.display()));
        }
    }
    if let Some(error) = library_error {
        view = view.child(format!(
            "The installed library could not be loaded: {error}"
        ));
    }
    if let UiState::Ready(glass) = state {
        view = view.child(format!("Selected: {} {}", glass.name, glass.version));
    }
    match state {
        UiState::Opening(path) => {
            view = view.child(format!("Opening {} in the background…", path.display()));
        }
        UiState::Importing(path) => {
            view = view.child(format!("Verifying and importing {}…", path.display()));
        }
        UiState::Imported(glass) => {
            view = view
                .child(format!("Installed: {} {}", glass.name, glass.version))
                .child(format!("Publisher: {}", glass.publisher))
                .child(format!("Content digest: {}", glass.digest))
                .child(format!("Installed at: {}", glass.path.display()));
        }
        _ => {}
    }
    view
}

fn details_view(state: &UiState) -> Div {
    match state {
        UiState::Empty => div()
            .v_flex()
            .size_full()
            .child("No bundle selected")
            .child("Publisher, verification, and preflight details appear after inspection.")
            .child("Boot is unavailable until a bundle passes verification and preflight."),
        UiState::Opening(path) => div()
            .v_flex()
            .size_full()
            .child("Opening bundle in the background…")
            .child(path.display().to_string()),
        UiState::Loading => div()
            .v_flex()
            .size_full()
            .child("Inspecting bundle…")
            .child("Verification and preflight are pending."),
        UiState::Importing(_) => div()
            .v_flex()
            .size_full()
            .child("Verifying archive and importing package…"),
        UiState::Validating => div()
            .v_flex()
            .size_full()
            .child("Checking manifest fields only…")
            .child("Signature and publisher trust are not checked by Validate."),
        UiState::ManifestValidated(report) => manifest_validation_view(report),
        UiState::Planning => div()
            .v_flex()
            .size_full()
            .child("Building verified launch plan…")
            .child("Checking signature, publisher trust, and runtime preflight."),
        UiState::Planned(plan) => {
            preview_details_view("Verified launch plan", &plan.preview, Some(&plan.command))
        }
        UiState::Removing(details) => removal_details_view(details),
        UiState::Imported(glass) => div()
            .v_flex()
            .size_full()
            .child(format!("{} {} installed", glass.name, glass.version))
            .child(format!("Trusted publisher: {}", glass.publisher))
            .child("The archive was verified and copied into the local package store."),
        UiState::Ready(glass) => preview_details_view("Ready", glass, None),
        UiState::Blocked(blocked) => {
            let mut view = div()
                .v_flex()
                .size_full()
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
            .size_full()
            .child(format!("{} failed", failure.phase))
            .child(failure.summary.clone())
            .child(failure.detail.clone()),
        UiState::Booting(run) => div()
            .v_flex()
            .size_full()
            .child("Booting")
            .child(format!("Status: {}", run.status)),
        UiState::Stopping(run) => div()
            .v_flex()
            .size_full()
            .child("Stopping")
            .child(format!("Status: {}", run.status)),
        UiState::Running(run) => div()
            .v_flex()
            .size_full()
            .child(format!("Running: {}", run.name))
            .child(format!("Status: {}", run.status))
            .child(format!("Elapsed: {}", run.elapsed)),
    }
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
        .size_full()
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
        .size_full()
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
        .size_full()
        .child("Removing installed release…")
        .child(format!("Release: {} {}", details.name, details.version))
        .child(format!("SHA-256: {}", details.content_sha256))
        .child(format!("Path: {}", details.path.display()))
        .child("Immutable files remain until library garbage collection.")
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--rootglass-tui-session") {
        if let Err(error) = rootglass_tui::run_terminal(None) {
            eprintln!("embedded Rootglass TUI session failed: {error}");
            std::process::exit(1);
        }
        return;
    }

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys([
                KeyBinding::new("cmd-o", OpenBundle, Some(ROOTGLASS_CONTEXT)),
                KeyBinding::new("cmd-.", StopGlass, Some(ROOTGLASS_CONTEXT)),
            ]);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| RootglassWindow::new(window, cx))
            })
            .expect("failed to open Rootglass window");
        });

    #[cfg(feature = "embedded-terminal")]
    embedded_terminal::wait_for_cleanup();
}
