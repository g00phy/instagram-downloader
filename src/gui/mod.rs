//! The desktop window (`igdl-gui`): a JDownloader-style front end over the same
//! queue, archive, crawler and download engine as the CLI.
//!
//! [`view`] renders state and sends [`protocol::Command`]s; [`controller`] runs on
//! its own thread with a tokio runtime, does the work, and streams
//! [`protocol::Update`]s back.
//!
//! [`run`] tries the OpenGL renderer (glow) first and falls back to wgpu
//! (Direct3D 12 with the WARP software rasterizer on Windows, Vulkan/OpenGL ES
//! elsewhere). Without a console on Windows, panics and startup errors are
//! written to a crash log ([`install_panic_hook`], [`append_crash_log`]).

pub mod controller;
pub mod protocol;
pub mod state;
pub mod view;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use eframe::egui;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::cli::GlobalArgs;
use crate::config;
use crate::error::{Error, Result};
use crate::redact::redact;
use controller::{ControllerConfig, ControllerHandle, UiLogLayer};
use view::IgdlApp;

/// Environment variable that forces one renderer (`glow` or `wgpu`) instead of
/// trying glow first and wgpu second.
pub const ENV_RENDERER: &str = "IGDL_RENDERER";

/// File name of the window's crash log (panic reports and startup errors).
pub const CRASH_LOG_FILE: &str = "igdl-gui-crash.log";

/// Opens the window and blocks until it is closed.
///
/// Tries glow, then wgpu (or only the renderer named by [`ENV_RENDERER`]): one
/// that cannot create the window (no OpenGL 2+, no usable GPU adapter) is logged
/// and the next one gets a go. The controller is started at most once, by the
/// attempt that got a window. When none works the error says what to do.
pub fn run(global: GlobalArgs) -> Result<()> {
    // Logging first, so warnings from loading the settings are not lost.
    init_logging(&global);
    let settings = config::load(global.config.as_deref())?;
    let config_path = global.config.clone().or_else(config::default_config_path);
    let renderers = renderer_order(std::env::var(ENV_RENDERER).ok().as_deref())?;
    let mut config = Some(ControllerConfig {
        global,
        settings,
        config_path,
    });
    run_renderers(&renderers, |renderer| open_window(renderer, &mut config))
}

/// The renderers to try, in order: the one named by `IGDL_RENDERER`, or glow
/// then wgpu when it is unset or empty.
fn renderer_order(choice: Option<&str>) -> Result<Vec<eframe::Renderer>> {
    let choice = choice.map(str::trim).unwrap_or_default();
    if choice.is_empty() {
        return Ok(vec![eframe::Renderer::Glow, eframe::Renderer::Wgpu]);
    }
    match choice.to_ascii_lowercase().as_str() {
        "glow" => Ok(vec![eframe::Renderer::Glow]),
        "wgpu" => Ok(vec![eframe::Renderer::Wgpu]),
        _ => Err(Error::Config(format!(
            "{ENV_RENDERER} must be `glow` or `wgpu`, not `{choice}`"
        ))),
    }
}

/// How one renderer attempt ended.
#[derive(Debug)]
enum Attempt {
    /// The window was open and has been closed.
    Closed,
    /// This renderer could not create the window and the controller was not
    /// started: the next renderer may try.
    Unavailable(String),
    /// A failure another renderer cannot fix (the window was already up, or
    /// there is no window system at all).
    Failed(String),
}

/// Runs `open` for each renderer until one gets a window.
fn run_renderers(
    renderers: &[eframe::Renderer],
    mut open: impl FnMut(eframe::Renderer) -> Attempt,
) -> Result<()> {
    let mut failures = Vec::new();
    for &renderer in renderers {
        match open(renderer) {
            Attempt::Closed => return Ok(()),
            Attempt::Failed(message) => return Err(Error::Config(message)),
            Attempt::Unavailable(reason) => {
                tracing::warn!("the {renderer} renderer cannot open the window: {reason}");
                failures.push((renderer, reason));
            }
        }
    }
    Err(Error::Config(no_renderer_message(&failures)))
}

/// The error shown when no renderer could open the window: what each one said,
/// then what the user can do about it.
fn no_renderer_message(failures: &[(eframe::Renderer, String)]) -> String {
    let mut message = String::from("cannot open the window: no graphics renderer works here.\n");
    for (renderer, reason) in failures {
        let api = renderer_api(*renderer);
        message.push_str(&format!("\n• {renderer} ({api}): {reason}"));
    }
    message.push_str("\n\nInstall or update the graphics driver");
    if cfg!(windows) {
        message.push_str(
            " (from the PC or graphics card maker, not the Microsoft Basic Display Adapter)",
        );
    }
    message.push_str(
        ". Remote Desktop sessions and virtual machines without 3D acceleration often \
         lack OpenGL 2+: enable 3D acceleration in the VM or start igdl-gui locally. \
         The igdl command line works without a window.",
    );
    message
}

