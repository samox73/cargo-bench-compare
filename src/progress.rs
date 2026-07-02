use std::io::{IsTerminal, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use regex::Regex;

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
    let mut painted = false;
    let mut last_paint = Instant::now() - Duration::from_secs(1);
    loop {
        let force = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::RunStart { run, total, side }) => {
                state.current = Some(Current {
                    run,
                    total,
                    side,
                    started: Instant::now(),
                    fraction: None,
                });
                true
            }
            Ok(Event::Fraction(f)) => {
                if let Some(current) = &mut state.current {
                    current.fraction = Some(f.clamp(0.0, 1.0));
                }
                false
            }
            Ok(Event::RunEnd) => {
                if let Some(current) = state.current.take() {
                    let secs = current.started.elapsed().as_secs_f64();
                    match current.side {
                        Side::Base => state.base.push(secs),
                        Side::Candidate => state.candidate.push(secs),
                    }
                }
                false
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if painted {
                    let _ = write!(std::io::stderr(), "\r\x1b[K");
                    let _ = std::io::stderr().flush();
                }
                return;
            }
        };
        if (force || last_paint.elapsed() >= Duration::from_millis(100))
            && let Some(current) = state.current
        {
            let elapsed = current.started.elapsed().as_secs_f64();
            let line = compose_line(
                current.run,
                current.total,
                current.side,
                current.fraction,
                elapsed,
                eta_secs(&state),
            );
            let _ = write!(std::io::stderr(), "\r\x1b[K{line}");
            let _ = std::io::stderr().flush();
            painted = true;
            last_paint = Instant::now();
        }
    }
}

fn compose_line(
    run: u32,
    total: u32,
    side: Side,
    fraction: Option<f64>,
    elapsed_secs: f64,
    eta_secs: Option<f64>,
) -> String {
    let mut parts = vec![
        format!("run {run}/{total}"),
        match side {
            Side::Base => "base".to_owned(),
            Side::Candidate => "candidate".to_owned(),
        },
    ];
    if let Some(fraction) = fraction {
        parts.push(format!("{}%", (fraction * 100.0).floor() as u32));
    }
    parts.push(fmt_duration(elapsed_secs));
    if let Some(eta) = eta_secs {
        parts.push(format!("eta ~{}", fmt_duration(eta)));
    }
    parts.join(" · ")
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
    fn compose_line_variants() {
        assert_eq!(
            compose_line(3, 10, Side::Candidate, None, 4.2, None),
            "run 3/10 · candidate · 4.2s"
        );
        assert_eq!(
            compose_line(3, 10, Side::Candidate, Some(0.37), 4.2, Some(41.0)),
            "run 3/10 · candidate · 37% · 4.2s · eta ~41.0s"
        );
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
