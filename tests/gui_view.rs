//! Drives the window headless (egui_kittest + AccessKit) with fake controller
//! updates and checks the commands it sends back.
#![cfg(feature = "gui")]

use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use eframe::egui::{self, ViewportCommand, ViewportEvent, ViewportId};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use igdl::config::Settings;
use igdl::error::ErrorKind;
use igdl::gui::controller::UiLink;
use igdl::gui::protocol::{
    AccountStatus, Command, GrabRequest, LogLevel, LogLine, StagedFile, StagedPackage, Update,
};
use igdl::gui::view::IgdlApp;
use igdl::links::ProfileSection;
use igdl::model::{Category, Item, ItemError, ItemStatus, MediaFile, MediaKind, Package};
use igdl::naming::DEFAULT_TEMPLATE;
use igdl::queue::Queue;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

struct Window {
    harness: Harness<'static, IgdlApp>,
    updates: mpsc::Sender<Update>,
    commands: UnboundedReceiver<Command>,
}

impl Window {
    /// A window with a fake controller that sent its usual start-up updates.
    fn new() -> Self {
        let (command_tx, commands) = unbounded_channel();
        let (updates, update_rx) = mpsc::channel();
        let link = UiLink {
            commands: command_tx,
            updates: update_rx,
        };
        let harness = Harness::builder()
            .with_size([1180.0, 720.0])
            .build_ui_state(
                |ui, app: &mut IgdlApp| {
                    app.tick(ui.ctx());
                    app.draw(ui);
                },
                IgdlApp::new(link, None),
            );
        let mut window = Window {
            harness,
            updates,
            commands,
        };
        window.send(Update::Settings {
            settings: Box::default(),
            config_path: Some(PathBuf::from("/home/tester/.config/igdl/config.toml")),
            default_out_dir: PathBuf::from("/home/tester/Downloads"),
            cookies_override: None,
        });
        window.send(Update::Account(AccountStatus::Loaded));
        window.send(Update::Queue(Arc::new(Queue::default())));
        window.send(Update::Staged(Vec::new()));
        window.send(Update::Grabbing { pending_links: 0 });
        window.send(Update::Downloading(false));
        window.run();
        window
    }

    fn send(&mut self, update: Update) {
        self.updates.send(update).unwrap();
    }

    fn run(&mut self) {
        self.harness.run();
    }

    fn click(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        self.run();
    }

    /// Clicks and runs a few frames (for screens with a spinner, which never settle).
    fn click_steps(&mut self, label: &str) {
        self.harness.get_by_label_contains(label).click();
        self.harness.run_steps(3);
    }

    /// Clicks and runs exactly one frame, so that frame's viewport commands can be read.
    fn click_once(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        self.harness.step();
    }

    fn click_containing(&mut self, label: &str) {
        self.harness.get_by_label_contains(label).click();
        self.run();
    }

    fn type_into(&mut self, label: &str, text: &str) {
        self.harness.get_by_label(label).focus();
        self.harness.step();
        self.harness.get_by_label(label).type_text(text);
        self.run();
    }

    fn sent(&mut self) -> Vec<Command> {
        let mut sent = Vec::new();
        while let Ok(command) = self.commands.try_recv() {
            sent.push(command);
        }
        sent
    }

    fn shows(&self, label: &str) -> bool {
        self.harness.query_all_by_label(label).next().is_some()
    }

    fn shows_containing(&self, label: &str) -> bool {
        self.harness
            .query_all_by_label_contains(label)
            .next()
            .is_some()
    }

    fn is_enabled(&self, label: &str) -> bool {
        !self
            .harness
            .get_by_label(label)
            .accesskit_node()
            .is_disabled()
    }

    fn viewport_commands(&self) -> Vec<ViewportCommand> {
        self.harness.output().viewport_output[&ViewportId::ROOT]
            .commands
            .clone()
    }
}

fn media(key: &str, kind: MediaKind) -> MediaFile {
    MediaFile {
        key: key.to_string(),
        post_id: format!("90{key}"),
        shortcode: Some("C0ffeeXYZ12".to_string()),
        index: 1,
        count: 1,
        kind,
        category: Category::Post,
        url: "https://cdn.example.invalid/media.jpg".to_string(),
        width: Some(1080),
        height: Some(1350),
        owner: "example_user".to_string(),
        owner_id: Some("42".to_string()),
        taken_at: 1_714_521_600,
        caption: None,
        highlight: None,
        ext: match kind {
            MediaKind::Image => "jpg".to_string(),
            MediaKind::Video => "mp4".to_string(),
        },
    }
}

