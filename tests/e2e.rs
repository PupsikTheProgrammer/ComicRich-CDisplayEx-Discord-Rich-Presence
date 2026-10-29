//! End-to-end checks on a real desktop: open comics the way a reader does, with
//! CDisplayEx and Discord running, and follow ComicRich through its log. They
//! open, drive and close CDisplayEx windows, so they only run on request:
//!
//! ```text
//! set COMICRICH_E2E_COMIC=D:\Comic Books\Series\Issue 07.cbz      (required; the next file in its folder is used too)
//! set COMICRICH_E2E_COLLECTION=D:\Comic Books\Some Omnibus.cbz    (optional; an issue starts within its first 13 pages)
//! set COMICRICH_E2E_NESTED=D:\Comic Books\Wrapped.cbr.cbr         (optional; an archive inside an archive)
//! set COMICRICH_E2E_PDF=D:\Comic Books\Large Scan.pdf             (optional)
//! cargo test --test e2e -- --ignored --nocapture --test-threads=1
//! ```
//!
//! CDisplayEx remembers the page each comic was left at; the tests restore those
//! bookmarks afterwards. Settings (`comicrich.toml`) are read from next to the test
//! build of the program. Times are measured to ComicRich knowing the comic and
//! page: Discord itself sometimes takes long to accept a connection right after
//! another one closed, which is not ComicRich's doing.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(45);
/// From the double-click to ComicRich knowing the comic and page.
const RESOLVED_WITHIN: Duration = Duration::from_secs(2);
/// From confirming the open dialog to ComicRich following the new file.
const MENU_SWITCH_WITHIN: Duration = Duration::from_secs(2);
/// ComicRich stays 15 s after the last window closes.
const IDLE_EXIT_WITHIN: Duration = Duration::from_secs(20);
const BIN: &str = env!("CARGO_BIN_EXE_comicrich");

/// Opening a comic twice: the second time its metadata comes from the cache, so
/// the very first update already has the cover.
#[test]
#[ignore = "opens CDisplayEx; needs COMICRICH_E2E_COMIC and a running Discord"]
fn double_click_shows_the_cover() {
    let comic = comic();

    let first = App::open(&comic, "first");
    let resolved = first.wait_for(WAIT, |log| first_page(log, &comic).is_some());
    let cover = first.wait_for(WAIT, |log| presences(log).any(|l| l.contains("large_image: Some(")));
    let log = first.close("first open");
    assert!(log.contains("connected to Discord"), "Discord is not running");
    let resolved = resolved.expect("comic was not identified");
    assert!(resolved <= RESOLVED_WITHIN, "identifying the comic took {resolved:?}");
    assert!(cover.is_some(), "no cover was found");

    let second = App::open(&comic, "second");
    second.wait_for(WAIT, |log| presences(log).next().is_some()).expect("no presence on the second open");
    let log = second.close("second open (cached)");
    let first_update = presences(&log).next().unwrap();
    assert!(first_update.contains("large_image: Some("), "cached cover missing from the first update: {first_update}");
}

