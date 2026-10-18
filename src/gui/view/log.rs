//! The Log tab: what the controller (and the window) reported, newest at the bottom.

use chrono::{Local, TimeZone};
use eframe::egui::{self, Label, RichText, ScrollArea, Ui};

use super::style::Palette;
use crate::gui::protocol::{LogLevel, LogLine};
use crate::gui::state::ViewState;

/// Draws the log; looking at it marks its errors as seen.
pub(super) fn show(ui: &mut Ui, state: &mut ViewState) {
    if state.unseen_errors() > 0 {
        state.mark_log_seen();
        // The toolbar badge was already drawn this frame.
        ui.ctx().request_repaint();
    }
    let palette = Palette::of(ui);
    let mut clear = false;
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.label(RichText::new(format!("{} line(s)", state.log().len())).weak());
        },
        |ui| {
            if ui
                .add_enabled(!state.log().is_empty(), egui::Button::new("Clear"))
                .clicked()
            {
                clear = true;
            }
            if ui
                .add_enabled(!state.log().is_empty(), egui::Button::new("Copy all"))
                .clicked()
            {
                let text: String = state
                    .log()
                    .iter()
                    .map(|line| format_line(line) + "\n")
                    .collect();
                ui.ctx().copy_text(text);
            }
        },
    );
    if clear {
        state.clear_log();
    }
    ui.separator();

    let lines = state.log();
    if lines.is_empty() {
        ui.label(RichText::new("Nothing logged yet.").weak());
        return;
    }
    let row_height = ui
        .text_style_height(&egui::TextStyle::Body)
        .max(ui.text_style_height(&egui::TextStyle::Monospace));
    ScrollArea::vertical()
        .id_salt("igdl_log")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show_rows(ui, row_height, lines.len(), |ui, range| {
            for line in lines.range(range) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(local_time(line.at)).monospace().weak());
                    let (level, color) = match line.level {
                        LogLevel::Info => ("INFO ", palette.gray),
                        LogLevel::Warn => ("WARN ", palette.amber),
                        LogLevel::Error => ("ERROR", palette.red),
                    };
                    ui.label(RichText::new(level).monospace().strong().color(color));
                    let text = RichText::new(&line.text);
                    let text = if line.level == LogLevel::Error {
                        text.color(color)
                    } else {
                        text
                    };
                    ui.add(Label::new(text).truncate());
                });
            }
        });
}

/// `HH:MM:SS` in local time.
fn local_time(at: i64) -> String {
    Local
        .timestamp_opt(at, 0)
        .single()
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_string())
}

/// One line as plain text, for "Copy all".
fn format_line(line: &LogLine) -> String {
    let level = match line.level {
        LogLevel::Info => "INFO",
        LogLevel::Warn => "WARN",
        LogLevel::Error => "ERROR",
    };
    format!("{} {level} {}", local_time(line.at), line.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_lines() {
        let time = local_time(1_714_570_620);
        assert_eq!(time.len(), 8);
        assert_eq!(time.matches(':').count(), 2);
        let line = LogLine {
            at: 1_714_570_620,
            level: LogLevel::Warn,
            text: "rate limited, waiting 30s".into(),
        };
        assert_eq!(
            format_line(&line),
            format!("{time} WARN rate limited, waiting 30s")
        );
    }
}