/// The graphics API behind `renderer` on this platform (see [`native_options`]).
fn renderer_api(renderer: eframe::Renderer) -> &'static str {
    match renderer {
        eframe::Renderer::Glow => "OpenGL",
        eframe::Renderer::Wgpu if cfg!(windows) => "Direct3D 12/Vulkan",
        eframe::Renderer::Wgpu if cfg!(target_os = "macos") => "Metal",
        eframe::Renderer::Wgpu => "Vulkan/OpenGL ES",
    }
}

/// One `run_native` attempt with `renderer`. The app creator runs only once the
/// renderer is up (eframe creates the window and the painter first), and it
/// takes the controller config out of `slot`, so a second attempt can only
/// happen while the controller has not been started.
fn open_window(renderer: eframe::Renderer, slot: &mut Option<ControllerConfig>) -> Attempt {
    let result = eframe::run_native(
        "igdl",
        native_options(renderer),
        Box::new(|creation| {
            let config = slot
                .take()
                .ok_or("the controller was already started by an earlier window")?;
            let ctx = creation.egui_ctx.clone();
            let (handle, link) =
                ControllerHandle::spawn(config, Arc::new(move || ctx.request_repaint()));
            Ok(Box::new(IgdlApp::new(link, Some(handle))))
        }),
    );
    match result {
        Ok(()) => Attempt::Closed,
        Err(err) => classify_failure(renderer, &err, slot.is_none()),
    }
}

/// Whether a failed `run_native` may be retried with another renderer.
fn classify_failure(renderer: eframe::Renderer, err: &eframe::Error, app_started: bool) -> Attempt {
    if app_started {
        Attempt::Failed(format!("the window stopped ({renderer} renderer): {err}"))
    } else if matches!(err, eframe::Error::WinitEventLoop(_)) {
        // The event loop is shared by both renderers: no display/window system.
        Attempt::Failed(format!("cannot open the window: {err}"))
    } else {
        Attempt::Unavailable(err.to_string())
    }
}

/// Window options for one attempt.
fn native_options(renderer: eframe::Renderer) -> eframe::NativeOptions {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("igdl — Instagram downloader")
            .with_app_id("igdl")
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([760.0, 420.0]),
        renderer,
        // A failed attempt must return here (not exit the process) for the
        // fallback; eframe keeps the event loop in a thread-local, so
        // `run_native` can be called again on this thread.
        run_and_return: true,
        ..Default::default()
    };
    if cfg!(windows)
        && let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup
    {
        setup.instance_descriptor.backends =
            windows_wgpu_backends(eframe::wgpu::Backends::from_env());
    }
    options
}

/// The wgpu backends on Windows, where wgpu is the fallback for a missing or
/// broken OpenGL driver: Direct3D 12, which also offers the WARP software
/// rasterizer (wgpu picks CPU adapters last, so a real GPU still wins), and
/// Vulkan. wgpu's OpenGL backend would hit the same driver glow just failed on.
/// `from_env` (`WGPU_BACKEND`) still overrides this.
fn windows_wgpu_backends(from_env: Option<eframe::wgpu::Backends>) -> eframe::wgpu::Backends {
    from_env.unwrap_or(eframe::wgpu::Backends::DX12 | eframe::wgpu::Backends::VULKAN)
}

/// stderr gets warnings (or `IGDL_LOG`), the Log tab gets igdl's INFO and above.
fn init_logging(global: &GlobalArgs) {
    let default = match global.verbose {
        0 => "warn",
        1 => "igdl=info,warn",
        _ => "igdl=debug,info",
    };
    let stderr_filter =
        EnvFilter::try_from_env("IGDL_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let window_filter = Targets::new()
        .with_target("igdl", LevelFilter::INFO)
        .with_default(LevelFilter::WARN);
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(console::colors_enabled_stderr())
                .without_time()
                .with_filter(stderr_filter),
        )
        .with(UiLogLayer.with_filter(window_filter))
        .try_init();
}

// ---------------------------------------------------------------------------
// Crash log

/// Set by the panic hook once it has run.
static PANICKED: AtomicBool = AtomicBool::new(false);
/// Set once the user has been told about a panic (see [`notify_panic`]).
static NOTIFIED: AtomicBool = AtomicBool::new(false);
/// The crash log the first panic report went to.
static PANIC_LOG: OnceLock<PathBuf> = OnceLock::new();

/// Directories for [`CRASH_LOG_FILE`], best first: the data directory
/// ([`config::default_data_dir`]), then the temp directory.
pub fn crash_log_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = config::default_data_dir() {
        dirs.push(dir);
    }
    let temp = std::env::temp_dir();
    if !dirs.contains(&temp) {
        dirs.push(temp);
    }
    dirs
}