/// A comic opened with a double-click, then the next one in its folder from
/// CDisplayEx's own open dialog, then a page turn in the new comic.
#[test]
#[ignore = "opens CDisplayEx; needs COMICRICH_E2E_COMIC and a running Discord"]
fn menu_open_switches_the_comic() {
    let first = comic();
    let second = next_in_folder(&first);

    let app = App::open(&first, "menu");
    app.wait_for(WAIT, |log| presences(log).next().is_some()).expect("first comic was not shown");
    let pid = app.cdisplayex_pids().first().copied().expect("CDisplayEx window not found");
    let window = ui::window(pid, "TMainForm").expect("CDisplayEx main window");

    let dialog = ui::show_open_dialog(window, pid).expect("CDisplayEx open dialog did not appear");
    let confirmed = app.started.elapsed();
    ui::open_in_dialog(dialog, &second);
    let switched = app.wait_for(Duration::from_secs(10), |log| first_page(log, &second).is_some());
    let shown = app.wait_for(WAIT, |log| after_page(log, &second).any(|l| l.contains("presence updated: Some(")));

    // The page turn must count pages of the new comic.
    std::thread::sleep(Duration::from_millis(1500));
    ui::press(window, ui::VK_NEXT);
    let turned = app.wait_for(Duration::from_secs(5), |log| {
        let mut pages: Vec<usize> = log.lines().filter_map(page_line).filter(|(_, p)| Path::new(p) == second).map(|(n, _)| n).collect();
        pages.dedup();
        pages.len() >= 2
    });

    // A PDF is read by a helper process, and all PDFs name their pages alike.
    let pdf_path = env_comic("COMICRICH_E2E_PDF");
    let pdf = pdf_path.as_ref().map(|pdf| {
        let dialog = ui::show_open_dialog(window, pid).expect("CDisplayEx open dialog did not appear");
        ui::open_in_dialog(dialog, pdf);
        let found = app.wait_for(Duration::from_secs(10), |log| first_page(log, pdf).is_some());
        let wrong = app.log().lines().filter_map(page_line).skip_while(|(_, p)| Path::new(p) != second)
            .find(|(_, p)| Path::new(p) != second && Path::new(p) != pdf).map(|(_, p)| p.to_string());
        (found, wrong)
    });
    let log = app.close("menu open");
    if let Some((found, wrong)) = pdf {
        assert_eq!(wrong, None, "a PDF opened from the menu was taken for another file");
        assert!(found.is_some(), "a PDF opened from the menu was not followed");
    }

    let delay = switched.expect("ComicRich did not follow the comic opened from the menu").saturating_sub(confirmed);
    println!("--- followed {delay:?} after the open dialog was confirmed");
    assert!(delay <= MENU_SWITCH_WITHIN, "switch took {delay:?}");
    assert!(shown.is_some(), "presence did not show the new comic");
    assert!(turned.is_some(), "page turn was not counted in the new comic");
    let mut later = log.lines().filter_map(page_line).skip_while(|(_, p)| Path::new(p) != second);
    assert!(later.all(|(_, p)| Path::new(p) == second || pdf_path.as_deref() == Some(Path::new(p))), "went back to the first comic");
}

/// Two comics double-clicked one after the other: the presence follows the
/// window in use, falls back to the other when one closes, clears when both are
/// closed, and ComicRich then exits by itself.
#[test]
#[ignore = "opens CDisplayEx; needs COMICRICH_E2E_COMIC and a running Discord"]
fn two_windows_follow_the_one_in_use() {
    let a = comic();
    let b = next_in_folder(&a);

    let mut app = App::open(&a, "windows");
    app.wait_for(WAIT, |log| presences(log).next().is_some()).expect("first comic was not shown");
    let a_details = last_details(&app.log()).unwrap();

    // A second double-click while ComicRich runs: the new process starts
    // CDisplayEx, hands over to the running one and exits.
    let handed_over = Command::new(BIN).arg("--open").arg(&b).status().expect("start second comicrich");
    assert!(handed_over.success());
    app.wait_for(WAIT, |log| after_page(log, &b).any(|l| l.contains("presence updated: Some(")))
        .expect("second comic was not shown");
    let b_details = last_details(&app.log()).unwrap();
    assert_ne!(a_details, b_details);

    let pids = app.cdisplayex_pids();
    assert_eq!(pids.len(), 2, "expected two CDisplayEx windows");
    let (pa, pb) = (pids[0], pids[1]);

    ui::focus(ui::window(pa, "TMainForm").expect("first window"));
    app.wait_for(Duration::from_secs(25), |log| {
        log.contains(&format!("switched to CDisplayEx window pid={pa}")) && last_details(log).as_deref() == Some(&a_details)
    })
    .expect("presence did not follow the window brought to the front");

    ui::close(pa);
    app.wait_for(Duration::from_secs(25), |log| {
        log.contains(&format!("window closed (pid={pa})")) && last_details(log).as_deref() == Some(&b_details)
    })
    .expect("presence did not fall back to the remaining window");

    ui::close(pb);
    app.wait_for(Duration::from_secs(10), |log| log.contains("clearing presence")).expect("presence was not cleared");
    let exited = app.exited_within(IDLE_EXIT_WITHIN);
    let log = app.close("two windows");
    assert!(exited, "ComicRich did not exit after the last window closed");
    assert!(log.contains("exiting"), "no idle exit in the log");
}

