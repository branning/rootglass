use gpui_kit::component::button::*;
use gpui_kit::component::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::*;
use rootglass_app::{AppService, RunId, RunOptions, RuntimeEvent};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
enum UiState {
    Empty,
    Loading,
    Ready(GlassPreview),
    Blocked(BlockedDetails),
    Failed(FailureDetails),
    Booting(RunDetails),
    Running(RunDetails),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GlassPreview {
    path: PathBuf,
    name: String,
    identifier: String,
    version: String,
    description: String,
    publisher: String,
    signature: SignatureStatus,
    preflight: PreflightDetails,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SignatureStatus {
    Verified(String),
    Untrusted(String),
    Invalid(String),
    Tampered(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreflightDetails {
    architecture: String,
    profile: String,
    memory_mb: u32,
    cpus: u32,
    qemu: String,
    acceleration: String,
    runtime_assets: Vec<String>,
    blockers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BlockedDetails {
    path: Option<PathBuf>,
    reason: String,
    preflight: PreflightDetails,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FailureDetails {
    path: Option<PathBuf>,
    phase: String,
    summary: String,
    detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunDetails {
    name: String,
    status: String,
    elapsed: String,
}

struct UiModel {
    state: UiState,
    service: AppService,
    selected_path: Option<PathBuf>,
    run_id: Option<RunId>,
    console: String,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            state: UiState::Empty,
            service: AppService::new(),
            selected_path: None,
            run_id: None,
            console: String::new(),
        }
    }
}

impl UiModel {
    fn select_bundle(&mut self, path: PathBuf) {
        self.selected_path = Some(path.clone());
        self.state = UiState::Loading;
        match self.service.inspect(&path) {
            Ok(summary) => match self.service.preflight(&path) {
                Ok(report) => {
                    self.state = UiState::Ready(GlassPreview {
                        path,
                        name: summary.name,
                        identifier: summary.identifier,
                        version: summary.version,
                        description: summary.description,
                        publisher: summary.publisher_name,
                        signature: SignatureStatus::Verified(summary.publisher_identity),
                        preflight: preflight_details(report),
                    });
                }
                Err(error) => {
                    self.state = UiState::Blocked(BlockedDetails {
                        path: Some(path),
                        reason: error.summary,
                        preflight: PreflightDetails {
                            blockers: vec![error.detail],
                            ..empty_preflight()
                        },
                    });
                }
            },
            Err(error) => {
                self.state = UiState::Failed(FailureDetails {
                    path: Some(path),
                    phase: format!("{:?}", error.phase),
                    summary: error.summary,
                    detail: error.detail,
                });
            }
        }
    }

    fn start(&mut self) {
        let Some(path) = self.selected_path.clone() else { return };
        match self.service.start(&path, RunOptions::default()) {
            Ok(run_id) => {
                self.run_id = Some(run_id);
                self.state = UiState::Booting(RunDetails {
                    name: "Selected glass".to_string(),
                    status: "Booting; waiting for guest login prompt".to_string(),
                    elapsed: String::new(),
                });
            }
            Err(error) => {
                self.state = UiState::Failed(FailureDetails {
                    path: Some(path),
                    phase: format!("{:?}", error.phase),
                    summary: error.summary,
                    detail: error.detail,
                });
            }
        }
    }

    fn poll(&mut self) {
        let Some(run) = self.run_id.clone() else { return };
        for _ in 0..32 {
            match self.service.poll_event(Duration::ZERO) {
                Ok(Some(RuntimeEvent::SerialChunk { run: event_run, bytes, .. }))
                    if event_run == run =>
                {
                    self.console.push_str(&String::from_utf8_lossy(&bytes));
                    if self.console.len() > 80_000 {
                        let mut drain = self.console.len() - 80_000;
                        while !self.console.is_char_boundary(drain) {
                            drain += 1;
                        }
                        self.console.drain(..drain);
                    }
                }
                Ok(Some(RuntimeEvent::GuestReady { run: event_run, .. }))
                    if event_run == run =>
                {
                    self.state = UiState::Running(RunDetails {
                        name: "Selected glass".to_string(),
                        status: "Running".to_string(),
                        elapsed: String::new(),
                    });
                }
                Ok(Some(RuntimeEvent::Exited { run: event_run, exit_code, .. }))
                    if event_run == run =>
                {
                    self.run_id = None;
                    self.state = UiState::Failed(FailureDetails {
                        path: self.selected_path.clone(),
                        phase: "Guest".to_string(),
                        summary: "QEMU exited unexpectedly.".to_string(),
                        detail: format!("Exit code: {exit_code:?}"),
                    });
                }
                Ok(Some(RuntimeEvent::Failed {
                    run: event_run,
                    phase,
                    summary,
                    detail,
                })) if event_run == run => {
                    self.run_id = None;
                    self.state = UiState::Failed(FailureDetails {
                        path: self.selected_path.clone(),
                        phase: format!("{phase:?}"),
                        summary,
                        detail,
                    });
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    self.run_id = None;
                    self.state = UiState::Failed(FailureDetails {
                        path: self.selected_path.clone(),
                        phase: format!("{:?}", error.phase),
                        summary: error.summary,
                        detail: error.detail,
                    });
                    break;
                }
            }
        }
    }

    fn stop(&mut self) {
        if let Some(run) = self.run_id.clone() {
            match self.service.stop(&run) {
                Ok(()) => {
                    self.run_id = None;
                    if let Some(path) = self.selected_path.clone() {
                        self.select_bundle(path);
                    } else {
                        self.state = UiState::Empty;
                    }
                }
                Err(error) => {
                    self.state = UiState::Failed(FailureDetails {
                        path: self.selected_path.clone(),
                        phase: format!("{:?}", error.phase),
                        summary: error.summary,
                        detail: error.detail,
                    });
                }
            }
        }
    }

    fn send_input(&mut self, text: &str) {
        let Some(run) = self.run_id.as_ref() else {
            return;
        };
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\n');
        if let Err(error) = self.service.write_serial(run, &bytes) {
            self.state = UiState::Failed(FailureDetails {
                path: self.selected_path.clone(),
                phase: format!("{:?}", error.phase),
                summary: error.summary,
                detail: error.detail,
            });
        }
    }
}

fn empty_preflight() -> PreflightDetails {
    PreflightDetails {
        architecture: "Unknown".to_string(),
        profile: "Unknown".to_string(),
        memory_mb: 0,
        cpus: 0,
        qemu: "Not checked".to_string(),
        acceleration: "Not checked".to_string(),
        runtime_assets: vec![],
        blockers: vec![],
    }
}

fn preflight_details(report: rootglass_app::PreflightReport) -> PreflightDetails {
    PreflightDetails {
        architecture: report.summary.architecture,
        profile: report.summary.profile,
        memory_mb: report.summary.memory_mb,
        cpus: report.summary.cpus,
        qemu: "Resolved by the launch plan; executable check pending".to_string(),
        acceleration: "Selected by host preflight; capability detail pending".to_string(),
        runtime_assets: report.readiness,
        blockers: vec![],
    }
}

struct RootglassWindow {
    model: UiModel,
    input: Entity<InputState>,
    _poll_task: Task<()>,
    _input_subscription: Subscription,
}

impl Render for RootglassWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .size_full()
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
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_bundle(window, cx)
                            })),
                    )
                    .child(
                        Button::new("boot-glass").label("Boot").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.model.start();
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        Button::new("refresh-console").label("Refresh console").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.model.poll();
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        Button::new("stop-glass").label("Stop").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.model.stop();
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .size_full()
                    .gap_2()
                    .child(library_view(&self.model.state))
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
                    .child(Input::new(&self.input))
                    .child("Press Enter to send"),
            )
    }
}

