//! Reads CDisplayEx state from `HKCU\Software\CDisplayEx`.
//!
//! Observed behavior (1.10.33): `Comics\<N>\path` is written as soon as a file is
//! opened for the first time; `lastpage` and `History` only when leaving a file.
//! `History\<i>` values are `Comics` ids, most recent first.

use std::path::PathBuf;

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    REG_DWORD, REG_EXPAND_SZ, REG_SZ,
};

use crate::win::{from_wide, wide};

const ROOT: &str = r"Software\CDisplayEx";
const MAX_HISTORY: usize = 64;

#[derive(Debug, Clone)]
pub struct ComicEntry {
    pub id: u32,
    pub path: PathBuf,
    pub last_page: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub comics: Vec<ComicEntry>,
    /// `Comics` ids, most recently left first.
    pub history: Vec<u32>,
    pub last_open_folder: Option<PathBuf>,
}

pub fn read() -> Snapshot {
    let mut snap = Snapshot::default();
    let Some(root) = Key::open(HKEY_CURRENT_USER, ROOT) else { return snap };

    if let Some(comics) = root.open_sub("Comics") {
        for name in comics.subkeys() {
            let (Ok(id), Some(key)) = (name.parse(), comics.open_sub(&name)) else { continue };
            let Some(path) = key.value("path").filter(|p| !p.is_empty()) else { continue };
            let last_page = key.value("lastpage").and_then(|v| v.parse().ok());
            snap.comics.push(ComicEntry { id, path: PathBuf::from(path), last_page });
        }
    }
    if let Some(history) = root.open_sub("History") {
        snap.history = (0..MAX_HISTORY)
            .map_while(|i| history.value(&i.to_string()))
            .filter_map(|v| v.parse().ok())
            .collect();
    }
    if let Some(options) = root.open_sub("Options") {
        snap.last_open_folder = options.value("lastopenfolder").filter(|p| !p.is_empty()).map(PathBuf::from);
    }
    snap
}

/// A string (or DWORD) value; `name` "" is the key's default value.
pub fn read_value(root: HKEY, path: &str, name: &str) -> Option<String> {
    Key::open(root, path)?.value(name)
}

struct Key(HKEY);

impl Key {
    fn open(parent: HKEY, sub: &str) -> Option<Key> {
        let sub = wide(sub);
        let mut handle: HKEY = std::ptr::null_mut();
        let rc = unsafe { RegOpenKeyExW(parent, sub.as_ptr(), 0, KEY_READ, &mut handle) };
        (rc == 0).then_some(Key(handle))
    }

    fn open_sub(&self, sub: &str) -> Option<Key> {
        Key::open(self.0, sub)
    }

    fn subkeys(&self) -> Vec<String> {
        let mut names = Vec::new();
        for index in 0.. {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    buf.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                break;
            }
            names.push(from_wide(&buf[..len as usize]));
        }
        names
    }

    /// String or DWORD value rendered as text.
    fn value(&self, name: &str) -> Option<String> {
        let name = wide(name);
        let mut kind = 0;
        let mut size = 0u32;
        let rc = unsafe {
            RegQueryValueExW(self.0, name.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut size)
        };
        if rc != 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        let rc = unsafe {
            RegQueryValueExW(self.0, name.as_ptr(), std::ptr::null(), &mut kind, data.as_mut_ptr(), &mut size)
        };
        if rc != 0 {
            return None;
        }
        data.truncate(size as usize);
        match kind {
            REG_SZ | REG_EXPAND_SZ => {
                let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                Some(from_wide(&units))
            }
            REG_DWORD if data.len() >= 4 => {
                Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]).to_string())
            }
            _ => None,
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}
