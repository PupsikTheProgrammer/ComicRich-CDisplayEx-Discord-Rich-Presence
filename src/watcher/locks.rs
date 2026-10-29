//! Which comic file a CDisplayEx process is showing, from the outside.
//!
//! A double-clicked comic arrives on the process command line. A file opened from
//! the menu is found in the process handle table: CDisplayEx keeps an archive open
//! only while loading it (~200 ms), and its window title switches to the first page
//! inside that span, so a snapshot taken right after the title event names the
//! file. PDFs are read by a helper child process, `pdfreader.exe`, which holds the
//! file for about half a second before the title changes, so children are
//! included. A snapshot takes a millisecond or two.

use std::ffi::c_void;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{GetFileType, GetFinalPathNameByHandleW, FILE_TYPE_DISK};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::win::from_wide;

const PROCESS_HANDLE_SNAPSHOT_INFORMATION: u32 = 51;
const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
/// The window is closing; nothing to report.
const STATUS_PROCESS_IS_TERMINATING: i32 = 0xC000_010A_u32 as i32;
const COMIC_EXTENSIONS: &[&str] = &["cbz", "cbr", "zip", "rar", "pdf", "cb7", "7z"];

#[repr(C)]
struct SnapshotHeader {
    number_of_handles: usize,
    reserved: usize,
}

#[repr(C)]
struct HandleEntry {
    handle_value: HANDLE,
    handle_count: usize,
    pointer_count: usize,
    granted_access: u32,
    object_type_index: u32,
    handle_attributes: u32,
    reserved: u32,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut c_void, length: u32, returned: *mut u32)
        -> i32;
}

/// Comic files currently open in the given process or its child processes.
pub fn open_comics(pid: u32) -> Vec<PathBuf> {
    std::iter::once(pid).chain(children(pid)).flat_map(process_comics).collect()
}

fn process_comics(pid: u32) -> Vec<PathBuf> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE, 0, pid);
        if process.is_null() {
            return Vec::new();
        }
        let files = handle_paths(process);
        CloseHandle(process);
        files.into_iter().filter(|p| is_comic(p)).collect()
    }
}

fn children(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snapshot, &mut entry);
        while ok != 0 {
            if entry.th32ParentProcessID == pid && entry.th32ProcessID != pid {
                found.push(entry.th32ProcessID);
            }
            ok = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
    }
    found
}

/// The comic passed on the command line (double-click in Explorer), if any.
pub fn command_line_comic(pid: u32) -> Option<PathBuf> {
    let line = command_line(pid)?;
    // Skip the executable, quoted or not; the rest is the file argument.
    let rest = match line.strip_prefix('"') {
        Some(s) => s.split_once('"')?.1,
        None => line.split_once(' ')?.1,
    };
    let path = PathBuf::from(rest.trim().trim_matches('"'));
    (is_comic(&path) && path.is_file()).then_some(path)
}

fn command_line(pid: u32) -> Option<String> {
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buf = vec![0u64; 1024];
        let mut returned = 0u32;
        let status = NtQueryInformationProcess(
            process,
            PROCESS_COMMAND_LINE_INFORMATION,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut returned,
        );
        CloseHandle(process);
        if status < 0 {
            return None;
        }
        let s = &*(buf.as_ptr() as *const UnicodeString);
        let chars = std::slice::from_raw_parts(s.buffer, s.length as usize / 2);
        Some(String::from_utf16_lossy(chars))
    }
}

unsafe fn handle_paths(process: HANDLE) -> Vec<PathBuf> {
    // u64 words keep the buffer aligned for the structures read from it.
    let mut buf = vec![0u64; 8 * 1024];
    loop {
        let bytes = (buf.len() * 8) as u32;
        let mut returned = 0u32;
        let status =
            NtQueryInformationProcess(process, PROCESS_HANDLE_SNAPSHOT_INFORMATION, buf.as_mut_ptr().cast(), bytes, &mut returned);
        if status == STATUS_INFO_LENGTH_MISMATCH && buf.len() < 1 << 20 {
            buf.resize((returned as usize / 8 + 1).max(buf.len() * 2), 0);
            continue;
        }
        if status < 0 {
            if status != STATUS_PROCESS_IS_TERMINATING {
                log!("handle snapshot failed: {status:#x}");
            }
            return Vec::new();
        }
        break;
    }

    let header = &*(buf.as_ptr() as *const SnapshotHeader);
    let capacity = (buf.len() * 8 - size_of::<SnapshotHeader>()) / size_of::<HandleEntry>();
    let entries = buf.as_ptr().cast::<u8>().add(size_of::<SnapshotHeader>()).cast::<HandleEntry>();
    let mut paths = Vec::new();
    for i in 0..header.number_of_handles.min(capacity) {
        let entry = &*entries.add(i);
        let mut dup: HANDLE = std::ptr::null_mut();
        if DuplicateHandle(process, entry.handle_value, GetCurrentProcess(), &mut dup, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
            continue;
        }
        // Only disk files: name queries on pipes can block.
        if GetFileType(dup) == FILE_TYPE_DISK {
            let mut name = [0u16; 1024];
            let len = GetFinalPathNameByHandleW(dup, name.as_mut_ptr(), name.len() as u32, 0) as usize;
            if len > 0 && len < name.len() {
                let path = from_wide(&name[..len]);
                paths.push(PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(&path)));
            }
        }
        CloseHandle(dup);
    }
    paths
}

fn is_comic(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| COMIC_EXTENSIONS.iter().any(|c| e.eq_ignore_ascii_case(c)))
}
