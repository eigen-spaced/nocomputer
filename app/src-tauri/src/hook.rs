//! The keyboard path while the viewer controls a host, on Windows: a low-level keyboard
//! hook (WH_KEYBOARD_LL). Only a hook sees the Windows keys before the Start menu does,
//! and it keeps WebView2's own shortcuts (reload, print, find) from firing.
//!
//! The hook runs in a helper process, this executable started with `--keyboard-helper`:
//! Windows does not call a low-level hook owned by the process whose WebView2 window has
//! the focus (seen on Windows 11; a hook in any other process is called). The viewer
//! tells the helper when its window is in front and what the settings are, one JSON
//! line per change on the helper's stdin; the helper installs the hook only while the
//! window is in front and answers with one JSON line per forwarded key or viewer
//! shortcut on its stdout. Keys go only to the session over that private pipe; neither
//! process logs or stores them. The helper exits when the viewer closes its stdin.
//!
//! Foreground changes come from a WinEvent hook in the viewer: window focus events
//! don't do, since the top-level window loses focus to the WebView2 child whenever the
//! page is clicked. Keys are captured only while the window is in front and a session
//! is up, so the Windows keys aren't taken for nothing, and a key pressed with the
//! pointer off the window stays with Windows. `keys.rs` decides what each key does.

use crate::keys::{CommandKey, Shortcut};
use crate::live::Live;
use nocomputer_proto::{Input, KeyState};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use tauri::Manager;

/// Viewer to helper.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct Settings {
    active: bool,
    command: CommandKey,
    fullscreen: bool,
}

/// Helper to viewer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Event {
    Key { usage: u16, state: KeyState },
    Shortcut(Shortcut),
}

struct Viewer {
    settings: Settings,
    helper: Option<std::process::ChildStdin>,
    #[cfg_attr(not(windows), allow(dead_code))]
    window: isize,
    foreground: bool,
    connected: bool,
    /// Keys sent to the host, counted for the status bar; never which keys.
    keys_sent: u64,
}

static VIEWER: Mutex<Viewer> = Mutex::new(Viewer {
    settings: Settings { active: false, command: CommandKey::Win, fullscreen: false },
    helper: None,
    window: 0,
    foreground: false,
    connected: false,
    keys_sent: 0,
});

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

fn viewer() -> std::sync::MutexGuard<'static, Viewer> {
    VIEWER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether keys are going to the host right now (or why not), and how many have.
pub fn status() -> (Result<(), &'static str>, u64) {
    let v = viewer();
    let state = if v.helper.is_none() {
        Err("keyboard helper not running")
    } else if !v.foreground {
        Err("window not in front")
    } else if !v.connected {
        Err("no session")
    } else if !v.settings.active {
        Err("hook not switched on")
    } else {
        Ok(())
    };
    (state, v.keys_sent)
}

fn update(change: impl FnOnce(&mut Settings)) {
    let mut v = viewer();
    change(&mut v.settings);
    send_settings(&mut v);
}

fn send_settings(v: &mut Viewer) {
    let line = serde_json::to_string(&v.settings).expect("settings serialize");
    if let Some(stdin) = v.helper.as_mut() {
        use std::io::Write;
        if writeln!(stdin, "{line}").and_then(|_| stdin.flush()).is_err() {
            v.helper = None;
        }
    }
}

pub fn set_command(command: CommandKey) {
    update(|s| s.command = command);
}

pub fn set_fullscreen(on: bool) {
    update(|s| s.fullscreen = on);
}

/// Called by `live.rs` as sessions start and end.
pub fn set_connected(on: bool) {
    set_state(|v| v.connected = on);
}

/// Applies a change, then turns the hook on or off to match.
fn set_state(change: impl FnOnce(&mut Viewer)) {
    let active = {
        let mut v = viewer();
        change(&mut v);
        let active = v.foreground && v.connected;
        if active == v.settings.active {
            return;
        }
        v.settings.active = active;
        send_settings(&mut v);
        active
    };
    if !active {
        // Whatever was held stays behind with this window.
        release_all();
    }
}

fn release_all() {
    if let Some(live) = APP.get().and_then(|a| a.try_state::<Live>()) {
        live.send_input(Input::ReleaseAll);
    }
}

#[cfg(not(windows))]
pub fn watch_foreground(_app: &tauri::AppHandle, _window: isize) {}

#[cfg(not(windows))]
pub fn run_helper(_window: &str) -> i32 {
    2
}

#[cfg(windows)]
pub use windows::{run_helper, watch_foreground};

