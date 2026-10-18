//! The Settings tab: a form over a copy of [`Settings`], saved with `ApplySettings`.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Button, DragValue, Grid, RichText, ScrollArea, TextEdit, Ui};

use super::Outbox;
use super::linkgrabber::{section_label, sections_from};
use super::style::Palette;
use crate::config::{MAX_JOBS, MAX_RETRIES, Settings, parse_rate};
use crate::gui::protocol::{AccountStatus, Command};
use crate::gui::state::ViewState;
use crate::links::ProfileSection;
use crate::model::{Category, MediaFile, MediaKind};
use crate::naming::{DEFAULT_TEMPLATE, PLACEHOLDERS, render, validate_template};

/// Values the downloader uses when the settings leave them unset.
const DEFAULT_JOBS: usize = 4;
const DEFAULT_RETRIES: u32 = 5;
const DEFAULT_API_DELAY: f64 = 1.5;

/// The settings as edited, plus the settings they started from.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SettingsForm {
    original: Settings,
    pub cookies: String,
    pub out_dir: String,
    pub template: String,
    pub jobs: usize,
    pub retries: u32,
    pub limit_rate: String,
    pub api_delay: f64,
    pub user_agent: String,
    pub set_mtime: bool,
    /// Default profile sections, in [`ProfileSection::ALL`] order.
    pub sections: [bool; 5],
}

impl Default for SettingsForm {
    fn default() -> Self {
        SettingsForm::new(&Settings::default())
    }
}

impl SettingsForm {
    /// A form showing `settings` (unset values show their defaults).
    pub fn new(settings: &Settings) -> Self {
        let display = |path: &Option<PathBuf>| {
            path.as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        };
        SettingsForm {
            original: settings.clone(),
            cookies: display(&settings.cookies),
            out_dir: display(&settings.out_dir),
            template: settings
                .template
                .clone()
                .unwrap_or_else(|| DEFAULT_TEMPLATE.to_string()),
            jobs: settings.jobs.unwrap_or(DEFAULT_JOBS),
            retries: settings.retries.unwrap_or(DEFAULT_RETRIES),
            limit_rate: settings.limit_rate.clone().unwrap_or_default(),
            api_delay: settings.api_delay.unwrap_or(DEFAULT_API_DELAY),
            user_agent: settings.user_agent.clone().unwrap_or_default(),
            set_mtime: settings.set_mtime.unwrap_or(true),
            sections: sections_from(settings.include.as_deref()),
        }
    }

    /// The settings the form describes. Untouched values come back exactly as
    /// they were (unset stays unset), so an unchanged form equals its original.
    pub fn settings(&self) -> Settings {
        let original = &self.original;
        Settings {
            out_dir: path_field(&self.out_dir, &original.out_dir),
            cookies: path_field(&self.cookies, &original.cookies),
            data_dir: original.data_dir.clone(),
            jobs: value_field(self.jobs, original.jobs, DEFAULT_JOBS),
            template: template_field(&self.template, &original.template),
            retries: value_field(self.retries, original.retries, DEFAULT_RETRIES),
            api_delay: value_field(self.api_delay, original.api_delay, DEFAULT_API_DELAY),
            limit_rate: text_field(&self.limit_rate, &original.limit_rate),
            user_agent: text_field(&self.user_agent, &original.user_agent),
            include: sections_field(&self.sections, &original.include),
            set_mtime: value_field(self.set_mtime, original.set_mtime, true),
        }
    }

    /// Whether the form differs from the settings it started from.
    pub fn is_dirty(&self) -> bool {
        self.settings() != self.original
    }

    /// The edited settings, or why they cannot be saved.
    pub fn validate(&self) -> Result<Settings, String> {
        if self.sections.iter().all(|ticked| !ticked) {
            return Err("Tick at least one default profile section".into());
        }
        let settings = self.settings();
        settings.validate().map_err(|err| err.to_string())?;
        if let Some(template) = &settings.template {
            validate_template(template).map_err(|err| format!("template: {err}"))?;
        }
        Ok(settings)
    }

    /// The speed limit's problem, if it has one (empty = unlimited).
    fn rate_error(&self) -> Option<String> {
        let rate = self.limit_rate.trim();
        if rate.is_empty() {
            return None;
        }
        parse_rate(rate).err().map(|err| err.to_string())
    }