fn item(id: u64, name: &str, status: ItemStatus) -> Item {
    Item {
        id,
        media: media(&format!("k{id}"), MediaKind::Image),
        rel_path: PathBuf::from(format!("example_user/posts/{name}")),
        status,
        bytes_done: if status == ItemStatus::Done { 2048 } else { 0 },
        bytes_total: (status == ItemStatus::Done).then_some(2048),
        error: (status == ItemStatus::Failed).then(|| ItemError {
            kind: ErrorKind::Network,
            message: "connection reset by peer".to_string(),
        }),
        path: None,
        note: None,
    }
}

/// Two packages: one finished, one with a failed, a running and a queued file.
fn sample_queue() -> Arc<Queue> {
    Arc::new(Queue {
        version: 1,
        next_id: 20,
        packages: vec![
            Package {
                id: 1,
                name: "post C0ffeeXYZ12".to_string(),
                source: "https://www.instagram.com/p/C0ffeeXYZ12/".to_string(),
                out_dir: PathBuf::from("/home/tester/Downloads"),
                created_at: 0,
                items: vec![
                    item(2, "done_one.jpg", ItemStatus::Done),
                    item(3, "done_two.jpg", ItemStatus::Done),
                ],
            },
            Package {
                id: 10,
                name: "example_user · posts".to_string(),
                source: "https://www.instagram.com/example_user/".to_string(),
                out_dir: PathBuf::from("/home/tester/Downloads"),
                created_at: 0,
                items: vec![
                    item(11, "broken.jpg", ItemStatus::Failed),
                    item(12, "running.jpg", ItemStatus::Downloading),
                    item(13, "waiting.jpg", ItemStatus::Queued),
                ],
            },
        ],
    })
}

fn staged(id: u64, name: &str, files: usize) -> StagedPackage {
    StagedPackage {
        id,
        name: name.to_string(),
        source: format!("https://www.instagram.com/{name}/"),
        out_dir: PathBuf::from("/home/tester/Downloads"),
        files: (0..files)
            .map(|n| StagedFile {
                media: media(
                    &format!("{id}-{n}"),
                    if n % 2 == 0 {
                        MediaKind::Image
                    } else {
                        MediaKind::Video
                    },
                ),
                rel_path: PathBuf::from(format!("{name}/posts/file_{n}.jpg")),
            })
            .collect(),
        archived: 1,
        already_queued: 0,
        notes: vec!["stories: none available".to_string()],
    }
}

#[test]
fn shows_packages_statuses_and_counts() {
    let mut window = Window::new();
    window.send(Update::Queue(sample_queue()));
    window.send(Update::Downloading(true));
    window.send(Update::Progress {
        item_id: 12,
        done: 1024,
        total: Some(4096),
    });
    window.run();

    assert!(window.shows("post C0ffeeXYZ12"));
    assert!(window.shows("example_user · posts"));
    assert!(window.shows("done_one.jpg"));
    assert!(window.shows("broken.jpg"));
    assert!(window.shows("Failed"));
    assert!(window.shows("Downloading"));
    assert!(window.shows("25%"));
    assert!(window.shows_containing("⬇ Downloads (5)"));
    assert!(window.shows("1 downloading · 1 queued · 2 done · 1 failed"));
    assert!(window.is_enabled("⏹ Stop"));
    assert!(window.is_enabled("↻ Retry failed"));
    assert!(window.is_enabled("🗑 Clear finished"));

    // Collapsing a package hides its files.
    window.click("Collapse post C0ffeeXYZ12");
    assert!(!window.shows("done_one.jpg"));
    assert!(window.shows("Expand post C0ffeeXYZ12"));
}

#[test]
fn toolbar_buttons_send_commands() {
    let mut window = Window::new();
    assert!(!window.is_enabled("▶ Start all"), "nothing queued yet");
    window.send(Update::Queue(sample_queue()));
    window.run();

    window.click("▶ Start all");
    assert_eq!(window.sent(), vec![Command::Start]);
    assert!(!window.is_enabled("⏹ Stop"), "not downloading");

    window.click("↻ Retry failed");
    window.click("🗑 Clear finished");
    assert_eq!(
        window.sent(),
        vec![Command::Retry(Vec::new()), Command::ClearFinished]
    );

    window.send(Update::Downloading(true));
    window.run();
    window.click("⏹ Stop");
    assert_eq!(window.sent(), vec![Command::Stop]);
}

