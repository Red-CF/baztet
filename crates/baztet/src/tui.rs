use std::time::{Duration, Instant};

use ratatui::{
    backend::{Backend, CrosstermBackend},
    crossterm,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style, Stylize},
    widgets::{Block, Cell, Paragraph, Row, Table},
    Terminal,
};

use baztet_core::baztet_bep::BepOutput;
use baztet_core::exec::{ExecChild, ExecResult};

/// Forked `bazel test` driven through the alt screen; returns the same
/// [`ExecResult`] the non-TUI path would have produced. Every exit path
/// restores the terminal (the guard's Drop covers early `?`s).
pub async fn run_test(child: ExecChild, pending: Vec<String>) -> anyhow::Result<ExecResult> {
    let mut terminal = TuiTerminal::enter()?;
    let result = terminal.drive(child, pending).await;
    drop(terminal);
    result
}
/// Owns the alt-screen/raw-mode pair until dropped.
struct TuiTerminal {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TuiTerminal {
    fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        let terminal = match Terminal::new(CrosstermBackend::new(std::io::stdout())) {
            Ok(t) => t,
            Err(e) => {
                restore();
                return Err(e.into());
            }
        };
        Ok(Self { terminal })
    }

    async fn drive(
        &mut self,
        child: ExecChild,
        pending: Vec<String>,
    ) -> anyhow::Result<ExecResult> {
        let started = Instant::now();
        let bep_path = child.bep_path().to_owned();
        let waiter = tokio::spawn(async move { child.wait().await.map_err(anyhow::Error::from) });
        let mut state = BepOutput::default();
        let mut last_len = 0u64;
        loop {
            if waiter.is_finished() {
                return waiter.await?;
            }
            if crossterm::event::poll(Duration::from_millis(200))? {
                if let crossterm::event::Event::Key(k) = crossterm::event::read()? {
                    let quit = k.kind == crossterm::event::KeyEventKind::Press
                        && (k.code == crossterm::event::KeyCode::Char('q')
                            || (k
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL)
                                && k.code == crossterm::event::KeyCode::Char('c')));
                    if quit {
                        // capture what bazel wrote so far, then kill it
                        if let Some(out) = ExecChild::live_bep_snapshot(&bep_path) {
                            state = out;
                        }
                        waiter.abort();
                        return Ok(ExecResult {
                            code: 130,
                            bep: Some(state),
                            ..Default::default()
                        });
                    }
                }
            }
            // the sidecar only grows: skip re-parse while its size is stable
            let len = std::fs::metadata(&bep_path).map(|m| m.len()).unwrap_or(0);
            if len > last_len {
                last_len = len;
                if let Some(out) = ExecChild::live_bep_snapshot(&bep_path) {
                    state = out;
                }
            }
            draw(
                &mut self.terminal,
                &state,
                pending.len(),
                &state.test_summaries,
                started.elapsed(),
            )?;
        }
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show,
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

fn draw(
    terminal: &mut Terminal<impl Backend>,
    _state: &BepOutput,
    requested: usize,
    summaries: &[baztet_core::baztet_bep::TestSummary],
    elapsed: Duration,
) -> anyhow::Result<()> {
    let done = summaries.len();
    // the dashboard denominates by the requested list, which never grows
    // while targets finish; old code grew it on every completion
    let total = requested.max(done);
    let header = format!(
        " baztet test — {done}/{total} targets · {:.0}s",
        elapsed.as_secs_f64()
    );
    let mut rows: Vec<Row> = summaries
        .iter()
        .map(|s| {
            Row::new([Cell::from(s.label.clone()), Cell::from(s.status.clone())])
                .style(target_style(&s.status))
        })
        .collect();
    if requested > done {
        rows.push(
            Row::new([Cell::from(format!(
                "… {} still running/pending",
                requested - done
            ))])
            .style(Style::new().dim()),
        );
    }
    terminal.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(f.area());
        let table = Table::new(
            rows,
            [Constraint::Percentage(75), Constraint::Percentage(25)],
        )
        .header(Row::new(["target", "status"]).style(Style::new().bold()))
        .block(Block::bordered().title(header));
        f.render_widget(table, chunks[0]);
        f.render_widget(Paragraph::new("q / ctrl-c cancels bazel"), chunks[1]);
    })?;
    Ok(())
}

fn target_style(status: &str) -> Style {
    match status {
        "PASSED" => Style::new().fg(Color::Green),
        "FAILED" | "TIMEOUT" | "ERROR" => Style::new().fg(Color::Red),
        "FLAKY" => Style::new().fg(Color::Yellow),
        _ => Style::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baztet_core::baztet_bep::TestSummary;
    use ratatui::backend::TestBackend;

    fn summary(label: &str, status: &str) -> TestSummary {
        TestSummary {
            label: label.to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn header_denominator_stabilizes() {
        let requested = 10;
        let summaries = [summary("//a:one", "PASSED"), summary("//a:two", "FAILED")];
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            &BepOutput::default(),
            requested,
            &summaries,
            Duration::from_secs(5),
        )
        .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("2/10 targets"), "header got: {text}");
        assert!(text.contains("//a:one"), "row missing: {text}");
        assert!(
            text.contains("8 still running"),
            "pending row missing: {text}"
        );
    }

    #[test]
    fn header_shows_full_when_all_finish() {
        let requested = 2;
        let summaries = [summary("//a:one", "PASSED"), summary("//a:two", "FLAKY")];
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            &BepOutput::default(),
            requested,
            &summaries,
            Duration::from_secs(9),
        )
        .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("2/2 targets"), "header got: {text}");
        assert!(!text.contains("still running"), "leftover: {text}");
    }

    #[test]
    fn target_styles_map_to_verdicts() {
        assert_eq!(target_style("PASSED").fg, Some(Color::Green));
        assert_eq!(target_style("FAILED").fg, Some(Color::Red));
        assert_eq!(target_style("TIMEOUT").fg, Some(Color::Red));
        assert_eq!(target_style("ERROR").fg, Some(Color::Red));
        assert_eq!(target_style("FLAKY").fg, Some(Color::Yellow));
        assert_eq!(target_style("NO_STATUS").fg, None);
    }
}
