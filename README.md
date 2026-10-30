# instagram-downloader (`igdl`)

A JDownloader-style download manager for Instagram, written in Rust.

Paste links, let the **LinkGrabber** resolve them into packages of files, review the
list, then let the **download queue** fetch everything in parallel, resumably, and
without downloading anything twice. Authentication uses **your own browser cookies**
(a `cookies.txt` / JSON export); `igdl` never asks for or stores a password.

Two front ends share the same download list and archive: the **`igdl-gui` desktop
window** and the **`igdl` command line**.

![igdl-gui downloading](docs/gui-downloads.png)

```powershell
igdl cookies check --cookies .\cookies.txt --online        # who am I logged in as?
igdl get https://www.instagram.com/p/C0ffeeXYZ12/ -o D:\insta   # grab + download now
igdl grab natgeo --include posts,stories --limit 50 -o D:\insta # add to queue only
igdl list                                                   # review the queue
igdl start -j 6                                             # download everything queued
```

> Status: v0.1. Everything below is implemented and covered by ~280 tests that run
> against a local mock of Instagram's web API and CDN (the window included). It has not yet been checked
> against live Instagram, whose private API changes without notice; see
> [Limitations](#limitations).

## Why "like JDownloader"

| JDownloader concept | `igdl` |
|---|---|
| LinkGrabber: paste links, they are crawled into packages before anything downloads | `igdl grab` resolves posts, reels, profiles, stories and highlights into **packages** (one per link) of **items** (one per file) |
| Download list that survives restarts | Persistent queue (`queue.json`); `igdl start` picks up where it stopped, partial files resume with HTTP `Range` |
| Parallel downloads, auto-retry | `-j/--jobs` parallel transfers, retry with jittered exponential backoff, global `--limit-rate` |
| Expired links are re-checked | Instagram CDN URLs are signed and expire; on `403/410` the item is re-resolved through the API and retried |
| Duplicate check | Archive of downloaded media IDs (`archive.txt`) + existing-file check; `--update` stops a profile crawl at the first already-downloaded post |
| Accounts | Cookies, sent **only** to the Instagram API host, never to the CDN, never logged |
| Clipboard observer | "Clipboard monitoring" in the window's LinkGrabber, or `igdl watch` |
| Packager / folder rules | Filename templates: `{owner}/{category}/{date}_{id}_{index}.{ext}` |

## Install

Requires a stable Rust toolchain, 1.95 or newer.

```powershell
cargo install --locked --path .   # installs igdl and igdl-gui into the cargo bin folder
# or
cargo build --release --locked    # binaries in target/release/
cargo build --release --locked --no-default-features   # command line only, no window
```

`--locked` builds the exact dependency versions in `Cargo.lock` that were tested;
without it `cargo install` picks newer ones.

### Windows step by step

1. Install Rust with [rustup-init.exe](https://rustup.rs) and accept its offer to
   install the **Visual Studio C++ Build Tools** (or `winget install Rustlang.Rustup`
   plus `winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait
   --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`).
2. **Close every PowerShell, Windows Terminal and VS Code window and open a new
   one.** rustup adds `%USERPROFILE%\.cargo\bin` to PATH, but windows that were
   already open never see it (`cargo : The term 'cargo' is not recognized`). For the
   current window only: `$env:Path += ";$env:USERPROFILE\.cargo\bin"`.
3. Check: `cargo --version` (1.95+, otherwise `rustup update`).
4. In the repository folder: `cargo install --locked --path .`, then run `igdl-gui`
   (or `igdl --help`). `where.exe igdl-gui` shows where it was installed.

Troubleshooting:

* *Access is denied* while installing: `igdl-gui.exe` is still running; close it first.
* Building from a network or mapped drive is slow or fails with locking errors: build
  locally with `$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\igdl-build"` first.
* Windows 11 *Smart App Control* can block the unsigned helper programs cargo builds;
  if the build fails with "An Application Control policy has blocked this file",
  turn Smart App Control off (Windows Security → App & browser control) or use a
  prebuilt `igdl-gui.exe`.
* Windows on ARM additionally needs LLVM/clang (`winget install LLVM.LLVM`) for the
  TLS library.
* Downloaded (unsigned) `.exe` files trigger SmartScreen: *More info → Run anyway*.

PowerShell specifics: `@name` means *splatting* in PowerShell, so write `natgeo` or
`'@natgeo'` instead of a bare `@natgeo`. Paths printed on stdout are UTF-8; before
piping them into other PowerShell 5.1 commands run
`[Console]::OutputEncoding = [Text.Encoding]::UTF8` so non-ASCII names survive.

## Desktop window (`igdl-gui`)

Start `igdl-gui` (on Windows double-click `igdl-gui.exe`; it opens without a console
window). Set your cookie file once under **⚙ Settings**, then:

1. **🔗 LinkGrabber**: paste links or `@usernames` (any text works, or `📂 Load file…`),
   pick profile sections, limit, date range, image/video filter and the folder, and
   click **🔍 Grab links**. Each link becomes a package you can expand and review;
   nothing is downloaded yet. Tick **Clipboard monitoring** to grab every Instagram
   link you copy.
2. **⬇ Add & start** moves the checked packages to the download list and starts them
   (or **Add to download list** to start later with **▶ Start all**).
3. **⬇ Downloads** shows packages and files with live progress, speed and ETA.
   Right-click for Open file / Open folder / Retry / Remove / Copy source link;
   **⏹ Stop** pauses (partial files resume), **↻ Retry failed**, **🗑 Clear finished**.
4. **📜 Log** shows what the LinkGrabber and downloader did; errors get a red badge.

![igdl-gui LinkGrabber](docs/gui-linkgrabber.png)

The account chip (top right) verifies the cookies with Instagram (`@you` in green when
logged in; amber "Login not verified" when Instagram is only throttling the check,
which does not mean the cookies are bad). Downloads go to `Downloads\Instagram`
unless you choose another default folder. Settings are saved to the same
`config.toml` the command line reads. Double-clicking `igdl.exe` (the command-line
tool) just shows its help; use `igdl-gui.exe` for the window.
Closing the window while downloads run asks first; partial files are kept either way.
While the window is open it owns the download list: `igdl list` still works, but
commands that change the list (`grab`, `start`, …) report that another igdl is using it.

## Cookies (authentication)

Instagram's web API needs a logged-in session for almost everything. Log in to
instagram.com in your browser, then export the cookies with an extension such as
*Get cookies.txt LOCALLY* (Chrome/Edge) or *cookies.txt* (Firefox):

```powershell
igdl cookies check --cookies .\cookies.txt            # offline: parses, lists names + expiry
igdl cookies check --cookies .\cookies.txt --online   # also asks Instagram who you are
```

Accepted formats (auto-detected):

* **Netscape `cookies.txt`** (tab-separated, `#HttpOnly_` lines included).
* **JSON**: an array of cookie objects (Cookie-Editor / EditThisCookie export) or an
  object with a `cookies` array (Playwright `storageState`).
* **Header string**: a file containing `sessionid=…; csrftoken=…; ds_user_id=…`.

Only cookies for `instagram.com` are used. `sessionid` is required, `csrftoken` and
`ds_user_id` are strongly recommended. Point `igdl` at the file with `--cookies FILE`,
the `IGDL_COOKIES` environment variable, or `cookies = "…"` in the config file.

Cookie handling rules: values are never printed, logged, written to the queue or
the archive, or sent anywhere except the Instagram API host. Your cookie file is
never modified (cookies Instagram rotates during a run are kept in memory only).
Treat the file like a password: anyone holding `sessionid` is logged in as you.

## Supported links

| Link | Resolves to |
|---|---|
| `instagram.com/p/<code>/`, `/reel/<code>/`, `/reels/<code>/`, `/tv/<code>/`, `/<user>/p/<code>/` | one post (all carousel items); `?img_index=N` keeps only item *N* |
| `instagram.com/<user>/` , `@user` (`'@user'` in PowerShell), `user` | the profile's posts (or `--include` sections) |
| `instagram.com/<user>/reels/` | the profile's reels |
| `instagram.com/<user>/tagged/` | posts the user is tagged in |
| `instagram.com/stories/<user>/` | the user's current stories |
| `instagram.com/stories/<user>/<id>/` | one story item |
| `instagram.com/stories/highlights/<id>/` | one highlight reel |

Links may be wrapped in Markdown (`[t](url)`, `<url>`, list markers); tracking
parameters (`igsh`, `utm_*`) are ignored. `instagr.am` links work too.

## Commands

| Command | What it does |
|---|---|
| `igdl get LINK…` | LinkGrabber + download in one go; finished packages leave the queue, failed items stay for `retry`. `--dry-run` only prints what would be downloaded |
| `igdl grab LINK…` | Resolve links and add the packages to the queue (nothing is downloaded) |
| `igdl start` | Download every queued item (resumes partial files). `--package ID` limits it |
| `igdl list` | Show packages (plus failed/running items). `--items` every item, `--failed`, `--json` |
| `igdl retry [ID…]` | Re-queue failed items (all, or the given package/item IDs) |
| `igdl remove ID…` | Remove packages or items from the queue (downloaded files are kept) |
| `igdl clear` | Remove finished packages (`--failed`, `--all`) |
| `igdl cookies check` | Validate a cookie file; `--online` verifies the session with Instagram |
| `igdl watch` | Clipboard observer: queue Instagram links as you copy them (`--start` downloads them too) |

Links can also come from a file (`-i links.txt`, one per line, `#` comments) or from
stdin (`-i -`).

### Common options

| Option | Default | |
|---|---|---|
| `--cookies FILE` | `IGDL_COOKIES` / config | cookie export to authenticate with |
| `-o, --out DIR` | current directory (window: `Downloads\Instagram`) | download root (stored per package) |
| `-j, --jobs N` | `4` | parallel downloads |
| `--include LIST` | `posts` | profile sections: `posts,reels,tagged,stories,highlights` |
| `--limit N` | none | max posts per profile section (for highlights: max highlight reels) |
| `--since DATE`, `--until DATE` | none | `YYYY-MM-DD`, filter posts, reels and tagged posts by date (UTC) |
| `--only image\|video` | both | media type filter |
| `--update` | off | stop a profile crawl at the first already-downloaded post |
| `--template T` | see below | output path template |
| `--no-archive` | off | ignore and don't update the archive |
| `--retries N` | `5` | retries per file for network/5xx/429 errors |
| `--limit-rate RATE` | none | global bandwidth cap, e.g. `500K`, `2M` |
| `--api-delay SECS` | `1.5` | minimum pause between Instagram API requests (+ jitter) |
| `--user-agent UA` | desktop Chrome | best set to the browser the cookies came from |
| `--data-dir DIR` | platform data dir | where `queue.json` / `archive.txt` live (`IGDL_DATA_DIR`) |
| `-v` / `-q` | | more / less logging (stderr) |

### Filename templates

Default: `{owner}/{category}/{date}_{id}_{index}.{ext}`, e.g.
`natgeo/posts/2024-05-01_C6abcDEFghi_01.jpg`, `natgeo/stories/2024-05-02_3359…_01.mp4`.

| Placeholder | Value |
|---|---|
| `{owner}` `{owner_id}` | username / numeric user id |
| `{category}` | `posts`, `reels`, `tagged`, `stories`, `highlights` |
| `{id}` | shortcode when the item has one, otherwise the media id |
| `{shortcode}` `{post_id}` `{media_id}` | individual identifiers |
| `{index}` `{count}` | 1-based position in a carousel (`01`) and number of files in the post |
| `{date}` `{time}` `{datetime}` `{timestamp}` | `2024-05-01`, `13-37-00`, `20240501_133700`, unix seconds (UTC) |
| `{kind}` `{ext}` | `image`/`video`, file extension |
| `{width}` `{height}` | pixel size when known |
| `{highlight}` | highlight title (empty otherwise) |
| `{caption}` | first 50 characters of the caption |

Every path component is sanitized for Windows (reserved names, `<>:"/\|?*`,
trailing dots/spaces, length). Templates cannot escape the output directory.

## Files and output

| File | Location | |
|---|---|---|
| `config.toml` | config dir (`%APPDATA%\igdl\config`, `~/.config/igdl`) or `--config` | defaults for the options above |
| `queue.json` | data dir (`%APPDATA%\igdl\data`, `~/.local/share/igdl`) | the download list; locked while `igdl` runs |
| `archive.txt` | data dir | one media id per line for every finished file |
| `*.part` | next to the final file | partial download, resumed on the next run |
| `igdl-gui-crash.log` | data dir (or the temp folder) | written if the window crashes; attach it to bug reports |

```toml
# config.toml
out_dir = 'D:\insta'
cookies = 'C:\Users\me\secrets\instagram-cookies.txt'
jobs = 4
template = "{owner}/{category}/{date}_{id}_{index}.{ext}"
api_delay = 2.0
```

stdout carries only the paths of newly downloaded files (one per line) for `get`
and `start`, so the output can be piped; `list`, `cookies check` and `--dry-run`
print their report to stdout. Progress bars, logs and the summary go to stderr.
Downloaded files get their modification time set to the post date (`--no-mtime`
disables this).

Exit codes: `0` everything downloaded or skipped, `1` at least one link or file
failed, `2` usage/config/cookie error, `130` interrupted (Ctrl+C; partial files are
kept and resume next time).

## Rate limits and safety

Instagram rate-limits and may temporarily lock accounts that scrape aggressively.
`igdl` keeps API calls sequential with a randomized pause (`--api-delay`), backs off
on `429` / "please wait a few minutes", and stops with an authentication error
(instead of retrying) on `login_required` or `checkpoint_required`. CDN downloads are
parallel but carry no cookies. Large profile crawls are better done with `--limit`
and `--update` over several runs than in one go.

## Limitations

* Uses Instagram's private web API (`/api/v1/…`), which can change at any time.
* Videos are the best **progressive** MP4 Instagram offers; separate DASH
  video/audio streams (sometimes higher resolution) are not merged.
* Cookies must be exported to a file; reading browser cookie stores directly is
  not supported (Chromium's app-bound encryption makes that unreliable).
* Private accounts are only reachable if the cookie's account follows them.
* No password login, 2FA, or captcha/checkpoint solving; resolve those in your
  browser and re-export the cookies.

## Development

```powershell
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests never touch the network: the Instagram API and CDN are mocked with a local
HTTP server (`IGDL_API_BASE` points the client at it). `IGDL_LOG` takes a
`tracing` filter (e.g. `IGDL_LOG=igdl=debug`) for troubleshooting.
`IGDL_RENDERER=glow|wgpu` forces the window's renderer (it normally tries OpenGL
first and falls back to Direct3D 12/Vulkan, e.g. over Remote Desktop). See [AGENTS.md](AGENTS.md)
for the architecture and contributor/agent guidelines.

## Legal

For downloading content you are allowed to download (your own posts, content you
have permission to archive). Respect creators' rights and Instagram's Terms of Use.
