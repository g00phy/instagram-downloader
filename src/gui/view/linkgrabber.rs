//! The LinkGrabber tab: paste links, choose crawl options, review the staged
//! packages and move them into the download list. Also the clipboard monitor.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Button, CollapsingHeader, ComboBox, DragValue, Grid, Key, RichText, ScrollArea, TextEdit,
    Ui,
};

use super::style::Palette;
use super::{Outbox, Tab};
use crate::config::parse_date;
use crate::gui::protocol::{Command, GrabRequest, LogLevel, StagedPackage};
use crate::gui::state::{ViewState, kind_counts};
use crate::links::{ProfileSection, read_links};
use crate::model::MediaKind;
use crate::naming::{DEFAULT_TEMPLATE, validate_template};
use crate::watch::Clipboard;

/// How often the clipboard monitor looks for new links.
const CLIPBOARD_POLL: Duration = Duration::from_secs(1);

/// Files listed per expanded staged package (the rest is summarised).
const MAX_LISTED_FILES: usize = 200;

/// Placeholder of the links box.
pub(super) const LINKS_HINT: &str =
    "Paste Instagram links or @usernames — one per line, any text works";

/// The LinkGrabber options, as edited.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GrabForm {
    pub text: String,
    /// Ticked profile sections, in [`ProfileSection::ALL`] order.
    pub sections: [bool; 5],
    /// Max posts per profile section; 0 = no limit.
    pub limit: usize,
    pub only: Option<MediaKind>,
    pub since: String,
    pub until: String,
    pub update: bool,
    pub out_dir: String,
    pub template: String,
    /// Edited fields keep their value when new settings arrive.
    sections_edited: bool,
    out_dir_edited: bool,
    template_edited: bool,
}

impl Default for GrabForm {
    fn default() -> Self {
        GrabForm {
            text: String::new(),
            sections: sections_from(None),
            limit: 0,
            only: None,
            since: String::new(),
            until: String::new(),
            update: false,
            out_dir: String::new(),
            template: DEFAULT_TEMPLATE.to_string(),
            sections_edited: false,
            out_dir_edited: false,
            template_edited: false,
        }
    }
}

impl GrabForm {
    /// Takes the defaults from the settings, except for fields the user edited.
    fn load_defaults(&mut self, state: &ViewState) {
        let settings = &state.settings;
        if !self.sections_edited {
            self.sections = sections_from(settings.include.as_deref());
        }
        if !self.out_dir_edited {
            let dir = settings
                .out_dir
                .clone()
                .unwrap_or_else(|| state.default_out_dir.clone());
            self.out_dir = dir.display().to_string();
        }
        if !self.template_edited {
            self.template = settings
                .template
                .clone()
                .unwrap_or_else(|| DEFAULT_TEMPLATE.to_string());
        }
    }

    /// The ticked sections.
    pub fn sections(&self) -> Vec<ProfileSection> {
        ProfileSection::ALL
            .into_iter()
            .zip(self.sections)
            .filter_map(|(section, ticked)| ticked.then_some(section))
            .collect()
    }

    /// Why `since` is not a date (empty is fine).
    fn since_error(&self) -> Option<String> {
        date_error(&self.since, false)
    }

    /// Why `until` is not a date (empty is fine).
    fn until_error(&self) -> Option<String> {
        date_error(&self.until, true)
    }

    fn template_error(&self) -> Option<String> {
        validate_template(self.template.trim())
            .err()
            .map(|err| err.to_string())
    }

    /// The first problem with the options, if any.
    pub fn options_error(&self) -> Option<String> {
        if self.sections.iter().all(|ticked| !ticked) {
            return Some("Tick at least one profile section".into());
        }
        if let Some(error) = self.since_error() {
            return Some(format!("Since: {error}"));
        }
        if let Some(error) = self.until_error() {
            return Some(format!("Until: {error}"));
        }
        if let (Ok(since), Ok(until)) = (
            parse_date(&self.since, false),
            parse_date(&self.until, true),
        ) && since > until
        {
            return Some("Since is after Until".into());
        }
        if let Some(error) = self.template_error() {
            return Some(format!("Template: {error}"));
        }
        if self.out_dir.trim().is_empty() {
            return Some("Choose a folder to save to".into());
        }
        None
    }

