//! The Downloads tab: packages and their files in a table, with selection,
//! a context menu and removal.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use eframe::egui::{
    self, Align, Button, Color32, Id, Key, Layout, Modal, ProgressBar, RichText, Sense, Ui,
};
use egui_extras::{Column, TableBuilder, TableRow};

use super::style::{Palette, status_text};
use super::{Outbox, Tab};
use crate::gui::protocol::{Command, LogLevel};
use crate::gui::state::{
    PackageAggregate, PackageStatus, ViewState, format_bytes, format_eta, format_speed,
};
use crate::model::{Item, ItemStatus, MediaKind, Package};
use crate::queue::Queue;

const ROW_HEIGHT: f32 = 22.0;
/// Item names are indented under their package by this much.
const INDENT: f32 = 24.0;
/// Packages with at most this many files start expanded.
const AUTO_EXPAND_MAX_ITEMS: usize = 12;

/// Selection, expansion and the pending removal question.
#[derive(Debug, Default)]
pub(super) struct DownloadsView {
    /// Selected package and item ids.
    selection: BTreeSet<u64>,
    /// Packages whose items are shown.
    expanded: HashSet<u64>,
    /// Packages already given their default expansion.
    seen: HashSet<u64>,
    /// Ids waiting for the "remove N files?" confirmation.
    confirm_remove: Option<Vec<u64>>,
}

/// One table line.
#[derive(Debug, Clone, Copy)]
enum Row {
    Package(usize),
    Item(usize, usize),
}

/// What happened in the table this frame.
#[derive(Debug)]
enum Action {
    /// Left click (Ctrl/Cmd toggles instead of replacing the selection).
    Click(u64),
    /// Right click: act on this row (select it unless it already is).
    Focus(u64),
    ToggleExpanded(u64),
    Retry,
    Remove,
    Open(PathBuf),
    Copy(String),
}

impl DownloadsView {
    pub fn show(&mut self, ui: &mut Ui, state: &ViewState, out: &mut Outbox, now: Instant) {
        let queue = state.queue.clone();
        self.prune(&queue);
        if queue.packages.is_empty() {
            empty_state(ui, out);
            return;
        }

        let rows = self.rows(&queue);
        let mut actions = Vec::new();
        ui.scope(|ui| {
            // Selectable text would swallow the clicks meant for the rows.
            ui.style_mut().interaction.selectable_labels = false;
            self.table(ui, state, &rows, now, &mut actions);
        });
        if !self.selection.is_empty()
            && !ui.ctx().text_edit_focused()
            && ui.input(|input| input.key_pressed(Key::Delete))
        {
            actions.push(Action::Remove);
        }

        let toggle = ui.input(|input| input.modifiers.command);
        for action in actions {
            self.apply(action, toggle, &queue, ui.ctx(), out);
        }
        self.remove_dialog(ui.ctx(), &queue, out);
    }

    /// Forgets ids that left the queue; gives new packages their default expansion.
    fn prune(&mut self, queue: &Queue) {
        let mut ids = HashSet::new();
        for package in &queue.packages {
            ids.insert(package.id);
            ids.extend(package.items.iter().map(|item| item.id));
            if self.seen.insert(package.id) && package.items.len() <= AUTO_EXPAND_MAX_ITEMS {
                self.expanded.insert(package.id);
            }
        }
        self.selection.retain(|id| ids.contains(id));
        self.expanded.retain(|id| ids.contains(id));
        self.seen.retain(|id| ids.contains(id));
    }

    fn rows(&self, queue: &Queue) -> Vec<Row> {
        let mut rows = Vec::new();
        for (p, package) in queue.packages.iter().enumerate() {
            rows.push(Row::Package(p));
            if self.expanded.contains(&package.id) {
                rows.extend((0..package.items.len()).map(|i| Row::Item(p, i)));
            }
        }
        rows
    }

