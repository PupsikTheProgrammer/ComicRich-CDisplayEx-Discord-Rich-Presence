//! What double-clicking `comicrich.exe` itself does: a few standard Windows
//! message boxes to install, update or uninstall it, for readers who never open a
//! terminal. The `--install` and `--uninstall` commands do the same in a console.

use std::path::Path;
use std::process::Command;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
};

use crate::install;
use crate::win::wide;

const TITLE: &str = "ComicRich";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SETTINGS_HINT: &str =
    "Optional: add a Metron token or a Comic Vine API key in the settings to get official titles and covers.";

/// Started without arguments, as by a double-click on the program.
pub fn run() {
    let current = std::env::current_exe().ok();
    match install::installed_exe() {
        None => {
            let question = "ComicRich shows the comic you are reading in CDisplayEx as your Discord status.\n\n\
                 Install it? Comics you double-click will then start it together with CDisplayEx. \
                 No administrator rights are needed.";
            if ask(question) {
                finish_install("ComicRich is installed. Double-click a comic to try it.");
            }
        }
        Some(installed) if current.as_deref().is_some_and(|c| same_path(c, &installed)) => {
            let question = format!(
                "ComicRich {VERSION} is installed and starts when you double-click a comic.\n\n\
                 Open the settings?\n\n\
                 To uninstall it, use Settings → Apps → Installed apps."
            );
            if ask(&question) {
                open_settings();
            }
        }
        Some(_) => {
            let question = format!(
                "ComicRich is already installed.\n\nReplace it with this version ({VERSION})? Your settings are kept."
            );
            if ask(&question) {
                finish_install("ComicRich is updated.");
            }
        }
    }
}

/// Uninstalling from Settings → Apps → Installed apps.
pub fn uninstall() {
    if !ask("Uninstall ComicRich? Comics will open directly in CDisplayEx again.") {
        return;
    }
    if let Err(e) = install::uninstall() {
        show(&format!("ComicRich could not be uninstalled:\n\n{e}"), MB_ICONERROR);
        return;
    }
    let everything = ask("ComicRich is uninstalled.\n\nAlso delete its settings and cache?");
    install::remove_files(everything);
}

fn finish_install(done: &str) {
    match install::install() {
        Ok(_) => {
            if ask(&format!("{done}\n\n{SETTINGS_HINT}\n\nOpen the settings now?")) {
                open_settings();
            }
        }
        Err(e) => show(&format!("ComicRich could not be installed:\n\n{e}"), MB_ICONERROR),
    }
}

fn open_settings() {
    if let Some(path) = install::settings_path() {
        if Command::new("notepad").arg(&path).spawn().is_err() {
            show(&format!("The settings are in\n{}", path.display()), MB_ICONINFORMATION);
        }
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn ask(text: &str) -> bool {
    message(text, MB_YESNO | MB_ICONQUESTION) == IDYES
}

fn show(text: &str, icon: u32) {
    message(text, MB_OK | icon);
}

fn message(text: &str, style: u32) -> i32 {
    let (text, title) = (wide(text), wide(TITLE));
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), style) }
}
