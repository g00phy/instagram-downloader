//! Command-line interface: argument definitions, settings resolution
//! (flags > environment > `config.toml` > defaults) and exit codes.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::app::{self, ListView, Options, Report};
use crate::config::{self, Settings};
use crate::download::DownloadConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::instagram::{ClientConfig, CrawlOptions, DEFAULT_API_BASE, DEFAULT_USER_AGENT};
use crate::links::ProfileSection;
use crate::model::MediaKind;
use crate::naming::DEFAULT_TEMPLATE;
use crate::progress::{self, StderrWriter, Style};
use crate::queue::ClearFilter;

/// Exit code: everything downloaded or skipped.
pub const EXIT_OK: u8 = 0;
/// Exit code: at least one link or file failed.
pub const EXIT_FAILED: u8 = 1;
/// Exit code: usage, configuration or cookie error.
pub const EXIT_USAGE: u8 = 2;
/// Exit code: interrupted with Ctrl+C.
pub const EXIT_INTERRUPTED: u8 = 130;

/// JDownloader-style Instagram downloader: grab links into packages, download
/// them in parallel with resume and retries, authenticated with your cookies.
#[derive(Debug, Parser)]
#[command(name = "igdl", version, propagate_version = true, max_term_width = 100)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Command,
}

/// Options accepted by every command (and by `igdl-gui`).
#[derive(Debug, Clone, Default, Args)]
pub struct GlobalArgs {
    /// Cookie export to authenticate with (cookies.txt, JSON, or a `name=value; …` header)
    #[arg(long, global = true, env = "IGDL_COOKIES", value_name = "FILE")]
    pub cookies: Option<PathBuf>,
    /// Settings file [default: <config dir>/igdl/config.toml]
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Where queue.json and archive.txt live [default: platform data dir]
    #[arg(long, global = true, env = "IGDL_DATA_DIR", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// User agent for API requests (best: the browser the cookies came from)
    #[arg(long, global = true, value_name = "UA")]
    pub user_agent: Option<String>,
    /// Minimum seconds between Instagram API requests [default: 1.5]
    #[arg(long, global = true, value_name = "SECS")]
    pub api_delay: Option<f64>,
    /// Instagram API base URL (for testing against a mock server)
    #[arg(long, global = true, env = "IGDL_API_BASE", hide = true)]
    pub api_base: Option<String>,
    /// More logging (-v info, -vv debug)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Only errors and results
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Print one line per file instead of progress bars
    #[arg(long, global = true)]
    pub no_progress: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Grab links and download them now (LinkGrabber + start)
    Get(GetArgs),
    /// Resolve links into packages and add them to the queue (no download)
    Grab(GrabArgs),
    /// Download everything queued; partial files resume
    Start(StartArgs),
    /// Show the queue
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Re-queue failed items (all, or the given package/item ids)
    Retry(IdsArgs),
    /// Remove packages or items from the queue (downloaded files are kept)
    #[command(visible_alias = "rm")]
    Remove(RequiredIdsArgs),
    /// Remove finished packages from the queue
    Clear(ClearArgs),
    /// Inspect the cookie file
    #[command(subcommand)]
    Cookies(CookiesCommand),
    /// Watch the clipboard and queue copied Instagram links
    Watch(WatchArgs),
}

/// Where links come from.
#[derive(Debug, Args)]
pub struct LinkArgs {
    /// Post/reel/profile/story/highlight URLs, @username or username
    #[arg(value_name = "LINK")]
    pub links: Vec<String>,
    /// Read links from a file, one per line (`-` = stdin); repeatable
    #[arg(short = 'i', long = "input", value_name = "FILE")]
    pub inputs: Vec<PathBuf>,
}

/// What to collect and where to put it.
#[derive(Debug, Clone, Default, Args)]
pub struct CrawlArgs {
    /// Download root [default: current directory]
    #[arg(short, long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Profile sections for profile links
    #[arg(long, value_delimiter = ',', value_name = "LIST")]
    pub include: Vec<SectionArg>,
    /// Max posts per profile section
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// Only posts from this date on (YYYY-MM-DD, UTC)
    #[arg(long, value_name = "DATE")]
    pub since: Option<String>,
    /// Only posts up to this date (YYYY-MM-DD, UTC)
    #[arg(long, value_name = "DATE")]
    pub until: Option<String>,
    /// Only images or only videos
    #[arg(long, value_name = "KIND")]
    pub only: Option<KindArg>,
    /// Stop a profile crawl at the first already-downloaded post
    #[arg(long)]
    pub update: bool,
    /// Output path template (see README)
    #[arg(long, value_name = "T")]
    pub template: Option<String>,
    /// Ignore and don't update the archive of downloaded files
    #[arg(long)]
    pub no_archive: bool,
}