    /// Where the sample post would be saved with the current template.
    fn template_preview(&self) -> Result<PathBuf, String> {
        let template = match self.template.trim() {
            "" => DEFAULT_TEMPLATE,
            template => template,
        };
        render(template, &sample_media()).map_err(|err| err.to_string())
    }

    /// The settings were saved: they are the new baseline.
    fn mark_saved(&mut self, settings: Settings) {
        self.original = settings;
    }
}

/// An unchanged path keeps its original value; empty means unset. See
/// [`clean_path`] for what is trimmed.
fn path_field(text: &str, original: &Option<PathBuf>) -> Option<PathBuf> {
    if original
        .as_deref()
        .is_some_and(|path| path.display().to_string() == text)
    {
        return original.clone();
    }
    let text = clean_path(text);
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// A typed or pasted path without surrounding whitespace and without one pair
/// of surrounding double quotes, which Explorer's "Copy as path" adds (a Windows
/// path cannot contain `"`).
pub(super) fn clean_path(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(text)
}

/// An unchanged text keeps its original value; empty means unset.
fn text_field(text: &str, original: &Option<String>) -> Option<String> {
    if original.as_deref() == Some(text) {
        return original.clone();
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Like [`text_field`], but the default template counts as unset when it was.
fn template_field(text: &str, original: &Option<String>) -> Option<String> {
    let value = text_field(text, original);
    if original.is_none() && value.as_deref() == Some(DEFAULT_TEMPLATE) {
        return None;
    }
    value
}

/// A value equal to the default stays unset if it was unset.
fn value_field<T: PartialEq + Copy>(value: T, original: Option<T>, default: T) -> Option<T> {
    match original {
        None if value == default => None,
        _ => Some(value),
    }
}

/// The ticked sections as `include` names; the same set as before keeps the
/// original spelling, and the default (posts) stays unset if it was.
fn sections_field(ticked: &[bool; 5], original: &Option<Vec<String>>) -> Option<Vec<String>> {
    if sections_from(original.as_deref()) == *ticked
        && original.as_ref().is_none_or(|list| !list.is_empty())
    {
        return original.clone();
    }
    Some(
        ProfileSection::ALL
            .into_iter()
            .zip(ticked)
            .filter(|(_, ticked)| **ticked)
            .map(|(section, _)| section.as_str().to_string())
            .collect(),
    )
}

/// The post the template preview is rendered for (made-up values).
fn sample_media() -> MediaFile {
    MediaFile {
        key: "3359000000000000001".into(),
        post_id: "3359000000000000001".into(),
        shortcode: Some("C6abcDEFghi".into()),
        index: 1,
        count: 3,
        kind: MediaKind::Image,
        category: Category::Post,
        url: String::new(),
        width: Some(1080),
        height: Some(1350),
        owner: "natgeo".into(),
        owner_id: Some("123456789".into()),
        // 2024-05-01 13:37:00 UTC
        taken_at: 1_714_570_620,
        caption: Some("A sample caption".into()),
        highlight: None,
        ext: "jpg".into(),
    }
}

/// What a placeholder stands for (README "Filename templates").
fn placeholder_help(name: &str) -> &'static str {
    match name {
        "owner" => "username of the post owner",
        "owner_id" => "numeric user id",
        "category" => "posts, reels, tagged, stories or highlights",
        "id" => "shortcode if the item has one, otherwise the media id",
        "shortcode" => "post shortcode",
        "post_id" => "id of the post",
        "media_id" => "id of the file",
        "index" => "position in a carousel (01)",
        "count" => "number of files in the post",
        "date" => "2024-05-01 (UTC)",
        "time" => "13-37-00 (UTC)",
        "datetime" => "20240501_133700 (UTC)",
        "timestamp" => "unix seconds",
        "kind" => "image or video",
        "ext" => "file extension",
        "width" => "pixel width when known",
        "height" => "pixel height when known",
        "highlight" => "highlight title (empty otherwise)",
        "caption" => "first 50 characters of the caption",
        _ => "",
    }
}

/// State of the Settings tab.
#[derive(Debug, Default)]
pub(super) struct SettingsView {
    form: SettingsForm,
    /// Last `settings_generation` the form was loaded from.
    generation: u64,
}

impl SettingsView {
    /// Reloads the form when new settings arrive, unless the user is editing it.
    pub fn sync(&mut self, state: &ViewState) {
        if state.settings_generation == self.generation {
            return;
        }
        self.generation = state.settings_generation;
        if self.form.is_dirty() {
            self.form.original = state.settings.clone();
        } else {
            self.form = SettingsForm::new(&state.settings);
        }
    }

    pub fn show(&mut self, ui: &mut Ui, state: &ViewState, out: &mut Outbox) {
        let palette = Palette::of(ui);
        ScrollArea::vertical()
            .id_salt("igdl_settings")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                Grid::new("igdl_settings_grid")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| self.fields(ui, state, palette, out));
                ui.add_space(8.0);
                ui.separator();
                self.actions(ui, state, palette, out);
            });
    }

    fn fields(&mut self, ui: &mut Ui, state: &ViewState, palette: Palette, out: &mut Outbox) {
        let form = &mut self.form;

        let label = ui.label("Cookies file");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.add(
                    TextEdit::singleline(&mut form.cookies)
                        .hint_text("cookies.txt or JSON export from your browser")
                        .desired_width(380.0),
                )
                .labelled_by(label.id);
                if ui.button("Browse…").clicked()
                    && let Some(file) = pick_file(&form.cookies)
                {
                    form.cookies = file.display().to_string();
                }
                let dirty = form.is_dirty();
                let saved = form.validate();
                let has_cookies =
                    !clean_path(&form.cookies).is_empty() || state.cookies_override.is_some();
                let can_check = has_cookies && (!dirty || saved.is_ok());
                if ui
                    .add_enabled(can_check, Button::new("Check login"))
                    .on_hover_text("Save, then ask Instagram which account the cookies belong to")
                    .clicked()
                {
                    if dirty && let Ok(settings) = saved {
                        out.send(Command::ApplySettings(Box::new(settings.clone())));
                        form.mark_saved(settings);
                    }
                    out.send(Command::CheckAccount);
                }
            });
            if let Some(path) = &state.cookies_override {
                ui.colored_label(palette.amber, cookies_override_note(path));
            }
            account_line(ui, state, palette);
        });
        ui.end_row();

        let label = ui.label("Default download folder");
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut form.out_dir)
                    .hint_text(state.default_out_dir.display().to_string())
                    .desired_width(380.0),
            )
            .labelled_by(label.id);
            if ui.button("Browse…").clicked() {
                let mut dialog = rfd::FileDialog::new().set_title("Default download folder");
                if !clean_path(&form.out_dir).is_empty() {
                    dialog = dialog.set_directory(clean_path(&form.out_dir));
                }
                if let Some(dir) = dialog.pick_folder() {
                    form.out_dir = dir.display().to_string();
                }
            }
        });
        ui.end_row();

        let label = ui.label("Filename template");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.add(
                    TextEdit::singleline(&mut form.template)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(DEFAULT_TEMPLATE)
                        .desired_width(380.0),
                )
                .labelled_by(label.id);
                ui.menu_button("Placeholders", |ui| {
                    ui.label(RichText::new("Click to append to the template").weak());
                    Grid::new("igdl_placeholders")
                        .num_columns(2)
                        .show(ui, |ui| {
                            for name in PLACEHOLDERS {
                                if ui
                                    .button(RichText::new(format!("{{{name}}}")).monospace())
                                    .clicked()
                                {
                                    form.template.push_str(&format!("{{{name}}}"));
                                    ui.close();
                                }
                                ui.label(placeholder_help(name));
                                ui.end_row();
                            }
                        });
                });
                if ui.small_button("Default").clicked() {
                    form.template = DEFAULT_TEMPLATE.to_string();
                }
            });
            match form.template_preview() {
                Ok(path) => ui.label(
                    RichText::new(format!("Preview: {}", path.display()))
                        .monospace()
                        .weak(),
                ),
                Err(error) => ui.colored_label(palette.red, error),
            };
        });
        ui.end_row();

        let label = ui.label("Parallel downloads");
        ui.add(DragValue::new(&mut form.jobs).range(1..=MAX_JOBS))
            .labelled_by(label.id);
        ui.end_row();

        let label = ui.label("Retries per file");
        ui.add(DragValue::new(&mut form.retries).range(0..=MAX_RETRIES))
            .labelled_by(label.id);
        ui.end_row();

        let label = ui.label("Speed limit");
        ui.horizontal(|ui| {
            let error = form.rate_error();
            let mut edit = TextEdit::singleline(&mut form.limit_rate)
                .hint_text("unlimited")
                .desired_width(110.0);
            if error.is_some() {
                edit = edit.text_color(palette.red);
            }
            ui.add(edit).labelled_by(label.id);
            match error {
                Some(error) => ui.colored_label(palette.red, error),
                None => {
                    ui.label(RichText::new("e.g. 500K or 2M per second; empty = unlimited").weak())
                }
            };
        });
        ui.end_row();

        let label = ui.label("API delay");
        ui.horizontal(|ui| {
            ui.add(
                DragValue::new(&mut form.api_delay)
                    .range(0.0..=60.0)
                    .speed(0.05)
                    .max_decimals(2)
                    .suffix(" s"),
            )
            .labelled_by(label.id);
            ui.label(RichText::new("pause between Instagram API requests").weak());
        });
        ui.end_row();

        let label = ui.label("User agent");
        ui.add(
            TextEdit::singleline(&mut form.user_agent)
                .hint_text("desktop Chrome (best: the browser the cookies came from)")
                .desired_width(520.0),
        )
        .labelled_by(label.id);
        ui.end_row();

        ui.label("File dates");
        ui.checkbox(&mut form.set_mtime, "Set file date to the post date");
        ui.end_row();

        ui.label("Default profile sections");
        ui.horizontal_wrapped(|ui| {
            for (section, ticked) in ProfileSection::ALL
                .into_iter()
                .zip(form.sections.iter_mut())
            {
                ui.checkbox(ticked, section_label(section));
            }
        });
        ui.end_row();
    }

    fn actions(&mut self, ui: &mut Ui, state: &ViewState, palette: Palette, out: &mut Outbox) {
        let dirty = self.form.is_dirty();
        let validated = self.form.validate();
        ui.horizontal(|ui| {
            let save = ui
                .add_enabled(
                    dirty && validated.is_ok(),
                    Button::new(RichText::new("Save").strong()).min_size(egui::vec2(80.0, 0.0)),
                )
                .on_hover_text("Save to the config file and apply");
            if save.clicked()
                && let Ok(settings) = &validated
            {
                out.send(Command::ApplySettings(Box::new(settings.clone())));
                self.form.mark_saved(settings.clone());
            }
            if ui
                .add_enabled(dirty, Button::new("Revert"))
                .on_hover_text("Discard the changes")
                .clicked()
            {
                self.form = SettingsForm::new(&self.form.original);
            }
            match &validated {
                Err(error) => {
                    ui.colored_label(palette.red, error);
                }
                Ok(_) if dirty => {
                    ui.colored_label(palette.amber, "Unsaved changes");
                }
                Ok(_) => {}
            }
        });
        ui.add_space(6.0);
        let location = match &state.config_path {
            Some(path) => format!("Saved to {}", path.display()),
            None => "There is no config file location; changes last until igdl closes.".to_string(),
        };
        ui.label(RichText::new(location).weak());
        ui.label(
            RichText::new("Parallel downloads and speed limit apply to the next start.").weak(),
        );
    }
}

