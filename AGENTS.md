# AGENTS.md: instagram-downloader (`igdl`)

Guidance for AI coding agents (and humans) working in this repository. README.md is
the user-facing specification; this file covers how the code is organised, the rules
every change must follow, and what Instagram's API looks like.

## Status
v0.1 implemented: every module, the CLI, the desktop window (`igdl-gui`, `gui`
feature) and end-to-end tests (`tests/cli_e2e.rs` drives the `igdl` binary against a
mocked API + CDN; `tests/gui_controller.rs` and `tests/gui_view.rs` cover the window).
Built as: docs → skeleton with shared types → four modules in parallel (one agent per
git worktree) → CLI integration → end-to-end tests → GUI protocol → controller and
window in parallel → screenshots under Xvfb. Not yet verified against live Instagram; see
"Unverified API assumptions" below before changing the client.

## Purpose
A JDownloader-style Instagram downloader: links go through a **LinkGrabber**
(resolve → packages of files), into a **persistent queue**, and are downloaded by a
**parallel, resumable, retrying engine**. Authentication is cookie-based only.
Single static binary `igdl`, Windows first-class, Linux/macOS supported.

## Architecture

```text
links ──► links::parse_link ──► Target
                                  │
                instagram::resolve (Client: cookies, throttle, API)
                                  │
                                  ▼
                          Vec<MediaFile> ──► naming::render ──► Item (rel_path)
                                                                 │
                                         queue::Queue (Package{items}) ◄─► queue.json
                                                                 │
                         download::Downloader (jobs, Range resume, retry, refresh)
                                  │                              │
                           files on disk                 archive.txt (media ids)
```

