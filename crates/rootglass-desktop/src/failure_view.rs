use crate::ui_model::{BlockedDetails, FailureDetails, GlassPreview, SignatureStatus, UiState};
use gpui_kit::base::StyledExt;
use gpui_kit::component::text::TextView;
use gpui_kit::component::*;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::*;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryFailure {
    Preflight(BlockedDetails),
    Launch(FailureDetails),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckTone {
    Passed,
    Blocked,
    Neutral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchCheck {
    name: &'static str,
    value: String,
    tone: CheckTone,
}

impl RecoveryFailure {
    pub(crate) fn from_ui_state(state: &UiState) -> Option<Self> {
        match state {
            UiState::Blocked(blocked) => Some(Self::Preflight(blocked.clone())),
            UiState::Failed(failure)
                if failure.path.is_some()
                    && matches!(
                        failure.phase.as_str(),
                        "Validating"
                            | "Verifying"
                            | "Ready"
                            | "Starting"
                            | "Booting"
                            | "Running"
                            | "Failed"
                            | "Guest"
                    ) =>
            {
                Some(Self::Launch(failure.clone()))
            }
            _ => None,
        }
    }

    pub(crate) fn retry_path(&self, selected_path: Option<&PathBuf>) -> Option<PathBuf> {
        let failure_path = match self {
            Self::Preflight(blocked) => blocked.path.as_ref(),
            Self::Launch(failure) => failure.path.as_ref(),
        };
        failure_path.cloned().or_else(|| selected_path.cloned())
    }

    pub(crate) fn summary(&self) -> String {
        match self {
            Self::Preflight(blocked) => blocked
                .preflight
                .blockers
                .first()
                .map(|blocker| one_line(blocker, 160))
                .filter(|blocker| !blocker.is_empty())
                .unwrap_or_else(|| blocked.reason.clone()),
            Self::Launch(failure) => failure.summary.clone(),
        }
    }

    pub(crate) fn title(&self, glass_name: &str, guest_was_ready: bool) -> String {
        if guest_was_ready {
            return format!("{glass_name} stopped unexpectedly");
        }
        match self {
            Self::Preflight(_) => format!("Couldn’t start {glass_name}"),
            Self::Launch(failure) if is_compatibility_failure(failure) => {
                format!("Couldn’t start {glass_name}")
            }
            Self::Launch(failure) if failure.phase == "Guest" => {
                format!("{glass_name} failed during startup")
            }
            Self::Launch(failure)
                if matches!(failure.phase.as_str(), "Validating" | "Verifying") =>
            {
                format!("Couldn’t verify {glass_name}")
            }
            Self::Launch(_) => format!("Couldn’t start {glass_name}"),
        }
    }

    pub(crate) fn explanation(&self, guest_was_ready: bool) -> &'static str {
        if guest_was_ready {
            "The guest reached its ready signal before this failure. Retry checks will verify the bundle again without resetting the selected saved state."
        } else {
            "Retry checks will verify the selected glass again without resetting or replacing its saved state."
        }
    }

    fn checks(&self, preview: Option<&GlassPreview>, guest_was_ready: bool) -> Vec<LaunchCheck> {
        match self {
            Self::Preflight(_blocked) => vec![
                check("Bundle format", "Passed", CheckTone::Passed),
                check("Publisher & signature", "Passed", CheckTone::Passed),
                check(
                    "Computer compatibility",
                    format!("Blocked · {}", self.summary()),
                    CheckTone::Blocked,
                ),
            ],
            Self::Launch(failure) => {
                let verified = preview.is_some_and(|preview| {
                    matches!(&preview.signature, SignatureStatus::Verified(_))
                });
                let compatibility_failure = is_compatibility_failure(failure);
                let bundle_status = if failure.phase == "Validating" && !compatibility_failure {
                    check(
                        "Bundle format",
                        format!("Failed · {}", one_line(&failure.summary, 160)),
                        CheckTone::Blocked,
                    )
                } else if verified {
                    check("Bundle format", "Passed", CheckTone::Passed)
                } else {
                    check("Bundle format", "Not confirmed", CheckTone::Neutral)
                };
                let signature_status = if failure.phase == "Verifying" {
                    check(
                        "Publisher & signature",
                        format!("Failed · {}", one_line(&failure.summary, 160)),
                        CheckTone::Blocked,
                    )
                } else if verified {
                    check("Publisher & signature", "Passed", CheckTone::Passed)
                } else {
                    check("Publisher & signature", "Not confirmed", CheckTone::Neutral)
                };
                let launch_check = if compatibility_failure {
                    check(
                        "Computer compatibility",
                        format!("Failed · {}", one_line(&failure.summary, 160)),
                        CheckTone::Blocked,
                    )
                } else if failure.phase == "Guest" && guest_was_ready {
                    check(
                        "Guest runtime",
                        format!("Failed · {}", one_line(&failure.summary, 160)),
                        CheckTone::Blocked,
                    )
                } else if failure.phase == "Guest" {
                    check(
                        "Guest readiness",
                        format!("Not reached · {}", one_line(&failure.summary, 150)),
                        CheckTone::Blocked,
                    )
                } else if preview.is_some() {
                    check(
                        "QEMU startup",
                        format!("Failed · {}", one_line(&failure.summary, 160)),
                        CheckTone::Blocked,
                    )
                } else {
                    check(
                        "Launch check",
                        format!("Failed · {}", one_line(&failure.summary, 160)),
                        CheckTone::Blocked,
                    )
                };
                vec![bundle_status, signature_status, launch_check]
            }
        }
    }

    pub(crate) fn diagnostics(&self) -> String {
        match self {
            Self::Preflight(blocked) => {
                let mut details = vec![
                    "Phase: Preflight".to_string(),
                    format!("Summary: {}", blocked.reason),
                    format!(
                        "Guest: {} / {}",
                        blocked.preflight.architecture, blocked.preflight.profile
                    ),
                    format!("Acceleration: {}", blocked.preflight.acceleration),
                ];
                if let Some(path) = &blocked.path {
                    details.push(format!("Bundle: {}", path.display()));
                }
                details.extend(
                    blocked
                        .preflight
                        .blockers
                        .iter()
                        .map(|detail| format!("Preflight detail: {detail}")),
                );
                details.join("\n")
            }
            Self::Launch(failure) => {
                let mut details = vec![
                    format!("Phase: {}", failure.phase),
                    format!("Summary: {}", failure.summary),
                ];
                if let Some(path) = &failure.path {
                    details.push(format!("Bundle: {}", path.display()));
                }
                details.push(String::new());
                details.push(failure.detail.clone());
                details.join("\n")
            }
        }
    }
}

pub(crate) fn content(
    failure: &RecoveryFailure,
    glass_name: &str,
    preview: Option<&GlassPreview>,
    guest_was_ready: bool,
    diagnostics_open: bool,
    cx: &App,
) -> impl IntoElement {
    let mut checks_panel = div()
        .v_flex()
        .gap_3()
        .p_5()
        .bg(cx.theme().group_box)
        .child(eyebrow("LAUNCH CHECKS", cx));
    for row in failure.checks(preview, guest_was_ready) {
        let color = match row.tone {
            CheckTone::Passed => cx.theme().foreground,
            CheckTone::Blocked => cx.theme().danger_foreground,
            CheckTone::Neutral => cx.theme().muted_foreground,
        };
        checks_panel = checks_panel.child(
            div()
                .h_flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .w(px(250.0))
                        .flex_shrink_0()
                        .child(row.name)
                        .text_color(cx.theme().secondary_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(row.value)
                        .text_color(color),
                ),
        );
    }
    let mut view = div()
        .v_flex()
        .flex_1()
        .h_full()
        .min_h(px(0.0))
        .min_w(px(0.0))
        .gap_6()
        .overflow_y_scrollbar()
        .p_8()
        .bg(cx.theme().background)
        .child(eyebrow(format!("LIBRARY / {}", glass_name.to_uppercase()), cx))
        .child(
            div()
                .child(failure.title(glass_name, guest_was_ready))
                .text_size(px(28.0))
                .text_color(cx.theme().foreground),
        )
        .child(
            div()
                .child(failure.explanation(guest_was_ready))
                .text_size(px(14.0))
                .text_color(cx.theme().secondary_foreground),
        )
        .child(
            div()
                .v_flex()
                .gap_3()
                .p_5()
                .bg(cx.theme().danger)
                .child(
                    div()
                        .child(failure.summary())
                        .text_size(px(20.0))
                        .text_color(cx.theme().danger_foreground),
                )
                .child(
                    div()
                        .child(if matches!(failure, RecoveryFailure::Preflight(_)) {
                            "This glass cannot start until the blocked preflight check passes."
                        } else if guest_was_ready {
                            "The runtime failed after Rootglass observed guest-ready evidence."
                        } else {
                            "The service could not complete launch. The guest is not shown as ready."
                        })
                        .text_size(px(14.0))
                        .text_color(cx.theme().foreground),
                ),
        )
        .child(checks_panel);
    if diagnostics_open {
        view = view.child(
            div()
                .v_flex()
                .gap_3()
                .p_5()
                .bg(cx.theme().group_box)
                .child(eyebrow("LAUNCH DIAGNOSTICS", cx))
                .child(TextView::markdown(
                    "launch-failure-diagnostics",
                    fenced_text(&failure.diagnostics()),
                )),
        );
    }
    view
}

fn check(name: &'static str, value: impl Into<String>, tone: CheckTone) -> LaunchCheck {
    LaunchCheck {
        name,
        value: value.into(),
        tone,
    }
}

fn is_compatibility_failure(failure: &FailureDetails) -> bool {
    let description = format!("{} {}", failure.summary, failure.detail).to_ascii_lowercase();
    description.contains("runtime requirements")
        || description.contains("acceleration")
        || description.contains("/dev/kvm")
}

fn one_line(value: &str, max_chars: usize) -> String {
    let line = value.lines().next().unwrap_or_default().trim();
    let mut shortened = line.chars().take(max_chars).collect::<String>();
    if line.chars().count() > max_chars {
        shortened.push('…');
    }
    shortened
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

#[cfg(test)]
mod tests {
    use super::{CheckTone, RecoveryFailure};
    use crate::ui_model::{BlockedDetails, FailureDetails, PreflightDetails};
    use std::path::PathBuf;

    fn acceleration_blocked() -> RecoveryFailure {
        RecoveryFailure::Preflight(BlockedDetails {
            path: Some(PathBuf::from("sensor-lab.glass")),
            reason: "Required acceleration is unavailable".to_string(),
            preflight: PreflightDetails {
                architecture: "aarch64".to_string(),
                profile: "virt-linux".to_string(),
                memory_mb: 512,
                cpus: 1,
                qemu: "qemu-system-aarch64".to_string(),
                acceleration: "KVM required".to_string(),
                runtime_assets: vec![],
                blockers: vec!["KVM is unavailable on this computer".to_string()],
            },
        })
    }

    #[test]
    fn preflight_failure_shows_concrete_blocker_without_expanding_diagnostics() {
        let failure = acceleration_blocked();
        let rows = failure.checks(None, false);
        assert_eq!(rows[0].value, "Passed");
        assert_eq!(rows[1].value, "Passed");
        assert_eq!(rows[2].name, "Computer compatibility");
        assert_eq!(
            rows[2].value,
            "Blocked · KVM is unavailable on this computer"
        );
        assert_eq!(rows[2].tone, CheckTone::Blocked);
        assert!(failure
            .diagnostics()
            .contains("KVM is unavailable on this computer"));
    }

    #[test]
    fn retry_uses_the_failed_bundle_path_and_preserves_service_detail() {
        let failure = RecoveryFailure::Launch(FailureDetails {
            path: Some(PathBuf::from("sensor-lab.glass")),
            phase: "Starting".to_string(),
            summary: "QEMU could not be started.".to_string(),
            detail: "failed to connect to QMP: connection refused".to_string(),
        });
        assert_eq!(
            failure.retry_path(Some(&PathBuf::from("other.glass"))),
            Some(PathBuf::from("sensor-lab.glass"))
        );
        assert!(failure
            .diagnostics()
            .contains("failed to connect to QMP: connection refused"));
        assert_eq!(
            failure.title("Sensor lab", false),
            "Couldn’t start Sensor lab"
        );
    }

    #[test]
    fn runtime_failure_after_ready_is_not_presented_as_failed_startup() {
        let failure = RecoveryFailure::Launch(FailureDetails {
            path: Some(PathBuf::from("sensor-lab.glass")),
            phase: "Guest".to_string(),
            summary: "QEMU exited unexpectedly.".to_string(),
            detail: "Exit code: Some(1)".to_string(),
        });
        assert_eq!(
            failure.title("Sensor lab", true),
            "Sensor lab stopped unexpectedly"
        );
        assert_eq!(failure.checks(None, true)[2].name, "Guest runtime");
    }
}
