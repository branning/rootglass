//! Ratatui rendering for the shared Rootglass UI model.

use crate::{
    BlockedDetails, FailureDetails, GlassPreview, ImportedDetails, PreflightDetails, RunDetails,
    SignatureStatus, UiActions, UiModel, UiState,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use std::fmt::Display;

const TITLE_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const LABEL_STYLE: Style = Style::new().fg(Color::Gray);
const SELECTED_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const ENABLED_STYLE: Style = Style::new().fg(Color::Green);
const DISABLED_STYLE: Style = Style::new().fg(Color::DarkGray);
const WARNING_STYLE: Style = Style::new().fg(Color::Yellow);
const ERROR_STYLE: Style = Style::new().fg(Color::Red);

/// Draws the current model into a Ratatui frame. This function has no terminal,
/// GPUI, filesystem, or process side effects and can be exercised with
/// [`ratatui::backend::TestBackend`].
pub fn render(frame: &mut Frame<'_>, model: &UiModel) {
    render_with_prompt(frame, model, None);
}

/// Draws the shared model and an optional keyboard-input prompt. The prompt is
/// used by the terminal runner and keeps input-mode details out of the model.
pub fn render_with_prompt(frame: &mut Frame<'_>, model: &UiModel, prompt: Option<&str>) {
    let constraints = if prompt.is_some() {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
            Constraint::Length(4),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(4),
        ]
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    frame.render_widget(
        Paragraph::new("Rootglass · Headless TUI")
            .style(TITLE_STYLE)
            .block(Block::default().borders(Borders::ALL)),
        sections[0],
    );

    let actions_area = if let Some(prompt) = prompt {
        frame.render_widget(
            Paragraph::new(prompt).block(Block::default().borders(Borders::ALL).title("Input")),
            sections[2],
        );
        sections[3]
    } else {
        sections[2]
    };

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(sections[1]);

    render_library(frame, model, columns[0]);

    let details = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Percentage(42)])
        .split(columns[1]);
    render_details(frame, &model.state, details[0]);
    render_console(frame, &model.console, details[1]);
    render_actions(frame, model.actions(), actions_area);
}

fn render_library(frame: &mut Frame<'_>, model: &UiModel, area: Rect) {
    let mut lines = Vec::new();
    if let Some(error) = &model.library_error {
        lines.push(Line::from(Span::styled(
            format!("Library error: {error}"),
            ERROR_STYLE,
        )));
    }
    if model.installed_glasses.is_empty() {
        lines.push(Line::from("No installed bundles."));
    } else {
        for glass in &model.installed_glasses {
            let selected = model.selected_path.as_ref() == Some(&glass.package_path);
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                SELECTED_STYLE
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(format!("{} {}", glass.name, glass.version), style),
            ]));
            lines.push(Line::from(Span::styled(
                format!("  {}", glass.package_path.display()),
                LABEL_STYLE,
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Library"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_details(frame: &mut Frame<'_>, state: &UiState, area: Rect) {
    let (title, lines) = state_details(state);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn state_details(state: &UiState) -> (&'static str, Vec<Line<'static>>) {
    match state {
        UiState::Empty => (
            "Empty",
            vec![
                Line::from("No bundle selected."),
                Line::from("Open a .glass bundle to inspect it."),
            ],
        ),
        UiState::Loading => ("Loading", vec![Line::from("Inspecting bundle…")]),
        UiState::Importing(path) => (
            "Importing",
            vec![
                Line::from("Verifying and importing archive…"),
                field("Path", path.display()),
            ],
        ),
        UiState::Imported(details) => ("Installed", imported_lines(details)),
        UiState::Ready(preview) => ("Ready", preview_lines(preview)),
        UiState::Blocked(details) => ("Blocked", blocked_lines(details)),
        UiState::Failed(details) => ("Failed", failure_lines(details)),
        UiState::Booting(run) => ("Booting", run_lines(run)),
        UiState::Stopping(run) => ("Stopping", run_lines(run)),
        UiState::Running(run) => ("Running", run_lines(run)),
    }
}

fn imported_lines(details: &ImportedDetails) -> Vec<Line<'static>> {
    vec![
        field("Name", &details.name),
        field("Version", &details.version),
        field("Publisher", &details.publisher),
        field("Digest", &details.digest),
        field("Path", details.path.display()),
    ]
}

fn preview_lines(preview: &GlassPreview) -> Vec<Line<'static>> {
    let signature = match &preview.signature {
        SignatureStatus::Verified(publisher) => format!("Verified · {publisher}"),
        SignatureStatus::Untrusted(publisher) => format!("Untrusted · {publisher}"),
        SignatureStatus::Invalid(reason) => format!("Invalid · {reason}"),
        SignatureStatus::Tampered(reason) => format!("Tampered · {reason}"),
    };
    let mut lines = vec![
        field("Name", &preview.name),
        field("Identifier", &preview.identifier),
        field("Version", &preview.version),
        field("Publisher", &preview.publisher),
        field("Signature", signature),
        field("Path", preview.path.display()),
        Line::from("Preflight"),
        field("Architecture", &preview.preflight.architecture),
        field("Profile", &preview.preflight.profile),
        field("Memory", format!("{} MiB", preview.preflight.memory_mb)),
        field("CPUs", preview.preflight.cpus),
        field("QEMU", &preview.preflight.qemu),
        field("Acceleration", &preview.preflight.acceleration),
    ];
    lines.extend(
        preview
            .preflight
            .runtime_assets
            .iter()
            .map(|asset| field("Runtime", asset)),
    );
    lines.extend(
        preview
            .preflight
            .blockers
            .iter()
            .map(|blocker| Line::from(Span::styled(format!("Blocker: {blocker}"), WARNING_STYLE))),
    );
    lines
}

fn blocked_lines(blocked: &BlockedDetails) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        blocked.reason.clone(),
        WARNING_STYLE,
    ))];
    if let Some(path) = &blocked.path {
        lines.push(field("Path", path.display()));
    }
    lines.extend(preflight_lines(&blocked.preflight));
    lines
}