impl RootglassWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Type to guest console…"));
        let _input_subscription = cx.subscribe_in(
            &input,
            window,
            |view, state, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = state.read(cx).value().to_string();
                    view.model.send_input(&text);
                    state.update(cx, |state, cx| state.set_value("", window, cx));
                    cx.notify();
                }
            },
        );
        let _poll_task = cx.spawn(async move |view, cx| loop {
            Timer::after(Duration::from_millis(100)).await;
            if view
                .update(cx, |view, cx| {
                    view.model.poll();
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        });
        Self {
            model: UiModel::default(),
            input,
            _poll_task,
            _input_subscription,
        }
    }

    fn open_bundle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Select a Rootglass .glass file or directory".into()),
        });
        let view = cx.entity();

        cx.spawn_in(window, async move |_, window| {
            let selected = selection
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .and_then(|paths| paths.into_iter().next());

            window
                .update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if let Some(path) = selected {
                            this.model.select_bundle(path);
                        }
                        cx.notify();
                    })
                })
                .ok()
        })
        .detach();
    }
}

fn library_view(state: &UiState) -> Div {
    let mut view = div()
        .v_flex()
        .child("Library")
        .child("No glasses are installed.")
        .child("Open an existing .glass bundle to inspect it.");
    if let UiState::Ready(glass) = state {
        view = view.child(format!("Selected: {} {}", glass.name, glass.version));
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
                .child(format!(
                    "Acceleration: {}",
                    glass.preflight.acceleration
                ));
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
            view = view.child(format!(
                "Acceleration: {}",
                blocked.preflight.acceleration
            ));
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
        UiState::Ready(_) => div().child("Status: Ready · Stopped."),
        UiState::Blocked(_) => div().child("Status: Blocked · Stopped."),
        UiState::Failed(error) => div().child(format!("Status: Failed · {}", error.summary)),
        UiState::Booting(run) => div().child(format!("Status: {}", run.status)),
        UiState::Running(run) => div().child(format!("Status: {}", run.status)),
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| RootglassWindow::new(window, cx))
            })
            .expect("failed to open Rootglass window");
        });
}
