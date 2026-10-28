//! Diagnostic logging: to the console with `--debug`, or appended to the file
//! named by the `COMICRICH_LOG` environment variable (useful when ComicRich is
//! started by a double-click and has no console).

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Console::{
    AllocConsole, AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

static CONSOLE: AtomicBool = AtomicBool::new(false);
static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

pub fn enable_console() {
    // Release builds are GUI-subsystem: unless stdout is already redirected,
    // reuse the launching terminal or open a new console.
    unsafe {
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        if (stdout.is_null() || stdout as isize == -1) && AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            AllocConsole();
        }
    }
    CONSOLE.store(true, Ordering::Relaxed);
}

fn file() -> Option<&'static Mutex<File>> {
    FILE.get_or_init(|| {
        let path = std::env::var_os("COMICRICH_LOG")?;
        File::options().create(true).append(true).open(path).ok().map(Mutex::new)
    })
    .as_ref()
}

pub fn enabled() -> bool {
    CONSOLE.load(Ordering::Relaxed) || file().is_some()
}

pub fn write(line: &str) {
    if CONSOLE.load(Ordering::Relaxed) {
        println!("{line}");
    }
    if let Some(file) = file() {
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn timestamp() -> String {
    let mut t = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

macro_rules! log {
    ($($arg:tt)*) => {
        if $crate::log::enabled() {
            $crate::log::write(&format!("{} {}", $crate::log::timestamp(), format!($($arg)*)));
        }
    };
}
