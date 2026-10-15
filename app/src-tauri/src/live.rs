//! The live session: video from the host, and input and clipboard to it.
//!
//! `connect.rs` pairs and logs in, then hands the session over with [`Live::begin`];
//! there is one at a time. Frames are pulled by the page:
//! `live_next` waits for the next frame and returns it as raw bytes,
//!
//!   [u8 flags (bit0 keyframe, bit1 key_us set, bit2 new stream)][u64 LE host_us][u32 LE key_us]
//!   [u8 codec_len][codec][Annex B access unit]
//!
//! `host_us` is the host's monotonic clock: usable as decoder timestamps and for
//! differences between frames, never against this machine's clock. `key_us` marks the
//! first frame to arrive after a key went down: the time from reading that key-down from
//! the hook to handing this frame to the page, on this machine's clock (0 otherwise). If the page falls
//! behind, frames are dropped up to the next keyframe and one is requested, so the
//! decoder never gets a frame whose keyframe it lacks. A keyframe whose SPS differs from
//! the last one sent is a new stream (a new size, say), and the page starts a fresh
//! decoder for it.
//!
//! The page reports the size it can show 1:1 with `live_view_size`, and the host streams
//! at that size (or its native size), so the picture is scaled once, on the host.
//!
//! Input goes the other way: the page sends pointer and wheel input with `live_input`,
//! and on Windows the keyboard hook (`hook.rs`) sends keys. Neither is logged. Notices
//! from the host (input unavailable, capture stopped) appear in `live_info`.
//!
//! Clipboard text is shared both ways while the switch is on (`clipboard.rs`); only
//! counts reach `live_info`.

use crate::clipboard::{Clipboard, Event};
use nocomputer_proto::{Error, Input, KeyState, NoticeKind, Session};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::Response;
use tokio::sync::{mpsc, watch};

/// Frames buffered for the page before dropping to the next keyframe.
const QUEUE_FRAMES: usize = 8;

#[derive(Debug, Clone, Serialize, Default)]
pub struct LiveInfo {
    pub address: String,
    pub host_name: Option<String>,
    pub host_fingerprint: Option<String>,
    /// Frames dropped here because the page fell behind.
    pub dropped: u64,
    /// The latest notice from the host, for display.
    pub notice: Option<String>,
    /// The keyboard hook is on (the window is in front), and keys sent so far.
    pub keyboard_active: bool,
    /// Why not, when not.
    pub keyboard_off: Option<&'static str>,
    pub keys_sent: u64,
    /// Key-down read from the hook → written to the connection.
    pub key_send: Timing,
    /// Key-down read from the hook → the next video frame received.
    pub key_frame: Timing,
    pub clipboard: ClipboardInfo,
}

/// Clipboard sharing, as counts; never the text.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ClipboardInfo {
    /// Sharing is running for this session.
    pub on: bool,
    pub sent: u64,
    pub received: u64,
    /// Copies here that weren't text, were marked private, or were too long.
    pub skipped: u64,
    /// The clipboard stayed busy, or text from the host couldn't be written.
    pub failed: u64,
    /// Why sharing stopped, if it did on its own.
    pub error: Option<String>,
}

/// Over the last `SAMPLES` measurements, in ms on this machine's monotonic clock.
#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct Timing {
    pub avg_ms: f64,
    pub max_ms: f64,
    pub n: u64,
}

const SAMPLES: usize = 32;

#[derive(Default)]
struct Samples(std::collections::VecDeque<f64>, u64);

impl Samples {
    fn add(&mut self, d: Duration) -> Timing {
        if self.0.len() == SAMPLES {
            self.0.pop_front();
        }
        self.0.push_back(d.as_secs_f64() * 1000.0);
        self.1 += 1;
        let max_ms = self.0.iter().copied().fold(0.0, f64::max);
        Timing { avg_ms: self.0.iter().sum::<f64>() / self.0.len() as f64, max_ms, n: self.1 }
    }
}

/// A frame for the page, and when the key-down before it was read, if it is the first since.
type Frame = (Vec<u8>, Option<Instant>);
type Frames = mpsc::Receiver<Result<Frame, String>>;

/// The session on screen, if any.
#[derive(Default)]
pub struct Live {
    current: Mutex<Option<Arc<Conn>>>,
    /// Why the last session ended, when the host or the network ended it, for the
    /// start screen.
    ended: Mutex<Option<String>>,
}

