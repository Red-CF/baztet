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

/// Which command the dashboard mirrors: `test` shows BEP verdicts,
/// `build`/`coverage` show target completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashMode {
    Test,
    Build,
}

/// A forked bazel command driven through the alt screen; returns the same
/// [`ExecResult`] the non-dashboard path would have produced, terminal
/// restored on every exit path.
pub async fn run(
    mode: DashMode,
    child: ExecChild,
    pending: Vec<String>,
) -> anyhow::Result<ExecResult> {
    let mut terminal = TuiTerminal::enter()?;
    let result = terminal.drive(mode, child, pending).await;
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
        mode: DashMode,
        child: ExecChild,
        pending: Vec<String>,
    ) -> anyhow::Result<ExecResult> {
        let started = Instant::now();
        let bep_path = child.bep_path().to_owned();
        // Option: awaiting consumes the handle; ownership back-checks the
        // early-abort path in the loop below
        let mut waiter = Some(tokio::spawn(async move {
            child.wait().await.map_err(anyhow::Error::from)
        }));
        let mut state = BepOutput::default();
        let mut last_len = 0u64;
        let mut last_parse = started
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(started);
        // the loop breaks through several paths; if it bails out early the
        // waiter still owns the live Child — aborting drops the Child and
        // kill_on_drop terminates bazel
        let outcome: anyhow::Result<ExecResult> = loop {
            if waiter.as_ref().unwrap().is_finished() {
                break match waiter.take().unwrap().await {
                    Ok(Ok(res)) => Ok(res),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(e.into()),
                };
            }
            match crossterm::event::poll(Duration::from_millis(200)) {
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            }
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(k)) => {
                    let quit = k.kind == crossterm::event::KeyEventKind::Press
                        && (k.code == crossterm::event::KeyCode::Char('q')
                            || (k
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL)
                                && k.code == crossterm::event::KeyCode::Char('c')));
                    if quit {
                        // capture what bazel wrote so far, then kill it
                        waiter.as_ref().unwrap().abort();
                        if let Some(out) = ExecChild::live_bep_snapshot(&bep_path) {
                            state = out;
                        }
                        break Ok(ExecResult {
                            code: 130,
                            bep: Some(state),
                            ..Default::default()
                        });
                    }
                }
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            }
            // the sidecar only grows: skip re-parse while its size is
            // stable, and at most ~2x/s while it grows (an incremental
            // cursor is the upgrade path)
            let len = std::fs::metadata(&bep_path).map(|m| m.len()).unwrap_or(0);
            if len > last_len && last_parse.elapsed() >= Duration::from_millis(500) {
                last_len = len;
                last_parse = Instant::now();
                if let Some(out) = ExecChild::live_bep_snapshot(&bep_path) {
                    state = out;
                }
            }
            if let Err(e) = draw(
                &mut self.terminal,
                mode,
                &state,
                pending.len(),
                started.elapsed(),
            ) {
                break Err(e);
            }
        };
        if outcome.is_err() {
            if let Some(h) = waiter.as_ref() {
                h.abort();
            }
        }
        outcome
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
    mode: DashMode,
    out: &BepOutput,
    requested: usize,
    elapsed: Duration,
) -> anyhow::Result<()> {
    let (done, mut rows): (usize, Vec<Row>) = match mode {
        DashMode::Test => {
            let rows: Vec<Row> = out
                .test_summaries
                .iter()
                .map(|s| {
                    Row::new([Cell::from(s.label.clone()), Cell::from(s.status.clone())])
                        .style(test_style(&s.status))
                })
                .collect();
            (out.test_summaries.len(), rows)
        }
        // build rows: failures first, then completions, then the running tail
        DashMode::Build => {
            let mut failures: Vec<Row> = Vec::new();
            let mut built: Vec<Row> = Vec::new();
            out.target_status.iter().for_each(|(label, success)| {
                let verdict = |txt: &str| {
                    Row::new([Cell::from(label.clone()), Cell::from(txt.to_string())]).style(
                        if *success {
                            Style::new().fg(Color::Green)
                        } else {
                            Style::new().fg(Color::Red)
                        },
                    )
                };
                if *success {
                    built.push(verdict("BUILT"));
                } else {
                    failures.push(verdict("FAILED"));
                }
            });
            let done = out.target_status.len();
            (done, [failures, built].concat())
        }
    };
    // the denominator is the requested list: it never grows while targets finish
    let total = requested.max(done);
    let header = format!(
        " baztet {} — {done}/{total} targets · {:.0}s",
        mode_title(mode),
        elapsed.as_secs_f64()
    );
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

fn test_style(status: &str) -> Style {
    match status {
        "PASSED" => Style::new().fg(Color::Green),
        "FAILED" | "TIMEOUT" | "ERROR" => Style::new().fg(Color::Red),
        "FLAKY" => Style::new().fg(Color::Yellow),
        _ => Style::new(),
    }
}

fn mode_title(mode: DashMode) -> &'static str {
    match mode {
        DashMode::Test => "test",
        DashMode::Build => "build/coverage",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baztet_core::baztet_bep::TestSummary;
    use ratatui::backend::TestBackend;

    #[test]
    fn dashboard_rows_by_mode() {
        let mut out = BepOutput::default();
        out.target_status.insert("//a:good".into(), true);
        out.target_status.insert("//a:bad".into(), false);
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            DashMode::Build,
            &out,
            5,
            Duration::from_secs(3),
        )
        .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("//a:bad"), "failure row missing: {text}");
        assert!(text.contains("FAILED"), "FAILED verdict missing: {text}");
        assert!(text.contains("2/5 targets"), "header got: {text}");
        assert!(text.contains("3 still running"), "pending missing: {text}");
    }

    fn summary(label: &str, status: &str) -> TestSummary {
        TestSummary {
            label: label.to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn header_denominator_stabilizes() {
        let requested = 10;
        let mut out = BepOutput::default();
        out.test_summaries = [summary("//a:one", "PASSED"), summary("//a:two", "FAILED")].into();
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            DashMode::Test,
            &out,
            requested,
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
        let mut out = BepOutput::default();
        out.test_summaries = [summary("//a:one", "PASSED"), summary("//a:two", "FLAKY")].into();
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            DashMode::Test,
            &out,
            requested,
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
    fn test_statuses_map_to_verdicts() {
        assert_eq!(test_style("PASSED").fg, Some(Color::Green));
        assert_eq!(test_style("FAILED").fg, Some(Color::Red));
        assert_eq!(test_style("TIMEOUT").fg, Some(Color::Red));
        assert_eq!(test_style("ERROR").fg, Some(Color::Red));
        assert_eq!(test_style("FLAKY").fg, Some(Color::Yellow));
        assert_eq!(test_style("NO_STATUS").fg, None);
    }
}
