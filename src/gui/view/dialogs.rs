//! Window-level modals: "quit while downloading?" and the fatal-error screen.

use eframe::egui::{self, Id, Modal, RichText, ViewportCommand};

use crate::gui::state::ViewState;

/// Asks before closing the window while downloads run.
#[derive(Debug, Default)]
pub(super) struct QuitDialog {
    /// The question is on screen.
    asking: bool,
    /// The user agreed to quit (or there is nothing to protect any more).
    confirmed: bool,
}

impl QuitDialog {
    /// Cancels a close request while downloads run and shows the question instead.
    pub fn handle_close_request(&mut self, ctx: &egui::Context, state: &ViewState) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if state.downloading && !self.confirmed && state.fatal.is_none() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.asking = true;
        }
    }

    /// Closes the window for good.
    pub fn quit(&mut self, ctx: &egui::Context) {
        self.confirmed = true;
        self.asking = false;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.asking {
            return;
        }
        let response = Modal::new(Id::unique("igdl_quit_dialog")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading("Quit igdl?");
            ui.add_space(4.0);
            ui.label(
                "Downloads are running. Quit anyway? Partial files are kept and resume next time.",
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Quit").clicked() {
                    self.quit(ui.ctx());
                }
                if ui.button("Cancel").clicked() {
                    self.asking = false;
                }
            });
        });
        if response.should_close() {
            self.asking = false;
        }
    }
}

/// The controller cannot work: show why, and offer only `Quit`.
pub(super) fn fatal(ctx: &egui::Context, state: &ViewState, quit: &mut QuitDialog) {
    let Some(message) = &state.fatal else {
        return;
    };
    Modal::new(Id::unique("igdl_fatal_dialog")).show(ctx, |ui| {
        ui.set_max_width(440.0);
        ui.heading(RichText::new("⚠ igdl cannot continue").color(ui.visuals().error_fg_color));
        ui.add_space(4.0);
        ui.label(message);
        ui.add_space(8.0);
        if ui.button("Quit").clicked() {
            quit.quit(ui.ctx());
        }
    });
}
