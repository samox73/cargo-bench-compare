use std::io::IsTerminal;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;

/// Bar resolution per measurement run: each run owns an equal segment of the
/// bar, and within-run fractions (from --progress-regex) fill it continuously.
const UNITS_PER_RUN: u64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Base,
    Candidate,
}

pub enum Event {
    RunStart { run: u32, total: u32, side: Side },
    Fraction(f64),
    RunEnd,
}

pub struct Progress {
    tx: Option<mpsc::Sender<Event>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Progress {
    pub fn new(enabled: bool) -> Progress {
        if !enabled || !std::io::stderr().is_terminal() {
            return Progress {
                tx: None,
                handle: None,
            };
        }
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || render(rx));
        Progress {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    pub fn send(&self, event: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }

    pub fn sender(&self) -> Option<mpsc::Sender<Event>> {
        self.tx.clone()
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct State {
    current: Option<Current>,
    base: Vec<f64>,
    candidate: Vec<f64>,
}

#[derive(Clone, Copy)]
struct Current {
    run: u32,
    total: u32,
    side: Side,
    started: Instant,
    fraction: Option<f64>,
}

fn render(rx: mpsc::Receiver<Event>) {
    let mut state = State {
        current: None,
        base: Vec::new(),
        candidate: Vec::new(),
    };
    let mut bar: Option<ProgressBar> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::RunStart { run, total, side }) => {
                state.current = Some(Current {
                    run,
                    total,
                    side,
                    started: Instant::now(),
                    fraction: None,
                });
                let bar = bar.get_or_insert_with(|| new_bar(total));
                bar.set_style(bar_style(side));
                bar.set_prefix(prefix_for(run, total, side));
            }
            Ok(Event::Fraction(f)) => {
                if let Some(current) = &mut state.current {
                    current.fraction = Some(f.clamp(0.0, 1.0));
                }
            }
            Ok(Event::RunEnd) => {
                if let Some(current) = state.current.take() {
                    let secs = current.started.elapsed().as_secs_f64();
                    match current.side {
                        Side::Base => state.base.push(secs),
                        Side::Candidate => state.candidate.push(secs),
                    }
                    if let Some(bar) = &bar {
                        bar.set_position(u64::from(current.run) * UNITS_PER_RUN);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(bar) = &bar {
                    bar.finish_and_clear();
                }
                return;
            }
        }
        if let (Some(bar), Some(current)) = (&bar, state.current) {
            bar.set_position(bar_position(current.run, current.fraction));
            bar.set_message(compose_message(
                current.started.elapsed().as_secs_f64(),
                eta_secs(&state),
            ));
        }
    }
}

fn new_bar(total_runs: u32) -> ProgressBar {
    let bar = ProgressBar::new(u64::from(total_runs) * UNITS_PER_RUN);
    bar.enable_steady_tick(Duration::from_millis(100));
    bar
}

/// KITT scanner spinner: a dot sweeping back and forth inside a bracket.
/// All animation frames are 7 cells wide; the last entry is indicatif's
/// "finished" frame — never visible here (the bar is cleared) but kept
/// width-consistent anyway.
const TICK_FRAMES: &[&str] = &[
    "▐●    ▌",
    "▐ ●   ▌",
    "▐  ●  ▌",
    "▐   ● ▌",
    "▐    ●▌",
    "▐   ● ▌",
    "▐  ●  ▌",
    "▐ ●   ▌",
    "▐     ▌",
];

/// One-line build status on stderr: KITT spinner, side-tinted "building
/// base/candidate" label, the latest cargo status line, and elapsed time.
/// Returns None when `side` is None (--no-progress) or stderr is not a
/// terminal — callers then stream cargo's output unchanged instead.
pub fn build_status_bar(side: Option<Side>) -> Option<ProgressBar> {
    let side = side?;
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let color = side_color(side);
    let bar = ProgressBar::new_spinner();
    bar.set_style(
        ProgressStyle::with_template(&format!(
            "{{spinner}} {{prefix:.{color}.bold}} · {{msg}} · {{elapsed}}"
        ))
        .expect("progress template must parse")
        .tick_strings(TICK_FRAMES),
    );
    bar.set_prefix(format!("building {}", side_label(side)));
    bar.set_message("starting cargo");
    bar.enable_steady_tick(Duration::from_millis(100));
    Some(bar)
}

/// Spinner and bar stay uncolored; only the "run X/Y · side" prefix is tinted
/// by the side currently measured: cyan = base, magenta = candidate. Styling
/// lives entirely in the template so the prefix/message helpers stay plain,
/// testable strings.
fn bar_style(side: Side) -> ProgressStyle {
    let color = side_color(side);
    ProgressStyle::with_template(&format!(
        "{{spinner}} {{bar:42}} {{percent:>3.bold}}% · {{prefix:.{color}.bold}} · {{msg}}"
    ))
    .expect("progress template must parse")
    .progress_chars("█▉▊▋▌▍▎▏░")
    .tick_strings(TICK_FRAMES)
}

fn side_color(side: Side) -> &'static str {
    match side {
        Side::Base => "cyan",
        Side::Candidate => "magenta",
    }
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Base => "base",
        Side::Candidate => "candidate",
    }
}

fn prefix_for(run: u32, total: u32, side: Side) -> String {
    format!("run {run}/{total} · {}", side_label(side))
}

/// The bar spans all runs equally: run `run` (1-based) fills the segment
/// [(run-1)/total, run/total], continuously when a within-run fraction is known.
fn bar_position(run: u32, fraction: Option<f64>) -> u64 {
    let base = (u64::from(run) - 1) * UNITS_PER_RUN;
    let within = fraction.unwrap_or(0.0).clamp(0.0, 1.0) * UNITS_PER_RUN as f64;
    base + within as u64
}

fn compose_message(elapsed_secs: f64, eta_secs: Option<f64>) -> String {
    match eta_secs {
        Some(eta) => format!(
            "{} · eta ~{}",
            fmt_duration(elapsed_secs),
            fmt_duration(eta)
        ),
        None => fmt_duration(elapsed_secs),
    }
}

fn fmt_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        let secs = secs as u64;
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        let mins = secs as u64 / 60;
        format!("{}h{:02}m", mins / 60, mins % 60)
    }
}