/// Paging through a collected edition names the issue being read.
#[test]
#[ignore = "opens CDisplayEx; needs COMICRICH_E2E_COLLECTION and a running Discord"]
fn collection_names_the_current_issue() {
    let Some(collection) = env_comic("COMICRICH_E2E_COLLECTION") else { return };
    let app = App::open(&collection, "collection");
    app.wait_for(WAIT, |log| presences(log).next().is_some()).expect("collection was not shown");
    let pid = app.cdisplayex_pids().first().copied().expect("CDisplayEx window not found");
    let window = ui::window(pid, "TMainForm").expect("CDisplayEx main window");

    // CDisplayEx skips key presses while it loads pages, so each turn is confirmed.
    let page = |app: &App| app.log().lines().filter_map(page_line).last().map_or(0, |(n, _)| n);
    ui::press(window, ui::VK_HOME);
    for _ in 0..40 {
        let before = page(&app);
        if before >= 13 {
            break;
        }
        ui::press(window, ui::VK_NEXT);
        app.wait_for(Duration::from_secs(2), |log| log.lines().filter_map(page_line).last().is_some_and(|(n, _)| n != before));
    }
    // Discord allows 5 updates per 20 s, so the last one may come a little later.
    let named = app.wait_for(Duration::from_secs(25), |log| {
        log.lines().filter_map(page_line).last().is_some_and(|(n, _)| n >= 13)
            && last_details(log).is_some_and(|d| d.contains('#'))
    });
    let log = app.close("collection");
    assert!(named.is_some(), "no issue named after paging in: {:?}", last_details(&log));
}

/// An archive inside an archive and a large PDF are shown as fast as anything
/// else, with their page count.
#[test]
#[ignore = "opens CDisplayEx; needs COMICRICH_E2E_NESTED or COMICRICH_E2E_PDF and a running Discord"]
fn nested_archive_and_large_pdf_show_quickly() {
    for (var, label) in [("COMICRICH_E2E_NESTED", "nested"), ("COMICRICH_E2E_PDF", "pdf")] {
        let Some(comic) = env_comic(var) else { continue };
        let app = App::open(&comic, label);
        let resolved = app.wait_for(WAIT, |log| first_page(log, &comic).is_some());
        app.wait_for(WAIT, |log| presences(log).next().is_some());
        let log = app.close(label);
        let resolved = resolved.unwrap_or_else(|| panic!("{label}: comic was not identified"));
        assert!(resolved <= RESOLVED_WITHIN, "{label}: identifying the comic took {resolved:?}");
        let (_, total) = first_page(&log, &comic).unwrap();
        assert!(total < 5000, "{label}: page count unknown");
        let shown = presences(&log).next().unwrap_or_else(|| panic!("{label}: no presence"));
        assert!(shown.contains(&format!(" {total}\")")), "{label}: page count missing from the presence: {shown}");
    }
}

fn comic() -> PathBuf {
    env_comic("COMICRICH_E2E_COMIC").expect("set COMICRICH_E2E_COMIC to a comic file")
}

fn env_comic(var: &str) -> Option<PathBuf> {
    let Some(path) = std::env::var_os(var).map(PathBuf::from) else {
        println!("--- skipped: set {var}");
        return None;
    };
    assert!(path.is_file(), "no such comic: {}", path.display());
    Some(path)
}

/// The comic after `path` in its folder (or before it, for the last one).
fn next_in_folder(path: &Path) -> PathBuf {
    let is_comic = |p: &Path| p.extension().and_then(|e| e.to_str()).is_some_and(|e| ["cbz", "cbr"].contains(&e.to_lowercase().as_str()));
    let mut comics: Vec<PathBuf> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| is_comic(p))
        .collect();
    comics.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    let i = comics.iter().position(|p| p == path).expect("comic is in its folder");
    let other = if i + 1 < comics.len() { i + 1 } else { i.checked_sub(1).expect("a second comic in the folder") };
    comics.swap_remove(other)
}

