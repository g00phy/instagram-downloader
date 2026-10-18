//! The window: toolbar, Downloads / LinkGrabber / Settings / Log tabs, status bar.
//!
//! Each frame the app drains the controller's [`Update`]s into a [`ViewState`],
//! draws it, and sends the [`Command`]s the user's clicks produced. Nothing here
//! touches the network, the queue file or the archive.

mod dialogs;
mod downloads;
mod linkgrabber;
mod log;
mod settings;
mod style;
mod toolbar;

use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use eframe::egui;

use super::controller::{ControllerHandle, UiLink};
use super::protocol::{Command, LogLevel, LogLine, Update};
use super::state::ViewState;

/// How often the window refreshes while downloads or the LinkGrabber run.
const BUSY_REPAINT: Duration = Duration::from_millis(250);

/// The tabs under the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Downloads,
    LinkGrabber,
    Settings,
    Log,
}

/// What the views asked for during a frame; the app carries it out afterwards.
#[derive(Debug, Default)]
struct Outbox {
    commands: Vec<Command>,
    goto: Option<Tab>,
    notices: Vec<(LogLevel, String)>,
}

impl Outbox {
    /// Queues a command for the controller.
    fn send(&mut self, command: Command) {
        self.commands.push(command);
    }

    /// Switches to another tab after this frame.
    fn goto(&mut self, tab: Tab) {
        self.goto = Some(tab);
    }

    /// Takes over everything `other` asked for.
    fn merge(&mut self, other: Outbox) {
        self.commands.extend(other.commands);
        self.goto = other.goto.or(self.goto);
        self.notices.extend(other.notices);
    }

    /// Adds a line of the window's own to the Log tab (e.g. a file that would not open).
    fn notice(&mut self, level: LogLevel, text: impl Into<String>) {
        self.notices.push((level, text.into()));
    }
}

/// The eframe application.
pub struct IgdlApp {
    link: UiLink,
    controller: Option<ControllerHandle>,
    /// Set once the controller's channel closed (so it is reported only once).
    disconnected: bool,
    state: ViewState,
    tab: Tab,
    downloads: downloads::DownloadsView,
    grabber: linkgrabber::GrabberView,
    settings: settings::SettingsView,
    quit: dialogs::QuitDialog,
    outbox: Outbox,
}

impl IgdlApp {
    /// `controller` is `None` in tests (the test drives `link` directly).
    pub fn new(link: UiLink, controller: Option<ControllerHandle>) -> Self {
        IgdlApp {
            link,
            controller,
            disconnected: false,
            state: ViewState::default(),
            tab: Tab::Downloads,
            downloads: downloads::DownloadsView::default(),
            grabber: linkgrabber::GrabberView::default(),
            settings: settings::SettingsView::default(),
            quit: dialogs::QuitDialog::default(),
            outbox: Outbox::default(),
        }
    }

    /// Applies every update waiting in the channel, then lets the forms pick up
    /// new settings.
    fn drain_updates(&mut self, now: Instant) {
        loop {
            match self.link.updates.try_recv() {
                Ok(update) => self.state.apply(update, now),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Keep the controller's own `Fatal` message if it sent one.
                    if !self.disconnected && self.controller.is_some() && self.state.fatal.is_none()
                    {
                        self.state.apply(
                            Update::Fatal("The background worker stopped unexpectedly.".into()),
                            now,
                        );
                    }
                    self.disconnected = true;
                    break;
                }
            }
        }
        self.grabber.sync(&self.state);
        self.settings.sync(&self.state);
    }

    /// Sends queued commands, switches tabs, logs notices.
    fn flush(&mut self, ctx: &egui::Context) {
        for command in self.outbox.commands.drain(..) {
            // A closed channel means the controller is gone; `Fatal` already says so.
            let _ = self.link.commands.send(command);
        }
        if let Some(tab) = self.outbox.goto.take() {
            self.tab = tab;
            // The new tab is drawn on the next frame; make sure there is one.
            ctx.request_repaint();
        }
        let now = chrono::Utc::now().timestamp();
        for (level, text) in self.outbox.notices.drain(..) {
            self.state.push_log(LogLine {
                at: now,
                level,
                text,
            });
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(super::state::status_summary(&self.state.counts()));
                if self.state.grabbing > 0 {
                    ui.separator();
                    ui.add(egui::Spinner::new());
                    ui.label(format!(
                        "LinkGrabber: resolving {} link(s)…",
                        self.state.grabbing
                    ));
                }
            },
            |ui| {
                let mut hint = match &self.state.config_path {
                    Some(path) => format!("Config: {}", path.display()),
                    None => "Config: not saved".to_string(),
                };
                if let Some(data_dir) = &self.state.settings.data_dir {
                    hint = format!("Data: {} · {hint}", data_dir.display());
                }
                ui.add(egui::Label::new(egui::RichText::new(hint).small().weak()).truncate());
            },
        );
    }
}

impl IgdlApp {
    /// Per-frame background work: drain updates, poll the clipboard, handle a
    /// close request, send commands. [`eframe::App::logic`] calls it; tests
    /// call it directly (no eframe frame needed).
    pub fn tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.drain_updates(now);
        self.grabber.poll_clipboard(now, &mut self.outbox);
        self.quit.handle_close_request(ctx, &self.state);
        self.flush(ctx);
        if self.state.downloading || self.state.grabbing > 0 {
            ctx.request_repaint_after(BUSY_REPAINT);
        }
        if let Some(delay) = self.grabber.poll_delay(now) {
            ctx.request_repaint_after(delay);
        }
    }

    /// Draws the whole window into `ui`. [`eframe::App::ui`] calls it; tests
    /// call it directly.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        self.drain_updates(now);

        egui::Panel::top("igdl_toolbar").show(ui, |ui| {
            toolbar::show(ui, &self.state, &mut self.tab, &mut self.outbox, now);
        });
        egui::Panel::bottom("igdl_status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Downloads => self.downloads.show(ui, &self.state, &mut self.outbox, now),
            Tab::LinkGrabber => self.grabber.show(ui, &self.state, &mut self.outbox),
            Tab::Settings => self.settings.show(ui, &self.state, &mut self.outbox),
            Tab::Log => log::show(ui, &mut self.state),
        });

        self.quit.show(ui.ctx());
        dialogs::fatal(ui.ctx(), &self.state, &mut self.quit);
        self.flush(ui.ctx());
    }
}

impl eframe::App for IgdlApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(controller) = self.controller.take() {
            controller.shutdown();
        }
    }
}