#[test]
fn removing_several_files_asks_first() {
    let mut window = Window::new();
    window.send(Update::Queue(sample_queue()));
    window.run();

    window.click("example_user · posts");
    window.harness.key_press(egui::Key::Delete);
    window.run();
    assert!(window.sent().is_empty(), "three files: confirmation first");
    assert!(window.shows_containing("3 files will be removed"));
    window.click("Remove");
    assert_eq!(window.sent(), vec![Command::Remove(vec![10])]);

    // A single file goes without asking.
    window.click("waiting.jpg");
    window.harness.key_press(egui::Key::Delete);
    window.run();
    assert_eq!(window.sent(), vec![Command::Remove(vec![13])]);
}

#[test]
fn context_menu_retries_the_selection() {
    let mut window = Window::new();
    window.send(Update::Queue(sample_queue()));
    window.run();

    window.harness.get_by_label("broken.jpg").click_secondary();
    window.run();
    window.click("Retry");
    assert_eq!(window.sent(), vec![Command::Retry(vec![11])]);
}

#[test]
fn ctrl_click_extends_the_selection() {
    let mut window = Window::new();
    window.send(Update::Queue(sample_queue()));
    window.run();

    window.click("done_one.jpg");
    window
        .harness
        .get_by_label("waiting.jpg")
        .click_modifiers(egui::Modifiers::COMMAND);
    window.run();
    // Right-clicking a selected row keeps the selection: both files go.
    window.harness.get_by_label("waiting.jpg").click_secondary();
    window.run();
    window.click("Remove");
    assert!(window.shows_containing("2 files will be removed"));
    window.click("Remove");
    assert_eq!(window.sent(), vec![Command::Remove(vec![2, 13])]);

    // Ctrl-clicking a selected row deselects it.
    window.click("done_two.jpg");
    window
        .harness
        .get_by_label("done_two.jpg")
        .click_modifiers(egui::Modifiers::COMMAND);
    window.run();
    window.harness.key_press(egui::Key::Delete);
    window.run();
    assert!(window.sent().is_empty(), "nothing selected");
}

#[test]
fn context_menu_copies_the_source_link() {
    let mut window = Window::new();
    window.send(Update::Queue(sample_queue()));
    window.run();

    window
        .harness
        .get_by_label("example_user · posts")
        .click_secondary();
    window.run();
    window.harness.get_by_label("Copy source link").click();
    window.harness.step();
    let copied = window
        .harness
        .output()
        .platform_output
        .commands
        .iter()
        .any(|command| {
            *command
                == egui::OutputCommand::CopyText(
                    "https://www.instagram.com/example_user/".to_string(),
                )
        });
    assert!(copied);
}

#[test]
fn empty_list_points_to_the_linkgrabber() {
    let mut window = Window::new();
    assert!(window.shows("No downloads yet — paste links in the LinkGrabber"));
    window.click("🔗 Open the LinkGrabber");
    assert!(window.shows("Links to grab"));
}

#[test]
fn grab_links_sends_the_text_and_options() {
    let mut window = Window::new();
    window.click_containing("🔗 LinkGrabber");
    assert!(!window.is_enabled("🔍 Grab links"), "no links yet");

    window.type_into(
        "Links to grab",
        "@example_user https://www.instagram.com/p/C0ffeeXYZ12/",
    );
    window.click("Reels");
    window.click("Only");
    window.click("Videos");
    window.click("Only new posts (stop at the first downloaded one)");
    window.type_into("Since", "2024-01-01");
    assert!(window.is_enabled("🔍 Grab links"));
    window.click("🔍 Grab links");

    assert_eq!(
        window.sent(),
        vec![Command::Grab(GrabRequest {
            text: "@example_user https://www.instagram.com/p/C0ffeeXYZ12/".to_string(),
            sections: vec![ProfileSection::Posts, ProfileSection::Reels],
            limit: None,
            only: Some(MediaKind::Video),
            since: "2024-01-01".to_string(),
            until: String::new(),
            update: true,
            out_dir: PathBuf::from("/home/tester/Downloads"),
            template: DEFAULT_TEMPLATE.to_string(),
        })]
    );
    assert_eq!(
        window
            .harness
            .get_by_label("Links to grab")
            .value()
            .as_deref(),
        Some(""),
        "the box is cleared"
    );
}

