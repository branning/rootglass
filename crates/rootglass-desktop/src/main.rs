use gpui_kit::actions;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::*;
use rootglass_app::registry::InstalledGlass;
use rootglass_desktop::ui_model::{
    lock_service, SignatureStatus, StartEffect, StopRequest, UiModel, UiState,
};
use std::path::PathBuf;
use std::time::Duration;

actions!(rootglass, [OpenBundle, StopGlass]);
const ROOTGLASS_CONTEXT: &str = "RootglassWindow";

struct RootglassWindow {
    model: UiModel,
    input: Entity<InputState>,
    _library_task: Task<()>,
    _poll_task: Task<()>,
    _input_subscription: Subscription,
}

impl Render for RootglassWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = self.model.actions();
        div()
            .v_flex()
            .size_full()
            .key_context(ROOTGLASS_CONTEXT)
            .on_action(cx.listener(Self::on_open_bundle))
            .on_action(cx.listener(Self::on_stop_shortcut))
            .child(
                div()
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
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .size_full()
                    .gap_2()
                    .child(library_view(
                        &self.model.state,
                        &self.model.installed_glasses,
                        self.model.library_error.as_deref(),
                        actions.open,
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
        Self {
            model,
            input,
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
        let background_executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            let selected = selection
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .and_then(|paths| paths.into_iter().next());
            let Some(path) = selected else { return };
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
                Ok(None) => return,
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
                let _ = view.update(cx, |view, cx| {
                    view.model.finish_import(&ticket, result);
                    cx.notify();
                });
            } else {
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
                let _ = view.update(cx, |view, cx| {
                    view.model.finish_inspection(&ticket, result);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn select_bundle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.model.actions().open {
            return;
        }
        let Some(ticket) = self.model.begin_inspection(path.clone()) else {
            return;
        };
        let service = self.model.shared_service();
        let background_executor = cx.background_executor().clone();
        cx.notify();
        cx.spawn(async move |view, cx| {
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
            let _ = view.update(cx, |view, cx| {
                view.model.finish_inspection(&ticket, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn start_guest(&mut self, cx: &mut Context<Self>) {
        let Some((ticket, path)) = self.model.begin_start() else {
            return;
        };
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
            let effect = view.update(cx, |view, cx| {
                let effect = view.model.finish_start(&ticket, result);
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
            Some(StopRequest::CancelStartingOperation) => cx.notify(),
            Some(StopRequest::StopRun { ticket, run }) => {
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
            let effect = view.update(cx, |view, cx| {
                let effect = view.model.finish_restart(&ticket, result);
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

fn library_view(
    state: &UiState,
    installed_glasses: &[InstalledGlass],
    library_error: Option<&str>,
    can_select: bool,
    cx: &mut Context<RootglassWindow>,
) -> Div {
    let mut view = div().v_flex().child("Library");
    if installed_glasses.is_empty() {
        view = view
            .child("No glasses are installed.")
            .child("Open an existing .glass bundle to inspect it.");
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
        UiState::Loading => div()
            .v_flex()
            .size_full()
            .child("Inspecting bundle…")
            .child("Verification and preflight are pending."),
        UiState::Importing(_) => div()
            .v_flex()
            .size_full()
            .child("Verifying archive and importing package…"),
        UiState::Imported(glass) => div()
            .v_flex()
            .size_full()
            .child(format!("{} {} installed", glass.name, glass.version))
            .child(format!("Trusted publisher: {}", glass.publisher))
            .child("The archive was verified and copied into the local package store."),
        UiState::Ready(glass) => {
            let signature = match &glass.signature {
                SignatureStatus::Verified(publisher) => format!("Verified: {publisher}"),
                SignatureStatus::Untrusted(publisher) => format!("Untrusted: {publisher}"),
                SignatureStatus::Invalid(reason) => format!("Invalid: {reason}"),
                SignatureStatus::Tampered(reason) => format!("Tampered: {reason}"),
            };
            let mut view = div()
                .v_flex()
                .size_full()
                .child(format!("{}  {}", glass.name, glass.version))
                .child(format!("Identifier: {}", glass.identifier))
                .child(glass.description.clone())
                .child(format!("Publisher: {}", glass.publisher))
                .child(format!("Signature: {signature}"))
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
            view
        }
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
        UiState::Loading => div().child("Status: Loading · Console is inactive."),
        UiState::Importing(_) => div().child("Status: Verifying archive · Console is inactive."),
        UiState::Imported(_) => div().child("Status: Installed · Stopped."),
        UiState::Ready(_) => div().child("Status: Ready · Stopped."),
        UiState::Blocked(_) => div().child("Status: Blocked · Stopped."),
        UiState::Failed(error) => div().child(format!("Status: Failed · {}", error.summary)),
        UiState::Booting(run) => div().child(format!("Status: {}", run.status)),
        UiState::Stopping(run) => div().child(format!("Status: {}", run.status)),
        UiState::Running(run) => div().child(format!("Status: {}", run.status)),
    }
}

fn main() {
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
}