/// Appends `entry` (redacted) to [`CRASH_LOG_FILE`] in the first of `dirs`
/// that can be written (created if missing) and returns that file.
pub fn append_crash_log(dirs: &[PathBuf], entry: &str) -> Option<PathBuf> {
    let entry = redact(entry);
    dirs.iter().find_map(|dir| {
        let path = dir.join(CRASH_LOG_FILE);
        append_to(&path, &entry).ok().map(|()| path)
    })
}

fn append_to(path: &Path, entry: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(entry.as_bytes())
}

/// A crash log entry: a timestamped heading, then `key: value` lines and a
/// blank line. Values may span several lines.
fn crash_entry(heading: &str, fields: &[(&str, &str)]) -> String {
    let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let mut entry = format!(
        "[{time}] igdl-gui {} {heading}\n",
        env!("CARGO_PKG_VERSION")
    );
    for (key, value) in fields {
        entry.push_str(&format!("{key}: {}\n", value.trim_end()));
    }
    entry.push('\n');
    entry
}

/// The crash log entry for an error that kept the window from opening.
pub fn startup_error_entry(err: &Error) -> String {
    crash_entry("could not start", &[("error", &err.to_string())])
}

/// The crash log entry for a panic: thread, message, location and backtrace
/// (always captured: nobody sets `RUST_BACKTRACE` for a window).
fn panic_entry(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let message = info.payload_as_str().unwrap_or("(no message)");
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_string());
    let backtrace = format!("\n{}", std::backtrace::Backtrace::force_capture());
    crash_entry(
        "panicked",
        &[
            ("thread", thread.name().unwrap_or("unnamed")),
            ("message", message),
            ("location", &location),
            ("backtrace", &backtrace),
        ],
    )
}

/// Installs a panic hook; call it on the thread that will run the window.
///
/// Every panic still gets the default report (stderr) and is also appended,
/// redacted, to the crash log in `dirs` (see [`append_crash_log`]). For a panic
/// on another thread (the controller, a download task) `notify` is then called
/// with that log, at most once per process (see [`notify_panic`]). On the window's
/// own thread nothing is shown from the hook, since a dialog there would run
/// inside winit's window procedure while it unwinds: call [`notify_panic`] once
/// the window is gone.
pub fn install_panic_hook(dirs: Vec<PathBuf>, notify: fn(Option<&Path>)) {
    let window_thread = std::thread::current().id();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        if let Some(path) = append_crash_log(&dirs, &panic_entry(info)) {
            let _ = PANIC_LOG.set(path);
        }
        PANICKED.store(true, Ordering::Release);
        if std::thread::current().id() != window_thread {
            notify_panic(notify);
        }
    }));
}

/// Calls `notify` with the crash log ([`panic_log`]) if the hook from
/// [`install_panic_hook`] has seen a panic and nobody has been notified yet.
pub fn notify_panic(notify: impl FnOnce(Option<&Path>)) {
    if PANICKED.load(Ordering::Acquire) && !NOTIFIED.swap(true, Ordering::AcqRel) {
        notify(panic_log());
    }
}

/// The crash log the first panic report was written to, if it could be written.
pub fn panic_log() -> Option<&'static Path> {
    PANIC_LOG.get().map(PathBuf::as_path)
}