/// How to download.
#[derive(Debug, Clone, Default, Args)]
pub struct DownloadArgs {
    /// Parallel downloads [default: 4]
    #[arg(short, long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=32))]
    pub jobs: Option<u16>,
    /// Retries per file for network/5xx/429 errors [default: 5]
    #[arg(long, value_name = "N")]
    pub retries: Option<u32>,
    /// Global bandwidth cap, e.g. 500K or 2M (bytes per second)
    #[arg(long, value_name = "RATE")]
    pub limit_rate: Option<String>,
    /// Keep the download time as file modification time
    #[arg(long)]
    pub no_mtime: bool,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    #[command(flatten)]
    pub links: LinkArgs,
    #[command(flatten)]
    pub crawl: CrawlArgs,
    #[command(flatten)]
    pub download: DownloadArgs,
    /// Only print the files that would be downloaded (stdout); change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct GrabArgs {
    #[command(flatten)]
    pub links: LinkArgs,
    #[command(flatten)]
    pub crawl: CrawlArgs,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Only these package ids
    #[arg(short, long = "package", value_name = "ID")]
    pub packages: Vec<u64>,
    /// Ignore and don't update the archive of downloaded files
    #[arg(long)]
    pub no_archive: bool,
    #[command(flatten)]
    pub download: DownloadArgs,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Show every item, not just packages
    #[arg(short, long, conflicts_with_all = ["failed", "json"])]
    pub items: bool,
    /// Show failed items only
    #[arg(long, conflicts_with = "json")]
    pub failed: bool,
    /// Print the whole queue as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct IdsArgs {
    /// Package or item ids (see `igdl list`)
    #[arg(value_name = "ID")]
    pub ids: Vec<u64>,
}

#[derive(Debug, Args)]
pub struct RequiredIdsArgs {
    /// Package or item ids (see `igdl list`)
    #[arg(value_name = "ID", required = true)]
    pub ids: Vec<u64>,
}

#[derive(Debug, Args)]
pub struct ClearArgs {
    /// Remove failed items instead of finished packages
    #[arg(long, conflicts_with = "all")]
    pub failed: bool,
    /// Empty the whole queue
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Subcommand)]
pub enum CookiesCommand {
    /// Parse the cookie file and report names and expiry (never values)
    Check {
        /// Also ask Instagram which account the session belongs to
        #[arg(long)]
        online: bool,
    },
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Download grabbed links right away
    #[arg(long)]
    pub start: bool,
    /// Clipboard polling interval in milliseconds
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    pub interval: u64,
    #[command(flatten)]
    pub crawl: CrawlArgs,
    #[command(flatten)]
    pub download: DownloadArgs,
}

/// `--include` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SectionArg {
    Posts,
    Reels,
    Tagged,
    Stories,
    Highlights,
}

impl From<SectionArg> for ProfileSection {
    fn from(value: SectionArg) -> Self {
        match value {
            SectionArg::Posts => ProfileSection::Posts,
            SectionArg::Reels => ProfileSection::Reels,
            SectionArg::Tagged => ProfileSection::Tagged,
            SectionArg::Stories => ProfileSection::Stories,
            SectionArg::Highlights => ProfileSection::Highlights,
        }
    }
}

impl From<ProfileSection> for SectionArg {
    fn from(value: ProfileSection) -> Self {
        match value {
            ProfileSection::Posts => SectionArg::Posts,
            ProfileSection::Reels => SectionArg::Reels,
            ProfileSection::Tagged => SectionArg::Tagged,
            ProfileSection::Stories => SectionArg::Stories,
            ProfileSection::Highlights => SectionArg::Highlights,
        }
    }
}

/// `--only` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Image,
    Video,
}

impl From<MediaKind> for KindArg {
    fn from(value: MediaKind) -> Self {
        match value {
            MediaKind::Image => KindArg::Image,
            MediaKind::Video => KindArg::Video,
        }
    }
}

impl From<KindArg> for MediaKind {
    fn from(value: KindArg) -> Self {
        match value {
            KindArg::Image => MediaKind::Image,
            KindArg::Video => MediaKind::Video,
        }
    }
}

