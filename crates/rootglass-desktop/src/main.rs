use gpui_kit::component::button::*;
use gpui_kit::component::*;
use gpui_kit::*;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
enum UiState {
    Empty,
    Loading,
    Ready(GlassPreview),
    Blocked(BlockedDetails),
    Failed(FailureDetails),
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
    cpus: u8,
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

#[derive(Default)]
struct MockInspectionService;

impl MockInspectionService {
    fn inspect(&self, path: &Path) -> UiState {
        if !path.exists() {
            return UiState::Failed(FailureDetails {
                path: Some(path.to_path_buf()),
                phase: "Open".to_string(),
                summary: "The selected path does not exist.".to_string(),
                detail: path.display().to_string(),
            });
        }

        UiState::Blocked(BlockedDetails {
            path: Some(path.to_path_buf()),
            reason: "The signature and preflight services are not connected yet.".to_string(),
            preflight: PreflightDetails {
                architecture: "Unknown".to_string(),
                profile: "Unknown".to_string(),
                memory_mb: 0,
                cpus: 0,
                qemu: "Not checked".to_string(),
                acceleration: "Not checked".to_string(),
                runtime_assets: vec!["Not checked".to_string()],
                blockers: vec![
                    "Signature verification is unavailable.".to_string(),
                    "Host and runtime preflight is unavailable.".to_string(),
                ],
            },
        })
    }
}

struct UiModel {
    state: UiState,
    inspector: MockInspectionService,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            state: UiState::Empty,
            inspector: MockInspectionService,
        }
    }
}

impl UiModel {
    // This is the selection seam for the future native picker. The mock service
    // fails closed and never creates a pretend installed glass.
    #[allow(dead_code)]
    fn select_bundle(&mut self, path: PathBuf) {
        self.state = UiState::Loading;
        self.state = self.inspector.inspect(&path);
    }
}

struct RootglassWindow {
    model: UiModel,
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
    }
}

impl RootglassWindow {
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
        UiState::Running(run) => div().child(format!("Status: {}", run.status)),
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| RootglassWindow {
                    model: UiModel::default(),
                })
            })
            .expect("failed to open Rootglass window");
        });
}
