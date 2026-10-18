//! Sending [`Update`]s to the window.

use std::sync::mpsc::Sender;

use super::Notify;
use crate::gui::protocol::{LogLevel, LogLine, Update};
use crate::redact;

/// The controller's end of the update channel; every send wakes the window.
#[derive(Clone)]
pub(super) struct Sink {
    updates: Sender<Update>,
    notify: Notify,
}

impl Sink {
    pub(super) fn new(updates: Sender<Update>, notify: Notify) -> Self {
        Sink { updates, notify }
    }

    /// Sends one update and asks the window to repaint. A closed window is ignored.
    pub(super) fn send(&self, update: Update) {
        if self.updates.send(update).is_ok() {
            (self.notify)();
        }
    }

    /// Sends a redacted log line.
    pub(super) fn log(&self, level: LogLevel, text: impl AsRef<str>) {
        self.send(Update::Log(log_line(level, text.as_ref())));
    }

    pub(super) fn info(&self, text: impl AsRef<str>) {
        self.log(LogLevel::Info, text);
    }

    pub(super) fn warn(&self, text: impl AsRef<str>) {
        self.log(LogLevel::Warn, text);
    }

    pub(super) fn error(&self, text: impl AsRef<str>) {
        self.log(LogLevel::Error, text);
    }
}

/// A log line stamped now, with secrets and signed URL parameters removed.
pub(super) fn log_line(level: LogLevel, text: &str) -> LogLine {
    LogLine {
        at: chrono::Utc::now().timestamp(),
        level,
        text: redact::redact(text),
    }
}