/// Parses arguments, runs the command and returns the process exit code.
pub async fn run() -> u8 {
    let cli = Cli::parse();
    init_logging(&cli.global);
    let cancel = CancellationToken::new();
    spawn_interrupt_handler(cancel.clone());
    match execute(cli, &cancel).await {
        Ok(report) if report.interrupted => EXIT_INTERRUPTED,
        Ok(report) if report.failed > 0 => EXIT_FAILED,
        Ok(_) => EXIT_OK,
        Err(err) => {
            progress::note(&format!("error: {err}"));
            app::hint(&err);
            exit_code(&err)
        }
    }
}

/// Exit code for an error that aborted the whole command.
pub fn exit_code(err: &Error) -> u8 {
    match err.kind() {
        ErrorKind::Config | ErrorKind::Cookies => EXIT_USAGE,
        ErrorKind::Cancelled => EXIT_INTERRUPTED,
        _ => EXIT_FAILED,
    }
}

async fn execute(cli: Cli, cancel: &CancellationToken) -> Result<Report> {
    let settings = config::load(cli.global.config.as_deref())?;
    let global = &cli.global;
    match &cli.command {
        Command::Get(args) => {
            let options = resolve_options(global, &settings, Some(&args.crawl), &args.download)?;
            let links = app::collect_links(&args.links.links, &args.links.inputs)?;
            app::get(&options, &links, args.dry_run, cancel).await
        }
        Command::Grab(args) => {
            let options = resolve_options(
                global,
                &settings,
                Some(&args.crawl),
                &DownloadArgs::default(),
            )?;
            let links = app::collect_links(&args.links.links, &args.links.inputs)?;
            app::grab(&options, &links, cancel).await
        }
        Command::Start(args) => {
            let mut options = resolve_options(global, &settings, None, &args.download)?;
            options.use_archive = !args.no_archive;
            app::start(&options, &args.packages, cancel).await
        }
        Command::List(args) => {
            let view = if args.json {
                ListView::Json
            } else if args.failed {
                ListView::Failed
            } else if args.items {
                ListView::Items
            } else {
                ListView::Packages
            };
            app::list(&data_dir(global, &settings)?, view)?;
            Ok(Report::default())
        }
        Command::Retry(args) => {
            app::retry(&data_dir(global, &settings)?, &args.ids)?;
            Ok(Report::default())
        }
        Command::Remove(args) => {
            app::remove(&data_dir(global, &settings)?, &args.ids)?;
            Ok(Report::default())
        }
        Command::Clear(args) => {
            let filter = if args.all {
                ClearFilter::All
            } else if args.failed {
                ClearFilter::Failed
            } else {
                ClearFilter::Finished
            };
            app::clear(&data_dir(global, &settings)?, filter)?;
            Ok(Report::default())
        }
        Command::Cookies(CookiesCommand::Check { online }) => {
            let options = resolve_options(global, &settings, None, &DownloadArgs::default())?;
            app::cookies_check(&options, *online).await?;
            Ok(Report::default())
        }
        Command::Watch(args) => {
            let options = resolve_options(global, &settings, Some(&args.crawl), &args.download)?;
            let interval = Duration::from_millis(args.interval.max(100));
            app::watch(&options, args.start, interval, cancel).await
        }
    }
}

/// The data directory: flag/env, else config, else the platform default.
pub fn data_dir(global: &GlobalArgs, settings: &Settings) -> Result<PathBuf> {
    match global
        .data_dir
        .clone()
        .or_else(|| settings.data_dir.clone())
    {
        Some(dir) => absolute(&dir),
        None => config::default_data_dir(),
    }
}

fn absolute(path: &std::path::Path) -> Result<PathBuf> {
    std::path::absolute(path)
        .map_err(|err| Error::io(format!("cannot resolve {}", path.display()), err))
}