fn eta_secs(state: &State) -> Option<f64> {
    let current = state.current?;
    let mean = mean_for(state, current.side)?;
    let elapsed = current.started.elapsed().as_secs_f64();
    let remainder = if let Some(fraction) = current.fraction.filter(|f| *f >= 0.01) {
        elapsed * (1.0 - fraction) / fraction
    } else {
        (mean - elapsed).max(0.0)
    };
    let remaining = ((current.run + 1)..=current.total)
        .map(side_for_run)
        .filter_map(|side| mean_for(state, side))
        .sum::<f64>();
    Some(remainder + remaining)
}

fn mean_for(state: &State, side: Side) -> Option<f64> {
    let values = match side {
        Side::Base => &state.base,
        Side::Candidate => &state.candidate,
    };
    if !values.is_empty() {
        return Some(values.iter().sum::<f64>() / values.len() as f64);
    }
    let count = state.base.len() + state.candidate.len();
    (count > 0).then(|| {
        (state.base.iter().sum::<f64>() + state.candidate.iter().sum::<f64>()) / count as f64
    })
}

fn side_for_run(run: u32) -> Side {
    if run % 2 == 1 {
        Side::Base
    } else {
        Side::Candidate
    }
}

pub fn progress_fraction(pattern: &Regex, line: &str) -> Option<f64> {
    let caps = pattern.captures(line)?;
    let fraction = if let (Some(done), Some(total)) = (caps.name("done"), caps.name("total")) {
        fraction_from(done.as_str(), total.as_str())?
    } else if caps.len() >= 3 {
        fraction_from(caps.get(1)?.as_str(), caps.get(2)?.as_str())?
    } else {
        let value = caps
            .name("percent")
            .or_else(|| caps.get(1))?
            .as_str()
            .parse::<f64>()
            .ok()?;
        value / 100.0
    };
    Some(fraction.clamp(0.0, 1.0))
}

fn fraction_from(done: &str, total: &str) -> Option<f64> {
    let done = done.parse::<f64>().ok()?;
    let total = total.parse::<f64>().ok()?;
    (total > 0.0).then_some(done / total)
}

#[derive(Default)]
pub struct LineScanner {
    partial: Vec<u8>,
    discarding: bool,
}

