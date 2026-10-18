#![cfg(feature = "gui")]
//! igdl-gui's panic hook: every panic lands, redacted, in the crash log, and the
//! user is notified once — from the hook for other threads, never from the
//! window's own thread. One test only: the hook is process-wide.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use igdl::gui::{CRASH_LOG_FILE, install_panic_hook, notify_panic, panic_log};

/// The logs `notify` was called with.
static NOTIFIED: Mutex<Vec<Option<PathBuf>>> = Mutex::new(Vec::new());

fn record(log: Option<&Path>) {
    NOTIFIED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(log.map(Path::to_path_buf));
}

fn notified() -> Vec<Option<PathBuf>> {
    NOTIFIED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[test]
fn panics_are_logged_redacted_and_reported_once() {
    let tmp = tempfile::tempdir().unwrap();
    let not_a_dir = tmp.path().join("file");
    std::fs::write(&not_a_dir, "x").unwrap();
    let data = tmp.path().join("data dir");
    let log = data.join(CRASH_LOG_FILE);

    // Nothing panicked yet: nobody is notified.
    install_panic_hook(vec![not_a_dir, data.clone()], record);
    notify_panic(record);
    assert!(notified().is_empty());

    // A panic on the window's own thread is logged but not reported by the hook.
    let caught = std::panic::catch_unwind(|| panic!("ui thread broke"));
    assert!(caught.is_err());
    assert!(notified().is_empty());
    assert_eq!(panic_log(), Some(log.as_path()));

    // A panic on another thread is logged (secrets redacted) and reported.
    let worker = std::thread::Builder::new()
        .name("igdl-controller".into())
        .spawn(|| {
            panic!("bad cookie sessionid=1234567890abc from https://cdn.example/a.jpg?oe=65&oh=sig")
        })
        .unwrap();
    assert!(worker.join().is_err());
    assert_eq!(notified(), vec![Some(log.clone())]);

    // Later panics are logged too, but the user is told only once.
    assert!(std::thread::spawn(|| panic!("again")).join().is_err());
    notify_panic(record);
    assert_eq!(notified().len(), 1);

    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(text.matches(" panicked\n").count(), 3, "{text}");
    assert!(text.starts_with('['), "{text}");
    assert!(text.contains("] igdl-gui "), "{text}");
    assert!(text.contains("message: ui thread broke"), "{text}");
    assert!(text.contains("thread: igdl-controller"), "{text}");
    assert!(text.contains("message: again"), "{text}");
    assert!(text.contains("location: tests"), "{text}");
    assert!(text.contains("gui_crash_log.rs:"), "{text}");
    assert!(text.contains("backtrace:"), "{text}");
    assert!(text.contains("sessionid=***"), "{text}");
    assert!(!text.contains("1234567890abc"), "{text}");
    assert!(!text.contains("oh=sig"), "{text}");
}
