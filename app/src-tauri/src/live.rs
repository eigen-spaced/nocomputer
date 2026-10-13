//! --connect HOST: live video from a paired host.
//!
//! The host must already be paired and pinned (with `nocomputer-pair pair HOST`; this
//! viewer uses the same identity and known-hosts file). Frames are pulled by the page:
//! `live_next` waits for the next frame and returns it as raw bytes,
//!
//!   [u8 flags (bit0 keyframe, bit1 key_us set)][u64 LE host_us][u32 LE key_us]
//!   [u8 codec_len][codec][Annex B access unit]
//!
//! `host_us` is the host's monotonic clock: usable as decoder timestamps and for
//! differences between frames, never against this machine's clock. `key_us` marks the
//! first frame to arrive after a key went down: the time from reading that key-down from
//! the hook to handing this frame to the page, on this machine's clock (0 otherwise). If the page falls
//! behind, frames are dropped up to the next keyframe and one is requested, so the
//! decoder never gets a frame whose keyframe it lacks.
//!
//! Input goes the other way: the page sends pointer and wheel input with `live_input`,
//! and on Windows the keyboard hook (`hook.rs`) sends keys. Neither is logged. Notices
//! from the host (input unavailable, capture stopped) appear in `live_info`.

use nocomputer_proto::{load_or_create_identity, paths, platform_keystore, Error, Input, KeyState, KnownHosts, NoticeKind, Outcome, Viewer};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::Response;
use tokio::sync::mpsc;

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

pub struct Live {
    address: String,
    via: Option<String>,
    info: Arc<Mutex<LiveInfo>>,
    /// Created by the first `live_next`, so nothing is dropped while the page loads.
    frames: tokio::sync::Mutex<Option<Frames>>,
    keyframe: Mutex<Option<mpsc::UnboundedSender<()>>>,
    /// With when each was sent, for timing.
    input: Mutex<Option<mpsc::UnboundedSender<(Input, Instant)>>>,
    /// A session is up, so input has somewhere to go.
    connected: Arc<AtomicBool>,
    /// Ends the session; the task then reports on `closed`.
    shutdown: Arc<tokio::sync::Notify>,
    closed: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

fn device_name() -> String {
    let raw = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "nocomputer viewer".into());
    raw.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string()
}