/// What the user is told after a panic, pointing to the crash `log`.
pub fn panic_message(log: Option<&Path>) -> String {
    let report = match log {
        Some(path) => format!("A report was written to:\n{}", path.display()),
        None => "No report could be written.".to_string(),
    };
    format!(
        "igdl-gui ran into an internal error and could not finish what it was doing.\n\n\
         {report}\n\nPlease attach it when you report the problem."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::Renderer::{Glow, Wgpu};

    #[test]
    fn renderer_order_defaults_to_glow_then_wgpu() {
        assert_eq!(renderer_order(None).unwrap(), vec![Glow, Wgpu]);
        assert_eq!(renderer_order(Some("  ")).unwrap(), vec![Glow, Wgpu]);
    }

    #[test]
    fn renderer_order_honours_an_explicit_choice() {
        assert_eq!(renderer_order(Some("glow")).unwrap(), vec![Glow]);
        assert_eq!(renderer_order(Some(" WGPU ")).unwrap(), vec![Wgpu]);
        let err = renderer_order(Some("vulkan")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("IGDL_RENDERER"), "{err}");
    }

    #[test]
    fn falls_back_to_the_next_renderer_when_one_cannot_open_the_window() {
        let mut tried = Vec::new();
        let result = run_renderers(&[Glow, Wgpu], |renderer| {
            tried.push(renderer);
            match renderer {
                Glow => Attempt::Unavailable("egui_glow requires opengl 2.0+".into()),
                Wgpu => Attempt::Closed,
            }
        });
        assert!(result.is_ok());
        assert_eq!(tried, vec![Glow, Wgpu]);
    }

    #[test]
    fn does_not_retry_once_the_window_was_up() {
        let mut tried = Vec::new();
        let result = run_renderers(&[Glow, Wgpu], |renderer| {
            tried.push(renderer);
            Attempt::Failed("the window stopped (glow renderer): boom".into())
        });
        assert_eq!(tried, vec![Glow]);
        assert_eq!(
            result.unwrap_err().to_string(),
            "the window stopped (glow renderer): boom"
        );
    }

    #[test]
    fn explains_what_to_do_when_no_renderer_works() {
        let err = run_renderers(&[Glow, Wgpu], |renderer| {
            Attempt::Unavailable(format!("{renderer} said no"))
        })
        .unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("glow (OpenGL): glow said no"), "{message}");
        let wgpu = format!("wgpu ({}): wgpu said no", renderer_api(Wgpu));
        assert!(message.contains(&wgpu), "{message}");
        assert!(message.contains("graphics driver"));
        assert!(message.contains("Remote Desktop"));
        assert!(message.contains("command line"));
        // Windows-only advice stays off other platforms.
        assert_eq!(
            message.contains("Microsoft Basic Display Adapter"),
            cfg!(windows),
            "{message}"
        );
    }

    #[test]
    fn only_failures_before_the_app_started_are_retried() {
        let no_gl = eframe::Error::OpenGL(eframe::egui_glow::PainterError::from(
            "egui_glow requires opengl 2.0+. ".to_string(),
        ));
        let attempt = classify_failure(Glow, &no_gl, false);
        assert!(
            matches!(&attempt, Attempt::Unavailable(reason) if reason.contains("opengl 2.0")),
            "{attempt:?}"
        );
        let no_adapter =
            eframe::Error::Wgpu(eframe::egui_wgpu::WgpuError::NoSurfaceFormatsAvailable);
        assert!(matches!(
            classify_failure(Wgpu, &no_adapter, false),
            Attempt::Unavailable(_)
        ));
        assert!(matches!(
            classify_failure(Glow, &no_gl, true),
            Attempt::Failed(_)
        ));
    }

    #[test]
    fn native_options_keep_the_event_loop_for_another_attempt() {
        let options = native_options(Wgpu);
        assert_eq!(options.renderer, Wgpu);
        assert!(options.run_and_return);
        let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &options.wgpu_options.wgpu_setup
        else {
            panic!("expected a fresh wgpu setup");
        };
        if cfg!(windows) {
            let expected = windows_wgpu_backends(eframe::wgpu::Backends::from_env());
            assert_eq!(setup.instance_descriptor.backends, expected);
        }
    }

    #[test]
    fn wgpu_on_windows_uses_direct3d_12_and_vulkan_unless_wgpu_backend_says_otherwise() {
        use eframe::wgpu::Backends;
        let backends = windows_wgpu_backends(None);
        assert!(backends.contains(Backends::DX12), "{backends:?}");
        assert!(backends.contains(Backends::VULKAN), "{backends:?}");
        assert!(!backends.contains(Backends::GL), "{backends:?}");
        assert_eq!(windows_wgpu_backends(Some(Backends::GL)), Backends::GL);
    }

    #[test]
    fn crash_log_goes_to_the_first_writable_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = tmp.path().join("file");
        std::fs::write(&not_a_dir, "x").unwrap();
        let fallback = tmp.path().join("new").join("data");
        let dirs = vec![not_a_dir, fallback.clone()];

        let path = append_crash_log(&dirs, "first\n").unwrap();
        assert_eq!(path, fallback.join(CRASH_LOG_FILE));
        assert_eq!(append_crash_log(&dirs, "second\n").unwrap(), path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn crash_log_entries_are_redacted() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Error::Config(
            "bad cookie sessionid=1234567890abc from https://cdn.example/a.jpg?oe=65&oh=sig".into(),
        );
        let path =
            append_crash_log(&[tmp.path().to_path_buf()], &startup_error_entry(&err)).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("could not start"), "{text}");
        assert!(text.contains("error: bad cookie sessionid=***"), "{text}");
        assert!(!text.contains("1234567890abc"), "{text}");
        assert!(!text.contains("oh=sig"), "{text}");
        assert!(text.ends_with("\n\n"));
    }

    #[test]
    fn panic_message_points_to_the_log() {
        let path = Path::new("/data/igdl-gui-crash.log");
        assert!(panic_message(Some(path)).contains(&path.display().to_string()));
        assert!(panic_message(None).contains("No report"));
    }

    #[test]
    fn crash_log_dirs_end_with_the_temp_dir() {
        let dirs = crash_log_dirs();
        assert_eq!(dirs.last(), Some(&std::env::temp_dir()));
    }
}