impl LineScanner {
    pub fn push(&mut self, chunk: &[u8], on_line: &mut dyn FnMut(&str)) {
        for &byte in chunk {
            if byte == b'\n' || byte == b'\r' {
                if !self.discarding && !self.partial.is_empty() {
                    let line = String::from_utf8_lossy(&self.partial);
                    on_line(&line);
                }
                self.partial.clear();
                self.discarding = false;
            } else if self.discarding {
                continue;
            } else {
                self.partial.push(byte);
                if self.partial.len() > 64 * 1024 {
                    self.partial.clear();
                    self.discarding = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_fraction_two_groups() {
        let pattern = Regex::new(r"step (\d+)/(\d+)").unwrap();
        assert_eq!(progress_fraction(&pattern, "step 5/20"), Some(0.25));
        assert_eq!(progress_fraction(&pattern, "step 5/0"), None);
    }

    #[test]
    fn progress_fraction_named_groups() {
        let pattern = Regex::new(r"(?P<done>\d+) of (?P<total>\d+)").unwrap();
        assert_eq!(progress_fraction(&pattern, "7 of 10"), Some(0.7));
    }

    #[test]
    fn progress_fraction_percent_and_clamp() {
        let pattern = Regex::new(r"(\d+(?:\.\d+)?)%").unwrap();
        assert_eq!(progress_fraction(&pattern, "42%"), Some(0.42));
        assert_eq!(progress_fraction(&pattern, "150%"), Some(1.0));
    }

    #[test]
    fn progress_fraction_unparseable() {
        let pattern = Regex::new(r"step ([a-z]+)/(\d+)").unwrap();
        assert_eq!(progress_fraction(&pattern, "step five/20"), None);
    }

    #[test]
    fn line_scanner_reassembles_chunks() {
        let mut scanner = LineScanner::default();
        let mut lines = Vec::new();
        scanner.push(b"step 1", &mut |line| lines.push(line.to_owned()));
        scanner.push(b"/10\nstep 2/10\rstep", &mut |line| {
            lines.push(line.to_owned())
        });
        assert_eq!(lines, ["step 1/10", "step 2/10"]);
        scanner.push(b" 3/10\n", &mut |line| lines.push(line.to_owned()));
        assert_eq!(lines, ["step 1/10", "step 2/10", "step 3/10"]);
    }

    #[test]
    fn line_scanner_discards_overlong_partial() {
        let mut scanner = LineScanner::default();
        let mut lines = Vec::new();
        scanner.push(&vec![b'x'; 64 * 1024 + 1], &mut |line| {
            lines.push(line.to_owned())
        });
        scanner.push(b"tail\nok\n", &mut |line| lines.push(line.to_owned()));
        assert_eq!(lines, ["ok"]);
    }

    #[test]
    fn fmt_duration_formats() {
        assert_eq!(fmt_duration(4.24), "4.2s");
        assert_eq!(fmt_duration(63.0), "1m03s");
        assert_eq!(fmt_duration(3723.0), "1h02m");
    }

    #[test]
    fn compose_message_variants() {
        assert_eq!(compose_message(4.2, None), "4.2s");
        assert_eq!(compose_message(4.2, Some(41.0)), "4.2s · eta ~41.0s");
    }

    #[test]
    fn prefix_names_run_and_side() {
        assert_eq!(prefix_for(3, 10, Side::Candidate), "run 3/10 · candidate");
        assert_eq!(prefix_for(1, 4, Side::Base), "run 1/4 · base");
    }

    #[test]
    fn tick_frames_share_one_width() {
        // frames of differing widths would make the line jitter every tick
        for frame in TICK_FRAMES {
            assert_eq!(frame.chars().count(), 7, "frame {frame:?}");
        }
    }

    #[test]
    fn bar_styles_parse_for_both_sides() {
        // bar_style panics on an invalid template; the render thread would
        // swallow that panic, so pin it down here
        let _ = bar_style(Side::Base);
        let _ = bar_style(Side::Candidate);
    }

    #[test]
    fn bar_position_fills_one_segment_per_run() {
        assert_eq!(bar_position(1, None), 0);
        assert_eq!(bar_position(1, Some(0.5)), UNITS_PER_RUN / 2);
        assert_eq!(bar_position(2, None), UNITS_PER_RUN);
        assert_eq!(
            bar_position(3, Some(0.25)),
            2 * UNITS_PER_RUN + UNITS_PER_RUN / 4
        );
        assert_eq!(bar_position(4, Some(1.5)), 4 * UNITS_PER_RUN);
    }

    #[test]
    fn eta_from_fraction_and_means() {
        let state = State {
            current: Some(Current {
                run: 2,
                total: 4,
                side: Side::Candidate,
                started: Instant::now() - Duration::from_secs(1),
                fraction: Some(0.5),
            }),
            base: vec![2.0],
            candidate: Vec::new(),
        };
        assert!(eta_secs(&state).is_some_and(|eta| (eta - 5.0).abs() < 0.05));
    }
}