    /// A grab of `text` with these options.
    pub fn request(&self, text: String) -> Result<GrabRequest, String> {
        if let Some(problem) = self.options_error() {
            return Err(problem);
        }
        if read_links(&text).is_empty() {
            return Err("No links or usernames found".into());
        }
        Ok(GrabRequest {
            text,
            sections: self.sections(),
            limit: (self.limit > 0).then_some(self.limit),
            only: self.only,
            since: self.since.trim().to_string(),
            until: self.until.trim().to_string(),
            update: self.update,
            out_dir: PathBuf::from(self.out_dir.trim()),
            template: self.template.trim().to_string(),
        })
    }
}

/// Ticked sections for a settings `include` list (default: posts only).
pub(super) fn sections_from(include: Option<&[String]>) -> [bool; 5] {
    let chosen: Vec<ProfileSection> = include
        .unwrap_or_default()
        .iter()
        .filter_map(|name| name.parse().ok())
        .collect();
    if chosen.is_empty() {
        return ProfileSection::ALL.map(|section| section == ProfileSection::Posts);
    }
    ProfileSection::ALL.map(|section| chosen.contains(&section))
}

/// `Posts`, `Reels`, … for checkbox labels.
pub(super) fn section_label(section: ProfileSection) -> &'static str {
    match section {
        ProfileSection::Posts => "Posts",
        ProfileSection::Reels => "Reels",
        ProfileSection::Tagged => "Tagged",
        ProfileSection::Stories => "Stories",
        ProfileSection::Highlights => "Highlights",
    }
}

fn date_error(text: &str, end_of_day: bool) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    parse_date(text, end_of_day)
        .err()
        .map(|err| err.to_string())
}

fn only_label(only: Option<MediaKind>) -> &'static str {
    match only {
        None => "All",
        Some(MediaKind::Image) => "Images",
        Some(MediaKind::Video) => "Videos",
    }
}

/// JDownloader's "clipboard monitoring": grabs Instagram links as they are copied.
#[derive(Default)]
struct ClipboardWatch {
    enabled: bool,
    /// Opened on the first poll.
    clipboard: Option<Clipboard>,
    error: Option<String>,
    next_poll: Option<Instant>,
}

impl ClipboardWatch {
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.next_poll = None;
        if enabled {
            self.error = None;
        } else {
            // Re-enabling starts from the clipboard contents at that moment.
            self.clipboard = None;
        }
    }
}

/// State of the LinkGrabber tab.
#[derive(Default)]
pub(super) struct GrabberView {
    form: GrabForm,
    /// Last `settings_generation` the form defaults were taken from.
    generation: u64,
    /// Staged packages the user unticked (new ones start ticked).
    unticked: HashSet<u64>,
    /// Staged packages whose files are listed.
    expanded: HashSet<u64>,
    clipboard: ClipboardWatch,
}

impl GrabberView {
    /// Picks up new settings and forgets packages that left the LinkGrabber.
    pub fn sync(&mut self, state: &ViewState) {
        if state.settings_generation != self.generation {
            self.generation = state.settings_generation;
            self.form.load_defaults(state);
        }
        let ids: HashSet<u64> = state.staged.iter().map(|package| package.id).collect();
        self.unticked.retain(|id| ids.contains(id));
        self.expanded.retain(|id| ids.contains(id));
    }

    /// When the window should wake up for the next clipboard poll.
    pub fn poll_delay(&self, now: Instant) -> Option<Duration> {
        if !self.clipboard.enabled {
            return None;
        }
        let wait = self
            .clipboard
            .next_poll
            .map_or(Duration::ZERO, |at| at.saturating_duration_since(now));
        Some(wait.max(Duration::from_millis(100)))
    }

