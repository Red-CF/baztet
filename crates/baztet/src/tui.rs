//! The live dashboard: alt screen over the forked bazel command, with the
//! verdict list and a cat walking beneath it while bazel works.

use std::io::Read;
use std::time::Duration;

use ratatui::{
    backend::{Backend, CrosstermBackend},
    crossterm,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Terminal,
};

use crate::sprites::WalkingCat;
use baztet_core::baztet_bep::BepOutput;
use baztet_core::exec::{ExecChild, ExecResult};

/// Forked bazel command driven through the alt screen; returns the same
/// [`ExecResult`] the non-dashboard path would have produced, terminal
/// restored on every exit path. `title` names the command on screen.
pub async fn run(child: ExecChild, title: &str) -> anyhow::Result<ExecResult> {
    let mut terminal = TuiTerminal::enter()?;
    let result = terminal.drive(child, title).await;
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

    async fn drive(&mut self, child: ExecChild, title: &str) -> anyhow::Result<ExecResult> {
        let bep_path = child.bep_path().to_owned();
        // clone before the move: the dashboards' live progress lives in the
        // console capture, and the waiter task consumes the ExecChild
        let console = child.console_handle();
        // Option: awaiting consumes the handle; ownership back-checks the
        // early-abort path in the loop below
        let mut waiter = Some(tokio::spawn(async move {
            child.wait().await.map_err(anyhow::Error::from)
        }));
        let mut state = BepOutput::default();
        let offset = 0usize;
        let (tw, th) = crossterm::terminal::size().unwrap_or((80, 24));
        let mut walker = WalkingCat::new(tw, th);
        // the loop breaks through several paths; if it bails out early the
        // waiter still owns the live Child — aborting drops the Child and
        // kill_on_drop terminates bazel
        let outcome: anyhow::Result<ExecResult> = loop {
            if waiter.as_ref().unwrap().is_finished() {
                // the waiter's wait() already read and deleted the sidecar;
                // the completed verdicts are in the result's BEP output
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
                    &state,
                    walker.as_ref(),
                    &console,
                    title,
                    true,
                );
                break result;
            }
            // walk at ~120ms per frame: 8 frames ≈ 1s per crossing half
            let key_ready = match crossterm::event::poll(Duration::from_millis(120)) {
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
                                // bazel exited in the window: take the real
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
                    Ok(crossterm::event::Event::Resize(w, h)) => {
                        // re-pick the sprite size; never panic on odd sizes
                        walker = WalkingCat::new(w, h);
                    }
                    _ => {}
                }
            }
            if let Some(g) = walker.as_mut() {
                g.tick();
            }
            draw(
                &mut self.terminal,
                &state,
                walker.as_ref(),
                &console,
                title,
                false,
            )?;
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

fn draw<B>(
    terminal: &mut Terminal<B>,
    out: &BepOutput,
    walker: Option<&WalkingCat>,
    console: &baztet_core::exec::ConsoleCapture,
    title: &str,
    settled: bool,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    terminal.draw(|f| {
        let sprite_h = walker.map(|g| g.size.dims().1).unwrap_or(0);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2 + sprite_h)])
            .split(f.area());

        // verdict list, gold frame
        let mut rows: Vec<Line> = Vec::new();
        for s in &out.test_summaries {
            rows.push(Line::from(format!(" {:<48} {}", s.label, s.status)));
        }
        if rows.is_empty() {
            rows.push(Line::from(Span::styled(
                " waiting for the first verdicts…",
                Style::new().dim(),
            )));
        }
        let list = Paragraph::new(rows).block(
            Block::bordered()
                .title(format!(" baztet {title} — live "))
                .title_style(Style::new().bold().fg(Color::Yellow))
                .border_style(Style::new().fg(Color::DarkGray)),
        );
        f.render_widget(list, chunks[0]);

        // strip: bazel's progress + the cat walking underneath
        let mut strip: Vec<Line> = vec![Line::from(Span::styled(
            console.last_progress(),
            Style::new().dim(),
        ))];
        if let Some(walker) = walker {
            for l in walker.frame_lines() {
                strip.push(Line::from(Span::styled(
                    l.clone(),
                    // the sprite wears the Bastet gold the dashboard's frame already wears;
                    // a single color for all cells keeps the density-shading effect intact
                    Style::new().fg(Color::Rgb(0xE5, 0xC0, 0x6B)),
                )));
            }
        } else {
            strip.push(Line::from(""));
        }
        if settled {
            strip.push(Line::from(Span::styled(
                if out.target_status.values().any(|ok| !ok) {
                    " ~ grumpy ~"
                } else {
                    " ~ meow ~"
                },
                Style::new().fg(if out.target_status.values().any(|ok| !ok) {
                    Color::Red
                } else {
                    Color::Green
                }),
            )));
        }
        f.render_widget(Paragraph::new(strip), chunks[1]);
    })?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn dashboard_lists_verdicts_and_walks() {
        let mut out = BepOutput::default();
        out.test_summaries
            .push(baztet_core::baztet_bep::TestSummary {
                label: "//a:one".into(),
                status: "PASSED".into(),
            });
        out.target_status.insert("//a:two".into(), true);

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let walker = WalkingCat::new(80, 20).unwrap();
        draw(
            &mut terminal,
            &out,
            Some(&walker),
            &baztet_core::exec::ConsoleCapture::default(),
            "test",
            true,
        )
        .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("//a:one"), "verdict row missing: {text}");
        assert!(text.contains("PASSED"), "verdict missing: {text}");
        assert!(text.contains("meow"), "goodbye missing: {text}");
    }

    #[test]
    fn sprite_size_falls_back_on_narrow_terminals() {
        // nothing fits: the animation omits itself, no broken draw
        assert!(WalkingCat::new(20, 10).is_none());
        // small fits
        assert!(WalkingCat::new(30, 15).is_some());
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
        let frame1 = baztet_core::baztet_bep::testing::framed_started(4242, "stub");
        f.write_all(&frame1).unwrap();
        f.sync_all().unwrap();
        let mut state = BepOutput::default();
        let off = ingest(&mut state, &path, 0);
        assert_eq!(off, frame1.len());
        assert_eq!(state.server_pid, Some(4242));

        let frame2 = baztet_core::baztet_bep::testing::framed_started(5, "x");
        f.write_all(&frame2).unwrap();
        f.sync_all().unwrap();
        let off2 = ingest(&mut state, &path, off);
        assert_eq!(off2, frame1.len() + frame2.len());
        assert_eq!(state.server_pid, Some(5));

        // a truncated tail stops the cursor without error and keeps state
        f.write_all(&frame1[..3]).unwrap();
        f.sync_all().unwrap();
        let off3 = ingest(&mut state, &path, off2);
        assert_eq!(off3, off2);
        assert_eq!(state.server_pid, Some(5));
        let _ = std::fs::remove_file(&path);
    }
}