fn failure_lines(failure: &FailureDetails) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(failure.summary.clone(), ERROR_STYLE)),
        field("Phase", &failure.phase),
        field("Details / correction", &failure.detail),
    ];
    if let Some(path) = &failure.path {
        lines.push(field("Path", path.display()));
    }
    lines
}

fn run_lines(run: &RunDetails) -> Vec<Line<'static>> {
    vec![
        field("Guest", &run.name),
        field("Status", &run.status),
        field("Elapsed", &run.elapsed),
    ]
}

fn preflight_lines(preflight: &PreflightDetails) -> Vec<Line<'static>> {
    let mut lines = vec![
        field("Architecture", &preflight.architecture),
        field("Profile", &preflight.profile),
        field("Memory", format!("{} MiB", preflight.memory_mb)),
        field("CPUs", preflight.cpus),
        field("QEMU", &preflight.qemu),
        field("Acceleration", &preflight.acceleration),
    ];
    lines.extend(
        preflight
            .runtime_assets
            .iter()
            .map(|asset| field("Runtime", asset)),
    );
    lines.extend(
        preflight
            .blockers
            .iter()
            .map(|blocker| Line::from(Span::styled(format!("Blocker: {blocker}"), WARNING_STYLE))),
    );
    lines
}

fn render_console(frame: &mut Frame<'_>, console: &str, area: Rect) {
    let mut lines = console.lines().rev().take(200).collect::<Vec<_>>();
    lines.reverse();
    let lines = if lines.is_empty() {
        vec![Line::from(
            "Console output appears here when a guest is running.",
        )]
    } else {
        lines
            .into_iter()
            .map(|line| Line::from(line.to_string()))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Guest console"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_actions(frame: &mut Frame<'_>, actions: UiActions, area: Rect) {
    let lines = vec![
        action_line([
            action("[o] Open", actions.open),
            action("[b] Boot", actions.boot),
            action("[s] Stop", actions.stop),
            action("[r] Restart", actions.restart),
            action("[Enter] Console", actions.console_input),
        ]),
        action_line([
            action("↑/↓ Select", true),
            action("[f] Refresh", true),
            action("[q] Quit", true),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Actions")),
        area,
    );
}

fn action_line<const N: usize>(spans: [Span<'static>; N]) -> Line<'static> {
    let mut line = Vec::with_capacity(spans.len() * 2 - 1);
    for (index, span) in spans.into_iter().enumerate() {
        if index > 0 {
            line.push(Span::raw("  "));
        }
        line.push(span);
    }
    Line::from(line)
}

fn action(label: &'static str, enabled: bool) -> Span<'static> {
    Span::styled(
        label,
        if enabled {
            ENABLED_STYLE
        } else {
            DISABLED_STYLE
        },
    )
}

fn field(label: &str, value: impl Display) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), LABEL_STYLE),
        Span::raw(value.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImportCompletion, InspectionOutcome, TuiService};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use rootglass_app::registry::InstalledGlass;
    use rootglass_app::{AppError, RunId, RuntimeEvent};
    use rootglass_core::LifecycleState;
    use std::path::{Path, PathBuf};

    #[derive(Default)]
    struct NoopService;

    impl TuiService for NoopService {
        fn installed_glasses(&mut self) -> Result<Vec<InstalledGlass>, AppError> {
            Ok(Vec::new())
        }

        fn import_archive(&mut self, _source: &Path) -> Result<InstalledGlass, AppError> {
            Err(app_error())
        }

        fn inspect_bundle(&mut self, _source: &Path) -> InspectionOutcome {
            InspectionOutcome::Failed(FailureDetails {
                path: None,
                phase: "Verification".into(),
                summary: "No-op test service".into(),
                detail: "No bundle was inspected.".into(),
            })
        }

        fn start(&mut self, _source: &Path) -> Result<RunId, AppError> {
            Err(app_error())
        }

        fn stop(&mut self, _run: &RunId) -> Result<(), AppError> {
            Err(app_error())
        }

        fn poll_events(&mut self) -> Result<Vec<RuntimeEvent>, AppError> {
            Ok(Vec::new())
        }

        fn write_serial(&mut self, _run: &RunId, _bytes: &[u8]) -> Result<(), AppError> {
            Err(app_error())
        }

        fn import_and_inspect(&mut self, _source: &Path) -> Result<ImportCompletion, AppError> {
            Err(app_error())
        }
    }

    fn app_error() -> AppError {
        AppError {
            phase: LifecycleState::Failed,
            summary: "Test service error".into(),
            detail: "No operation is expected in a render-only test.".into(),
        }
    }

    fn model(state: UiState) -> UiModel {
        UiModel::with_service(NoopService, state)
    }

    fn snapshot(model: &UiModel) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal is created");
        terminal
            .draw(|frame| render(frame, model))
            .expect("model renders to TestBackend");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn preflight() -> PreflightDetails {
        PreflightDetails {
            architecture: "x86_64".into(),
            profile: "linux".into(),
            memory_mb: 512,
            cpus: 2,
            qemu: "qemu-system-x86_64".into(),
            acceleration: "tcg".into(),
            runtime_assets: vec!["kernel.img".into()],
            blockers: Vec::new(),
        }
    }

    fn preview() -> GlassPreview {
        GlassPreview {
            path: PathBuf::from("/library/hello.glass"),
            name: "Hello World".into(),
            identifier: "rootglass.hello".into(),
            version: "0.1.0".into(),
            description: "Example guest".into(),
            publisher: "Rootglass Examples".into(),
            signature: SignatureStatus::Verified("fixture-key".into()),
            preflight: preflight(),
        }
    }

    fn run(status: &str) -> RunDetails {
        RunDetails {
            name: "Hello World".into(),
            status: status.into(),
            elapsed: "00:00:03".into(),
        }
    }

    #[test]
    fn core_states_render_their_status_and_recovery_details() {
        let cases = [
            (UiState::Empty, "No bundle selected."),
            (UiState::Loading, "Inspecting bundle"),
            (UiState::Ready(preview()), "Verified · fixture-key"),
            (
                UiState::Blocked(BlockedDetails {
                    path: Some(PathBuf::from("blocked.glass")),
                    reason: "Missing runtime asset".into(),
                    preflight: preflight(),
                }),
                "Missing runtime asset",
            ),
            (
                UiState::Failed(FailureDetails {
                    path: Some(PathBuf::from("bad.glass")),
                    phase: "Verification".into(),
                    summary: "Bundle is not trusted".into(),
                    detail: "Add the publisher key, then reopen the bundle.".into(),
                }),
                "Add the publisher key",
            ),
            (UiState::Booting(run("Starting guest")), "Starting guest"),
            (UiState::Running(run("Running")), "Running"),
            (UiState::Stopping(run("Stopping guest")), "Stopping guest"),
        ];

        for (state, expected) in cases {
            assert!(snapshot(&model(state)).contains(expected));
        }
    }

    #[test]
    fn selected_library_entry_and_enabled_actions_are_visible() {
        let mut model = model(UiState::Ready(preview()));
        model.selected_path = Some(PathBuf::from("/library/hello.glass"));
        model.installed_glasses.push(InstalledGlass {
            content_sha256: "abc123".into(),
            package_path: PathBuf::from("/library/hello.glass"),
            identifier: "rootglass.hello".into(),
            name: "Hello World".into(),
            version: "0.1.0".into(),
            publisher_name: "Rootglass Examples".into(),
            publisher_identity: "fixture-key".into(),
            key_id: "fixture".into(),
            original_source: PathBuf::from("hello.glass"),
            imported_at_unix_seconds: 1,
        });

        let output = snapshot(&model);
        assert!(output.contains("▶ Hello World 0.1.0"));
        assert!(output.contains("[o] Open"));
        assert!(output.contains("[b] Boot"));
        assert!(output.contains("[Enter] Console"));
        assert!(output.contains("[q] Quit"));
        assert!(output.contains("[f] Refresh"));
    }

    #[test]
    fn terminal_input_prompt_is_rendered_by_the_shared_renderer() {
        let model = model(UiState::Empty);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal is created");
        terminal
            .draw(|frame| {
                render_with_prompt(frame, &model, Some("Bundle path: /tmp/example.glass"))
            })
            .expect("prompt renders to TestBackend");
        let buffer = terminal.backend().buffer();
        let output = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(output.contains("Bundle path: /tmp/example.glass"));
    }

    #[test]
    fn console_output_is_rendered_and_stopped_state_disables_boot() {
        let mut model = model(UiState::Running(run("Running")));
        model.run_id = Some(RunId(7));
        model.console = "rootglass-hello login:\n".into();

        let output = snapshot(&model);
        assert!(output.contains("rootglass-hello login:"));
        assert!(output.contains("[s] Stop"));
        assert!(output.contains("[r] Restart"));
        assert!(!model.actions().boot);
    }
}