    /// About once a second while monitoring: grab newly copied links.
    pub fn poll_clipboard(&mut self, now: Instant, out: &mut Outbox) {
        let watch = &mut self.clipboard;
        if !watch.enabled || watch.next_poll.is_some_and(|at| now < at) {
            return;
        }
        watch.next_poll = Some(now + CLIPBOARD_POLL);
        let clipboard = match &mut watch.clipboard {
            Some(clipboard) => clipboard,
            None => match Clipboard::open() {
                Ok(clipboard) => watch.clipboard.insert(clipboard),
                Err(err) => {
                    watch.enabled = false;
                    watch.error = Some(format!("Clipboard monitoring is off: {err}"));
                    return;
                }
            },
        };
        let links = clipboard.new_links();
        if links.is_empty() {
            return;
        }
        match self.form.request(links.join("\n")) {
            Ok(request) => {
                out.notice(
                    LogLevel::Info,
                    format!("Clipboard: grabbing {} copied link(s)", links.len()),
                );
                out.send(Command::Grab(request));
            }
            Err(problem) => {
                watch.error = Some(format!("Copied links were not grabbed: {problem}"));
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui, state: &ViewState, out: &mut Outbox) {
        let palette = Palette::of(ui);
        ui.add_space(4.0);
        self.links_box(ui, out);
        ui.add_space(4.0);
        self.options(ui, palette);
        ui.add_space(6.0);
        self.grab_row(ui, state, palette, out);
        ui.add_space(4.0);
        ui.separator();
        self.staged(ui, state, palette, out);
    }

    fn links_box(&mut self, ui: &mut Ui, out: &mut Outbox) {
        let response = ui
            .add(
                TextEdit::multiline(&mut self.form.text)
                    .hint_text(LINKS_HINT)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            )
            .accessible_name("Links to grab");
        // Ctrl+Enter grabs straight from the box.
        if response.has_focus()
            && ui.input(|input| input.modifiers.command && input.key_pressed(Key::Enter))
        {
            self.grab(out);
        }
    }

    fn options(&mut self, ui: &mut Ui, palette: Palette) {
        let form = &mut self.form;
        ui.horizontal_wrapped(|ui| {
            ui.label("Profile sections:");
            for (section, ticked) in ProfileSection::ALL
                .into_iter()
                .zip(form.sections.iter_mut())
            {
                if ui.checkbox(ticked, section_label(section)).changed() {
                    form.sections_edited = true;
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            let label = ui.label("Limit");
            ui.add(DragValue::new(&mut form.limit).range(0..=1_000_000))
                .labelled_by(label.id)
                .on_hover_text("Most posts per profile section (0 = no limit)");
            ui.add_space(10.0);
            let label = ui.label("Only");
            ComboBox::from_id_salt("igdl_grab_only")
                .selected_text(only_label(form.only))
                .show_ui(ui, |ui| {
                    for only in [None, Some(MediaKind::Image), Some(MediaKind::Video)] {
                        ui.selectable_value(&mut form.only, only, only_label(only));
                    }
                })
                .response
                .labelled_by(label.id);
            ui.add_space(10.0);
            let since_error = form.since_error();
            date_field(ui, "Since", &mut form.since, since_error, palette);
            let until_error = form.until_error();
            date_field(ui, "Until", &mut form.until, until_error, palette);
        });
        ui.checkbox(
            &mut form.update,
            "Only new posts (stop at the first downloaded one)",
        );
        ui.horizontal(|ui| {
            let label = ui.label("Save to");
            if ui
                .add(TextEdit::singleline(&mut form.out_dir).desired_width(420.0))
                .labelled_by(label.id)
                .changed()
            {
                form.out_dir_edited = true;
            }
            if ui.button("Browse…").clicked() {
                let mut dialog = rfd::FileDialog::new().set_title("Save downloads to");
                if !form.out_dir.trim().is_empty() {
                    dialog = dialog.set_directory(form.out_dir.trim());
                }
                if let Some(dir) = dialog.pick_folder() {
                    form.out_dir = dir.display().to_string();
                    form.out_dir_edited = true;
                }
            }
        });
        CollapsingHeader::new("Advanced")
            .id_salt("igdl_grab_advanced")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let label = ui.label("Filename template");
                    if ui
                        .add(
                            TextEdit::singleline(&mut form.template)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(420.0),
                        )
                        .labelled_by(label.id)
                        .changed()
                    {
                        form.template_edited = true;
                    }
                    if ui.small_button("Reset").clicked() {
                        form.template = DEFAULT_TEMPLATE.to_string();
                        form.template_edited = true;
                    }
                });
                match form.template_error() {
                    Some(error) => ui.colored_label(palette.red, error),
                    None => ui.colored_label(palette.green, "✔ Template is valid"),
                };
            });
    }

