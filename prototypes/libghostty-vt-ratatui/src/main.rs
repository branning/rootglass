use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

#[derive(Default)]
struct FixtureState {
    left_presses: u8,
    size: (u16, u16),
    pasted: String,
}

fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, state: &FixtureState) -> io::Result<()> {
    terminal.draw(|frame| {
        let lines = vec![
            Line::from(Span::styled(
                "RATATUI-GHOSTTY-FIXTURE",
                Style::default().fg(Color::Green),
            )),
            Line::from(format!("LEFT:{}", state.left_presses)),
            Line::from(format!("SIZE:{}x{}", state.size.0, state.size.1)),
            Line::from(format!("PASTE:{}", state.pasted)),
            Line::from("Press q to quit"),
        ];
        frame.render_widget(Paragraph::new(lines), frame.area());
    })?;
    Ok(())
}

fn fixture_child() -> io::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut state = FixtureState {
        size: crossterm::terminal::size()?,
        ..FixtureState::default()
    };
    draw(&mut terminal, &state)?;

    loop {
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.code == KeyCode::Left => {
                    state.left_presses = state.left_presses.saturating_add(1);
                    draw(&mut terminal, &state)?;
                }
                Event::Paste(text) => {
                    state.pasted = text;
                    draw(&mut terminal, &state)?;
                }
                Event::Resize(cols, rows) => {
                    state.size = (cols, rows);
                    draw(&mut terminal, &state)?;
                }
                Event::Key(key) if key.code == KeyCode::Char('q') => break,
                _ => {}
            }
        }
    }

    crossterm::terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableBracketedPaste)?;
    terminal.show_cursor()?;
    Ok(())
}

fn main() -> io::Result<()> {
    if std::env::args().any(|arg| arg == "--fixture-child") {
        fixture_child()?;
    }
    Ok(())
}
