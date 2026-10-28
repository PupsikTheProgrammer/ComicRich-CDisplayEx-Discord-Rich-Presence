//! Hooks ComicRich into double-clicking a comic, so it runs only while reading.
//!
//! Comics open through a file type (ProgID) registered by CDisplayEx, e.g.
//! `CDISPSHELL`, whose `shell\open\command` starts CDisplayEx. Installing writes a
//! per-user override of just that command, pointing at `comicrich.exe --open`,
//! which starts CDisplayEx itself. Icons and thumbnails stay CDisplayEx's, no
//! administrator rights are needed, and uninstalling deletes the override so the
//! original command applies again.

use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW, HKEY,
    HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_SZ,
};
use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

use crate::watcher::registry::read_value;
use crate::win::wide;

const EXTENSIONS: &[&str] = &[".cbz", ".cbr", ".cb7"];
const EXE_NAME: &str = "comicrich.exe";
const CONFIG_NAME: &str = "comicrich.toml";
const MARKER: &str = "--open";

/// ProgIDs of comic extensions whose machine-wide command starts CDisplayEx.
fn cdisplayex_prog_ids() -> Vec<String> {
    let mut ids: Vec<String> = EXTENSIONS
        .iter()
        .filter_map(|ext| read_value(HKEY_CLASSES_ROOT, ext, ""))
        .filter(|id| original_command(id).is_some_and(|c| c.to_lowercase().contains("cdisplayex.exe")))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn command_key(prog_id: &str) -> String {
    format!(r"Software\Classes\{prog_id}\shell\open\command")
}

/// The command as registered by CDisplayEx's installer, ignoring our override.
fn original_command(prog_id: &str) -> Option<String> {
    read_value(HKEY_LOCAL_MACHINE, &command_key(prog_id), "")
}

/// CDisplayEx.exe, from its registered open command.
pub fn cdisplayex_exe() -> Option<PathBuf> {
    let command = cdisplayex_prog_ids().first().and_then(|id| original_command(id))?;
    let exe = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => command.split_whitespace().next()?,
    };
    Some(PathBuf::from(exe)).filter(|p| p.is_file())
}

/// Starts CDisplayEx on `file`, the way Explorer would have.
pub fn launch_cdisplayex(file: &str) -> Result<(), String> {
    let exe = cdisplayex_exe().ok_or("CDisplayEx is not registered for comic files")?;
    Command::new(&exe).arg(file).spawn().map(|_| ()).map_err(|e| format!("cannot start {}: {e}", exe.display()))
}

fn install_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("ComicRich"))
        .ok_or_else(|| "LOCALAPPDATA is not set".to_string())
}

/// Copies the program (and its config) to a stable place and takes over
/// double-clicks on comics. Returns a report for the user.
pub fn install() -> Result<String, String> {
    let ids = cdisplayex_prog_ids();
    if ids.is_empty() {
        return Err("CDisplayEx is not registered for .cbz/.cbr files; install or repair CDisplayEx first".into());
    }
    let dir = install_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let target = dir.join(EXE_NAME);
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut report = Vec::new();

    if !same_file(&current, &target) {
        std::fs::copy(&current, &target).map_err(|e| {
            format!("cannot copy to {}: {e} (close any running ComicRich and try again)", target.display())
        })?;
        report.push(format!("program copied to {}", target.display()));
        let config = current.with_file_name(CONFIG_NAME);
        let target_config = dir.join(CONFIG_NAME);
        if config.is_file() && !target_config.exists() {
            std::fs::copy(&config, &target_config).map_err(|e| e.to_string())?;
            report.push(format!("settings copied to {}", target_config.display()));
        }
    }

    let command = format!("\"{}\" {MARKER} \"%1\"", target.display());
    for id in &ids {
        set_default_value(&command_key(id), &command)?;
        report.push(format!("double-click on {id} files now starts ComicRich"));
    }
    notify_shell();
    Ok(report.join("\n"))
}

/// Restores CDisplayEx as the direct handler of comic files.
pub fn uninstall() -> Result<String, String> {
    let mut report = Vec::new();
    for id in cdisplayex_prog_ids() {
        let key = command_key(&id);
        let ours = read_value(HKEY_CURRENT_USER, &key, "").is_some_and(|c| c.to_lowercase().contains(EXE_NAME));
        if !ours {
            continue;
        }
        delete_key(&key)?;
        // Remove the parents we created, but only while they hold nothing else.
        for parent in [format!(r"Software\Classes\{id}\shell\open"), format!(r"Software\Classes\{id}\shell"), format!(r"Software\Classes\{id}")] {
            if !delete_if_empty(&parent) {
                break;
            }
        }
        report.push(format!("{id} files open directly in CDisplayEx again"));
    }
    notify_shell();
    if report.is_empty() {
        report.push("ComicRich was not installed".into());
    } else if let Ok(dir) = install_dir() {
        report.push(format!("program files and cache remain in {} and can be deleted", dir.display()));
    }
    Ok(report.join("\n"))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn set_default_value(path: &str, value: &str) -> Result<(), String> {
    let wpath = wide(path);
    let data = wide(value);
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wpath.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_WRITE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return Err(format!("cannot create HKCU\\{path}: error {rc}"));
        }
        let rc = RegSetValueExW(key, std::ptr::null(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32);
        RegCloseKey(key);
        if rc != 0 {
            return Err(format!("cannot write HKCU\\{path}: error {rc}"));
        }
    }
    Ok(())
}

fn delete_key(path: &str) -> Result<(), String> {
    let rc = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(path).as_ptr()) };
    if rc == 0 { Ok(()) } else { Err(format!("cannot delete HKCU\\{path}: error {rc}")) }
}

/// Deletes a key without subkeys or values; returns whether it was deleted.
fn delete_if_empty(path: &str) -> bool {
    let wpath = wide(path);
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wpath.as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return false;
        }
        let (mut subkeys, mut values) = (0u32, 0u32);
        let rc = RegQueryInfoKeyW(
            key,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut subkeys,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut values,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        RegCloseKey(key);
        rc == 0 && subkeys == 0 && values == 0 && RegDeleteKeyW(HKEY_CURRENT_USER, wpath.as_ptr()) == 0
    }
}

fn notify_shell() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}