    fn grab_row(&mut self, ui: &mut Ui, state: &ViewState, palette: Palette, out: &mut Outbox) {
        let links = read_links(&self.form.text).len();
        let problem = self.form.options_error();
        ui.horizontal(|ui| {
            let enabled = links > 0 && problem.is_none();
            let button = ui.add_enabled(
                enabled,
                Button::new(RichText::new("🔍 Grab links").strong())
                    .min_size(egui::vec2(120.0, 26.0)),
            );
            let button = match (&problem, links) {
                (_, 0) => button.on_disabled_hover_text("Paste at least one link or @username"),
                (Some(problem), _) => button.on_disabled_hover_text(problem),
                (None, _) => button.on_hover_text(format!("Resolve {links} link(s)")),
            };
            if button.clicked() {
                self.grab(out);
            }
            if ui
                .button("📂 Load file…")
                .on_hover_text("Add the links from a text file (one per line, # starts a comment)")
                .clicked()
            {
                self.load_links_file(out);
            }
            if links > 0 {
                ui.label(RichText::new(format!("{links} link(s) found")).weak());
            }
            ui.add_space(12.0);
            let mut monitoring = self.clipboard.enabled;
            if ui
                .checkbox(&mut monitoring, "Clipboard monitoring")
                .on_hover_text("Grab Instagram links as you copy them, with the options above")
                .changed()
            {
                self.clipboard.set_enabled(monitoring);
            }
            if state.grabbing > 0 {
                ui.add_space(12.0);
                ui.add(egui::Spinner::new());
                ui.label(format!("Resolving {} link(s)…", state.grabbing));
            }
        });
        if links > 0
            && let Some(problem) = &problem
        {
            ui.colored_label(palette.red, problem);
        }
        if let Some(error) = &self.clipboard.error {
            ui.colored_label(palette.amber, error);
        }
    }

