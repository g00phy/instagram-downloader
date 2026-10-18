//! Colours and small decorations shared by the views, tuned for both themes.

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Ui};

use crate::model::ItemStatus;

/// Status colours for the current theme.
#[derive(Debug, Clone, Copy)]
pub(super) struct Palette {
    pub blue: Color32,
    pub green: Color32,
    pub red: Color32,
    pub amber: Color32,
    /// Queued / secondary text.
    pub gray: Color32,
    /// Skipped / de-emphasised text.
    pub muted: Color32,
}

impl Palette {
    /// The palette matching `ui`'s theme.
    pub fn of(ui: &Ui) -> Self {
        let visuals = ui.visuals();
        if visuals.dark_mode {
            Palette {
                blue: Color32::from_rgb(110, 170, 255),
                green: Color32::from_rgb(100, 205, 125),
                red: Color32::from_rgb(255, 115, 105),
                amber: Color32::from_rgb(240, 185, 70),
                gray: Color32::from_rgb(170, 170, 170),
                muted: visuals.weak_text_color(),
            }
        } else {
            Palette {
                blue: Color32::from_rgb(20, 95, 200),
                green: Color32::from_rgb(25, 130, 55),
                red: Color32::from_rgb(195, 35, 30),
                amber: Color32::from_rgb(160, 100, 0),
                gray: Color32::from_rgb(95, 95, 95),
                muted: visuals.weak_text_color(),
            }
        }
    }

    /// Colour of an item status label.
    pub fn status(&self, status: ItemStatus) -> Color32 {
        match status {
            ItemStatus::Queued => self.gray,
            ItemStatus::Downloading => self.blue,
            ItemStatus::Done => self.green,
            ItemStatus::Skipped => self.muted,
            ItemStatus::Failed => self.red,
        }
    }
}

/// Display name of an item status.
pub(super) fn status_text(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Queued => "Queued",
        ItemStatus::Downloading => "Downloading",
        ItemStatus::Done => "Done",
        ItemStatus::Skipped => "Skipped",
        ItemStatus::Failed => "Failed",
    }
}

/// A rounded, filled button ("chip") with readable text on `color`.
pub(super) fn chip(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    let fill = color.gamma_multiply(if ui.visuals().dark_mode { 0.25 } else { 0.15 });
    ui.add(
        egui::Button::new(RichText::new(text).color(color).strong())
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
            .corner_radius(CornerRadius::same(10)),
    )
}

/// A small red counter bubble (unseen errors).
pub(super) fn badge(ui: &mut Ui, count: usize, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(color)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(6, 0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(count.to_string())
                    .small()
                    .strong()
                    .color(Color32::WHITE),
            );
        })
        .response
}