/// `page 3/24 of D:\path.cbz` log lines: the page and the path.
fn page_line(line: &str) -> Option<(usize, &str)> {
    let rest = line.split_once(" page ")?.1;
    let (page, rest) = rest.split_once('/')?;
    let path = rest.split_once(" of ")?.1;
    Some((page.parse().ok()?, path))
}

/// The page and page count logged when `comic` was first identified.
fn first_page(log: &str, comic: &Path) -> Option<(usize, usize)> {
    log.lines().find_map(|l| {
        let (page, path) = page_line(l)?;
        let total = l.split_once(" page ")?.1.split_once('/')?.1.split_once(' ')?.0.parse().ok()?;
        (Path::new(path) == comic).then_some((page, total))
    })
}

/// Log lines from the first one identifying `comic` on.
fn after_page<'a>(log: &'a str, comic: &'a Path) -> impl Iterator<Item = &'a str> {
    log.lines().skip_while(move |l| !page_line(l).is_some_and(|(_, p)| Path::new(p) == comic))
}

/// Presence updates that showed something, in order.
fn presences(log: &str) -> impl Iterator<Item = &str> {
    log.lines().filter(|l| l.contains("presence updated: Some("))
}

/// The `details` line of the latest presence update.
fn last_details(log: &str) -> Option<String> {
    let line = presences(log).last()?;
    Some(line.split_once("details: \"")?.1.split_once("\", state")?.0.to_string())
}

/// ComicRich started the way a double-click starts it, logging to a file.
struct App {
    child: Child,
    log_path: PathBuf,
    started: Instant,
    bookmarks: Vec<(String, String)>,
}

impl App {
    fn open(comic: &Path, label: &str) -> App {
        let log_path = std::env::temp_dir().join(format!("comicrich-e2e-{}-{label}.log", std::process::id()));
        let _ = std::fs::remove_file(&log_path);
        let bookmarks = bookmarks::save();
        let started = Instant::now();
        let child = Command::new(BIN)
            .arg("--open")
            .arg(comic)
            .env("COMICRICH_LOG", &log_path)
            .spawn()
            .expect("start comicrich");
        App { child, log_path, started, bookmarks }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Time since start at which `done` first held for the log, if within `timeout`.
    fn wait_for(&self, timeout: Duration, done: impl Fn(&str) -> bool) -> Option<Duration> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let log = self.log();
            assert!(!log.contains("another instance is already running"), "close the running ComicRich first");
            if done(&log) {
                return Some(self.started.elapsed());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn exited_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn cdisplayex_pids(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self
            .log()
            .lines()
            .filter_map(|l| l.split("CDisplayEx found (pid=").nth(1)?.split([',', ')']).next()?.parse().ok())
            .collect();
        pids.dedup();
        pids
    }

    /// Closes CDisplayEx and ComicRich, restores the bookmarks; prints and returns the log.
    fn close(mut self, label: &str) -> String {
        let pids = self.cdisplayex_pids();
        for &pid in &pids {
            ui::close(pid);
        }
        bookmarks::restore(&self.bookmarks, &pids);
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Discord is slow to accept a connection right after one closed.
        std::thread::sleep(Duration::from_secs(2));
        let log = self.log();
        println!("--- {label}:");
        for line in log.lines() {
            println!("    {}", line.chars().take(200).collect::<String>());
        }
        log
    }
}

/// The page CDisplayEx saved for each comic in its registry.
mod bookmarks {
    use std::process::Command;

    const COMICS: &str = r"HKCU:\Software\CDisplayEx\Comics";

    fn powershell(script: &str) -> String {
        let out = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]).output().expect("powershell");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `(id, lastpage)` of every comic CDisplayEx knows.
    pub fn save() -> Vec<(String, String)> {
        let script = format!(
            "Get-ChildItem '{COMICS}' | ForEach-Object {{ '{{0}}={{1}}' -f $_.PSChildName, (Get-ItemProperty $_.PSPath).lastpage }}"
        );
        powershell(&script)
            .lines()
            .filter_map(|l| l.trim().split_once('=').map(|(a, b)| (a.to_string(), b.to_string())))
            .collect()
    }

    /// Once the CDisplayEx processes have exited (they save on exit), puts the
    /// saved pages back; comics first opened by the test start at the first page.
    pub fn restore(saved: &[(String, String)], pids: &[u32]) {
        let table: Vec<String> = saved.iter().filter(|(_, p)| !p.is_empty()).map(|(id, p)| format!("'{id}'={p}")).collect();
        let wait = if pids.is_empty() {
            String::new()
        } else {
            let ids: Vec<String> = pids.iter().map(u32::to_string).collect();
            format!("Wait-Process -Id {} -Timeout 10 -ErrorAction SilentlyContinue; ", ids.join(","))
        };
        let script = format!(
            "{wait}$saved = @{{{}}}; Get-ChildItem '{COMICS}' | ForEach-Object {{ \
                $id = $_.PSChildName; $page = if ($saved.ContainsKey($id)) {{ $saved[$id] }} else {{ 0 }}; \
                if ((Get-ItemProperty $_.PSPath).lastpage -ne $page) {{ Set-ItemProperty $_.PSPath -Name lastpage -Value $page; \"bookmark $id restored to $page\" }} }}",
            table.join(";")
        );
        for line in powershell(&script).lines() {
            println!("    {line}");
        }
    }
}

/// Drives CDisplayEx windows like a reader would.
mod ui {
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, EnumWindows, GetClassNameW, GetWindowThreadProcessId, IsWindowVisible, SendMessageW,
        SetForegroundWindow, WM_COMMAND, WM_SETTEXT,
    };