fn pick_file(current: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new()
        .set_title("Instagram cookie file")
        .add_filter("Cookie export", &["txt", "json"])
        .add_filter("All files", &["*"]);
    if let Some(dir) = Path::new(clean_path(current)).parent()
        && dir.is_dir()
    {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file()
}

/// Why the cookie field has no effect while `--cookies` / `IGDL_COOKIES` is set.
fn cookies_override_note(path: &Path) -> String {
    format!(
        "Using {} from IGDL_COOKIES or --cookies; this field is ignored while it is set",
        path.display()
    )
}

fn account_line(ui: &mut Ui, state: &ViewState, palette: Palette) {
    match &state.account {
        Some(AccountStatus::LoggedIn { username }) => {
            ui.colored_label(palette.green, format!("✔ Logged in as @{username}"));
        }
        Some(AccountStatus::Invalid(message)) => {
            ui.colored_label(palette.red, message);
        }
        Some(AccountStatus::Unverified(message)) => {
            ui.colored_label(
                palette.amber,
                format!("Could not verify right now: {message}. The cookies may still be fine; try grabbing a post."),
            );
        }
        Some(AccountStatus::Checking) => {
            ui.label(RichText::new("Checking with Instagram…").weak());
        }
        Some(AccountStatus::Loaded) => {
            ui.label(RichText::new("Cookies loaded; not checked with Instagram yet").weak());
        }
        Some(AccountStatus::NoCookies) | None => {
            ui.label(
                RichText::new(
                    "Log in to instagram.com in your browser, export its cookies (cookies.txt or JSON) and choose the file here.",
                )
                .weak(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Settings {
        Settings {
            out_dir: Some(PathBuf::from("/data/insta")),
            cookies: Some(PathBuf::from("/secrets/cookies.txt")),
            data_dir: Some(PathBuf::from("/data/igdl")),
            jobs: Some(6),
            template: Some("{owner}/{id}.{ext}".into()),
            retries: Some(3),
            api_delay: Some(2.0),
            limit_rate: Some("2M".into()),
            user_agent: Some("Mozilla/5.0 Test".into()),
            include: Some(vec!["posts".into(), "Stories".into()]),
            set_mtime: Some(false),
        }
    }

    #[test]
    fn unchanged_forms_round_trip() {
        for settings in [Settings::default(), full()] {
            let form = SettingsForm::new(&settings);
            assert_eq!(form.settings(), settings);
            assert!(!form.is_dirty());
            assert_eq!(form.validate().unwrap(), settings);
        }
    }

    #[test]
    fn edits_make_the_form_dirty() {
        let mut form = SettingsForm {
            jobs: 6,
            limit_rate: " 500K ".into(),
            cookies: "/c/cookies.txt".into(),
            sections: [false, true, false, false, false],
            ..SettingsForm::default()
        };
        assert!(form.is_dirty());
        let settings = form.validate().unwrap();
        assert_eq!(settings.jobs, Some(6));
        assert_eq!(settings.limit_rate.as_deref(), Some("500K"));
        assert_eq!(settings.cookies, Some(PathBuf::from("/c/cookies.txt")));
        assert_eq!(settings.include, Some(vec!["reels".to_string()]));
        assert_eq!(settings.template, None, "default template stays unset");

        form.mark_saved(settings);
        assert!(!form.is_dirty());

        // Clearing a value unsets it.
        let mut form = SettingsForm::new(&full());
        form.limit_rate.clear();
        form.out_dir = "  ".into();
        let settings = form.settings();
        assert_eq!(settings.limit_rate, None);
        assert_eq!(settings.out_dir, None);
        assert_eq!(settings.data_dir, full().data_dir, "not editable, kept");
    }

    #[test]
    fn pasted_paths_lose_whitespace_and_one_pair_of_quotes() {
        assert_eq!(
            clean_path(r#"  "C:\Users\me\cookies.txt" "#),
            r"C:\Users\me\cookies.txt"
        );
        assert_eq!(clean_path(" /data/insta \n"), "/data/insta");
        assert_eq!(clean_path(r#""D:\My Insta""#), r"D:\My Insta");
        assert_eq!(clean_path(r#"""x"""#), r#""x""#, "only one pair");
        assert_eq!(clean_path(r#""x"#), r#""x"#, "unpaired quotes stay");
        assert_eq!(clean_path(r#"""#), r#"""#);
        assert_eq!(clean_path(r#""""#), "");

        let form = SettingsForm {
            cookies: r#""C:\Users\me\cookies.txt""#.into(),
            out_dir: r#" "D:\My Insta" "#.into(),
            ..SettingsForm::default()
        };
        let settings = form.validate().unwrap();
        assert_eq!(
            settings.cookies,
            Some(PathBuf::from(r"C:\Users\me\cookies.txt"))
        );
        assert_eq!(settings.out_dir, Some(PathBuf::from(r"D:\My Insta")));

        let form = SettingsForm {
            out_dir: r#""""#.into(),
            ..SettingsForm::new(&full())
        };
        assert_eq!(form.settings().out_dir, None, "empty quotes unset it");
    }

    #[test]
    fn invalid_values_are_reported() {
        let form = SettingsForm {
            limit_rate: "fast".into(),
            ..SettingsForm::default()
        };
        assert!(form.rate_error().is_some());
        assert!(form.validate().unwrap_err().contains("limit_rate"));

        let form = SettingsForm {
            template: "{bogus}".into(),
            ..SettingsForm::default()
        };
        assert!(form.validate().is_err());
        assert!(form.template_preview().is_err());

        let form = SettingsForm {
            sections: [false; 5],
            ..SettingsForm::default()
        };
        assert!(form.validate().is_err());
    }

    #[test]
    fn previews_the_template() {
        let form = SettingsForm::default();
        let preview = form.template_preview().unwrap();
        assert_eq!(
            preview,
            PathBuf::from("natgeo/posts/2024-05-01_C6abcDEFghi_01.jpg")
        );
        for name in PLACEHOLDERS {
            assert!(!placeholder_help(name).is_empty(), "{name}");
        }
    }
}