    fn table(
        &self,
        ui: &mut Ui,
        state: &ViewState,
        rows: &[Row],
        now: Instant,
        actions: &mut Vec<Action>,
    ) {
        let palette = Palette::of(ui);
        let queue = &state.queue;
        TableBuilder::new(ui)
            .id_salt("igdl_downloads")
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(300.0).at_least(140.0).clip(true))
            .column(Column::initial(105.0).at_least(70.0).clip(true))
            .column(Column::initial(170.0).at_least(80.0))
            .column(Column::initial(80.0).at_least(55.0))
            .column(Column::initial(85.0).at_least(55.0))
            .column(Column::initial(65.0).at_least(45.0))
            .column(Column::remainder().at_least(120.0).clip(true))
            .header(24.0, |mut header| {
                for title in [
                    "Name", "Status", "Progress", "Size", "Speed", "ETA", "Saved to",
                ] {
                    header.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, rows.len(), |mut row| match rows[row.index()] {
                    Row::Package(p) => {
                        let package = &queue.packages[p];
                        self.package_row(&mut row, package, state, palette, now, actions);
                    }
                    Row::Item(p, i) => {
                        let package = &queue.packages[p];
                        let item = &package.items[i];
                        self.item_row(&mut row, package, item, state, palette, now, actions);
                    }
                });
            });
    }

    fn package_row(
        &self,
        row: &mut TableRow<'_, '_>,
        package: &Package,
        state: &ViewState,
        palette: Palette,
        now: Instant,
        actions: &mut Vec<Action>,
    ) {
        let aggregate = state.package_aggregate(package, now);
        let open = self.expanded.contains(&package.id);
        row.set_selected(self.selection.contains(&package.id));
        row.col(|ui| {
            let (arrow, verb) = if open {
                ("▾", "Collapse")
            } else {
                ("▸", "Expand")
            };
            if ui
                .add(Button::new(RichText::new(arrow).monospace().strong()).frame(false))
                .accessible_name(format!("{verb} {}", package.name))
                .clicked()
            {
                actions.push(Action::ToggleExpanded(package.id));
            }
            ui.label(RichText::new(&package.name).strong())
                .on_hover_text(&package.source);
        });
        row.col(|ui| {
            let (text, color) = package_status(&aggregate, palette);
            ui.label(RichText::new(text).color(color).strong());
        });
        row.col(|ui| {
            let text = format!(
                "{:.0}% · {}/{} files",
                aggregate.fraction * 100.0,
                aggregate.files_finished(),
                aggregate.files_total()
            );
            let fill = match aggregate.status() {
                PackageStatus::Failed(_) => Some(palette.red.gamma_multiply(0.7)),
                PackageStatus::Done | PackageStatus::Skipped => {
                    Some(palette.green.gamma_multiply(0.8))
                }
                _ => None,
            };
            ui.add(progress_bar(aggregate.fraction, fill).text(text));
        });
        row.col(|ui| {
            let text = if aggregate.sizes_known {
                format_bytes(aggregate.bytes_total)
            } else if aggregate.bytes_total > 0 {
                format!("{}+", format_bytes(aggregate.bytes_total))
            } else {
                "—".to_string()
            };
            ui.label(text);
        });
        row.col(|ui| {
            if aggregate.speed > 0.0 {
                ui.label(format_speed(aggregate.speed));
            }
        });
        row.col(|ui| {
            if aggregate.counts.downloading > 0 {
                ui.label(aggregate.eta().map_or_else(|| "—".to_string(), format_eta));
            }
        });
        row.col(|ui| {
            let folder = package.out_dir.display().to_string();
            ui.label(RichText::new(&folder).weak())
                .on_hover_text(folder);
        });
        self.row_interaction(row, Target::Package(package), state, actions);
    }

    #[allow(clippy::too_many_arguments)]
    fn item_row(
        &self,
        row: &mut TableRow<'_, '_>,
        package: &Package,
        item: &Item,
        state: &ViewState,
        palette: Palette,
        now: Instant,
        actions: &mut Vec<Action>,
    ) {
        row.set_selected(self.selection.contains(&item.id));
        row.col(|ui| {
            ui.add_space(INDENT);
            ui.label(match item.media.kind {
                MediaKind::Image => "🖼",
                MediaKind::Video => "🎞",
            });
            let name = item
                .rel_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| item.rel_path.display().to_string());
            ui.label(name)
                .on_hover_text(item.rel_path.display().to_string());
        });
        row.col(|ui| {
            let label = ui
                .label(RichText::new(status_text(item.status)).color(palette.status(item.status)));
            match (&item.error, &item.note) {
                (Some(error), _) if item.status == ItemStatus::Failed => {
                    label.on_hover_text(&error.message);
                }
                (_, Some(note)) => {
                    label.on_hover_text(note);
                }
                _ => {}
            }
        });
        row.col(|ui| item_progress(ui, state, item, palette));
        row.col(|ui| {
            let (_, total) = state.item_bytes(item);
            ui.label(total.map_or_else(|| "—".to_string(), format_bytes));
        });
        row.col(|ui| {
            if item.status == ItemStatus::Downloading
                && let Some(speed) = state.item_speed(item.id, now)
            {
                ui.label(format_speed(speed));
            }
        });
        row.col(|ui| {
            if item.status == ItemStatus::Downloading {
                let eta = state.item_eta(item, now);
                ui.label(eta.map_or_else(|| "—".to_string(), format_eta));
            }
        });
        row.col(|ui| {
            let path = item
                .path
                .clone()
                .unwrap_or_else(|| package.out_dir.join(&item.rel_path));
            ui.label(RichText::new(shown_path(&path, &package.out_dir)).weak())
                .on_hover_text(path.display().to_string());
        });
        self.row_interaction(row, Target::Item(package, item), state, actions);
    }

    fn row_interaction(
        &self,
        row: &TableRow<'_, '_>,
        target: Target<'_>,
        state: &ViewState,
        actions: &mut Vec<Action>,
    ) {
        let response = row.response();
        let id = target.id();
        if response.clicked() {
            actions.push(Action::Click(id));
        }
        if response.double_clicked()
            && let Target::Package(package) = target
        {
            actions.push(Action::ToggleExpanded(package.id));
        }
        if response.secondary_clicked() {
            actions.push(Action::Focus(id));
        }
        response.context_menu(|ui| self.context_menu(ui, target, state, actions));
    }

    fn context_menu(
        &self,
        ui: &mut Ui,
        target: Target<'_>,
        state: &ViewState,
        actions: &mut Vec<Action>,
    ) {
        let package = match target {
            Target::Item(package, item) => {
                let file = item
                    .path
                    .clone()
                    .filter(|_| item.status == ItemStatus::Done);
                if ui
                    .add_enabled(file.is_some(), Button::new("Open file"))
                    .clicked()
                    && let Some(file) = file
                {
                    actions.push(Action::Open(file));
                }
                if ui.button("Open folder").clicked() {
                    actions.push(Action::Open(item_folder(package, item)));
                }
                package
            }
            Target::Package(package) => {
                if ui.button("Open folder").clicked() {
                    actions.push(Action::Open(package.out_dir.clone()));
                }
                package
            }
        };
        ui.separator();
        let any_failed = state
            .queue
            .packages
            .iter()
            .flat_map(|package| {
                let whole = self.selection.contains(&package.id);
                package
                    .items
                    .iter()
                    .filter(move |item| whole || self.selection.contains(&item.id))
            })
            .any(|item| item.status == ItemStatus::Failed);
        if ui.add_enabled(any_failed, Button::new("Retry")).clicked() {
            actions.push(Action::Retry);
        }
        if ui.button("Remove").clicked() {
            actions.push(Action::Remove);
        }
        ui.separator();
        if ui
            .button("Copy source link")
            .on_hover_text(&package.source)
            .clicked()
        {
            actions.push(Action::Copy(package.source.clone()));
        }
    }

    fn apply(
        &mut self,
        action: Action,
        toggle: bool,
        queue: &Queue,
        ctx: &egui::Context,
        out: &mut Outbox,
    ) {
        match action {
            Action::Click(id) => {
                if toggle {
                    if !self.selection.remove(&id) {
                        self.selection.insert(id);
                    }
                } else {
                    self.selection = BTreeSet::from([id]);
                }
            }
            Action::Focus(id) => {
                if !self.selection.contains(&id) {
                    self.selection = BTreeSet::from([id]);
                }
            }
            Action::ToggleExpanded(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
            }
            Action::Retry => {
                // An empty list would mean "every failed file": never send that from here.
                if !self.selection.is_empty() {
                    out.send(Command::Retry(self.selection.iter().copied().collect()));
                }
            }
            Action::Remove => {
                let ids: Vec<u64> = self.selection.iter().copied().collect();
                if removed_items(queue, &ids) > 1 {
                    self.confirm_remove = Some(ids);
                } else if !ids.is_empty() {
                    out.send(Command::Remove(ids));
                    self.selection.clear();
                }
            }
            Action::Open(path) => {
                if let Err(err) = opener::open(&path) {
                    out.notice(
                        LogLevel::Warn,
                        format!("Cannot open {}: {err}", path.display()),
                    );
                }
            }
            Action::Copy(text) => ctx.copy_text(text),
        }
    }

    fn remove_dialog(&mut self, ctx: &egui::Context, queue: &Queue, out: &mut Outbox) {
        let Some(ids) = &self.confirm_remove else {
            return;
        };
        let count = removed_items(queue, ids);
        let mut decision = None;
        let response = Modal::new(Id::unique("igdl_remove_dialog")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading("Remove from the download list?");
            ui.add_space(4.0);
            ui.label(format!(
                "{count} files will be removed from the list. Files already on disk are kept."
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Remove").clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
        if response.should_close() {
            decision = decision.or(Some(false));
        }
        match decision {
            Some(true) => {
                if let Some(ids) = self.confirm_remove.take() {
                    out.send(Command::Remove(ids));
                    self.selection.clear();
                }
            }
            Some(false) => self.confirm_remove = None,
            None => {}
        }
    }
}

/// The row a click or context menu refers to.
#[derive(Clone, Copy)]
enum Target<'a> {
    Package(&'a Package),
    Item(&'a Package, &'a Item),
}

impl Target<'_> {
    fn id(&self) -> u64 {
        match self {
            Target::Package(package) => package.id,
            Target::Item(_, item) => item.id,
        }
    }
}

fn package_status(aggregate: &PackageAggregate, palette: Palette) -> (String, Color32) {
    match aggregate.status() {
        PackageStatus::Downloading => ("Downloading".into(), palette.blue),
        PackageStatus::Queued => ("Queued".into(), palette.gray),
        PackageStatus::Failed(count) => (format!("{count} failed"), palette.red),
        PackageStatus::Done => ("Done".into(), palette.green),
        PackageStatus::Skipped => ("Skipped".into(), palette.muted),
        PackageStatus::Empty => ("Empty".into(), palette.muted),
    }
}

fn item_progress(ui: &mut Ui, state: &ViewState, item: &Item, palette: Palette) {
    let fill = match item.status {
        ItemStatus::Failed => Some(palette.red.gamma_multiply(0.7)),
        ItemStatus::Done => Some(palette.green.gamma_multiply(0.8)),
        _ => None,
    };
    let bar = match (item.status, state.item_fraction(item)) {
        (ItemStatus::Skipped, _) => {
            ui.label(RichText::new("—").weak());
            return;
        }
        (_, Some(fraction)) => {
            progress_bar(fraction, fill).text(format!("{:.0}%", fraction * 100.0))
        }
        (ItemStatus::Downloading, None) => {
            let (done, _) = state.item_bytes(item);
            progress_bar(0.0, fill).text(format_bytes(done))
        }
        (_, None) => progress_bar(0.0, fill),
    };
    ui.add(bar);
}

/// A table-sized progress bar; an empty one shows no fill at all (egui would
/// otherwise draw a dot).
fn progress_bar(fraction: f32, fill: Option<Color32>) -> ProgressBar {
    let bar = ProgressBar::new(fraction).desired_height(16.0);
    if fraction <= 0.0 {
        bar.fill(Color32::TRANSPARENT)
    } else if let Some(fill) = fill {
        bar.fill(fill)
    } else {
        bar
    }
}

/// `path` relative to `root` when it is inside it (the package row shows `root`).
fn shown_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Where an item's file is (or will be).
fn item_folder(package: &Package, item: &Item) -> PathBuf {
    let file = item
        .path
        .clone()
        .unwrap_or_else(|| package.out_dir.join(&item.rel_path));
    file.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| package.out_dir.clone())
}

