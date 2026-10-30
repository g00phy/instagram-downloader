//! Terminal rendering of download progress (stderr) and of result paths (stdout).
//!
//! On a TTY each running transfer gets an indicatif bar under an overall bar;
//! otherwise one line per finished file is printed. `tracing` output is routed
//! through [`StderrWriter`] so log lines never tear the bars.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use indicatif::{HumanBytes, HumanDuration, MultiProgress, ProgressBar, ProgressStyle};

use crate::download::{Event, Outcome};
use crate::error::ErrorKind;

/// The process-wide bar container, shared with the `tracing` writer.
static MULTI: OnceLock<MultiProgress> = OnceLock::new();

fn multi() -> &'static MultiProgress {
    MULTI.get_or_init(MultiProgress::new)
}

/// `io::Write` for `tracing_subscriber` that suspends progress bars while a log
/// line is written.
pub struct StderrWriter;

impl Write for StderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match MULTI.get() {
            Some(multi) => multi.suspend(|| io::stderr().write(buf)),
            None => io::stderr().write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

/// How progress is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Live bars (stderr is a terminal and bars were not disabled).
    Bars,
    /// One line per finished, retried or failed file.
    Lines,
    /// Failures only.
    Quiet,
}

impl Style {
    /// Bars on a terminal, lines otherwise; `quiet` wins.
    pub fn detect(no_progress: bool, quiet: bool) -> Style {
        if quiet {
            Style::Quiet
        } else if !no_progress && io::stderr().is_terminal() {
            Style::Bars
        } else {
            Style::Lines
        }
    }
}

/// Totals of one download run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub done: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub bytes: u64,
}

/// Renders [`Event`]s for one download run.
pub struct Progress {
    style: Style,
    overall: Option<ProgressBar>,
    bars: HashMap<u64, ProgressBar>,
    names: HashMap<u64, String>,
    started: Instant,
    totals: Totals,
}

impl Progress {
    /// Starts rendering a run of `files` transfers.
    pub fn new(style: Style, files: usize) -> Self {
        let overall = (style == Style::Bars).then(|| {
            let bar = multi().add(ProgressBar::new(files as u64));
            bar.set_style(overall_style());
            bar.enable_steady_tick(Duration::from_millis(250));
            bar
        });
        Progress {
            style,
            overall,
            bars: HashMap::new(),
            names: HashMap::new(),
            started: Instant::now(),
            totals: Totals::default(),
        }
    }

    /// Prints a status line on stderr without disturbing the bars.
    pub fn note(&self, message: &str) {
        note(message);
    }

    /// Updates the display for one engine event.
    pub fn handle(&mut self, event: &Event) {
        match event {
            Event::Started {
                item_id,
                name,
                total,
                resumed_from,
            } => {
                self.names.insert(*item_id, name.clone());
                if self.style == Style::Bars {
                    let bar = self
                        .bars
                        .entry(*item_id)
                        .or_insert_with(|| multi().add(ProgressBar::no_length()));
                    bar.set_style(file_style(total.is_some()));
                    bar.set_message(name.clone());
                    if let Some(total) = total {
                        bar.set_length(*total);
                    }
                    bar.set_position(*resumed_from);
                }
            }
            Event::Progress {
                item_id,
                done,
                total,
            } => {
                if let Some(bar) = self.bars.get(item_id) {
                    if let Some(total) = total {
                        bar.set_length(*total);
                    }
                    bar.set_position(*done);
                }
            }
            Event::Retrying {
                item_id,
                attempt,
                delay,
                reason,
            } => {
                if self.style != Style::Quiet {
                    note(&format!(
                        "retry #{attempt} in {}: {} ({reason})",
                        HumanDuration(*delay),
                        self.name(*item_id)
                    ));
                }
            }
            Event::Refreshed { item_id, .. } => {
                tracing::info!("refreshed expired link for {}", self.name(*item_id));
            }
            Event::Finished { item_id, outcome } => self.finish_item(*item_id, outcome),
        }
    }

    fn finish_item(&mut self, item_id: u64, outcome: &Outcome) {
        if let Some(bar) = self.bars.remove(&item_id) {
            bar.finish_and_clear();
            multi().remove(&bar);
        }
        let name = self.name(item_id);
        match outcome {
            Outcome::Done { bytes, .. } => {
                self.totals.done += 1;
                self.totals.bytes += bytes;
                if self.style == Style::Lines {
                    note(&format!("done    {name} ({})", HumanBytes(*bytes)));
                }
            }
            Outcome::Skipped { reason, .. } => {
                self.totals.skipped += 1;
                if self.style == Style::Lines {
                    note(&format!("skipped {name}: {reason}"));
                }
            }
            Outcome::Failed { error } if error.kind == ErrorKind::Cancelled => {
                self.totals.cancelled += 1;
            }
            Outcome::Failed { error } => {
                self.totals.failed += 1;
                note(&format!("failed  {name}: {}", error.message));
            }
        }
        if let Some(overall) = &self.overall {
            overall.inc(1);
            overall.set_message(format!(
                "{} downloaded{}",
                HumanBytes(self.totals.bytes),
                if self.totals.failed > 0 {
                    format!(" · {} failed", self.totals.failed)
                } else {
                    String::new()
                }
            ));
        }
    }

    /// Prints a finished file's path on stdout (the machine-readable result).
    pub fn emit_path(&self, path: &Path) {
        let line = path.display().to_string();
        let print = || {
            let mut out = io::stdout().lock();
            // A closed stdout (e.g. `| head`) must not abort the run.
            let _ = writeln!(out, "{line}");
        };
        match self.style {
            Style::Bars => multi().suspend(print),
            _ => print(),
        }
    }

    /// Clears the bars and returns the totals.
    pub fn finish(mut self) -> Totals {
        for (_, bar) in self.bars.drain() {
            bar.finish_and_clear();
            multi().remove(&bar);
        }
        if let Some(overall) = self.overall.take() {
            overall.finish_and_clear();
            multi().remove(&overall);
        }
        self.totals
    }

    /// Time since the run started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn name(&self, item_id: u64) -> String {
        self.names
            .get(&item_id)
            .cloned()
            .unwrap_or_else(|| format!("#{item_id}"))
    }
}

/// Prints a line on stderr above any active bars.
pub fn note(message: &str) {
    match MULTI.get() {
        Some(multi) if !multi.is_hidden() => {
            let _ = multi.println(message);
        }
        _ => eprintln!("{message}"),
    }
}

/// Spinner frames for fonts without braille glyphs (the Windows console's
/// default fonts), last one shown when finished.
const ASCII_TICKS: &str = "|/-\\ ";

fn overall_style() -> ProgressStyle {
    let style = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] {bar:30.cyan/blue} {pos}/{len} files · {msg}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar());
    if cfg!(windows) {
        // Cannot panic: `tick_chars` asserts at least two frames and there are five.
        style.tick_chars(ASCII_TICKS)
    } else {
        style
    }
}

fn file_style(known_length: bool) -> ProgressStyle {
    let template = if known_length {
        "  {wide_msg} {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>11} {eta:>4}"
    } else {
        "  {wide_msg} {bytes:>10} {bytes_per_sec:>11}"
    };
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_bar())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_spinner_frames_are_valid() {
        assert!(ASCII_TICKS.is_ascii());
        // `tick_chars` panics with fewer than two frames (one spinning + the final one).
        let _ = ProgressStyle::default_spinner().tick_chars(ASCII_TICKS);
        let _ = overall_style();
    }
}
