//! Clipboard sharing: plain text, both ways, while a session is up and the switch is on.
//!
//! On Windows a message-only window on its own thread listens for clipboard changes
//! (AddClipboardFormatListener) and does every read and write, so the clipboard is always
//! opened by a window this module owns. Text copied here goes to the host with \r\n made
//! \n, once when sharing starts and then on each change. Text from the host is put on the
//! clipboard with \n made \r\n, marked CanUploadToCloudClipboard = 0 so Windows doesn't
//! sync it to the cloud clipboard.
//!
//! Not sent: copies other apps mark private (ExcludeClipboardContentFromMonitorProcessing,
//! Clipboard Viewer Ignore, CanIncludeInClipboardHistory or CanUploadToCloudClipboard not
//! 1), copies with no text, text that is empty up to its first NUL or over
//! `MAX_CLIPBOARD`, and text equal to the last sent or received, so the host's text
//! doesn't come back to it. Nothing here logs the text; the session only counts.

use nocomputer_proto::ClipboardText;
use std::sync::LazyLock;
use tokio::sync::watch;

/// What became of a change to this machine's clipboard, or of text from the host.
#[derive(Debug)]
pub enum Event {
    /// Copied here: send it.
    Copied(ClipboardText),
    /// Changed, but not to text that may be sent.
    Skipped,
    /// The clipboard stayed busy (another app held it open), or a write failed.
    Failed,
}

/// The switch in the status bar. Off until the page says otherwise, so a saved "off"
/// can't lose a race with the first session.
static SWITCH: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::Sender::new(false));

pub fn set_enabled(on: bool) {
    SWITCH.send_replace(on);
}

pub fn switch() -> watch::Receiver<bool> {
    SWITCH.subscribe()
}

