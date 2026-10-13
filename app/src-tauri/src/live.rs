//! --connect HOST: live video from a paired host.
//!
//! The host must already be paired and pinned (with `nocomputer-pair pair HOST`; this
//! viewer uses the same identity and known-hosts file). Frames are pulled by the page:
//! `live_next` waits for the next frame and returns it as raw bytes,
//!
//!   [u8 flags (bit0 keyframe)][u64 LE host_us][u8 codec_len][codec][Annex B access unit]
//!
//! `host_us` is the host's monotonic clock: usable as decoder timestamps and for
//! differences between frames, never against this machine's clock. If the page falls
//! behind, frames are dropped up to the next keyframe and one is requested, so the
//! decoder never gets a frame whose keyframe it lacks.

use nocomputer_proto::{load_or_create_identity, paths, platform_keystore, Error, KnownHosts, Outcome, Viewer};
use serde::Serialize;
use std::sync::{Arc, Mutex};
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
}

type Frames = mpsc::Receiver<Result<Vec<u8>, String>>;

pub struct Live {
    address: String,
    info: Arc<Mutex<LiveInfo>>,
    /// Created by the first `live_next`, so nothing is dropped while the page loads.
    frames: tokio::sync::Mutex<Option<Frames>>,
    keyframe: Mutex<Option<mpsc::UnboundedSender<()>>>,
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
    let mut out = Vec::with_capacity(10 + codec.len() + frame.data.len());
    out.push(frame.keyframe as u8);
    out.extend_from_slice(&frame.host_us.to_le_bytes());
    out.push(codec.len() as u8);
    out.extend_from_slice(codec);
    out.extend_from_slice(&frame.data);
    out
}

fn describe(e: Error) -> String {
    match e {
        Error::Remote { code, message } => format!("{code:?}: {message}"),
        other => other.to_string(),
    }
}

impl Live {
    pub fn new(address: &str) -> Self {
        let address = paths::with_default_port(address);
        let info = Arc::new(Mutex::new(LiveInfo { address: address.clone(), ..Default::default() }));
        Live { address, info, frames: Default::default(), keyframe: Default::default(), shutdown: Default::default(), closed: Default::default() }
    }

    /// Connects in the background; failures reach the page through `live_next`.
    fn start(&self) -> Frames {
        let (tx, rx) = mpsc::channel(QUEUE_FRAMES);
        let (keyframe_tx, keyframe_rx) = mpsc::unbounded_channel();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        *self.keyframe.lock().unwrap() = Some(keyframe_tx);
        *self.closed.lock().unwrap() = Some(closed_rx);
        let (address, info, shutdown) = (self.address.clone(), self.info.clone(), self.shutdown.clone());
        tauri::async_runtime::spawn(async move {
            if let Err(e) = run(address, info, tx.clone(), keyframe_rx, shutdown).await {
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

async fn run(
    address: String,
    info: Arc<Mutex<LiveInfo>>,
    tx: mpsc::Sender<Result<Vec<u8>, String>>,
    mut keyframe: mpsc::UnboundedReceiver<()>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(), String> {
    let state = paths::default_state_dir();
    let pinned = KnownHosts::at(state.join("known-hosts.json"))
        .get(&address)
        .map_err(describe)?
        .ok_or_else(|| format!("{address} is not paired; pair first with: nocomputer-pair pair {address}"))?;
    let identity = platform_keystore(&state, "device").and_then(|s| load_or_create_identity(s.as_ref())).map_err(describe)?;
    let viewer = Viewer::new(identity, device_name()).map_err(describe)?;
    let addr = paths::resolve(&address).await.map_err(|e| e.to_string())?;
    let Outcome::Ready(mut session) = viewer.connect(addr, Some(pinned.fingerprint)).await.map_err(describe)? else {
        return Err("unexpected first contact for a pinned host".into());
    };
    {
        let mut info = info.lock().unwrap();
        info.host_name = Some(session.host_name.clone());
        info.host_fingerprint = Some(session.host_fingerprint.to_string());
    }
    let mut video = session.video();
    let mut gap = false;
    loop {
        tokio::select! {
            frame = video.next() => {
                let frame = frame.map_err(describe)?;
                if gap && !frame.keyframe {
                    continue;
                }
                gap = false;
                match tx.try_send(Ok(encode(&frame))) {
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
            _ = shutdown.notified() => {
                session.close().await;
                return Ok(());
            }
        }
    }
}

#[tauri::command]
pub async fn live_next(live: tauri::State<'_, Live>) -> Result<Response, String> {
    let mut frames = live.frames.lock().await;
    let frames = frames.get_or_insert_with(|| live.start());
    match frames.recv().await {
        Some(Ok(frame)) => Ok(Response::new(frame)),
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

#[tauri::command]
pub fn live_info(live: tauri::State<'_, Live>) -> LiveInfo {
    live.info.lock().unwrap().clone()
}
