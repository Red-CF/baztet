//! The live dashboard: alt screen over the forked bazel command, with the
//! verdict list and a cat walking beneath it while bazel works.

use std::time::{Duration, Instant};

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
        // run wall-clock: the strip's elapsed timer beats against it
        let run_started = Instant::now();
        // the cursor path to the sidecar; the console capture carries the
        // live progress line; the waiter task consumes the ExecChild
        let bep_path = child.bep_path().to_owned();
        let console = child.console_handle();
        // Option: awaiting consumes the handle; ownership back-checks the
        // early-abort path in the loop below
        let mut waiter = Some(tokio::spawn(async move {
            child.wait().await.map_err(anyhow::Error::from)
        }));
        let mut state = BepOutput::default();
        let mut offset = 0usize;
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
                    run_started.elapsed(),
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
                            // then kill it: the capture already holds the
                            // live console; the sidecar only exists at
                            // bazel's exit
                            waiter.as_ref().unwrap().abort();
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
            // the sidecar holds the analysis batch and grows with every
            // finished test: the live verdict source (the console row below
            // only carries bazel's progress line)
            offset = ingest(&mut state, &bep_path, offset);
            draw(
                &mut self.terminal,
                &state,
                walker.as_ref(),
                &console,
                title,
                false,
                run_started.elapsed(),
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

/// The verdict's color: green pass, red fail, yellow flaky, gray unknown.
pub fn verdict_style(status: &str) -> Style {
    match status {
        "PASSED" | "BUILT" => Style::new().fg(Color::Green),
        // every real failure verdict: a failed-to-build run must not read
        // as an inert unknown row
        "FAILED"
        | "TIMEOUT"
        | "ERROR"
        | "FAILED_TO_BUILD"
        | "INCOMPLETE"
        | "REMOTE_FAILURE"
        | "TOOL_HALTED_BEFORE_TESTING" => Style::new().fg(Color::Red),
        "FLAKY" => Style::new().fg(Color::Yellow),
        // the UNKNOWN(n) fallback and NO_STATUS
        _ => Style::new().fg(Color::Gray),
    }
}

/// One console line's activity: (done, total) fraction and the target
/// being built or tested, straight from bazel's status text.
pub fn parse_status(line: &str) -> (Option<(u64, u64)>, Option<String>) {
    let mut fraction = None;
    let mut active = None;
    if let (Some(open), Some(close)) = (line.find('['), line.find(']')) {
        if close > open {
            let inside = &line[open + 1..close];
            if let Some(slash) = inside.rfind('/') {
                let done = inside[..slash].trim().replace(',', "");
                let total = inside[slash + 1..].trim().replace(',', "");
                if let (Ok(d), Ok(t)) = (done.parse(), total.parse()) {
                    fraction = Some((d, t));
                }
            }
        }
    }
    for word in ["Building", "Testing"] {
        let mut tail = line;
        while let Some(i) = tail.find(word) {
            let after = &tail[i + word.len()..];
            if after.starts_with(' ') {
                let t: String = after
                    .trim_start()
                    .trim_end_matches(';')
                    .chars()
                    .take_while(|c| !c.is_whitespace())
                    .collect();
                active = Some(format!("{word} {t}"));
                break;
            }
            tail = &tail[i + word.len()..];
        }
        if active.is_some() {
            break;
        }
    }
    (fraction, active)
}

/// The strip's bar: solid block fill in purple, dim dotted track behind it.
/// Glyph and color both differ, so the fill reads even on faint terminals.
pub fn bar_line(fraction: Option<(u64, u64)>, width: usize) -> Line<'static> {
    let Some((done, total)) = fraction.filter(|(_, t)| *t > 0) else {
        return Line::from(Span::styled(
            "░".repeat(width),
            Style::new().fg(Color::Rgb(0x5A, 0x4A, 0x73)),
        ));
    };
    let filled = ((done as f64 / total as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    Line::from(vec![
        Span::styled(
            "█".repeat(filled),
            Style::new().fg(Color::Rgb(0xB0, 0x7C, 0xFF)),
        ),
        Span::styled(
            "░".repeat(width - filled),
            Style::new().fg(Color::Rgb(0x5A, 0x4A, 0x73)),
        ),
    ])
}

fn draw<B>(
    terminal: &mut Terminal<B>,
    out: &BepOutput,
    walker: Option<&WalkingCat>,
    console: &baztet_core::exec::ConsoleCapture,
    title: &str,
    settled: bool,
    elapsed: Duration,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    terminal.draw(|f| {
        let sprite_h = walker.map(|g| g.size.dims().1).unwrap_or(0);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3 + sprite_h)])
            .split(f.area());

        // verdict list: the active target first, failures first after it.
        // one capture + one pass per line: text() copies the 256 KiB tail
        // and the strip reads the same numbers below
        let console_text = console.text();
        let mut fraction: Option<(u64, u64)> = None;
        let mut active: Option<String> = None;
        for line in console_text.lines().rev() {
            let (f, a) = parse_status(line);
            fraction = fraction.or(f);
            active = active.or(a);
            // numbers and target usually ride one line: keep the pair consistent
            if fraction.is_some() && active.is_some() {
                break;
            }
        }
        let mut rows: Vec<Line> = Vec::new();
        if let Some(a) = &active {
            rows.push(Line::from(Span::styled(
                format!(" {}", a),
                Style::new().bold().fg(Color::Yellow),
            )));
        }
        let mut failed_rows: Vec<Line> = Vec::new();
        let mut flaky_rows: Vec<Line> = Vec::new();
        let mut ok_rows: Vec<Line> = Vec::new();
        for s in &out.test_summaries {
            let row = Line::from(Span::styled(
                format!(" {:<48} {}", s.label, s.status),
                verdict_style(&s.status),
            ));
            // a flaky test passed on retry: own bucket between the reds and
            // the greens, never at the head of the failure block
            if s.status == "FLAKY" {
                flaky_rows.push(row);
            } else if s.status != "PASSED" {
                failed_rows.push(row);
            } else {
                ok_rows.push(row);
            }
        }
        for (label, success) in &out.target_status {
            let verdict = if *success { "BUILT" } else { "FAILED" };
            let row = Line::from(Span::styled(
                format!(" {:<48} {}", label, verdict),
                verdict_style(verdict),
            ));
            if *success {
                ok_rows.push(row);
            } else {
                failed_rows.push(row);
            }
        }
        rows.extend(failed_rows);
        rows.extend(flaky_rows);
        rows.extend(ok_rows);
        if rows.is_empty() {
            rows.push(Line::from(Span::styled(
                " bazel analyzing…",
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

        // strip: status beat, purple progress bar, cat walking underneath
        const MARKS: [&str; 3] = ["▸ ", "▹ ", "● "];
        let mark = if settled {
            MARKS[0]
        } else {
            MARKS[(elapsed.as_millis() / 400) as usize % MARKS.len()]
        };
        // the walk's status: the active target when there is one, else
        // the raw last line — from the capture already parsed above
        let raw_progress = console.last_progress();
        let shown = active.unwrap_or(raw_progress);
        let (tw, _) = crossterm::terminal::size().unwrap_or((80, 24));
        let max_width = tw.saturating_sub(16) as usize;
        let shown: String = shown.chars().take(max_width).collect();
        let mut status = vec![
            Span::styled(mark, Style::new().bold().fg(Color::Yellow)),
            Span::raw(if shown.is_empty() {
                "bazel starting…".to_string()
            } else {
                shown
            }),
        ];
        if !settled {
            status.push(Span::raw(format!(" · {:.0}s", elapsed.as_secs_f64())));
        }
        let mut strip: Vec<Line> = vec![
            Line::from(status),
            bar_line(fraction, tw.saturating_sub(6) as usize),
        ];
        if let Some(walker) = walker {
            // the cat walks the whole strip: shift the sprite to its live
            // column; ratatui clips whatever sticks out on the right, and
            // a negative lead clamps at the left edge
            let (w, _) = walker.size.dims();
            let col = walker.pos.clamp(0, tw as i32 - w as i32).max(0) as usize;
            for l in walker.frame_lines() {
                strip.push(Line::from(Span::styled(
                    format!("{}{}", " ".repeat(col), l),
                    // the sprite wears the Bastet gold; one color for all
                    // cells keeps the density-shading effect intact
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

/// Read and decode the BEP bytes appended since `offset`; returns the
/// new cursor. bazel flushes the analysis batch, then appends every
/// finished test's events: the cursor feeds the live verdict list.
fn ingest(state: &mut BepOutput, path: &std::path::Path, offset: usize) -> usize {
    use std::io::{Read, Seek, SeekFrom};
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
        assert!(text.contains("//a:one"), "verdict row missing: {text}");
        assert!(text.contains("PASSED"), "verdict missing: {text}");
        assert!(text.contains("meow"), "goodbye missing: {text}");
    }

    #[test]
    fn cat_frames_render_at_their_live_column() {
        // the sprite follows pos: the walking cat actually crosses the strip
        let mut walker = WalkingCat::new(80, 20).unwrap();
        for _ in 0..25 {
            walker.tick();
        }
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(
            &mut terminal,
            &BepOutput::default(),
            Some(&walker),
            &baztet_core::exec::ConsoleCapture::default(),
            "test",
            false,
            Duration::from_secs(3),
        )
        .unwrap();
        // sprite row is the last strip row: its lead edge is past column 25,
        // so the left quarter of that row holds only spaces
        let line = terminal.backend().buffer().content();
        let row0 = (20 - 1) * 80;
        assert!(
            line[row0..row0 + 25].iter().all(|c| c.symbol() == " "),
            "sprite sits at column 0 after walking right: {:?}",
            line[row0..row0 + 40]
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        );
    }

    #[test]
    fn bar_fill_block_vs_dotted_track() {
        let bar = bar_line(Some((2101, 4278)), 40);
        let span_widths: Vec<usize> = bar
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .collect();
        assert_eq!(span_widths, vec![20, 20]);
        assert_eq!(bar.spans[0].content.chars().next(), Some('█'));
        assert_eq!(bar.spans[1].content.chars().next(), Some('░'));
        assert_ne!(bar.spans[0].content, bar.spans[1].content);
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

#[cfg(test)]
mod perf_tests {
    use super::*;

    #[test]
    fn parse_status_finds_the_fraction_and_the_building_target() {
        let line = "(18:47:49) [3,101 / 4,278] 19 / 432 tests, 2 failed; Building pkg/dx/agents/bff/services/customers-api/liball-external-targets-class.jar (2218 source files); 13s";
        let (fraction, active) = parse_status(line);
        assert_eq!(fraction, Some((3101, 4278)));
        assert_eq!(active.as_deref(), Some("Building pkg/dx/agents/bff/services/customers-api/liball-external-targets-class.jar"));
    }

    #[test]
    fn parse_status_testing_and_misses() {
        let (f, a) = parse_status(
            "(16:04:08) INFO: Analyzed 432 targets (0 packages loaded, 0 targets configured).",
        );
        assert_eq!(f, None);
        assert_eq!(a, None);
        let (_, a) = parse_status("Testing //pkg/dx/agents/bff/libs/cache-client:tests");
        assert_eq!(
            a.as_deref(),
            Some("Testing //pkg/dx/agents/bff/libs/cache-client:tests")
        );
    }

    #[test]
    fn bar_fill_math_and_track() {
        let bar = bar_line(Some((2101, 4278)), 40);
        // span widths in CHARS — ─/━ are 3-byte UTF-8, don't compare bytes
        let span_widths: Vec<usize> = bar
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .collect();
        assert_eq!(span_widths, vec![20, 20]);
        // empty track when no fraction
        let track = bar_line(None, 40);
        assert_eq!(track.spans[0].content.chars().count(), 40);
        // zero total = track
        let track = bar_line(Some((5, 0)), 40);
        assert_eq!(track.spans[0].content.chars().count(), 40);
    }

    #[test]
    fn verdict_styles_map() {
        assert_eq!(verdict_style("PASSED").fg, Some(Color::Green));
        assert_eq!(verdict_style("BUILT").fg, Some(Color::Green));
        assert_eq!(verdict_style("FAILED").fg, Some(Color::Red));
        assert_eq!(verdict_style("TIMEOUT").fg, Some(Color::Red));
        assert_eq!(verdict_style("FLAKY").fg, Some(Color::Yellow));
        assert_eq!(verdict_style("NO_STATUS").fg, Some(Color::Gray));
    }
}