    /// Appends a links file (picked in a dialog) to the box.
    fn load_links_file(&mut self, out: &mut Outbox) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Links file")
            .add_filter("Text", &["txt", "md", "csv"])
            .add_filter("All files", &["*"])
            .pick_file()
        else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let text = &mut self.form.text;
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                // Windows editors may start the file with a byte order mark.
                text.push_str(contents.trim_start_matches('\u{feff}'));
            }
            Err(err) => out.notice(
                LogLevel::Warn,
                format!("Cannot read {}: {err}", path.display()),
            ),
        }
    }

    /// Sends the pasted text as a `Grab` and empties the box.
    fn grab(&mut self, out: &mut Outbox) {
        if let Ok(request) = self.form.request(self.form.text.clone()) {
            out.send(Command::Grab(request));
            self.form.text.clear();
        }
    }

    fn staged(&mut self, ui: &mut Ui, state: &ViewState, palette: Palette, out: &mut Outbox) {
        let ticked: Vec<&StagedPackage> = state
            .staged
            .iter()
            .filter(|package| !self.unticked.contains(&package.id))
            .collect();
        let ids: Vec<u64> = ticked.iter().map(|package| package.id).collect();
        let files: usize = ticked.iter().map(|package| package.files.len()).sum();
        let any = !ids.is_empty();

        egui::Sides::new().show(
            ui,
            |ui| {
                ui.heading("Staged");
                ui.label(
                    RichText::new(format!(
                        "{} package(s) · {} file(s)",
                        state.staged.len(),
                        state.staged_files()
                    ))
                    .weak(),
                );
            },
            |ui| {
                // Right to left.
                if ui
                    .add_enabled(any, Button::new("Discard"))
                    .on_hover_text("Drop the ticked packages from the LinkGrabber")
                    .clicked()
                {
                    out.send(Command::Discard {
                        staged: ids.clone(),
                    });
                }
                if ui
                    .add_enabled(any, Button::new("Add to download list"))
                    .on_hover_text("Queue the ticked packages without starting")
                    .clicked()
                {
                    out.send(Command::Accept {
                        staged: ids.clone(),
                        start: false,
                    });
                }
                if ui
                    .add_enabled(
                        any,
                        Button::new(RichText::new(format!("⬇ Add & start ({files})")).strong()),
                    )
                    .on_hover_text("Queue the ticked packages and start downloading")
                    .clicked()
                {
                    out.send(Command::Accept {
                        staged: ids.clone(),
                        start: true,
                    });
                    out.goto(Tab::Downloads);
                }
            },
        );
        ui.add_space(4.0);

        if state.staged.is_empty() {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(
                        "Resolved links show up here. Nothing is downloaded until you add them.",
                    )
                    .weak(),
                );
            });
            return;
        }
        ScrollArea::vertical()
            .id_salt("igdl_staged")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for package in &state.staged {
                    self.staged_package(ui, package, palette);
                    ui.add_space(4.0);
                }
            });
    }

    fn staged_package(&mut self, ui: &mut Ui, package: &StagedPackage, palette: Palette) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let open = self.expanded.contains(&package.id);
            ui.horizontal_wrapped(|ui| {
                let (arrow, verb) = if open {
                    ("▾", "Collapse")
                } else {
                    ("▸", "Expand")
                };
                if ui
                    .add(Button::new(RichText::new(arrow).monospace().strong()).frame(false))
                    .accessible_name(format!("{verb} {}", package.name))
                    .clicked()
                    && !self.expanded.remove(&package.id)
                {
                    self.expanded.insert(package.id);
                }
                let mut ticked = !self.unticked.contains(&package.id);
                if ui
                    .checkbox(&mut ticked, RichText::new(&package.name).strong())
                    .on_hover_text(&package.source)
                    .changed()
                {
                    if ticked {
                        self.unticked.remove(&package.id);
                    } else {
                        self.unticked.insert(package.id);
                    }
                }
                let (images, videos) = kind_counts(package);
                ui.label(format!(
                    "{} file(s): {images} image(s), {videos} video(s)",
                    package.files.len()
                ));
                if package.archived > 0 {
                    ui.label(
                        RichText::new(format!("{} already downloaded", package.archived)).weak(),
                    );
                }
                if package.already_queued > 0 {
                    ui.label(
                        RichText::new(format!("{} already in the list", package.already_queued))
                            .weak(),
                    );
                }
                ui.label(RichText::new(format!("📁 {}", package.out_dir.display())).weak());
            });
            for note in &package.notes {
                ui.colored_label(palette.amber, format!("ℹ {note}"));
            }
            if open {
                staged_files(ui, package);
            }
        });
    }
}