fn encode(frame: &nocomputer_proto::VideoFrame) -> Vec<u8> {
    let codec = frame.codec.as_bytes();
    let mut out = Vec::with_capacity(14 + codec.len() + frame.data.len());
    out.push(frame.keyframe as u8);
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

fn describe(e: Error) -> String {
    match e {
        Error::Remote { code, message } => format!("{code:?}: {message}"),
        other => other.to_string(),
    }
}

impl Live {
    /// `via`, if given, is dialed instead of `address`; the pin still comes from `address`.
    pub fn new(address: &str, via: Option<&str>) -> Self {
        let address = paths::with_default_port(address);
        let via = via.map(paths::with_default_port);
        let shown = match &via {
            Some(via) => format!("{address} via {via}"),
            None => address.clone(),
        };
        let info = Arc::new(Mutex::new(LiveInfo { address: shown, ..Default::default() }));
        Live {
            address,
            via,
            info,
            frames: Default::default(),
            keyframe: Default::default(),
            input: Default::default(),
            connected: Default::default(),
            shutdown: Default::default(),
            closed: Default::default(),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Queues input for the host, in order; dropped when no session is up.
    pub fn send_input(&self, input: Input) {
        if self.is_connected() {
            if let Some(tx) = self.input.lock().unwrap().as_ref() {
                let _ = tx.send((input, Instant::now()));
            }
        }
    }

    /// Connects in the background; failures reach the page through `live_next`.
    fn start(&self) -> Frames {
        let (tx, rx) = mpsc::channel(QUEUE_FRAMES);
        let (keyframe_tx, keyframe_rx) = mpsc::unbounded_channel();
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        *self.input.lock().unwrap() = Some(input_tx);
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        *self.keyframe.lock().unwrap() = Some(keyframe_tx);
        *self.closed.lock().unwrap() = Some(closed_rx);
        let address = (self.address.clone(), self.via.clone().unwrap_or_else(|| self.address.clone()));
        let (info, shutdown, connected) = (self.info.clone(), self.shutdown.clone(), self.connected.clone());
        tauri::async_runtime::spawn(async move {
            let result = run(address, info, tx.clone(), keyframe_rx, input_rx, connected.clone(), shutdown).await;
            connected.store(false, Ordering::Relaxed);
            if let Err(e) = result {
                let _ = tx.send(Err(e)).await;
            }
            let _ = closed_tx.send(());
        });
        rx
    }

    /// Ends the session so the host frees its slot at once rather than at the idle
    /// timeout. Blocks for at most `wait`.
    pub fn close(&self, wait: std::time::Duration) {
        self.shutdown.notify_one();
        if let Some(closed) = self.closed.lock().unwrap().take() {
            let _ = closed.recv_timeout(wait);
        }
    }
}

/// `address` is (where the pin is stored, where to dial).
async fn run(
    (address, dial): (String, String),
    info: Arc<Mutex<LiveInfo>>,
    tx: mpsc::Sender<Result<Frame, String>>,
    mut keyframe: mpsc::UnboundedReceiver<()>,
    mut input: mpsc::UnboundedReceiver<(Input, Instant)>,
    connected: Arc<AtomicBool>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(), String> {
    let state = paths::default_state_dir();
    let pinned = KnownHosts::at(state.join("known-hosts.json"))
        .get(&address)
        .map_err(describe)?
        .ok_or_else(|| format!("{address} is not paired; pair first with: nocomputer-pair pair {address}"))?;
    let identity = platform_keystore(&state, "device").and_then(|s| load_or_create_identity(s.as_ref())).map_err(describe)?;
    let viewer = Viewer::new(identity, device_name()).map_err(describe)?;
    let addr = paths::resolve(&dial).await.map_err(|e| e.to_string())?;
    let Outcome::Ready(mut session) = viewer.connect(addr, Some(pinned.fingerprint)).await.map_err(describe)? else {
        return Err("unexpected first contact for a pinned host".into());
    };
    {
        let mut info = info.lock().unwrap();
        info.host_name = Some(session.host_name.clone());
        info.host_fingerprint = Some(session.host_fingerprint.to_string());
    }
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
    while input.try_recv().is_ok() {} // nothing from before the session
    connected.store(true, Ordering::Relaxed);
    crate::hook::set_connected(true);
    let result = session_loop(&mut session, &mut video, &info, &tx, &mut keyframe, &mut input, &shutdown).await;
    reader.abort();
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
    keyframe: &mut mpsc::UnboundedReceiver<()>,
    input: &mut mpsc::UnboundedReceiver<(Input, Instant)>,
    shutdown: &tokio::sync::Notify,
) -> Result<(), String> {
    let mut gap = false;
    // When a key went down with no frame received since.
    let mut key_mark: Option<Instant> = None;
    let (mut key_send, mut key_frame) = (Samples::default(), Samples::default());
    loop {
        tokio::select! {
            Some(frame) = video.recv() => {
                let frame = frame.map_err(describe)?;
                let mark = key_mark.take();
                if let Some(mark) = mark {
                    info.lock().unwrap().key_frame = key_frame.add(mark.elapsed());
                }
                if gap && !frame.keyframe {
                    continue;
                }
                gap = false;
                match tx.try_send(Ok((encode(&frame), mark))) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // The page is behind: drop up to the next keyframe.
                        gap = true;
                        info.lock().unwrap().dropped += 1;
                        session.request_keyframe().await.map_err(describe)?;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                }
            }
            Some(()) = keyframe.recv() => session.request_keyframe().await.map_err(describe)?,
            Some((i, sent)) = input.recv() => {
                let down = matches!(i, Input::Key { state: KeyState::Down, .. });
                if down {
                    key_mark.get_or_insert(sent);
                }
                session.input(i).await.map_err(describe)?;
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
            _ = shutdown.notified() => return Ok(()),
        }
    }
}

#[tauri::command]
pub async fn live_next(live: tauri::State<'_, Live>) -> Result<Response, String> {
    let mut frames = live.frames.lock().await;
    let frames = frames.get_or_insert_with(|| live.start());
    match frames.recv().await {
        Some(Ok((mut frame, mark))) => {
            if let Some(mark) = mark {
                let us = u32::try_from(mark.elapsed().as_micros()).unwrap_or(u32::MAX).max(1);
                frame[0] |= 2;
                frame[9..13].copy_from_slice(&us.to_le_bytes());
            }
            Ok(Response::new(frame))
        }
        Some(Err(e)) => Err(e),
        None => Err("the connection ended".into()),
    }
}

#[tauri::command]
pub fn live_request_keyframe(live: tauri::State<'_, Live>) {
    if let Some(k) = live.keyframe.lock().unwrap().as_ref() {
        let _ = k.send(());
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

#[tauri::command]
pub fn live_info(live: tauri::State<'_, Live>) -> LiveInfo {
    let mut info = live.info.lock().unwrap().clone();
    let (state, keys_sent) = crate::hook::status();
    (info.keyboard_active, info.keyboard_off, info.keys_sent) = (state.is_ok(), state.err(), keys_sent);
    info
}
