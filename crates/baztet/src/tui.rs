use std::io::Read;
use std::time::{Duration, Instant};

use ratatui::{
    backend::{Backend, CrosstermBackend},
    crossterm,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
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
        // clone before the move: the dashboard's live progress lives in the
        // console capture, and the waiter task consumes the ExecChild
        let console = child.console_handle();
        // Option: awaiting consumes the handle; ownership back-checks the
        // early-abort path in the loop below
        let mut waiter = Some(tokio::spawn(async move {
            child.wait().await.map_err(anyhow::Error::from)
        }));
        let mut state = BepOutput::default();
        let mut offset = 0usize;
        // the loop breaks through several paths; if it bails out early the
        // waiter still owns the live Child — aborting drops the Child and
        // kill_on_drop terminates bazel
        let outcome: anyhow::Result<ExecResult> = loop {
            if waiter.as_ref().unwrap().is_finished() {
                // the waiter's wait() already read and deleted the sidecar;
                // the completed verdicts are in the result's BEP output —
                // draw this final frame so the dashboard closes showing the
                // run's state, not a mid-run one
                let result = match waiter.take().unwrap().await {
                    Ok(Ok(res)) => Ok(res),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(e.into()),
                };
                if let Ok(res) = &result {
                    if let Some(bep) = &res.bep {
                        state = bep.clone();
                    }
                }
                let _ = draw(
                    &mut self.terminal,
                    mode,
                    &state,
                    pending.len(),
                    started.elapsed(),
                    &console,
                );
                break result;
            }
            // read only when poll says an event is ready: a bare read blocks
            // forever when no key was pressed, freezing the whole draw loop
            let key_ready = match crossterm::event::poll(Duration::from_millis(200)) {
                Ok(ready) => ready,
                Err(e) => break Err(e.into()),
            };
            if key_ready {
                match crossterm::event::read() {
                    Ok(crossterm::event::Event::Key(k)) => {
                        let quit = k.kind == crossterm::event::KeyEventKind::Press
                            && (k.code == crossterm::event::KeyCode::Char('q')
                                || (k
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL)
                                    && k.code == crossterm::event::KeyCode::Char('c')));
                        if quit {
                            if waiter.as_ref().unwrap().is_finished() {
                                // bazel exited in the poll window: take the real
                                // result instead of fabricating a cancellation
                                break match waiter.take().unwrap().await {
                                    Ok(Ok(res)) => Ok(res),
                                    Ok(Err(e)) => Err(e),
                                    Err(e) => Err(e.into()),
                                };
                            }
                            // capture what bazel wrote so far, then kill it
                            waiter.as_ref().unwrap().abort();
                            ingest(&mut state, &bep_path, offset);
                            break Ok(ExecResult {
                                code: 130,
                                bep: Some(state),
                                ..Default::default()
                            });
                        }
                    }
                    Err(e) => break Err(e.into()),
                    _ => {}
                }
            }
            offset = ingest(&mut state, &bep_path, offset);
            if let Err(e) = draw(
                &mut self.terminal,
                mode,
                &state,
                pending.len(),
                started.elapsed(),
                &console,
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

/// Read and decode the BEP bytes appended since `offset`; returns the new
/// cursor. The sidecar is written whole at bazel's exit, so this mostly
/// serves mid-write tails and the quit path; the completed state comes
/// from the waiter's parsed result instead.
fn ingest(state: &mut BepOutput, path: &std::path::Path, offset: usize) -> usize {
    use std::io::{Seek, SeekFrom};
    if let Ok(mut f) = std::fs::File::open(path) {
        if f.seek(SeekFrom::Start(offset as u64)).is_ok() {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                // a truncated tail is normal mid-write: keep the old
                // state and the cursor where it was
                if let Ok((delta, consumed)) = baztet_core::baztet_bep::parse_prefix(&buf, 0) {
                    state.extend(delta);
                    return offset + consumed;
                }
            }
        }
    }
    offset
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

/// Bastet V2, with volume: the skull widens under the ears and the
/// cheeks flare wider than the eyes (a real 3D head profile), Bastet's
/// muzzle ω with whiskers attached to the cheeks, and a ground-shadow row
/// rendered dark below the body. Motion: the eyes change per frame and
/// the tail swishes between four positions.
const CAT_RUNNING: [&str; 4] = [
    r#"    /\_/\
   / o o \
 =(  ω  )=
   \___/
  /|    |\
 ( |*==*| )
  \|    |_|
 (__|  |__)""#,
    r#"    /\_/\
   / - - \
 =(  ω  )=
   \___/
  /|    |\
 ( |*==*| )
  \|    |_|
 (__/  \__)""#,
    r#"    /\=/\
   / o O \
 =(  ω  )=
   \___/
  /|    |\
 ( |*==*| )
  \|    |_|
 (\_|  |__)""#,
    r#"    /\_/\
   / ^ ^ \
 =(  ω  )=
   \___/
  /|    |\
 ( |*==*| )
  \|    |_|
 (_/    \_)""#,
];
const CAT_HAPPY: &str = r#"    /\_/\
   / ^ ^ \
 =(  ω  )=
   \___/
  /|    |\
 ( |*==*| )~
  \|    |_|
 (__/‾‾\__)
   ~ meow ~""#;
const CAT_HAPPY_SHADOW: &str = "      ~ ~~";
const CAT_GRUMPY: &str = r#"    /\_/\
   / x x \
 =( ____ )?
   \___/
  /|    |\
 ( |*==*| ):(
  \|    |_|
 (\|__|__)
   ~ grumpy ~""#;
const CAT_GRUMPY_SHADOW: &str = "      ~ ~~";

fn cat_shadow(settled: bool) -> &'static str {
    if settled {
        CAT_HAPPY_SHADOW
    } else {
        CAT_GRUMPY_SHADOW
    }
}

fn cat_frame(elapsed: Duration, settled: bool, any_failed: bool) -> &'static str {
    if !settled {
        let idx = (elapsed.as_millis() / 300) as usize % CAT_RUNNING.len();
        CAT_RUNNING[idx]
    } else if any_failed {
        CAT_GRUMPY
    } else {
        CAT_HAPPY
    }
}

fn draw<B>(
    terminal: &mut Terminal<B>,
    mode: DashMode,
    out: &BepOutput,
    requested: usize,
    elapsed: Duration,
    console: &baztet_core::exec::ConsoleCapture,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
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
    let settled = requested > 0 && done >= requested;
    let any_failed = match mode {
        DashMode::Test => out
            .test_summaries
            .iter()
            .any(|s| test_style(&s.status).fg == Some(Color::Red)),
        DashMode::Build => out.target_status.values().any(|ok| !ok),
    };
    terminal.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(f.area());
        let (main, cat_area) = {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(11)])
                .split(chunks[0]);
            (cols[0], cols[1])
        };
        let cat = cat_frame(elapsed, settled, any_failed);
        let shadow = cat_shadow(settled);
        // art above, dark shadow line below: the ground grounds the cat
        let (cat_zone, shadow_zone) = {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(cat_area);
            (rows[0], rows[1])
        };
        f.render_widget(
            Paragraph::new(cat).style(Style::new().fg(Color::Yellow)),
            cat_zone,
        );
        f.render_widget(
            Paragraph::new(shadow).style(Style::new().dim()),
            shadow_zone,
        );
        let table = Table::new(
            rows,
            [Constraint::Percentage(75), Constraint::Percentage(25)],
        )
        .header(Row::new(["target", "status"]).style(Style::new().bold().fg(Color::Yellow)))
        .block(
            Block::bordered()
                .title(header)
                .title_style(Style::new().bold().fg(Color::Yellow))
                .border_style(Style::new().fg(Color::DarkGray)),
        );
        f.render_widget(table, main);
        // live bazel progress in the footer: the console is the streaming
        // source (the BEP sidecar is written whole at exit)
        let progress = console.last_progress();
        let footer = if progress.is_empty() {
            "q / ctrl-c cancels bazel".to_string()
        } else {
            format!("q cancels bazel · {progress}")
        };
        f.render_widget(Paragraph::new(footer).style(Style::new().dim()), chunks[1]);
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
        let out = BepOutput {
            target_status: [
                ("//a:good".to_string(), true),
                ("//a:bad".to_string(), false),
            ]
            .into(),
            ..BepOutput::default()
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            DashMode::Build,
            &out,
            5,
            Duration::from_secs(3),
            &baztet_core::exec::ConsoleCapture::default(),
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
    fn ingest_reads_appended_frames() {
        use std::io::Write;
        use std::time::{SystemTime, UNIX_EPOCH};

        let path = std::env::temp_dir().join(format!(
            "baztet-ingest-{}.bin",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        // frame 1 written first; ingest at offset 0 → whole state
        let frame1 = build_started_frame();
        f.write_all(&frame1).unwrap();
        f.sync_all().unwrap();
        let mut state = BepOutput::default();
        let off = ingest(&mut state, &path, 0);
        assert_eq!(off, frame1.len());
        assert_eq!(state.server_pid, Some(4242));

        // frame 2 appended; ingesting from the cursor picks up only the tail
        let frame2 = build_started_frame2();
        f.write_all(&frame2).unwrap();
        f.sync_all().unwrap();
        let off2 = ingest(&mut state, &path, off);
        assert_eq!(off2, frame1.len() + frame2.len());
        // the last Started event wins, as in a real rebuild
        assert_eq!(state.server_pid, Some(5));

        // a truncated tail stops the cursor without error and keeps state
        f.write_all(&frame1[..3]).unwrap();
        f.sync_all().unwrap();
        let off3 = ingest(&mut state, &path, off2);
        assert_eq!(off3, off2);
        // truncated tail: the previous state stands
        assert_eq!(state.server_pid, Some(5));
        let _ = std::fs::remove_file(&path);
    }

    fn build_started_frame() -> Vec<u8> {
        baztet_core::baztet_bep::testing::framed_started(4242, "release 9.3.1-stub")
    }

    fn build_started_frame2() -> Vec<u8> {
        baztet_core::baztet_bep::testing::framed_started(5, "x")
    }

    #[test]
    fn header_denominator_stabilizes() {
        let requested = 10;
        let out = BepOutput {
            test_summaries: [summary("//a:one", "PASSED"), summary("//a:two", "FAILED")].into(),
            ..BepOutput::default()
        };
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            DashMode::Test,
            &out,
            requested,
            Duration::from_secs(5),
            &baztet_core::exec::ConsoleCapture::default(),
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
        let out = BepOutput {
            test_summaries: [summary("//a:one", "PASSED"), summary("//a:two", "FLAKY")].into(),
            ..BepOutput::default()
        };
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            DashMode::Test,
            &out,
            requested,
            Duration::from_secs(9),
            &baztet_core::exec::ConsoleCapture::default(),
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
    fn cat_reacts_to_verdicts() {
        // all built: happy goodbye
        let out = BepOutput {
            target_status: [("//a:one".to_string(), true), ("//a:two".to_string(), true)].into(),
            ..BepOutput::default()
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            DashMode::Build,
            &out,
            2,
            Duration::from_secs(3),
            &baztet_core::exec::ConsoleCapture::default(),
        )
        .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("(  ω  )") && text.contains("meow"),
            "happy cat missing: {text}"
        );

        // one failure: grumpy
        let out = BepOutput {
            target_status: [
                ("//a:one".to_string(), true),
                ("//a:two".to_string(), false),
            ]
            .into(),
            ..BepOutput::default()
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            DashMode::Build,
            &out,
            2,
            Duration::from_secs(3),
            &baztet_core::exec::ConsoleCapture::default(),
        )
        .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("( ____ )") && text.contains("grumpy"),
            "grumpy cat missing: {text}"
        );
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