| Module | Responsibility |
|---|---|
| `src/main.rs` | `#[tokio::main]`, calls `cli::run`, maps errors to exit codes. Nothing else. |
| `src/cli.rs` | clap definitions, merges CLI flags over `config::Settings`, command dispatch. |
| `src/app.rs` | Command implementations (`get`, `grab`, `start`, `list`, …) wiring the modules. |
| `src/progress.rs` | Renders `download::Event`s: indicatif bars on a TTY, plain lines otherwise. |
| `src/model.rs` | Shared data types: `MediaFile`, `MediaKind`, `Category`, `Package`, `Item`, `ItemStatus`. |
| `src/error.rs` | `Error` taxonomy + `ErrorKind` (serialisable) + `Result`. |
| `src/config.rs` | `Settings` (TOML), default config/data dirs, env overrides. |
| `src/cookies.rs` | Parse Netscape / JSON / header cookie files into `CookieJar`; validation report. |
| `src/redact.rs` | Scrub secrets (cookie values, signed URL params) from strings before display. |
| `src/links.rs` | URL/username → `Target`; Markdown unwrap; links files; shortcode ⇄ media id. |
| `src/naming.rs` | Filename templates, Windows-safe sanitising, path-escape prevention. |
| `src/instagram/client.rs` | HTTP client for the web API: headers, cookies, throttle, 429 backoff, error mapping. |
| `src/instagram/models.rs` | Lenient serde structs for API responses. |
| `src/instagram/extract.rs` | API media item → `Vec<MediaFile>` (best candidate selection, carousels). |
| `src/instagram/crawler.rs` | `resolve(Target)` with pagination, limits, date filters, `--update`; `refresh` of expired URLs. |
| `src/queue.rs` | `Queue` operations + `QueueStore` (atomic JSON save, exclusive lock). |
| `src/archive.rs` | Append-only `archive.txt` of finished media keys. |
| `src/download/` | `Downloader`: bounded parallelism, `.part` + Range resume, retries, rate limit, events. |
| `src/fsutil.rs` | UTF-8/UTF-16 text files (`read_text`), renames retried through brief Windows file locks. |
| `src/watch.rs` | Clipboard observer (`igdl watch`, the window's clipboard monitoring). |
| `src/bin/igdl-gui.rs` | Window entry point (`windows_subsystem = "windows"`), error dialog. |
| `src/gui/mod.rs` | `gui::run`: logging, eframe setup (glow, falling back to wgpu; `IGDL_RENDERER`), crash log, wires controller ⇄ window. |
| `src/gui/protocol.rs` | The contract: `Command` (window → controller) and `Update` (controller → window). |
| `src/gui/controller/` | Background thread + tokio runtime owning queue, archive, client, downloader; executes `Command`s, streams `Update`s; `UiLogLayer` forwards `tracing` to the Log tab. |
| `src/gui/state.rs` | Pure view-model (`ViewState::apply(Update)`): snapshots, live speeds, log ring. |
| `src/gui/view/` | egui window: toolbar, Downloads table, LinkGrabber, Settings, Log, dialogs. |

Dependency direction: `model`/`error` ← everything; `cookies`, `links`, `naming`
depend only on `model`/`error`; `redact` uses `cookies`; `config` uses `links`
(for `ProfileSection`); `instagram` uses `cookies`, `links`, `redact`; `download`
uses only `model`/`error` (its messages never contain URLs); only `app`/`cli` know
about all of them; `gui` sits beside `cli`/`app` and reuses their helpers. The window
(`gui::view`) never touches files or the network: everything goes through the
controller via `gui::protocol`. Do not introduce cycles or let lower layers print to
the terminal.

## Instagram web API notes

Base `https://www.instagram.com` (overridable with `IGDL_API_BASE` for tests). Calls
are `GET` unless marked `POST` (form-encoded), all with these headers:

```text
User-Agent:        desktop Chrome UA (configurable)
X-IG-App-ID:       936619743392459
X-ASBD-ID:         129477
X-IG-WWW-Claim:    0
X-Requested-With:  XMLHttpRequest
X-CSRFToken:       <csrftoken cookie>
Referer:           https://www.instagram.com/
Cookie:            <instagram.com cookies>
```

| Purpose | Endpoint | Data |
|---|---|---|
| Profile | `/api/v1/users/web_profile_info/?username=U` | `data.user{id, username, is_private, followed_by_viewer}` |
| Who am I | `/api/v1/accounts/current_user/?edit=true`, then `/api/v1/users/{ds_user_id}/info/` (one quick attempt each, no backoff) | `user{pk, username}` |
| Post | `/api/v1/media/{media_id}/info/` | `items[0]` |
| Posts feed | `/api/v1/feed/user/{user_id}/?count=12&max_id=M` | `items[]`, `more_available`, `next_max_id` |
| Reels | `POST /api/v1/clips/user/` form `target_user_id=ID&page_size=12&max_id=M` | `items[].media`, `paging_info{more_available, max_id}` |
| Tagged | `/api/v1/usertags/{user_id}/feed/?count=12&max_id=M` | like the posts feed |
| Stories | `/api/v1/feed/reels_media/?reel_ids={user_id}` | `reels_media[].items[]` (or `reels{id}.items`) |
| Highlights list | `/api/v1/highlights/{user_id}/highlights_tray/` | `tray[]{id: "highlight:N", title}` |
| Highlight | `/api/v1/feed/reels_media/?reel_ids=highlight:N` | as stories |

Media item fields: `pk` (number or string), `id` (`"{pk}_{owner_pk}"`), `code`
(shortcode), `media_type` (1 image, 2 video, 8 carousel), `taken_at` (unix secs),
`user{pk, username}`, `caption{text}` (nullable), `image_versions2.candidates[]{url,
width, height}`, `video_versions[]{url, width, height}`, `carousel_media[]` (children
with their own `pk`, `media_type`, candidates), `product_type` (`clips` = reel),
`timeline_pinned_user_ids` (pinned posts; ignore them for `--update`).
Pick the candidate with the largest `width*height`; prefer videos over the cover image.

Shortcode → media id: base64 digits over
`ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_`. Shortcodes of
private posts carry 28 extra characters: if `len > 28`, drop the last 28 first.

Error signals → `Error`:
* `401`/`403` with `login_required`, `require_login: true`, or a redirect to
  `/accounts/login/` → `Auth` (cookies missing/expired). `checkpoint_required` /
  `challenge_required` → `Auth` with "confirm the login in your browser".
* `429`, or `"Please wait a few minutes"` → `RateLimited`: back off (30s, 60s, 120s…)
  then give up.
* `404`, `data.user == null` → `NotFound`. `is_private && !followed_by_viewer` → `Private`.
* An HTML body where JSON was expected → `Auth` if it is the login page, else `Parse`.
* CDN `403`/`410` on a media URL → the signed URL expired: refresh via `media_info`
  once, then retry. CDN URLs carry an `oe=<hex unix time>` expiry parameter.

### Unverified API assumptions
The client was built from the endpoint shapes above and tested only against mocks.
When live behaviour differs, these are the first suspects:
* `POST /api/v1/clips/user/`: first page sent without `max_id`.
* `reel_ids=highlight%3AN`: the colon is percent-encoded in the query.
* `login_required` / `require_login` / `checkpoint_required` detection relies on
  those strings appearing in the JSON body or the redirect `Location`.
* `X-IG-WWW-Claim` is always `0` (the server's `x-ig-set-www-claim` is ignored).
* Progressive `video_versions` only; DASH manifests are ignored.

## Rules for every change

* **Secrets.** Never print, log, persist, or put in error messages any cookie value,
  `Cookie`/`Authorization` header, or `sessionid`. `CookieJar`'s `Debug` redacts.
  Cookies go only to the API base host, never to CDN URLs. Never write to the user's
  cookie file. Run user-visible strings that may contain URLs through `redact`.
* **No real network in tests.** Mock the API and CDN with `wiremock`. Fixtures in
  `tests/fixtures/` are hand-made: no real cookies, no real signed CDN URLs, no real
  people's data.
* **Errors.** Library code returns `crate::error::Result`; no `unwrap`/`expect`/`panic!`
  outside tests and provably-infallible spots (comment why). `anyhow` is not used.
* **Async.** tokio; reqwest with rustls. Cancellation via `tokio_util::sync::CancellationToken`.
  Ctrl+C must leave `.part` files and a consistent `queue.json` behind.
* **Files.** Write `queue.json` atomically (temp file + rename). Never overwrite an
  existing different file. Paths must work with spaces, Unicode and Windows reserved
  names; build them with `Path`/`PathBuf`, not string concatenation.
* **Output.** stdout is data (downloaded paths, `list`, `--dry-run`, reports); everything
  else (progress, logs, summaries) goes to stderr via `tracing` or `progress`.
* **Style.** Rust 2024 edition, `cargo fmt`, `cargo clippy --all-targets -- -D warnings`
  clean, doc comments on public items, small focused functions. Match surrounding code.
* **Scope.** No password login, posting, liking, following, captcha solving, or
  scraping beyond the links the user supplies. No ffmpeg/DASH merging in v0.1.

## Testing

```text
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

* Unit tests live next to the code (`#[cfg(test)] mod tests`).
* The window is tested headlessly with `egui_kittest` (labels are queried, so keep
  them stable). To look at it, run `igdl-gui` under `Xvfb` (glow renders with Mesa's
  software GL; winit needs `libxkbcommon-x11-0`) and drive it with `xdotool`.
* Integration tests in `tests/` drive the library and the `igdl` binary against a
  `wiremock` server; the client honours `IGDL_API_BASE`, the data dir comes from
  `--data-dir`/`IGDL_DATA_DIR`, and delays are set to zero via config/flags.
* Cover the failure paths, not just the happy path: expired cookies, 429, truncated
  transfers, resume, expired CDN URL refresh, Ctrl+C, already-archived items.

## Working as one of several agents

* Each agent works in its own git worktree/branch and owns the files it was assigned.
* `src/model.rs`, `src/error.rs`, and public signatures in other modules are shared
  contracts. Extend them additively; if a breaking change is unavoidable, say so in
  the hand-off instead of silently changing a contract another agent depends on.
* New dependencies go in `Cargo.toml` with a one-line reason in the hand-off.
* Before handing back: fmt, clippy and tests pass in your worktree, work is committed,
  and the hand-off lists what was done, what was not, and any contract changes.

## Completion criteria
`igdl get` on a post, a carousel, a profile (with `--limit`), stories and a highlight
produces the right files with the right names; `grab` + `start` does the same through
the queue; a re-run downloads nothing new; an interrupted download resumes; an expired
CDN URL is refreshed; expired cookies, rate limiting, private and missing accounts
produce distinct, actionable errors and exit codes; no cookie value appears anywhere
in output, logs, queue or archive. All verified with mocked endpoints; a real-account
check is left to the user.