#[test]
fn invalid_date_blocks_grabbing() {
    let mut window = Window::new();
    window.click_containing("🔗 LinkGrabber");
    window.type_into("Links to grab", "@example_user");
    window.type_into("Until", "2024-13-45");
    assert!(window.shows("use YYYY-MM-DD"));
    assert!(!window.is_enabled("🔍 Grab links"));
}

#[test]
fn staged_packages_are_accepted() {
    let mut window = Window::new();
    window.send(Update::Staged(vec![
        staged(5, "example_user", 4),
        staged(6, "other_user", 1),
    ]));
    window.send(Update::Grabbing { pending_links: 2 });
    window.harness.run_steps(3);
    window.click_steps("🔗 LinkGrabber");
    assert!(window.shows("Resolving 2 link(s)…"));
    assert!(window.shows_containing("4 file(s): 2 image(s), 2 video(s)"));
    assert!(window.shows("1 already downloaded"));
    assert!(window.shows("ℹ stories: none available"));
    assert!(window.shows_containing("🔗 LinkGrabber (5 staged)"));

    window.send(Update::Grabbing { pending_links: 0 });
    window.run();

    // Untick the second package: only the first is accepted.
    window.click("other_user");
    window.click("⬇ Add & start (4)");
    assert_eq!(
        window.sent(),
        vec![Command::Accept {
            staged: vec![5],
            start: true,
        }]
    );
    // "Add & start" switches to the download list.
    assert!(window.shows("No downloads yet — paste links in the LinkGrabber"));

    window.click_containing("🔗 LinkGrabber");
    window.click("other_user");
    window.click("Add to download list");
    window.click("Discard");
    assert_eq!(
        window.sent(),
        vec![
            Command::Accept {
                staged: vec![5, 6],
                start: false,
            },
            Command::Discard { staged: vec![5, 6] },
        ]
    );
}

#[test]
fn settings_are_saved() {
    let mut window = Window::new();
    window.click("⚙ Settings");
    assert!(!window.is_enabled("Save"), "nothing changed yet");
    assert!(
        window.shows_containing("2024-05-01_C6abcDEFghi_01.jpg"),
        "template preview"
    );

    window.type_into("Speed limit", "2M");
    window.type_into("Cookies file", "/home/tester/cookies.txt");
    assert!(window.is_enabled("Save"));
    window.click("Save");
    assert_eq!(
        window.sent(),
        vec![Command::ApplySettings(Box::new(Settings {
            cookies: Some(PathBuf::from("/home/tester/cookies.txt")),
            limit_rate: Some("2M".to_string()),
            ..Settings::default()
        }))]
    );
    assert!(!window.is_enabled("Save"), "saved");
}

#[test]
fn invalid_speed_limit_disables_save() {
    let mut window = Window::new();
    window.click("⚙ Settings");
    window.type_into("Speed limit", "fast");
    assert!(!window.is_enabled("Save"));
    assert!(window.shows_containing("invalid rate 'fast'"));
    window.click("Revert");
    assert_eq!(
        window
            .harness
            .get_by_label("Speed limit")
            .value()
            .as_deref(),
        Some("")
    );
}

#[test]
fn new_settings_reset_an_untouched_form() {
    let mut window = Window::new();
    window.click("⚙ Settings");
    window.send(Update::Settings {
        settings: Box::new(Settings {
            limit_rate: Some("500K".to_string()),
            ..Settings::default()
        }),
        config_path: None,
        default_out_dir: PathBuf::from("/home/tester/Downloads"),
        cookies_override: None,
    });
    window.run();
    assert_eq!(
        window
            .harness
            .get_by_label("Speed limit")
            .value()
            .as_deref(),
        Some("500K")
    );
}