/// Merges flags over config-file settings over defaults.
pub fn resolve_options(
    global: &GlobalArgs,
    settings: &Settings,
    crawl: Option<&CrawlArgs>,
    download: &DownloadArgs,
) -> Result<Options> {
    let api_delay = global.api_delay.or(settings.api_delay).unwrap_or(1.5);
    if !api_delay.is_finite() || api_delay < 0.0 {
        return Err(Error::Config(
            "--api-delay must be 0 or more seconds".into(),
        ));
    }
    let user_agent = global
        .user_agent
        .clone()
        .or_else(|| settings.user_agent.clone())
        .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
    let api_base = global
        .api_base
        .clone()
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
        .trim_end_matches('/')
        .to_string();
    let client = ClientConfig {
        api_base,
        user_agent: user_agent.clone(),
        api_delay: Duration::from_secs_f64(api_delay),
        ..ClientConfig::default()
    };

    let limit_rate = match download
        .limit_rate
        .as_deref()
        .or(settings.limit_rate.as_deref())
    {
        Some(rate) => Some(config::parse_rate(rate)?),
        None => None,
    };
    let download_config = DownloadConfig {
        jobs: download
            .jobs
            .map(usize::from)
            .or(settings.jobs)
            .unwrap_or(4)
            .clamp(1, 32),
        retries: download.retries.or(settings.retries).unwrap_or(5),
        user_agent,
        limit_rate,
        set_mtime: !download.no_mtime && settings.set_mtime.unwrap_or(true),
        ..DownloadConfig::default()
    };

    let mut crawl_options = CrawlOptions::default();
    let mut template = settings
        .template
        .clone()
        .unwrap_or_else(|| DEFAULT_TEMPLATE.to_string());
    let mut out_dir = settings.out_dir.clone();
    let mut use_archive = true;
    if let Some(sections) = &settings.include {
        crawl_options.sections = sections
            .iter()
            .map(|s| s.parse::<ProfileSection>())
            .collect::<Result<_>>()?;
    }
    if let Some(args) = crawl {
        if !args.include.is_empty() {
            crawl_options.sections = Vec::new();
            for section in &args.include {
                let section = ProfileSection::from(*section);
                if !crawl_options.sections.contains(&section) {
                    crawl_options.sections.push(section);
                }
            }
        }
        crawl_options.limit = args.limit;
        crawl_options.since = args
            .since
            .as_deref()
            .map(|d| config::parse_date(d, false))
            .transpose()?;
        crawl_options.until = args
            .until
            .as_deref()
            .map(|d| config::parse_date(d, true))
            .transpose()?;
        crawl_options.only = args.only.map(MediaKind::from);
        crawl_options.stop_at_archived = args.update;
        if let Some(t) = &args.template {
            template = t.clone();
        }
        if let Some(out) = &args.out {
            out_dir = Some(out.clone());
        }
        use_archive = !args.no_archive;
    }
    if crawl_options.stop_at_archived && !use_archive {
        return Err(Error::Config(
            "--update needs the archive; it cannot be combined with --no-archive".into(),
        ));
    }
    let out_dir = match out_dir {
        Some(dir) => absolute(&dir)?,
        None => std::env::current_dir()
            .map_err(|err| Error::io("cannot determine the current directory", err))?,
    };
    let cookies = global.cookies.clone().or_else(|| settings.cookies.clone());

    Ok(Options {
        cookies,
        data_dir: data_dir(global, settings)?,
        out_dir,
        template,
        client,
        download: download_config,
        crawl: crawl_options,
        use_archive,
        style: Style::detect(global.no_progress, global.quiet),
    })
}

fn init_logging(global: &GlobalArgs) {
    let default = if global.quiet {
        "error"
    } else {
        match global.verbose {
            0 => "warn",
            1 => "igdl=info,warn",
            _ => "igdl=debug,info",
        }
    };
    let filter = EnvFilter::try_from_env("IGDL_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(global.verbose > 1)
        .without_time()
        .with_writer(|| StderrWriter)
        .try_init();
}

/// First Ctrl+C: stop gracefully (partial files and queue are kept). Second: exit now.
fn spawn_interrupt_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        progress::note("interrupting… (press Ctrl+C again to quit immediately)");
        cancel.cancel();
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(i32::from(EXIT_INTERRUPTED));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn include_list_parses() {
        let cli = Cli::try_parse_from([
            "igdl",
            "grab",
            "natgeo",
            "--include",
            "posts,stories",
            "--limit",
            "5",
        ])
        .unwrap();
        let Command::Grab(args) = cli.command else {
            panic!("expected grab");
        };
        assert_eq!(
            args.crawl.include,
            vec![SectionArg::Posts, SectionArg::Stories]
        );
        assert_eq!(args.crawl.limit, Some(5));
    }

    #[test]
    fn jobs_are_bounded() {
        assert!(Cli::try_parse_from(["igdl", "start", "-j", "0"]).is_err());
        assert!(Cli::try_parse_from(["igdl", "start", "-j", "33"]).is_err());
        assert!(Cli::try_parse_from(["igdl", "start", "-j", "8"]).is_ok());
    }

    #[test]
    fn exit_codes_by_kind() {
        assert_eq!(exit_code(&Error::Config("x".into())), EXIT_USAGE);
        assert_eq!(exit_code(&Error::Cookies("x".into())), EXIT_USAGE);
        assert_eq!(exit_code(&Error::Cancelled), EXIT_INTERRUPTED);
        assert_eq!(exit_code(&Error::Auth("x".into())), EXIT_FAILED);
    }
}
