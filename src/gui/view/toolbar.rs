//! The top bar: download controls, account chip, total speed, and the tab row.

use std::time::Instant;

use eframe::egui::{self, Button, RichText, Ui};

use super::style::{Palette, badge, chip};
use super::{Outbox, Tab};
use crate::gui::protocol::{AccountStatus, Command};
use crate::gui::state::{ViewState, format_speed};

/// Draws the toolbar and the tab row; switches `tab` when a tab is clicked.
pub(super) fn show(ui: &mut Ui, state: &ViewState, tab: &mut Tab, out: &mut Outbox, now: Instant) {
    ui.add_space(4.0);
    let (left, right) = egui::Sides::new().show(
        ui,
        |ui| {
            let mut out = Outbox::default();
            controls(ui, state, &mut out);
            out
        },
        |ui| {
            let mut out = Outbox::default();
            // Right to left: speed at the edge, the account chip next to it.
            ui.label(
                RichText::new(format!("↓ {}", format_speed(state.total_speed(now))))
                    .monospace()
                    .strong(),
            )
            .on_hover_text("Total download speed");
            ui.add_space(8.0);
            account_chip(ui, state, &mut out);
            out
        },
    );
    out.merge(left);
    out.merge(right);
    ui.add_space(2.0);
    tabs(ui, state, tab);
    ui.add_space(2.0);
}

fn controls(ui: &mut Ui, state: &ViewState, out: &mut Outbox) {
    let counts = state.counts();
    if ui
        .add_enabled(counts.queued > 0, Button::new("▶ Start all"))
        .on_hover_text("Download every queued file")
        .on_disabled_hover_text("Nothing is queued")
        .clicked()
    {
        out.send(Command::Start);
    }
    if ui
        .add_enabled(state.downloading, Button::new("⏹ Stop"))
        .on_hover_text("Stop downloading; partial files are kept and resume later")
        .clicked()
    {
        out.send(Command::Stop);
    }
    if ui
        .add_enabled(counts.failed > 0, Button::new("↻ Retry failed"))
        .on_hover_text("Queue every failed file again")
        .clicked()
    {
        out.send(Command::Retry(Vec::new()));
    }
    let any_finished = state.queue.packages.iter().any(|package| {
        !package.items.is_empty() && package.items.iter().all(|item| item.status.is_finished())
    });
    if ui
        .add_enabled(any_finished, Button::new("🗑 Clear finished"))
        .on_hover_text("Remove finished packages from the list (files stay on disk)")
        .clicked()
    {
        out.send(Command::ClearFinished);
    }
}

fn account_chip(ui: &mut Ui, state: &ViewState, out: &mut Outbox) {
    let palette = Palette::of(ui);
    match &state.account {
        None => {}
        Some(AccountStatus::NoCookies) => {
            if chip(ui, "No cookies — set them in Settings", palette.amber)
                .on_hover_text(
                    "Instagram needs your browser cookies; choose the cookie file in Settings",
                )
                .clicked()
            {
                out.goto(Tab::Settings);
            }
        }
        Some(AccountStatus::Loaded) => {
            if ui
                .button("Cookies loaded — check")
                .on_hover_text("Ask Instagram which account the cookies belong to")
                .clicked()
            {
                out.send(Command::CheckAccount);
            }
        }
        Some(AccountStatus::Checking) => {
            ui.label("Checking login…");
            ui.add(egui::Spinner::new());
        }
        Some(AccountStatus::LoggedIn { username }) => {
            chip(ui, &format!("@{username}"), palette.green)
                .on_hover_text(format!("Logged in to Instagram as @{username}"));
        }
        Some(AccountStatus::Unverified(message)) => {
            if chip(ui, "Login not verified — retry", palette.amber)
                .on_hover_text(format!(
                    "Instagram did not confirm the login right now: {message}\nThe cookies may still be fine. Click to check again."
                ))
                .clicked()
            {
                out.send(Command::CheckAccount);
            }
        }
        Some(AccountStatus::Invalid(message)) => {
            let response = chip(ui, "Cookies problem", palette.red).on_hover_text(message);
            if response.clicked() {
                out.goto(Tab::Settings);
            }
        }
    }
}

fn tabs(ui: &mut Ui, state: &ViewState, tab: &mut Tab) {
    let palette = Palette::of(ui);
    ui.horizontal(|ui| {
        let downloads = format!("⬇ Downloads ({})", state.counts().total());
        let grabber = format!("🔗 LinkGrabber ({} staged)", state.staged_files());
        for (target, label) in [
            (Tab::Downloads, downloads.as_str()),
            (Tab::LinkGrabber, grabber.as_str()),
            (Tab::Settings, "⚙ Settings"),
            (Tab::Log, "📜 Log"),
        ] {
            let text = RichText::new(label).size(15.0);
            if ui.selectable_label(*tab == target, text).clicked() {
                *tab = target;
            }
        }
        let unseen = state.unseen_errors();
        if unseen > 0 {
            badge(ui, unseen, palette.red)
                .on_hover_text(format!("{unseen} new error(s) in the Log"));
        }
    });
}
