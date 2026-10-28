//! Small Win32 helpers shared across modules.

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject};

const MUTEX_NAME: &str = "Local\\ComicRich.SingleInstance";
/// Set by a second launch to ask the running instance not to exit.
const KEEPALIVE_NAME: &str = "Local\\ComicRich.KeepAlive";

/// NUL-terminated UTF-16 string for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decodes a UTF-16 buffer up to the first NUL.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Ownership of the single running ComicRich.
pub struct Instance {
    mutex: HANDLE,
    keepalive: HANDLE,
}

impl Instance {
    /// `None` if another ComicRich is running.
    pub fn acquire() -> Option<Instance> {
        let mutex = take_mutex()?;
        let name = wide(KEEPALIVE_NAME);
        let keepalive = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        Some(Instance { mutex, keepalive })
    }

    /// Whether a second launch asked us to stay since the last check.
    pub fn keepalive_requested(&self) -> bool {
        !self.keepalive.is_null() && unsafe { WaitForSingleObject(self.keepalive, 0) } == WAIT_OBJECT_0
    }

    /// Gives up ownership before exiting. Returns false, keeping ownership, if a
    /// launch asked us to stay in the meantime.
    pub fn release_for_exit(&mut self) -> bool {
        unsafe { CloseHandle(self.mutex) };
        if !self.keepalive_requested() {
            return true;
        }
        match take_mutex() {
            Some(mutex) => {
                self.mutex = mutex;
                false
            }
            // Another launch became the running instance already.
            None => true,
        }
    }
}

/// Asks the running instance to stay alive (another comic is being opened).
pub fn request_keepalive() {
    let name = wide(KEEPALIVE_NAME);
    unsafe {
        let event = CreateEventW(std::ptr::null(), 0, 0, name.as_ptr());
        if !event.is_null() {
            SetEvent(event);
            CloseHandle(event);
        }
    }
}

fn take_mutex() -> Option<HANDLE> {
    let name = wide(MUTEX_NAME);
    unsafe {
        let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if mutex.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(mutex);
            return None;
        }
        Some(mutex)
    }
}