#[test]
fn quoted_paths_are_saved_without_quotes() {
    let mut window = Window::new();
    window.click("⚙ Settings");
    window.type_into("Cookies file", r#" "C:\Users\tester\cookies.txt" "#);
    window.type_into("Default download folder", r#""D:\My Insta""#);
    window.click("Save");
    assert_eq!(
        window.sent(),
        vec![Command::ApplySettings(Box::new(Settings {
            cookies: Some(PathBuf::from(r"C:\Users\tester\cookies.txt")),
            out_dir: Some(PathBuf::from(r"D:\My Insta")),
            ..Settings::default()
        }))]
    );
}

#[test]
fn cookie_override_is_shown_and_allows_checking() {
    let mut window = Window::new();
    window.click("⚙ Settings");
    assert!(!window.shows_containing("IGDL_COOKIES"));
    assert!(!window.is_enabled("Check login"), "no cookie file anywhere");

    window.send(Update::Settings {
        settings: Box::default(),
        config_path: None,
        default_out_dir: PathBuf::from("/home/tester/Downloads"),
        cookies_override: Some(PathBuf::from("/home/tester/env-cookies.txt")),
    });
    window.run();
    assert!(window.shows(
        "Using /home/tester/env-cookies.txt from IGDL_COOKIES or --cookies; \
         this field is ignored while it is set"
    ));
    assert!(window.is_enabled("Check login"));
    window.click("Check login");
    assert_eq!(window.sent(), vec![Command::CheckAccount]);
}

#[test]
fn account_chip_reflects_the_cookies() {
    let mut window = Window::new();
    window.click("Cookies loaded — check");
    assert_eq!(window.sent(), vec![Command::CheckAccount]);

    window.send(Update::Account(AccountStatus::LoggedIn {
        username: "example_user".to_string(),
    }));
    window.run();
    assert!(window.shows("@example_user"));

    window.send(Update::Account(AccountStatus::NoCookies));
    window.run();
    window.click("No cookies — set them in Settings");
    assert!(window.shows("Speed limit"), "Settings tab is open");

    window.send(Update::Account(AccountStatus::Invalid(
        "the cookies expired".to_string(),
    )));
    window.run();
    assert!(window.shows("Cookies problem"));
    assert!(
        window.shows("the cookies expired"),
        "Settings shows the reason"
    );
}

#[test]
fn unverified_login_is_amber_and_clicking_checks_again() {
    let mut window = Window::new();
    window.send(Update::Account(AccountStatus::Unverified(
        "rate limited by Instagram: HTTP 429".to_string(),
    )));
    window.run();
    assert!(!window.shows("Cookies problem"));
    window.click("Login not verified — retry");
    assert_eq!(window.sent(), vec![Command::CheckAccount]);
}

#[test]
fn log_tab_shows_lines_and_clears_the_badge() {
    let mut window = Window::new();
    for (level, text) in [
        (LogLevel::Info, "grabbed 3 links"),
        (LogLevel::Error, "post C0ffeeXYZ12: not found"),
        (LogLevel::Error, "rate limited by Instagram"),
    ] {
        window.send(Update::Log(LogLine {
            at: 1_714_570_620,
            level,
            text: text.to_string(),
        }));
    }
    window.run();
    assert!(window.shows("2"), "unseen error badge");

    window.click("📜 Log");
    assert!(window.shows("post C0ffeeXYZ12: not found"));
    assert!(window.shows("ERROR"));
    assert!(!window.shows("2"), "badge cleared");

    window.click("Clear");
    assert!(window.shows("Nothing logged yet."));
}

#[test]
fn fatal_error_shows_the_message() {
    let mut window = Window::new();
    window.send(Update::Fatal(
        "another igdl process is using the queue".to_string(),
    ));
    window.run();
    assert!(window.shows("another igdl process is using the queue"));
    window.click_once("Quit");
    assert!(window.viewport_commands().contains(&ViewportCommand::Close));
}

#[test]
fn closing_while_downloading_asks_first() {
    let mut window = Window::new();
    window.send(Update::Queue(sample_queue()));
    window.send(Update::Downloading(true));
    window.run();

    window
        .harness
        .input_mut()
        .viewports
        .entry(ViewportId::ROOT)
        .or_default()
        .events
        .push(ViewportEvent::Close);
    window.harness.step();
    assert!(
        window
            .viewport_commands()
            .contains(&ViewportCommand::CancelClose)
    );
    window.run();
    assert!(window.shows_containing("Downloads are running. Quit anyway?"));

    window.click("Cancel");
    assert!(!window.shows_containing("Downloads are running"));

    window
        .harness
        .input_mut()
        .viewports
        .entry(ViewportId::ROOT)
        .or_default()
        .events
        .push(ViewportEvent::Close);
    window.harness.step();
    window.run();
    window.click_once("Quit");
    assert!(window.viewport_commands().contains(&ViewportCommand::Close));
}

#[test]
fn closing_while_idle_does_not_ask() {
    let mut window = Window::new();
    window
        .harness
        .input_mut()
        .viewports
        .entry(ViewportId::ROOT)
        .or_default()
        .events
        .push(ViewportEvent::Close);
    window.harness.step();
    assert!(
        !window
            .viewport_commands()
            .contains(&ViewportCommand::CancelClose)
    );
}