struct Conn {
    info: Arc<Mutex<LiveInfo>>,
    frames: tokio::sync::Mutex<Frames>,
    keyframe: mpsc::UnboundedSender<()>,
    /// The latest size from the page, for the host.
    view: watch::Sender<Option<(u16, u16)>>,
    /// With when each was sent, for timing.
    input: mpsc::UnboundedSender<(Input, Instant)>,
    /// The session is up, so input has somewhere to go.
    connected: Arc<AtomicBool>,
    /// Ends the session; the task then reports on `closed`.
    shutdown: Arc<tokio::sync::Notify>,
    closed: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

/// Requests from the page, for the session task.
struct FromPage {
    keyframe: mpsc::UnboundedReceiver<()>,
    view: watch::Receiver<Option<(u16, u16)>>,
    /// With when each was sent, for timing.
    input: mpsc::UnboundedReceiver<(Input, Instant)>,
}

fn encode(frame: &nocomputer_proto::VideoFrame, new_stream: bool) -> Vec<u8> {
    let codec = frame.codec.as_bytes();
    let mut out = Vec::with_capacity(14 + codec.len() + frame.data.len());
    out.push(frame.keyframe as u8 | (new_stream as u8) << 2);
    out.extend_from_slice(&frame.host_us.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // key_us, set by live_next
    out.push(codec.len() as u8);
    out.extend_from_slice(codec);
    match frame.keyframe.then(|| crate::sps::no_reorder(&frame.data)).flatten() {
        // So the decoder shows each frame at once (see sps.rs).
        Some(data) => out.extend_from_slice(&data),
        None => out.extend_from_slice(&frame.data),
    }
    out
}

impl Live {
    /// Shows `session` (after login), in place of any before it. `address` is for the
    /// status bar.
    pub fn begin(&self, session: Session, address: String) {
        let (tx, rx) = mpsc::channel(QUEUE_FRAMES);
        let (keyframe, keyframe_rx) = mpsc::unbounded_channel();
        let (view, view_rx) = watch::channel(None);
        let (input, input_rx) = mpsc::unbounded_channel();
        let page = FromPage { keyframe: keyframe_rx, view: view_rx, input: input_rx };
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let info = LiveInfo {
            address,
            host_name: Some(session.host_name.clone()),
            host_fingerprint: Some(session.host_fingerprint.to_string()),
            ..Default::default()
        };
        let conn = Arc::new(Conn {
            info: Arc::new(Mutex::new(info)),
            frames: tokio::sync::Mutex::new(rx),
            keyframe,
            view,
            input,
            connected: Default::default(),
            shutdown: Default::default(),
            closed: Mutex::new(Some(closed_rx)),
        });
        let (info, shutdown, connected) = (conn.info.clone(), conn.shutdown.clone(), conn.connected.clone());
        tauri::async_runtime::spawn(async move {
            let result = run(session, info, tx.clone(), page, connected.clone(), shutdown).await;
            connected.store(false, Ordering::Relaxed);
            if let Err(e) = result {
                let _ = tx.send(Err(e)).await;
            }
            let _ = closed_tx.send(());
        });
        self.ended.lock().unwrap().take();
        if let Some(old) = self.current.lock().unwrap().replace(conn) {
            old.shutdown.notify_one();
        }
    }

    fn conn(&self) -> Option<Arc<Conn>> {
        self.current.lock().unwrap().clone()
    }

    /// Queues input for the host, in order; dropped when no session is up.
    pub fn send_input(&self, input: Input) {
        if let Some(c) = self.conn().filter(|c| c.connected.load(Ordering::Relaxed)) {
            let _ = c.input.send((input, Instant::now()));
        }
    }

    /// Ends the session so the host frees its slot at once rather than at the idle
    /// timeout. The receiver reports when it has.
    fn stop(&self) -> Option<std::sync::mpsc::Receiver<()>> {
        let conn = self.current.lock().unwrap().take()?;
        conn.shutdown.notify_one();
        let closed = conn.closed.lock().unwrap().take();
        closed
    }

    /// [`Live::stop`], blocking for at most `wait`.
    pub fn close(&self, wait: Duration) {
        if let Some(closed) = self.stop() {
            let _ = closed.recv_timeout(wait);
        }
    }
}

async fn run(
    mut session: Session,
    info: Arc<Mutex<LiveInfo>>,
    tx: mpsc::Sender<Result<Frame, String>>,
    mut page: FromPage,
    connected: Arc<AtomicBool>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(), String> {
    // VideoReceiver::next is not cancel-safe: a frame cut short by another branch of the
    // select below would desynchronize its GOP. So frames are read in their own task.
    let (video_tx, mut video) = mpsc::channel(1);
    let mut receiver = session.video();
    let reader = tauri::async_runtime::spawn(async move {
        loop {
            let frame = receiver.next().await;
            let failed = frame.is_err();
            if video_tx.send(frame).await.is_err() || failed {
                return;
            }
        }
    });
    while page.input.try_recv().is_ok() {} // nothing from before the session
    connected.store(true, Ordering::Relaxed);
    crate::hook::set_connected(true);
    let result = session_loop(&mut session, &mut video, &info, &tx, &mut page, &shutdown).await;
    reader.abort();
    info.lock().unwrap().clipboard.on = false; // the watcher went with session_loop
    connected.store(false, Ordering::Relaxed);
    crate::hook::set_connected(false);
    if result.is_ok() {
        // Shut down, or the page went away: leave nothing held, and free the host at once.
        let _ = session.input(Input::ReleaseAll).await;
        session.close().await;
    }
    result
}

async fn session_loop(
    session: &mut nocomputer_proto::Session,
    video: &mut mpsc::Receiver<Result<nocomputer_proto::VideoFrame, Error>>,
    info: &Mutex<LiveInfo>,
    tx: &mpsc::Sender<Result<Frame, String>>,
    page: &mut FromPage,
    shutdown: &tokio::sync::Notify,
) -> Result<(), String> {
    let mut gap = false;
    // The SPS of the last keyframe sent to the page.
    let mut sps: Option<Vec<u8>> = None;
    // When a key went down with no frame received since.
    let mut key_mark: Option<Instant> = None;
    let (mut key_send, mut key_frame) = (Samples::default(), Samples::default());
    let mut switch = crate::clipboard::switch();
    let mut clipboard = None;
    share(&mut clipboard, *switch.borrow_and_update(), info);
    let mut from_host = session.clipboard_receiver();
    loop {
        tokio::select! {
            Some(frame) = video.recv() => {
                let frame = frame.map_err(|e| crate::connect::explain(&e))?;
                let mark = key_mark.take();
                if let Some(mark) = mark {
                    info.lock().unwrap().key_frame = key_frame.add(mark.elapsed());
                }
                if gap && !frame.keyframe {
                    continue;
                }
                gap = false;
                let found = frame.keyframe.then(|| crate::sps::find(&frame.data)).flatten();
                let new_stream = found.is_some_and(|s| sps.as_deref() != Some(s));
                match tx.try_send(Ok((encode(&frame, new_stream), mark))) {
                    Ok(()) if new_stream => sps = found.map(<[u8]>::to_vec),
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // The page is behind: drop up to the next keyframe.
                        gap = true;
                        info.lock().unwrap().dropped += 1;
                        session.request_keyframe().await.map_err(|e| crate::connect::explain(&e))?;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                }
            }
            Ok(()) = page.view.changed() => {
                let size = *page.view.borrow_and_update();
                if let Some((width, height)) = size {
                    session.view_size(width, height).await.map_err(|e| crate::connect::explain(&e))?;
                }
            }
            Some(()) = page.keyframe.recv() => session.request_keyframe().await.map_err(|e| crate::connect::explain(&e))?,
            Some((i, sent)) = page.input.recv() => {
                let down = matches!(i, Input::Key { state: KeyState::Down, .. });
                if down {
                    key_mark.get_or_insert(sent);
                }
                session.input(i).await.map_err(|e| crate::connect::explain(&e))?;
                if down {
                    info.lock().unwrap().key_send = key_send.add(sent.elapsed());
                }
            }
            Some(notice) = session.notice() => {
                let what = match notice.kind {
                    NoticeKind::InputUnavailable => "input unavailable",
                    NoticeKind::CaptureStopped => "capture stopped",
                    NoticeKind::Unknown => "host",
                };
                info.lock().unwrap().notice = Some(format!("{what}: {}", notice.message));
            }
            Ok(()) = switch.changed() => share(&mut clipboard, *switch.borrow_and_update(), info),
            event = next_copy(&mut clipboard), if clipboard.is_some() => {
                let mut info = info.lock().unwrap();
                let counts = &mut info.clipboard;
                match event {
                    Some(Event::Copied(text)) => {
                        session.send_clipboard(text);
                        counts.sent += 1;
                    }
                    Some(Event::Skipped) => counts.skipped += 1,
                    Some(Event::Failed) => counts.failed += 1,
                    None => {
                        clipboard = None;
                        (counts.on, counts.error) = (false, Some("the clipboard watcher stopped".into()));
                    }
                }
            }
            Some(text) = from_host.next() => {
                // Dropped while the switch is off.
                if let Some(c) = &clipboard {
                    c.apply(text);
                    info.lock().unwrap().clipboard.received += 1;
                }
            }
            _ = shutdown.notified() => return Ok(()),
        }
    }
}

/// Starts or stops watching this machine's clipboard. Starting sends what is on it now.
fn share(clipboard: &mut Option<Clipboard>, on: bool, info: &Mutex<LiveInfo>) {
    if on == clipboard.is_some() {
        return;
    }
    let counts = &mut info.lock().unwrap().clipboard;
    *clipboard = None;
    counts.error = None;
    if on {
        match Clipboard::start() {
            Ok(c) => *clipboard = Some(c),
            Err(e) => counts.error = Some(e),
        }
    }
    counts.on = clipboard.is_some();
}

async fn next_copy(clipboard: &mut Option<Clipboard>) -> Option<Event> {
    match clipboard {
        Some(c) => c.next().await,
        None => std::future::pending().await,
    }
}

#[tauri::command]
pub async fn live_next(live: tauri::State<'_, Live>) -> Result<Response, String> {
    let conn = live.conn().ok_or("not connected")?;
    let mut frames = conn.frames.lock().await;
    match frames.recv().await {
        Some(Ok((mut frame, mark))) => {
            if let Some(mark) = mark {
                let us = u32::try_from(mark.elapsed().as_micros()).unwrap_or(u32::MAX).max(1);
                frame[0] |= 2;
                frame[9..13].copy_from_slice(&us.to_le_bytes());
            }
            Ok(Response::new(frame))
        }
        Some(Err(e)) => {
            *live.ended.lock().unwrap() = Some(e.clone());
            Err(e)
        }
        None => Err("the connection ended".into()),
    }
}

/// The largest picture, in device pixels, the page can show 1:1, or
/// [`nocomputer_proto::VIEW_SIZE_NATIVE`] in both for native mode; the host then streams
/// at that size. The page sends it at the start and on each change.
#[tauri::command]
pub fn live_view_size(live: tauri::State<'_, Live>, width: u16, height: u16) {
    if let Some(c) = live.conn().filter(|_| width > 0 && height > 0) {
        c.view.send_replace(Some((width, height)));
    }
}

#[tauri::command]
pub fn live_request_keyframe(live: tauri::State<'_, Live>) {
    if let Some(c) = live.conn() {
        let _ = c.keyframe.send(());
    }
}

/// Pointer and wheel input from the page, in order. The page keeps one call in flight.
#[tauri::command]
pub fn live_input(live: tauri::State<'_, Live>, events: Vec<Input>) {
    for event in events {
        live.send_input(event);
    }
}

#[tauri::command]
pub fn live_settings(command_key: crate::keys::CommandKey) {
    crate::hook::set_command(command_key);
}

/// The status bar's clipboard switch. The page sets it before logging in.
#[tauri::command]
pub fn live_clipboard(on: bool) {
    crate::clipboard::set_enabled(on);
}

#[tauri::command]
pub fn live_info(live: tauri::State<'_, Live>) -> LiveInfo {
    let mut info = live.conn().map(|c| c.info.lock().unwrap().clone()).unwrap_or_default();
    let (state, keys_sent) = crate::hook::status();
    (info.keyboard_active, info.keyboard_off, info.keys_sent) = (state.is_ok(), state.err(), keys_sent);
    info
}

/// Why the last session ended, once; `None` when this PC ended it.
#[tauri::command]
pub fn live_ended(live: tauri::State<'_, Live>) -> Option<String> {
    live.ended.lock().unwrap().take()
}

/// Ends the session; the page then goes back to the start screen.
#[tauri::command]
pub async fn live_disconnect(live: tauri::State<'_, Live>) -> Result<(), String> {
    if let Some(closed) = live.stop() {
        let _ = tauri::async_runtime::spawn_blocking(move || closed.recv_timeout(Duration::from_secs(2))).await;
    }
    Ok(())
}