#[cfg(windows)]
mod windows {
    use super::{release_all, set_state, viewer, Event, Settings, APP};
    use crate::keys::{Keyboard, RawKey};
    use crate::live::Live;
    use nocomputer_proto::Input;
    use std::io::{BufRead, Write};
    use std::sync::{mpsc, Mutex};
    use tauri::{Emitter, Manager};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetAncestor, GetCursorPos, GetForegroundWindow, GetMessageW, WindowFromPoint, GA_ROOT, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
        EVENT_SYSTEM_FOREGROUND, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_UP, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WINEVENT_OUTOFCONTEXT,
        WM_APP,
    };

    // ---- viewer side ----

    /// Starts the helper, and tells it whenever `window` comes to the foreground or
    /// leaves it; leaving also releases everything held on the host. Call once, on the
    /// main thread, whose message loop delivers the foreground events.
    pub fn watch_foreground(app: &tauri::AppHandle, window: isize) {
        let helper = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .args(["--keyboard-helper", &window.to_string()])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
        });
        let mut child = match helper {
            Ok(c) => c,
            Err(e) => {
                eprintln!("keyboard helper did not start: {e}");
                return;
            }
        };
        let stdout = child.stdout.take().expect("piped stdout");
        {
            let mut v = viewer();
            v.window = window;
            v.helper = child.stdin.take();
        }
        let _ = APP.set(app.clone());
        let app = app.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                match serde_json::from_str::<Event>(&line) {
                    Ok(Event::Key { usage, state }) => {
                        if let Some(live) = app.try_state::<Live>() {
                            live.send_input(Input::Key { usage, state });
                            viewer().keys_sent += 1;
                        }
                    }
                    Ok(Event::Shortcut(s)) => {
                        let _ = app.emit("viewer-shortcut", s);
                    }
                    Err(_) => {}
                }
            }
            // The helper is gone (it should outlive the viewer): no more keys, none held.
            viewer().helper = None;
            release_all();
            let _ = child.wait();
        });
        // SAFETY: a valid callback; out of context, so it runs on this thread's message
        // loop. The hook lasts as long as the process.
        unsafe { SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, std::ptr::null_mut(), Some(foreground_changed), 0, 0, WINEVENT_OUTOFCONTEXT) };
        // Also poll: a missed foreground event must not leave the keyboard off (or on).
        std::thread::spawn(|| loop {
            // SAFETY: no preconditions.
            foreground(unsafe { GetForegroundWindow() } as isize);
            std::thread::sleep(std::time::Duration::from_millis(250));
        });
    }

    unsafe extern "system" fn foreground_changed(_: HWINEVENTHOOK, _: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
        foreground(hwnd as isize);
    }

    fn foreground(hwnd: isize) {
        set_state(|v| v.foreground = hwnd == v.window);
    }

    // ---- helper side ----

    struct Helper {
        keyboard: Keyboard,
        active: bool,
        window: isize,
        hook: usize,
        events: Option<mpsc::Sender<Event>>,
    }

    static HELPER: Mutex<Helper> = Mutex::new(Helper { keyboard: Keyboard::new(), active: false, window: 0, hook: 0, events: None });

    fn helper() -> std::sync::MutexGuard<'static, Helper> {
        HELPER.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks the hook's own injected keys, so it lets them through.
    const MARKER: usize = 0x6E63_6B62; // "ncKb"
    /// An unassigned virtual key: pressed between Alt's down and up, it stops Windows
    /// opening the window menu.
    const MASK_VK: u16 = 0xE8;
    /// Asks the hook thread to install or remove the hook to match `active`.
    const SET_HOOK: u32 = WM_APP + 1;

    /// `--keyboard-helper HWND`: runs until stdin closes.
    pub fn run_helper(window: &str) -> i32 {
        let Ok(window) = window.parse::<isize>() else { return 2 };
        let (events_tx, events) = mpsc::channel::<Event>();
        {
            let mut h = helper();
            h.window = window;
            h.events = Some(events_tx);
        }
        // Writes happen off the hook thread, which must never block.
        std::thread::spawn(move || {
            let mut out = std::io::stdout().lock();
            for event in events {
                let line = serde_json::to_string(&event).expect("events serialize");
                if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                    std::process::exit(0);
                }
            }
        });
        let (thread_tx, thread_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: MSG is plain data; PeekMessageW creates this thread's queue so that
            // posts to it can't be lost.
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), WM_APP, WM_APP, PM_NOREMOVE) };
            // SAFETY: no preconditions.
            let _ = thread_tx.send(unsafe { GetCurrentThreadId() });
            // SAFETY: a valid MSG; the hook procedure runs inside this loop.
            while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                if msg.message != SET_HOOK {
                    continue;
                }
                let mut h = helper();
                match (h.active, h.hook != 0) {
                    (true, false) => {
                        // SAFETY: a valid hook procedure and this module's handle.
                        h.hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(procedure), GetModuleHandleW(std::ptr::null()), 0) } as usize;
                    }
                    (false, true) => {
                        // SAFETY: the handle came from SetWindowsHookExW and is removed once.
                        unsafe { UnhookWindowsHookEx(h.hook as _) };
                        h.hook = 0;
                        h.keyboard.reset();
                    }
                    _ => {}
                }
            }
        });
        let Ok(hook_thread) = thread_rx.recv() else { return 1 };
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let Ok(settings) = serde_json::from_str::<Settings>(&line) else { continue };
            {
                let mut h = helper();
                if h.keyboard.command != settings.command {
                    h.keyboard.reset();
                }
                h.keyboard.command = settings.command;
                h.keyboard.fullscreen = settings.fullscreen;
                h.active = settings.active;
            }
            // SAFETY: posting to the hook thread's queue, which exists by now.
            unsafe { PostThreadMessageW(hook_thread, SET_HOOK, 0, 0) };
        }
        0
    }

    fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: MARKER } },
        }
    }

    /// Whether the pointer is over the viewer window and not over a window in front of it.
    fn cursor_over(window: isize) -> bool {
        let mut point = POINT { x: 0, y: 0 };
        // SAFETY: a valid POINT to write; WindowFromPoint and GetAncestor take any handle.
        unsafe { GetCursorPos(&mut point) != 0 && GetAncestor(WindowFromPoint(point), GA_ROOT) as isize == window }
    }

    /// Returns whether to keep the key from Windows.
    fn handle(h: &mut Helper, k: &KBDLLHOOKSTRUCT) -> bool {
        let down = k.flags & LLKHF_UP == 0;
        let extended = k.flags & LLKHF_EXTENDED != 0;
        let key = RawKey { scan: k.scanCode, extended, vk: k.vkCode, down };
        // With the pointer off the window, new keys stay with Windows (Win+Shift+S, say);
        // keys already held finish the way they started, so none is left stuck.
        if !cursor_over(h.window) && !h.keyboard.holds(&key) {
            return false;
        }
        let decision = h.keyboard.on_key(key);
        if let Some(events) = &h.events {
            if let Some((usage, state)) = decision.send {
                let _ = events.send(Event::Key { usage, state });
            }
            if let Some(s) = decision.shortcut {
                let _ = events.send(Event::Shortcut(s));
            }
        }
        // Alt (0xA4/0xA5) going up after Option+key: replay its release behind a mask key.
        if !down && !decision.swallow && matches!(k.vkCode, 0xA4 | 0xA5) && h.keyboard.take_alt_mask() {
            let alt_flags = KEYEVENTF_KEYUP | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
            let inputs = [key_input(MASK_VK, 0, 0), key_input(MASK_VK, 0, KEYEVENTF_KEYUP), key_input(k.vkCode as u16, k.scanCode as u16, alt_flags)];
            // SAFETY: a valid array of keyboard INPUTs.
            unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
            return true;
        }
        decision.swallow
    }

    unsafe extern "system" fn procedure(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT.
            let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            // Our own keys (the Alt mask) arrive re-entrantly, from SendInput inside
            // `handle` while HELPER is locked. Keep this check before the lock, or the
            // hook thread deadlocks.
            let ours = k.flags & LLKHF_INJECTED != 0 && k.dwExtraInfo == MARKER;
            if !ours {
                let mut h = helper();
                // SAFETY: no preconditions.
                let foreground = unsafe { GetForegroundWindow() } as isize;
                // The hook may still be on for a moment after the window left the front.
                if h.active && foreground == h.window && handle(&mut h, k) {
                    return 1;
                }
            }
        }
        // SAFETY: passing the event on, as every hook must.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_lines_have_a_fixed_shape() {
        let settings = Settings { active: true, command: CommandKey::RightAlt, fullscreen: false };
        assert_eq!(serde_json::to_string(&settings).unwrap(), r#"{"active":true,"command":"right_alt","fullscreen":false}"#);
        let key = Event::Key { usage: 4, state: KeyState::Repeat };
        assert_eq!(serde_json::from_str::<Event>(&serde_json::to_string(&key).unwrap()).unwrap(), key);
        let shortcut = Event::Shortcut(Shortcut::Fullscreen);
        assert_eq!(serde_json::from_str::<Event>(&serde_json::to_string(&shortcut).unwrap()).unwrap(), shortcut);
    }
}