fn staged_files(ui: &mut Ui, package: &StagedPackage) {
    ui.indent(("igdl_staged_files", package.id), |ui| {
        Grid::new(("igdl_staged_grid", package.id))
            .striped(true)
            .num_columns(3)
            .show(ui, |ui| {
                for file in package.files.iter().take(MAX_LISTED_FILES) {
                    ui.monospace(file.rel_path.display().to_string());
                    ui.label(file.media.kind.as_str());
                    match (file.media.width, file.media.height) {
                        (Some(width), Some(height)) => ui.label(format!("{width}×{height}")),
                        _ => ui.label(""),
                    };
                    ui.end_row();
                }
            });
        if package.files.len() > MAX_LISTED_FILES {
            ui.label(
                RichText::new(format!(
                    "… and {} more",
                    package.files.len() - MAX_LISTED_FILES
                ))
                .weak(),
            );
        }
    });
}

/// A `YYYY-MM-DD` field with a red hint while the text is not a date.
fn date_field(
    ui: &mut Ui,
    name: &str,
    value: &mut String,
    error: Option<String>,
    palette: Palette,
) {
    let label = ui.label(name);
    let mut edit = TextEdit::singleline(value)
        .hint_text("YYYY-MM-DD")
        .desired_width(108.0);
    if error.is_some() {
        edit = edit.text_color(palette.red);
    }
    ui.add(edit).labelled_by(label.id);
    if let Some(error) = error {
        ui.colored_label(palette.red, "use YYYY-MM-DD")
            .on_hover_text(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> GrabForm {
        GrabForm {
            out_dir: "/downloads".into(),
            ..GrabForm::default()
        }
    }

    #[test]
    fn builds_a_request_from_the_options() {
        let mut form = form();
        form.sections = [true, false, false, true, false];
        form.limit = 25;
        form.only = Some(MediaKind::Video);
        form.since = " 2024-01-01 ".into();
        form.update = true;
        let request = form.request("@example_user".into()).unwrap();
        assert_eq!(
            request,
            GrabRequest {
                text: "@example_user".into(),
                sections: vec![ProfileSection::Posts, ProfileSection::Stories],
                limit: Some(25),
                only: Some(MediaKind::Video),
                since: "2024-01-01".into(),
                until: String::new(),
                update: true,
                out_dir: PathBuf::from("/downloads"),
                template: DEFAULT_TEMPLATE.into(),
            }
        );
        form.limit = 0;
        assert_eq!(form.request("x".into()).unwrap().limit, None);
    }

    #[test]
    fn rejects_bad_options() {
        let mut form = form();
        assert!(form.request("   \n# comment".into()).is_err());
        form.since = "2024-13-01".into();
        assert!(form.options_error().unwrap().starts_with("Since"));
        form.since = "2024-05-02".into();
        form.until = "2024-05-01".into();
        assert_eq!(form.options_error().unwrap(), "Since is after Until");
        form.until = "2024-05-02".into();
        assert_eq!(form.options_error(), None, "same day is fine");
        form.template = "{nope}".into();
        assert!(form.options_error().unwrap().starts_with("Template"));
        form.template = DEFAULT_TEMPLATE.into();
        form.sections = [false; 5];
        assert!(form.options_error().is_some());
        form.sections = [true; 5];
        form.out_dir = " ".into();
        assert!(form.options_error().is_some());
    }

    #[test]
    fn defaults_follow_settings_until_edited() {
        let mut state = ViewState::default();
        state.default_out_dir = PathBuf::from("/home/me");
        state.settings.include = Some(vec!["reels".into(), "Stories".into()]);
        let mut form = GrabForm::default();
        form.load_defaults(&state);
        assert_eq!(form.out_dir, "/home/me");
        assert_eq!(
            form.sections(),
            vec![ProfileSection::Reels, ProfileSection::Stories]
        );

        state.settings.out_dir = Some(PathBuf::from("/data/insta"));
        form.sections_edited = true;
        form.sections = [true; 5];
        form.load_defaults(&state);
        assert_eq!(form.out_dir, "/data/insta");
        assert_eq!(form.sections, [true; 5]);
        assert_eq!(sections_from(None), [true, false, false, false, false]);
        assert_eq!(
            sections_from(Some(&["bogus".to_string()])),
            [true, false, false, false, false]
        );
    }
}