/// Number of files `Remove(ids)` would take out of the list.
fn removed_items(queue: &Queue, ids: &[u64]) -> usize {
    queue
        .packages
        .iter()
        .map(|package| {
            if ids.contains(&package.id) {
                package.items.len()
            } else {
                package
                    .items
                    .iter()
                    .filter(|item| ids.contains(&item.id))
                    .count()
            }
        })
        .sum()
}

fn empty_state(ui: &mut Ui, out: &mut Outbox) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.35).max(16.0));
        ui.label(
            RichText::new("No downloads yet — paste links in the LinkGrabber")
                .size(17.0)
                .weak(),
        );
        ui.add_space(10.0);
        if ui.button("🔗 Open the LinkGrabber").clicked() {
            out.goto(Tab::LinkGrabber);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Category, MediaFile};

    fn queue() -> Queue {
        let mut queue = Queue::default();
        let media = |key: &str| MediaFile {
            key: key.to_string(),
            post_id: "1".into(),
            shortcode: None,
            index: 1,
            count: 1,
            kind: MediaKind::Image,
            category: Category::Post,
            url: "https://cdn.example.invalid/a.jpg".into(),
            width: None,
            height: None,
            owner: "example_user".into(),
            owner_id: None,
            taken_at: 0,
            caption: None,
            highlight: None,
            ext: "jpg".into(),
        };
        queue.add_package(
            "a".into(),
            "https://www.instagram.com/p/A/".into(),
            PathBuf::from("/d"),
            vec![(media("1"), "a1.jpg".into()), (media("2"), "a2.jpg".into())],
        );
        queue.add_package(
            "b".into(),
            "https://www.instagram.com/p/B/".into(),
            PathBuf::from("/d"),
            vec![(media("3"), "b1.jpg".into())],
        );
        queue
    }

    #[test]
    fn counts_removed_items() {
        let queue = queue();
        // Ids: package a = 1 (items 2, 3), package b = 4 (item 5).
        assert_eq!(removed_items(&queue, &[1]), 2);
        assert_eq!(removed_items(&queue, &[5]), 1);
        assert_eq!(removed_items(&queue, &[1, 2, 5]), 3);
        assert_eq!(removed_items(&queue, &[99]), 0);
    }

    #[test]
    fn paths_are_shown_relative_to_the_package_folder() {
        assert_eq!(
            shown_path(Path::new("/d/user/posts/a.jpg"), Path::new("/d")),
            Path::new("user/posts/a.jpg").display().to_string()
        );
        assert_eq!(
            shown_path(Path::new("/elsewhere/a.jpg"), Path::new("/d")),
            Path::new("/elsewhere/a.jpg").display().to_string()
        );
    }

    #[test]
    fn item_folder_prefers_the_finished_path() {
        let queue = queue();
        let package = &queue.packages[0];
        let mut item = package.items[0].clone();
        assert_eq!(item_folder(package, &item), PathBuf::from("/d"));
        item.path = Some(PathBuf::from("/elsewhere/x/a1.jpg"));
        assert_eq!(item_folder(package, &item), PathBuf::from("/elsewhere/x"));
    }

    #[test]
    fn small_packages_start_expanded_and_selection_is_pruned() {
        let queue = queue();
        let mut view = DownloadsView::default();
        view.selection.insert(99);
        view.selection.insert(2);
        view.prune(&queue);
        assert!(view.expanded.contains(&1) && view.expanded.contains(&4));
        assert_eq!(view.selection, BTreeSet::from([2]));
        assert_eq!(view.rows(&queue).len(), 5);
        view.expanded.remove(&1);
        view.prune(&queue);
        assert!(!view.expanded.contains(&1), "default applies only once");
        assert_eq!(view.rows(&queue).len(), 3);
    }
}
