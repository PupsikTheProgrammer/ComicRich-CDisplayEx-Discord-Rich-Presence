# ComicRich — Discord Rich Presence for CDisplayEx

**English** · [Русский](README.ru.md)

An unofficial companion for the [CDisplayEx](https://www.cdisplayex.com/) comic reader that shows
in Discord what you are reading: the cover, the title, the issue and the page — and, in collected
editions, which issue inside the collection you are on.

```
Watching  Runaways #7: Rock Zombies: Part One
          Runaways Vol. 3 (2009) · Page 1 of 24
```

ComicRich starts by itself when you double-click a comic and exits 15 seconds after the last
CDisplayEx window closes. When you are not reading, it is not running at all.

## Features

- Detects the open file and the current page without plugins and without touching CDisplayEx.
- CBZ/ZIP, CBR/RAR (RAR4 and RAR5), archives inside archives, PDF. Other formats (such as CB7)
  are shown by file name.
- Metadata from `ComicInfo.xml`, file and folder names (`Runaways Vol 1\Runaways 01.cbr`).
- Official titles and covers from [Metron](https://metron.cloud) and
  [Comic Vine](https://comicvine.gamespot.com/api), looked up once and cached.
- Kind of publication: ongoing series issue (with the volume), limited series ("issue 3 of 6"),
  one-shot, annual, graphic novel, collected edition.
- Collected editions: which issue you are reading, found from folders, ComicInfo bookmarks or
  issue covers recognized among the pages. An estimate is marked "≈"; unidentified stretches
  stay unnumbered.
- Several CDisplayEx windows at once: the status follows the window you are using.
- Comics opened from CDisplayEx's own menu are followed too.

## Requirements

- Windows 10 or 11.
- [CDisplayEx](https://www.cdisplayex.com/) (tested with 1.10.33), registered for comic files.
- The Discord desktop app.

## Installation

1. Download `comicrich.exe` from the [latest release](../../releases/latest).
2. Double-click it and answer **Yes** to "Install it?".
3. Optional: when asked, open the settings and add a Metron token or a Comic Vine API key
   (see below) for official titles and covers. Without them, titles come from the files.

That's all: double-click a comic, and its cover and title appear in your Discord status.

Windows may warn "Windows protected your PC" the first time, as it does for any new program that is
not code-signed. Click **More info → Run anyway**.

The program goes to `%LOCALAPPDATA%\ComicRich\` together with its settings, and double-clicking a
`.cbz`, `.cbr` or `.cb7` file starts ComicRich, which opens CDisplayEx right away. No administrator
rights are needed: only the open command of CDisplayEx's file type is overridden for the current
user (`HKCU\Software\Classes\<CDisplayEx file type>\shell\open\command`); icons and thumbnails stay
CDisplayEx's.

**Updating:** double-click the new `comicrich.exe` and answer **Yes** to "Replace it with this
version?". Your settings are kept. Do the same if double-clicking comics stops starting ComicRich
after CDisplayEx was reinstalled or updated.

The same works from a terminal: `comicrich.exe --install`.

### Online databases (optional)

Both are free. Metron is tried first, Comic Vine is the fallback.

- **Metron:** create an account at [metron.cloud](https://metron.cloud), then create a token in
  the **API Tokens** section of your account page and put it into `token`.
- **Comic Vine:** sign up at [comicvine.gamespot.com](https://comicvine.gamespot.com/) and copy
  your key from the [API page](https://comicvine.gamespot.com/api/) into `api_key`.

## Settings

To open the settings, double-click `comicrich.exe` (the downloaded one, or
`%LOCALAPPDATA%\ComicRich\comicrich.exe`), or open `%LOCALAPPDATA%\ComicRich\comicrich.toml` in
Notepad. Every field is optional; changes apply the next time you open a comic.

| Field | Meaning |
|---|---|
| `app_id` | Discord application ID; the ComicRich application is used by default |
| `language` | `"en"` or `"ru"`: language of the status text |
| `online` | `true`/`false`: look comics up in the online databases |
| `[metron] token` | Metron API token |
| `[metron] username`, `password` | instead of a token; deprecated by Metron |
| `[comicvine] api_key` | Comic Vine API key |

The file holds your credentials in plain text; keep it to yourself.

To use your own Discord application (its name and icon are what Discord shows), create one in the
[Discord Developer Portal](https://discord.com/developers/applications), upload an icon — it is
shown when there is no cover — and set `app_id`.

## Uninstalling

Uninstall ComicRich like any program: **Settings → Apps → Installed apps → ComicRich → Uninstall**.
Comics open directly in CDisplayEx again, and you choose whether to keep the settings and cache.

From a terminal: `%LOCALAPPDATA%\ComicRich\comicrich.exe --uninstall` (keeps the folder
`%LOCALAPPDATA%\ComicRich\`, which can then be deleted).

## How it works

CDisplayEx has no plugin interface, so ComicRich watches it from the outside:

- the main window title, which names the image being shown (`Inner.cbr\Page 003.jpg`);
- the command line of each CDisplayEx process, which names the double-clicked file;
- the files CDisplayEx (or its PDF helper) holds open while loading another comic;
- CDisplayEx's registry state: its history and the pages it saves.

The image name is matched against the archive's page list to find the page number. The status is
sent to the Discord desktop app over its local connection (Discord RPC).

## Privacy and resources

- Only these go over the network: lookups to Metron and Comic Vine (series name, year and issue
  number), downloads of their cover images for recognizing issues in collections, and the public
  information about the Discord application. Your files are never uploaded.
- The cache, `%LOCALAPPDATA%\ComicRich\cache.json`, holds about 2 KB of text per comic. It prunes
  itself of deleted, changed and year-old comics.
- About 4 MB on disk; no CPU use while idle, since window changes arrive as Windows events.
- A comic is looked up once: 2–4 requests, or 2 for the next issue of a series. Recognizing the
  issues of a collection takes up to two minutes once, at the lowest background priority.

## Limitations

- ComicRich starts only on a double-click. If CDisplayEx is opened another way (Start menu,
  shortcut) and ComicRich is not already running, there is no status.
- Discord has no "Reading" activity, so "Watching" is used.
- Discord accepts a new connection about once every 30 seconds; when comics are opened and closed
  in very quick succession, the status can take that long to appear.
- Metron allows 20 requests per minute and 5000 per day. ComicRich sends at most 18 per minute and
  follows the remaining budget Metron reports; when it runs out for long, Comic Vine is used.
- If Metron refuses the token or password, ComicRich stops asking it until the credentials change:
  Metron bans addresses that keep sending bad logins.

## Troubleshooting

| Command | What it does |
|---|---|
| `set COMICRICH_LOG=C:\path\to\file.log` | write a log, also when started by a double-click (set it in the system environment variables) |
| `comicrich.exe --debug` | run with the log in a console (does not exit when idle) |
| `comicrich.exe --list <files>` | what ComicRich understands about files, without the network |
| `comicrich.exe --lookup <files>` | the full lookup, online databases and collection issues included |

## Building

```
cargo build --release
```

Requires Rust (stable, MSVC toolchain) and Visual Studio Build Tools with the C++ workload (for the
RAR library).

## Tests

```
cargo test
```

The end-to-end tests open real CDisplayEx windows (by double-click and through the Open dialog),
turn pages and check the Discord status, so they only run on request. They need Discord running
and `comicrich.toml` next to the test build (`target\debug`). CDisplayEx's saved pages are
restored afterwards.

```
set COMICRICH_E2E_COMIC=D:\Comics\Series\Issue 07.cbz      (required; the next file in the folder is used too)
set COMICRICH_E2E_COLLECTION=D:\Comics\Omnibus.cbz         (an issue must start within the first 13 pages)
set COMICRICH_E2E_NESTED=D:\Comics\Wrapped.cbr.cbr
set COMICRICH_E2E_PDF=D:\Comics\Large Scan.pdf
cargo test --test e2e -- --ignored --nocapture --test-threads=1
```

They cover: the cover on a first and a cached open, opening from the menu (PDFs included), two
windows at once and the exit when idle, the current issue in a collection, nested archives and
PDFs.

## Credits

Comic data and covers come from [Metron](https://metron.cloud) and
[Comic Vine](https://comicvine.gamespot.com/). ComicRich is not affiliated with CDisplayEx,
Discord, Metron or Comic Vine.
