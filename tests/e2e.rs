//! End-to-end checks on a real desktop: open comics the way a reader does, with
//! CDisplayEx and Discord running, and follow ComicRich through its log. They
//! open, drive and close CDisplayEx windows, so they only run on request:
//!
//! ```text
//! set COMICRICH_E2E_COMIC=D:\Comic Books\Some Comic 001.cbz
//! cargo test --test e2e -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Settings (`comicrich.toml`) are read from next to the test build of the program.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(45);
/// Without a cover yet, the file name is shown.
const FIRST_PRESENCE_WITHIN: Duration = Duration::from_secs(3);
/// From the cache, the cover is shown with the first or second update.
const CACHED_COVER_WITHIN: Duration = Duration::from_millis(1500);
/// From confirming the open dialog to ComicRich following the new file.
const MENU_SWITCH_WITHIN: Duration = Duration::from_secs(2);

/// Opening a comic twice: the second time its metadata comes from the cache.
#[test]
#[ignore = "opens CDisplayEx; needs COMICRICH_E2E_COMIC and a running Discord"]
fn double_click_shows_the_cover() {
    let comic = comic();

    let first = App::open(&comic, "first");
    let shown = first.wait_for(WAIT, |log| log.contains("presence updated: Some("));
    let cover = first.wait_for(WAIT, has_cover);
    let log = first.close("first open");
    assert!(log.contains("resolved: command line"), "comic was not identified");
    assert!(log.contains("connected to Discord"), "Discord is not running");
    let shown = shown.expect("no presence was shown");
    assert!(shown <= FIRST_PRESENCE_WITHIN, "first presence took {shown:?}");
    assert!(cover.is_some(), "no cover was found online");

    let second = App::open(&comic, "second");
    let cover = second.wait_for(WAIT, has_cover);
    second.close("second open (cached)");
    let cover = cover.expect("cached cover was not shown");
    assert!(cover <= CACHED_COVER_WITHIN, "cached cover took {cover:?}");
}

/// A comic opened with a double-click, then the next one in its folder from
/// CDisplayEx's own open dialog, then a page turn in the new comic.
#[test]
#[ignore = "opens CDisplayEx; needs COMICRICH_E2E_COMIC and a running Discord"]
fn menu_open_switches_the_comic() {
    let first = comic();
    let second = next_in_folder(&first);
    let second_name = second.display().to_string();
    let shows_second = |l: &str| page_line(l).is_some_and(|(_, path)| path == second_name);

    let app = App::open(&first, "menu");
    app.wait_for(WAIT, |log| log.contains("presence updated: Some(")).expect("first comic was not shown");
    let pid = app.cdisplayex_pid().expect("CDisplayEx window not found");
    let window = ui::window(pid, "TMainForm").expect("CDisplayEx main window");

    let dialog = ui::show_open_dialog(window, pid).expect("CDisplayEx open dialog did not appear");
    let confirmed = app.started.elapsed();
    ui::open_in_dialog(dialog, &second);
    let switched = app.wait_for(Duration::from_secs(10), |log| {
        log.lines().skip_while(|l| !shows_second(l)).any(|l| l.contains("presence updated: Some("))
    });

    // The page turn must count pages of the new comic.
    std::thread::sleep(Duration::from_millis(1500));
    ui::press(window, ui::VK_NEXT);
    let turned = app.wait_for(Duration::from_secs(5), |log| {
        let mut pages: Vec<usize> = log.lines().filter_map(page_line).filter(|(_, p)| *p == second_name).map(|(n, _)| n).collect();
        pages.dedup();
        pages.len() >= 2
    });
    let log = app.close("menu open");

    let switched = switched.expect("presence did not follow the comic opened from the menu");
    let delay = switched.saturating_sub(confirmed);
    println!("--- switched {delay:?} after the open dialog was confirmed");
    assert!(delay <= MENU_SWITCH_WITHIN, "switch took {delay:?}");
    assert!(turned.is_some(), "page turn was not counted in the new comic");
    let mut after_switch = log.lines().skip_while(|l| !shows_second(l)).filter_map(page_line);
    assert!(after_switch.all(|(_, p)| p == second_name), "went back to the first comic");
}

/// `page 3/24 of D:\path.cbz` log lines: the page and the path.
fn page_line(line: &str) -> Option<(usize, &str)> {
    let rest = line.split_once(" page ")?.1;
    let (page, rest) = rest.split_once('/')?;
    let path = rest.split_once(" of ")?.1;
    Some((page.parse().ok()?, path))
}

fn comic() -> PathBuf {
    let comic = std::env::var_os("COMICRICH_E2E_COMIC").map(PathBuf::from).expect("set COMICRICH_E2E_COMIC to a comic file");
    assert!(comic.is_file(), "no such comic: {}", comic.display());
    comic
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

fn has_cover(log: &str) -> bool {
    log.lines().any(|l| l.contains("presence updated: Some(") && l.contains("large_image: Some("))
}

/// ComicRich started the way a double-click starts it, logging to a file.
struct App {
    child: Child,
    log_path: PathBuf,
    started: Instant,
}

impl App {
    fn open(comic: &Path, label: &str) -> App {
        let log_path = std::env::temp_dir().join(format!("comicrich-e2e-{}-{label}.log", std::process::id()));
        let _ = std::fs::remove_file(&log_path);
        let started = Instant::now();
        let child = Command::new(env!("CARGO_BIN_EXE_comicrich"))
            .arg("--open")
            .arg(comic)
            .env("COMICRICH_LOG", &log_path)
            .spawn()
            .expect("start comicrich");
        App { child, log_path, started }
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

    fn cdisplayex_pids(&self) -> Vec<u32> {
        let log = self.log();
        log.lines()
            .filter_map(|l| l.split("CDisplayEx found (pid=").nth(1)?.split([',', ')']).next()?.parse().ok())
            .collect()
    }

    fn cdisplayex_pid(&self) -> Option<u32> {
        self.cdisplayex_pids().first().copied()
    }

    /// Closes CDisplayEx and ComicRich; prints and returns the log.
    fn close(mut self, label: &str) -> String {
        for pid in self.cdisplayex_pids() {
            // A normal close, so CDisplayEx remembers the page as usual.
            let _ = Command::new("taskkill").args(["/PID", &pid.to_string()]).output();
        }
        std::thread::sleep(Duration::from_secs(1));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let log = self.log();
        println!("--- {label}:");
        for line in log.lines() {
            println!("    {}", line.chars().take(200).collect::<String>());
        }
        log
    }
}

/// Drives CDisplayEx windows like a reader would.
mod ui {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, EnumWindows, GetClassNameW, GetWindowThreadProcessId, IsWindowVisible, SendMessageW,
        SetForegroundWindow, WM_COMMAND, WM_SETTEXT,
    };

    /// Next page in CDisplayEx (the arrow keys do not turn pages).
    pub const VK_NEXT: u8 = 0x22;
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

    pub fn press(window: HWND, vk: u8) {
        unsafe {
            SetForegroundWindow(window);
            std::thread::sleep(Duration::from_millis(200));
            keybd_event(vk, 0, 0, 0);
            keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
        }
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
