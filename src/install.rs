//! Hooks ComicRich into double-clicking a comic, so it runs only while reading.
//!
//! Comics open through a file type (ProgID) registered by CDisplayEx, e.g.
//! `CDISPSHELL`, whose `shell\open\command` starts CDisplayEx. Installing writes a
//! per-user override of just that command, pointing at `comicrich.exe --open`,
//! which starts CDisplayEx itself. Icons and thumbnails stay CDisplayEx's, no
//! administrator rights are needed, and uninstalling deletes the override so the
//! original command applies again. ComicRich is also listed under Settings → Apps
//! → Installed apps, from where it can be uninstalled like any program.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW, HKEY,
    HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD, REG_SZ,
};
use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

use crate::watcher::registry::read_value;
use crate::win::wide;

const EXTENSIONS: &[&str] = &[".cbz", ".cbr", ".cb7"];
const EXE_NAME: &str = "comicrich.exe";
/// Where an update moves the previous program, which may still be running.
const OLD_EXE_NAME: &str = "comicrich.old.exe";
const CONFIG_NAME: &str = "comicrich.toml";
const CONFIG_TEMPLATE: &str = include_str!("../comicrich.example.toml");
const MARKER: &str = "--open";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ComicRich";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

/// The installed program, if double-clicks on comics start it.
pub fn installed_exe() -> Option<PathBuf> {
    let exe = install_dir().ok()?.join(EXE_NAME);
    let hooked = cdisplayex_prog_ids().iter().any(|id| ours(id));
    (hooked && exe.is_file()).then_some(exe)
}

pub fn settings_path() -> Option<PathBuf> {
    Some(install_dir().ok()?.join(CONFIG_NAME))
}

/// Whether `prog_id`'s open command is ComicRich's override.
fn ours(prog_id: &str) -> bool {
    read_value(HKEY_CURRENT_USER, &command_key(prog_id), "").is_some_and(|c| c.to_lowercase().contains(EXE_NAME))
}

/// Copies the program to a stable place, creates its settings, takes over
/// double-clicks on comics and lists it among installed apps. Returns a report.
pub fn install() -> Result<String, String> {
    let ids = cdisplayex_prog_ids();
    if ids.is_empty() {
        return Err("CDisplayEx is not registered for .cbz/.cbr files. Install or repair CDisplayEx first.".into());
    }
    let dir = install_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let target = dir.join(EXE_NAME);
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut report = Vec::new();

    if !same_file(&current, &target) {
        replace_exe(&current, &target)?;
        report.push(format!("program copied to {}", target.display()));
    }
    let config = dir.join(CONFIG_NAME);
    if !config.exists() {
        // Settings next to the downloaded program are taken along; otherwise defaults.
        let beside = current.with_file_name(CONFIG_NAME);
        let result = if beside.is_file() { std::fs::copy(&beside, &config).map(|_| ()) } else { std::fs::write(&config, CONFIG_TEMPLATE) };
        result.map_err(|e| format!("cannot create {}: {e}", config.display()))?;
        report.push(format!("settings created at {}", config.display()));
    }

    let command = format!("\"{}\" {MARKER} \"%1\"", target.display());
    for id in &ids {
        set_values(&command_key(id), &[("", Value::Text(&command))])?;
        report.push(format!("double-click on {id} files now starts ComicRich"));
    }
    register_app(&dir, &target)?;
    notify_shell();
    Ok(report.join("\n"))
}

/// Puts `source` at `target`. A running ComicRich keeps its file open, but Windows
/// lets a running program's file be renamed, so the old one is moved aside first.
fn replace_exe(source: &Path, target: &Path) -> Result<(), String> {
    let old = target.with_file_name(OLD_EXE_NAME);
    let _ = std::fs::remove_file(&old);
    if target.exists() {
        let _ = std::fs::rename(target, &old);
    }
    std::fs::copy(source, target)
        .map_err(|e| format!("cannot copy to {}: {e}. Close CDisplayEx, wait 15 seconds and try again.", target.display()))?;
    // Still running, it stays until the next update or uninstall.
    let _ = std::fs::remove_file(&old);
    Ok(())
}

/// The entry under Settings → Apps → Installed apps.
fn register_app(dir: &Path, exe: &Path) -> Result<(), String> {
    let exe_text = exe.display().to_string();
    let uninstall = format!("\"{exe_text}\" --uninstall --gui");
    let quiet = format!("\"{exe_text}\" --uninstall");
    let size_kb = std::fs::metadata(exe).map_or(0, |m| (m.len() / 1024) as u32);
    set_values(
        UNINSTALL_KEY,
        &[
            ("DisplayName", Value::Text("ComicRich")),
            ("DisplayVersion", Value::Text(env!("CARGO_PKG_VERSION"))),
            ("Publisher", Value::Text("ComicRich")),
            ("DisplayIcon", Value::Text(&exe_text)),
            ("InstallLocation", Value::Text(&dir.display().to_string())),
            ("UninstallString", Value::Text(&uninstall)),
            ("QuietUninstallString", Value::Text(&quiet)),
            ("EstimatedSize", Value::Number(size_kb)),
            ("NoModify", Value::Number(1)),
            ("NoRepair", Value::Number(1)),
        ],
    )
}

/// Deletes the program, and with `everything` also its settings and cache, a
/// moment after this process exits (a running program cannot delete itself).
pub fn remove_files(everything: bool) {
    let Ok(dir) = install_dir() else { return };
    let targets = if everything {
        format!("rmdir /s /q \"{}\"", dir.display())
    } else {
        format!("del /f /q \"{}\" \"{}\"", dir.join(EXE_NAME).display(), dir.join(OLD_EXE_NAME).display())
    };
    let _ = Command::new("cmd")
        .raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & {targets}"))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// Restores CDisplayEx as the direct handler of comic files.
pub fn uninstall() -> Result<String, String> {
    let mut report = Vec::new();
    if delete_key(UNINSTALL_KEY).is_ok() {
        report.push("removed from installed apps".to_string());
    }
    for id in cdisplayex_prog_ids() {
        if !ours(&id) {
            continue;
        }
        delete_key(&command_key(&id))?;
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

enum Value<'a> {
    Text(&'a str),
    Number(u32),
}

/// Creates `HKCU\path` if needed and writes the values; the name "" is the default value.
fn set_values(path: &str, values: &[(&str, Value)]) -> Result<(), String> {
    let wpath = wide(path);
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
        for (name, value) in values {
            let name = wide(name);
            let rc = match value {
                Value::Text(s) => {
                    let data = wide(s);
                    RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
                }
                Value::Number(n) => RegSetValueExW(key, name.as_ptr(), 0, REG_DWORD, n.to_le_bytes().as_ptr(), 4),
            };
            if rc != 0 {
                RegCloseKey(key);
                return Err(format!("cannot write HKCU\\{path}: error {rc}"));
            }
        }
        RegCloseKey(key);
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
