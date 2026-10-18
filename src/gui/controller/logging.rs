//! [`UiLogLayer`]: `tracing` events → Log tab lines of the running controller.
//!
//! Events are handed to the controller's event loop over a channel instead of
//! being sent to the window directly, so `on_event` never calls back into the
//! window (an event emitted while the window holds its own locks must not
//! re-enter it) and never blocks.

use std::fmt;
use std::sync::{PoisonError, RwLock, TryLockError};

use tokio::sync::mpsc::UnboundedSender;
use tracing::field::{Field, Visit};
use tracing::{Level, Subscriber};
use tracing_subscriber::layer::Context;

use super::sink::log_line;
use crate::gui::protocol::{LogLevel, LogLine};

/// Where the running controller receives forwarded events (`None`: no controller).
static FORWARD: RwLock<Option<UnboundedSender<LogLine>>> = RwLock::new(None);

/// Makes `sender` the destination of [`UiLogLayer`] events.
pub(super) fn install(sender: UnboundedSender<LogLine>) {
    *FORWARD.write().unwrap_or_else(PoisonError::into_inner) = Some(sender);
}

/// Stops forwarding, unless another controller installed its own sender since.
pub(super) fn uninstall(sender: &UnboundedSender<LogLine>) {
    let mut slot = FORWARD.write().unwrap_or_else(PoisonError::into_inner);
    if slot
        .as_ref()
        .is_some_and(|current| current.same_channel(sender))
    {
        *slot = None;
    }
}

/// `tracing` layer that forwards `igdl` events (INFO and above) to the running
/// controller's window as [`crate::gui::protocol::Update::Log`] lines (redacted).
/// A no-op while no controller runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct UiLogLayer;

impl<S: Subscriber> tracing_subscriber::Layer<S> for UiLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let slot = match FORWARD.try_read() {
            Ok(slot) => slot,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            // A controller is starting or stopping right now: drop the event.
            Err(TryLockError::WouldBlock) => return,
        };
        let Some(sender) = slot.as_ref() else {
            return;
        };
        if sender.is_closed() {
            return;
        }
        let level = match *event.metadata().level() {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            _ => LogLevel::Info,
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        let _ = sender.send(log_line(level, &fields.text()));
    }
}

/// Collects an event's `message` and its other fields as `key=value`.
#[derive(Debug, Default)]
struct Fields {
    message: String,
    extra: Vec<String>,
}

impl Fields {
    fn add(&mut self, field: &Field, value: String) {
        let name = field.name();
        if name == "message" {
            self.message = value;
        } else if !name.starts_with("log.") {
            // `log.*` fields are metadata added by the `log` → `tracing` bridge.
            self.extra.push(format!("{name}={value}"));
        }
    }

    fn text(&self) -> String {
        let mut parts = Vec::with_capacity(self.extra.len() + 1);
        if !self.message.is_empty() {
            parts.push(self.message.clone());
        }
        parts.extend(self.extra.iter().cloned());
        parts.join(" ")
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.add(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.add(field, format!("{value:?}"));
    }
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    #[test]
    fn forwards_redacted_events_only_while_installed() {
        let subscriber = tracing_subscriber::registry().with(UiLogLayer);
        tracing::subscriber::with_default(subscriber, || {
            // Nothing installed: a no-op.
            tracing::warn!("before");

            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            install(sender.clone());
            tracing::warn!(
                user = "natgeo",
                count = 3,
                "cookie sessionid=SECRET-VALUE-42 rejected"
            );
            tracing::info!("fetched {} posts", 12);
            tracing::error!(url = "https://cdn.example/x.jpg?oh=SIGNED&oe=1", "failed");
            uninstall(&sender);
            tracing::warn!("after");

            let lines: Vec<LogLine> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
            assert_eq!(lines.len(), 3, "{lines:?}");
            assert_eq!(lines[0].level, LogLevel::Warn);
            assert!(
                lines[0].text.starts_with("cookie sessionid=***"),
                "{lines:?}"
            );
            assert!(lines[0].text.ends_with("user=natgeo count=3"), "{lines:?}");
            assert!(!lines[0].text.contains("SECRET-VALUE-42"));
            assert_eq!(lines[1].level, LogLevel::Info);
            assert_eq!(lines[1].text, "fetched 12 posts");
            assert_eq!(lines[2].level, LogLevel::Error);
            assert!(!lines[2].text.contains("SIGNED"), "{lines:?}");
        });
    }
}