fn to_host(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn to_local(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg(not(windows))]
pub struct Clipboard;

#[cfg(not(windows))]
impl Clipboard {
    pub fn start() -> Result<Clipboard, String> {
        Err("clipboard sharing is only on Windows".into())
    }

    pub fn apply(&self, _text: ClipboardText) {}

    pub async fn next(&mut self) -> Option<Event> {
        None
    }
}

#[cfg(windows)]
pub use windows::Clipboard;

#[cfg(windows)]
mod windows {
    use super::{to_host, to_local, Event};
    use nocomputer_proto::{ClipboardText, MAX_CLIPBOARD};
    use std::ptr::{null, null_mut};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassW, HWND_MESSAGE, MSG,
        WM_APP, WM_CLIPBOARDUPDATE, WNDCLASSW,
    };

    const CF_UNICODETEXT: u32 = 13;
    const CHANGED: u32 = WM_APP;
    const APPLY: u32 = WM_APP + 1;
    const STOP: u32 = WM_APP + 2;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Watches this machine's clipboard until dropped.
    pub struct Clipboard {
        window: isize,
        apply: std::sync::mpsc::Sender<ClipboardText>,
        events: mpsc::UnboundedReceiver<Event>,
    }

    impl Clipboard {
        /// Starts watching; the first event is the clipboard as it is now.
        pub fn start() -> Result<Clipboard, String> {
            let (ready_tx, ready) = std::sync::mpsc::channel();
            let (apply, apply_rx) = std::sync::mpsc::channel();
            let (events_tx, events) = mpsc::unbounded_channel();
            std::thread::spawn(move || run(ready_tx, apply_rx, events_tx));
            let window = ready.recv().map_err(|_| "the clipboard thread ended".to_string())??;
            Ok(Clipboard { window, apply, events })
        }

        /// Puts text from the host on the clipboard, unless it is already there. Only the
        /// newest of several waiting is written.
        pub fn apply(&self, text: ClipboardText) {
            if self.apply.send(text).is_ok() {
                unsafe { PostMessageW(self.window as HWND, APPLY, 0, 0) };
            }
        }

        /// Cancel-safe. `None` if the watcher stopped.
        pub async fn next(&mut self) -> Option<Event> {
            self.events.recv().await
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            unsafe { PostMessageW(self.window as HWND, STOP, 0, 0) };
        }
    }

    unsafe extern "system" fn window_proc(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if message == WM_CLIPBOARDUPDATE {
            // Handled in the loop below, which has the state.
            PostMessageW(window, CHANGED, 0, 0);
            return 0;
        }
        DefWindowProcW(window, message, w, l)
    }

    fn create_window() -> Result<HWND, String> {
        let class = wide("nocomputer-clipboard");
        unsafe {
            let instance = GetModuleHandleW(null());
            let wc = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
            RegisterClassW(&wc); // fails harmlessly if a previous session registered it
            let window = CreateWindowExW(0, class.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), instance, null());
            if window.is_null() {
                return Err("could not create the clipboard window".into());
            }
            if AddClipboardFormatListener(window) == 0 {
                DestroyWindow(window);
                return Err("could not watch the clipboard".into());
            }
            Ok(window)
        }
    }

    fn run(ready: std::sync::mpsc::Sender<Result<isize, String>>, apply: std::sync::mpsc::Receiver<ClipboardText>, events: mpsc::UnboundedSender<Event>) {
        let window = match create_window() {
            Ok(w) => w,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(window as isize));
        let mut state = State { window, formats: Formats::register(), last: None, applied: None, events };
        state.changed();
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                match msg.message {
                    CHANGED => state.changed(),
                    APPLY => {
                        if let Some(text) = apply.try_iter().last() {
                            state.apply(text);
                        }
                    }
                    STOP => break,
                    _ => {
                        DispatchMessageW(&msg);
                    }
                }
            }
            RemoveClipboardFormatListener(window);
            DestroyWindow(window);
        }
    }

    /// Formats other apps add to say a copy is private. 0 where registration failed.
    struct Formats {
        exclude: u32,
        viewer_ignore: u32,
        history: u32,
        cloud: u32,
    }

    impl Formats {
        fn register() -> Self {
            let r = |name: &str| unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) };
            Formats {
                exclude: r("ExcludeClipboardContentFromMonitorProcessing"),
                viewer_ignore: r("Clipboard Viewer Ignore"),
                history: r("CanIncludeInClipboardHistory"),
                cloud: r("CanUploadToCloudClipboard"),
            }
        }

        /// With the clipboard open. A DWORD format counts as private unless it holds 1.
        fn private(&self) -> bool {
            let present = |f: u32| f != 0 && unsafe { IsClipboardFormatAvailable(f) } != 0;
            let not_allowed = |f: u32| present(f) && read_dword(f) != Some(1);
            present(self.exclude) || present(self.viewer_ignore) || not_allowed(self.history) || not_allowed(self.cloud)
        }
    }

    fn read_dword(format: u32) -> Option<u32> {
        unsafe {
            let handle = GetClipboardData(format);
            if handle.is_null() || GlobalSize(handle) < 4 {
                return None;
            }
            let p = GlobalLock(handle) as *const u32;
            if p.is_null() {
                return None;
            }
            let value = p.read_unaligned();
            GlobalUnlock(handle);
            Some(value)
        }
    }

    /// The clipboard, open; closed on drop.
    struct Open;

    impl Open {
        /// Another app may hold it for a moment: retries for about 200 ms.
        fn new(window: HWND) -> Option<Open> {
            for _ in 0..10 {
                if unsafe { OpenClipboard(window) } != 0 {
                    return Some(Open);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            None
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            unsafe { CloseClipboard() };
        }
    }

    enum Read {
        Text(String),
        Skip,
        Busy,
    }

    struct State {
        window: HWND,
        formats: Formats,
        /// The last text sent or received, as the host has it.
        last: Option<String>,
        /// The clipboard's sequence number after our last write.
        applied: Option<u32>,
        events: mpsc::UnboundedSender<Event>,
    }

    impl State {
        fn changed(&mut self) {
            if self.applied == Some(unsafe { GetClipboardSequenceNumber() }) {
                return; // our own write
            }
            let event = match self.read() {
                Read::Text(text) if self.last.as_ref() == Some(&text) => return,
                Read::Text(text) => match ClipboardText::new(text) {
                    Some(text) => {
                        self.last = Some(text.as_str().to_owned());
                        Event::Copied(text)
                    }
                    None => Event::Skipped,
                },
                Read::Skip => Event::Skipped,
                Read::Busy => Event::Failed,
            };
            let _ = self.events.send(event);
        }

        fn read(&self) -> Read {
            let Some(_open) = Open::new(self.window) else { return Read::Busy };
            if self.formats.private() {
                return Read::Skip;
            }
            unsafe {
                let handle = GetClipboardData(CF_UNICODETEXT);
                if handle.is_null() {
                    return Read::Skip;
                }
                let p = GlobalLock(handle) as *const u16;
                if p.is_null() {
                    return Read::Skip;
                }
                let units = std::slice::from_raw_parts(p, GlobalSize(handle) / 2);
                let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
                // Each UTF-16 unit is at least half a UTF-8 byte, even after \r\n → \n.
                let read = if end / 2 > MAX_CLIPBOARD { Read::Skip } else { Read::Text(to_host(&String::from_utf16_lossy(&units[..end]))) };
                GlobalUnlock(handle);
                read
            }
        }

        fn apply(&mut self, text: ClipboardText) {
            if self.last.as_deref() == Some(text.as_str()) {
                return; // already here
            }
            if self.write(&wide(&to_local(text.as_str()))) {
                self.applied = Some(unsafe { GetClipboardSequenceNumber() });
                self.last = Some(text.into_string());
            } else {
                let _ = self.events.send(Event::Failed);
            }
        }

        fn write(&self, text: &[u16]) -> bool {
            let Some(_open) = Open::new(self.window) else { return false };
            if unsafe { EmptyClipboard() } == 0 {
                return false;
            }
            let bytes = unsafe { std::slice::from_raw_parts(text.as_ptr() as *const u8, text.len() * 2) };
            set(CF_UNICODETEXT, bytes) && (self.formats.cloud == 0 || set(self.formats.cloud, &0u32.to_ne_bytes()))
        }
    }

    /// With the clipboard open and emptied. On success the system owns the memory.
    fn set(format: u32, bytes: &[u8]) -> bool {
        unsafe {
            let memory: HANDLE = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
            if memory.is_null() {
                return false;
            }
            let p = GlobalLock(memory) as *mut u8;
            if p.is_null() {
                GlobalFree(memory);
                return false;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            GlobalUnlock(memory);
            if SetClipboardData(format, memory).is_null() {
                GlobalFree(memory);
                return false;
            }
            true
        }
    }

    // Uses this machine's real clipboard, and puts its text back afterwards:
    // cargo test -- --ignored clipboard
    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::UI::WindowsAndMessaging::{PeekMessageW, PM_REMOVE};

        fn put(window: HWND, formats: &[(u32, &[u8])]) {
            let _open = Open::new(window).expect("clipboard");
            assert_ne!(unsafe { EmptyClipboard() }, 0);
            for (format, bytes) in formats {
                assert!(set(*format, bytes));
            }
        }

        fn text(s: &str) -> Vec<u8> {
            wide(s).iter().flat_map(|u| u.to_ne_bytes()).collect()
        }

        fn current(window: HWND) -> Option<String> {
            let _open = Open::new(window)?;
            unsafe {
                let handle = GetClipboardData(CF_UNICODETEXT);
                let p = GlobalLock(handle) as *const u16;
                if p.is_null() {
                    return None;
                }
                let units = std::slice::from_raw_parts(p, GlobalSize(handle) / 2);
                let s = String::from_utf16_lossy(&units[..units.iter().position(|&u| u == 0).unwrap_or(units.len())]);
                GlobalUnlock(handle);
                Some(s)
            }
        }

        /// Waits up to 500 ms, pumping this thread's messages meanwhile: the other
        /// app's window must answer WM_DESTROYCLIPBOARD when the clipboard is emptied.
        fn next(c: &mut Clipboard) -> Option<Event> {
            let end = std::time::Instant::now() + Duration::from_millis(500);
            while std::time::Instant::now() < end {
                unsafe {
                    let mut msg: MSG = std::mem::zeroed();
                    while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        DispatchMessageW(&msg);
                    }
                }
                if let Ok(event) = c.events.try_recv() {
                    return Some(event);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            None
        }

        #[test]
        #[ignore]
        fn clipboard_copies_go_out_and_host_text_comes_in_once() {
            let other = create_window().unwrap(); // another app
            let saved = current(other);
            let formats = Formats::register();
            let mut c = Clipboard::start().unwrap();
            let _ = next(&mut c); // what was there

            put(other, &[(CF_UNICODETEXT, &text("one\r\ntwo"))]);
            assert!(matches!(next(&mut c), Some(Event::Copied(t)) if t.as_str() == "one\ntwo"));
            put(other, &[(CF_UNICODETEXT, &text("secret")), (formats.history, &0u32.to_ne_bytes())]);
            assert!(matches!(next(&mut c), Some(Event::Skipped)));
            put(other, &[(CF_UNICODETEXT, &text("ignore me")), (formats.exclude, &0u32.to_ne_bytes())]);
            assert!(matches!(next(&mut c), Some(Event::Skipped)));
            put(other, &[(CF_UNICODETEXT, &text("\0after a NUL"))]);
            assert!(matches!(next(&mut c), Some(Event::Skipped)));

            // From the host: written with \r\n, kept off the cloud clipboard, not echoed.
            c.apply(ClipboardText::new("a\nb".into()).unwrap());
            assert!(next(&mut c).is_none());
            assert_eq!(current(other).as_deref(), Some("a\r\nb"));
            {
                let _open = Open::new(other).unwrap();
                assert_eq!(read_dword(formats.cloud), Some(0));
            }
            // Copied again here, it isn't sent back.
            put(other, &[(CF_UNICODETEXT, &text("a\r\nb"))]);
            assert!(next(&mut c).is_none());

            drop(c);
            put(other, &[(CF_UNICODETEXT, &text(saved.as_deref().unwrap_or("")))]);
            unsafe { DestroyWindow(other) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{to_host, to_local};

    #[test]
    fn line_endings_convert_each_way() {
        assert_eq!(to_host("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(to_host("a\rb\n"), "a\rb\n"); // a lone \r is text, not a line ending
        assert_eq!(to_local("a\nb"), "a\r\nb");
        assert_eq!(to_local("a\r\nb\n"), "a\r\nb\r\n"); // never \r\r\n
        assert_eq!(to_host(&to_local("x\ny\n\nz")), "x\ny\n\nz");
    }
}