    /// Next page in CDisplayEx (the arrow keys do not turn pages).
    pub const VK_NEXT: u8 = 0x22;
    pub const VK_HOME: u8 = 0x24;
    /// CDisplayEx's "Load files" key.
    const VK_L: u8 = 0x4C;
    const IDOK: usize = 1;

    struct Search<'a> {
        pid: u32,
        class: &'a str,
        found: Option<HWND>,
    }

    fn class_of(hwnd: HWND) -> String {
        let mut buf = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        String::from_utf16_lossy(&buf[..len.max(0) as usize])
    }

    /// A visible top-level window of class `class` owned by `pid`.
    pub fn window(pid: u32, class: &str) -> Option<HWND> {
        unsafe extern "system" fn check(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let search = &mut *(lparam as *mut Search);
            let mut owner = 0;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == search.pid && IsWindowVisible(hwnd) != 0 && class_of(hwnd) == search.class {
                search.found = Some(hwnd);
                return 0;
            }
            1
        }
        let mut search = Search { pid, class, found: None };
        unsafe { EnumWindows(Some(check), &mut search as *mut Search as LPARAM) };
        search.found
    }

    pub fn focus(window: HWND) {
        unsafe { SetForegroundWindow(window) };
        std::thread::sleep(Duration::from_millis(200));
    }

    pub fn press(window: HWND, vk: u8) {
        focus(window);
        unsafe {
            keybd_event(vk, 0, 0, 0);
            keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    /// Asks CDisplayEx to close, as the window's close button does.
    pub fn close(pid: u32) {
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string()]).output();
    }

    pub fn show_open_dialog(window: HWND, pid: u32) -> Option<HWND> {
        press(window, VK_L);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(dialog) = self::window(pid, "#32770") {
                return Some(dialog);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// Types `path` into the dialog's file name box and confirms.
    pub fn open_in_dialog(dialog: HWND, path: &Path) {
        unsafe extern "system" fn find_edit(hwnd: HWND, lparam: LPARAM) -> BOOL {
            if class_of(hwnd) == "Edit" {
                *(lparam as *mut Option<HWND>) = Some(hwnd);
                return 0;
            }
            1
        }
        let mut edit: Option<HWND> = None;
        unsafe { EnumChildWindows(dialog, Some(find_edit), &mut edit as *mut Option<HWND> as LPARAM) };
        let edit = edit.expect("file name box in the open dialog");
        let text: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
        unsafe {
            SendMessageW(edit, WM_SETTEXT, 0, text.as_ptr() as LPARAM);
            SendMessageW(dialog, WM_COMMAND, IDOK, 0);
        }
    }
}
