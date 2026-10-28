//! Tracks CDisplayEx main windows. Every double-clicked comic starts its own
//! CDisplayEx process, so several windows may be open at once.
//!
//! Title changes and window destruction arrive through WinEvent hooks scoped to
//! each CDisplayEx process. A global foreground hook notices which window the
//! reader switches to and picks up new windows the moment they appear; a slow
//! timer is the safety net.

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetClassNameW, GetMessageW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindow, SetTimer, TranslateMessage, MSG, WM_TIMER,
};

use super::WatchEvent;
use crate::win::from_wide;

const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_OBJECT_DESTROY: u32 = 0x8001;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
const WINEVENT_OUTOFCONTEXT: u32 = 0;
const OBJID_WINDOW: i32 = 0;
const POLL_MS: u32 = 1500;
const MAIN_CLASS: &str = "TMainForm";
const EXE_NAME: &str = "cdisplayex.exe";

struct Tracked {
    hwnd: HWND,
    pid: u32,
    hooks: [HWINEVENTHOOK; 2],
    title: Option<String>,
}

impl Tracked {
    fn unhook(&self) {
        for hook in self.hooks {
            unsafe { UnhookWinEvent(hook) };
        }
    }
}

thread_local! {
    static TRACKED: RefCell<Vec<Tracked>> = const { RefCell::new(Vec::new()) };
    static SINK: RefCell<Option<Box<dyn Fn(WatchEvent)>>> = const { RefCell::new(None) };
    static TICK: RefCell<Option<Box<dyn FnMut(usize)>>> = const { RefCell::new(None) };
}

/// Runs the message loop on the calling thread. `tick` gets the number of open
/// CDisplayEx windows after every check (at least every 1.5 s).
pub fn run(sink: impl Fn(WatchEvent) + 'static, tick: impl FnMut(usize) + 'static) {
    SINK.with(|s| *s.borrow_mut() = Some(Box::new(sink)));
    TICK.with(|t| *t.borrow_mut() = Some(Box::new(tick)));
    unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        SetTimer(std::ptr::null_mut(), 0, POLL_MS, None);
        poll();
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == WM_TIMER {
                poll();
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn send(event: WatchEvent) {
    SINK.with(|s| {
        if let Some(sink) = s.borrow().as_ref() {
            sink(event);
        }
    });
}

fn poll() {
    let count = TRACKED.with(|cell| {
        // A hook callback may be delivered while we hold the borrow; the next tick covers it.
        let Ok(mut tracked) = cell.try_borrow_mut() else { return None };

        tracked.retain(|t| {
            let alive = unsafe { IsWindow(t.hwnd) } != 0;
            if !alive {
                closed(t);
            }
            alive
        });

        for (hwnd, pid) in find_main_windows() {
            if tracked.iter().any(|t| t.hwnd == hwnd) {
                continue;
            }
            let hook = |event, callback| unsafe {
                SetWinEventHook(event, event, std::ptr::null_mut(), Some(callback), pid, 0, WINEVENT_OUTOFCONTEXT)
            };
            let hooks = [hook(EVENT_OBJECT_NAMECHANGE, on_name_change), hook(EVENT_OBJECT_DESTROY, on_destroy)];
            log!("CDisplayEx found (pid={pid}, hooks={})", hooks.iter().all(|h| !h.is_null()));
            tracked.push(Tracked { hwnd, pid, hooks, title: None });
        }

        for t in tracked.iter_mut() {
            refresh_title(t);
        }
        Some(tracked.len())
    });
    if let Some(count) = count {
        TICK.with(|t| {
            if let Some(tick) = t.borrow_mut().as_mut() {
                tick(count);
            }
        });
    }
}

fn closed(t: &Tracked) {
    t.unhook();
    log!("CDisplayEx window closed (pid={})", t.pid);
    send(WatchEvent::Closed(t.pid));
}

unsafe extern "system" fn on_destroy(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW || id_child != 0 {
        return;
    }
    TRACKED.with(|cell| {
        if let Ok(mut tracked) = cell.try_borrow_mut() {
            if let Some(i) = tracked.iter().position(|t| t.hwnd == hwnd) {
                closed(&tracked.remove(i));
            }
        }
    });
}

fn refresh_title(t: &mut Tracked) {
    let title = window_text(t.hwnd);
    if t.title.as_deref() != Some(title.as_str()) {
        t.title = Some(title.clone());
        send(WatchEvent::Title { pid: t.pid, title });
    }
}

unsafe extern "system" fn on_name_change(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW || id_child != 0 {
        return;
    }
    TRACKED.with(|cell| {
        if let Ok(mut tracked) = cell.try_borrow_mut() {
            if let Some(t) = tracked.iter_mut().find(|t| t.hwnd == hwnd) {
                refresh_title(t);
            }
        }
    });
}

unsafe extern "system" fn on_foreground(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let known = TRACKED.with(|cell| {
        cell.try_borrow().ok().and_then(|tracked| tracked.iter().find(|t| t.hwnd == hwnd).map(|t| t.pid))
    });
    match known {
        Some(pid) => send(WatchEvent::Focus(pid)),
        // A CDisplayEx window that just appeared: start tracking it right away.
        None if main_window_pid(hwnd).is_some() => poll(),
        None => {}
    }
}

fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    from_wide(&buf[..len.max(0) as usize])
}

/// Process id if `hwnd` is a CDisplayEx main window. `TMainForm` is a generic
/// Delphi class name, so the owning process is checked too.
fn main_window_pid(hwnd: HWND) -> Option<u32> {
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    if from_wide(&class[..len.max(0) as usize]) != MAIN_CLASS {
        return None;
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    is_cdisplayex(pid).then_some(pid)
}

fn find_main_windows() -> Vec<(HWND, u32)> {
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = &mut *(lparam as *mut Vec<(HWND, u32)>);
        if let Some(pid) = main_window_pid(hwnd) {
            found.push((hwnd, pid));
        }
        1
    }

    let mut found: Vec<(HWND, u32)> = Vec::new();
    unsafe { EnumWindows(Some(callback), &mut found as *mut _ as LPARAM) };
    found
}

fn is_cdisplayex(pid: u32) -> bool {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        ok != 0 && from_wide(&buf[..len as usize]).to_lowercase().ends_with(EXE_NAME)
    }
}
