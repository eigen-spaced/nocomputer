//! Video from host to viewer: one QUIC unidirectional stream per GOP.
//!
//! Each stream starts at a keyframe and carries one header, then frames:
//!
//! ```text
//! header: [u8 VIDEO_STREAM_VERSION][u8 codec_len][codec, UTF-8, e.g. "avc1.4D0028"]
//! frame:  [u32 BE len][u8 flags (bit0 keyframe)][u64 BE host_us][len bytes of Annex B]
//! ```
//!
//! Only the first frame of a stream is a keyframe. When the viewer falls behind, the host
//! resets the current stream and drops everything up to the next keyframe, which starts a
//! new stream; the viewer discards a stream's partial frame when it is reset or replaced,
//! so the decoder never sees a frame whose keyframe it lacks. `host_us` is the host's
//! monotonic clock: meaningful only as differences between frames, never against the
//! viewer's clock.

use crate::Error;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

pub const VIDEO_STREAM_VERSION: u8 = 1;
pub const MAX_ACCESS_UNIT: usize = 4 * 1024 * 1024;
/// Frames queued for the controller before the host gives up on the current GOP (or, while
/// a keyframe is being written, on the frames behind it).
pub(crate) const QUEUE_FRAMES: usize = 120;
/// Age of the oldest queued frame at which the host gives up on the current GOP, however
/// the path is doing: the most video a slow path may fall behind.
pub(crate) const QUEUE_AGE_US: u64 = 2_000_000;
/// Silence from the viewer, beyond one round trip, while frames are waiting, at which the
/// host treats the path as stalled: it drops what is queued and anything captured until the
/// viewer is heard again, then asks for a keyframe. Frame age alone can't tell a stall from
/// a slow or jittery relay, where a fresh keyframe after each pause costs more than it
/// saves (live test over DERP, 2026-10-16). Acknowledgements keep arriving on a slow path.
pub(crate) const STALL_US: u64 = 500_000;
/// Bytes the host may buffer beyond quinn's congestion window. Quinn's default send window
/// (about 1.25 MB per stream) let a stalled path absorb over a second of video that then
/// arrived as a burst. Tracking the congestion window keeps a full path at any round trip
/// while little is left buffered when a stall resets the GOP.
pub(crate) const SEND_MARGIN: u64 = 256 * 1024;
const MAX_SEND_WINDOW: u64 = 4 * 1024 * 1024;
/// Stream reset code when the host abandons a GOP.
pub(crate) const RESET_GOP: u32 = 1;
/// Stop code when the viewer abandons a GOP superseded by a newer one.
const STOP_SUPERSEDED: u32 = 1;
const FLAG_KEYFRAME: u8 = 1;

pub(crate) fn host_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// One encoded picture from the capture side.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// The WebCodecs codec string, present exactly when this is a keyframe.
    codec: Option<String>,
    data: Arc<[u8]>,
}

impl AccessUnit {
    /// An IDR. `data` must carry the SPS and PPS in-band.
    pub fn keyframe(codec: impl Into<String>, data: impl Into<Arc<[u8]>>) -> Self {
        AccessUnit { codec: Some(codec.into()), data: data.into() }
    }

    pub fn delta(data: impl Into<Arc<[u8]>>) -> Self {
        AccessUnit { codec: None, data: data.into() }
    }

    pub fn is_keyframe(&self) -> bool {
        self.codec.is_some()
    }
}

/// A frame as the viewer receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub codec: Arc<str>,
    pub keyframe: bool,
    pub host_us: u64,
    pub data: Vec<u8>,
}

enum Item {
    Frame(AccessUnit, u64),
    Reset,
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<Item>,
    /// Dropping deltas until the next keyframe.
    gap: bool,
    /// The send loop is writing a keyframe. Abandoning it would discard it and ask for
    /// another just as large, which on a slow link never finishes.
    keyframe_in_flight: bool,
    /// The send loop is writing a frame.
    busy: bool,
    /// The path is stalled: frames are dropped as they arrive.
    stalled: bool,
    /// Ask for a keyframe at the next push: frames were dropped for a stall.
    want_keyframe: bool,
    closed: bool,
}

impl QueueState {
    fn overflowing(&self) -> bool {
        let mut frames = self.items.iter().filter_map(|i| match i {
            Item::Frame(_, at) => Some(*at),
            Item::Reset => None,
        });
        let Some(oldest) = frames.next() else { return false };
        1 + frames.count() >= QUEUE_FRAMES || host_us().saturating_sub(oldest) > QUEUE_AGE_US
    }

}

/// Frames waiting to be sent to one controlling session.
pub(crate) struct VideoQueue {
    state: Mutex<QueueState>,
    ready: Notify,
    reset: Notify,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Pushed {
    Queued,
    /// Dropped; a keyframe was already requested.
    Dropped,
    /// Dropped or overflowed; the caller must request a keyframe.
    NeedKeyframe,
}

impl VideoQueue {
    /// Starts in a gap: nothing is sent until the first keyframe.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(VideoQueue { state: Mutex::new(QueueState { gap: true, ..Default::default() }), ready: Notify::new(), reset: Notify::new() })
    }

    pub(crate) fn push(&self, au: AccessUnit) -> Pushed {
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return Pushed::Dropped;
        }
        let keyframe = au.is_keyframe();
        if s.stalled {
            return Pushed::Dropped;
        }
        if s.gap && !keyframe {
            return match std::mem::take(&mut s.want_keyframe) {
                true => Pushed::NeedKeyframe,
                false => Pushed::Dropped,
            };
        }
        if s.overflowing() {
            // While a keyframe is being written, keep it and drop only what is behind it.
            if s.keyframe_in_flight {
                s.items.clear();
            } else {
                s.items.clear();
                s.items.push_back(Item::Reset);
                self.reset.notify_one();
            }
            if !keyframe {
                s.gap = true;
                self.ready.notify_one();
                return Pushed::NeedKeyframe;
            }
        }
        if keyframe {
            s.gap = false;
            s.want_keyframe = false;
        }
        s.items.push_back(Item::Frame(au, host_us()));
        self.ready.notify_one();
        Pushed::Queued
    }

    /// Enters or leaves a stall. Returns true when leaving one, when the caller should ask
    /// for a keyframe in case nothing new is captured.
    fn stalled(&self, on: bool) -> bool {
        let mut s = self.state.lock().unwrap();
        let was = std::mem::replace(&mut s.stalled, on);
        if on && !was {
            // Resetting the GOP also discards what quinn still buffers for it.
            s.items.clear();
            s.items.push_back(Item::Reset);
            s.gap = true;
            s.want_keyframe = true;
            self.reset.notify_one();
            self.ready.notify_one();
        }
        was && !on
    }

    /// Frames are waiting or being written, so the viewer should be acknowledging them.
    fn sending(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.busy || s.items.iter().any(|i| matches!(i, Item::Frame(..)))
    }

    fn writing(&self, on: bool, keyframe: bool) {
        let mut s = self.state.lock().unwrap();
        s.busy = on;
        s.keyframe_in_flight = on && keyframe;
    }

    pub(crate) fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_one();
        self.reset.notify_one();
    }

    async fn pop(&self) -> Option<Item> {
        loop {
            {
                let mut s = self.state.lock().unwrap();
                if s.closed {
                    return None;
                }
                if let Some(item) = s.items.pop_front() {
                    return Some(item);
                }
            }
            self.ready.notified().await;
        }
    }
}

pub(crate) async fn write_gop_header(stream: &mut quinn::SendStream, codec: &str) -> Result<(), Error> {
    let codec = codec.as_bytes();
    let len = u8::try_from(codec.len()).map_err(|_| Error::Protocol("codec string too long".into()))?;
    let mut header = vec![VIDEO_STREAM_VERSION, len];
    header.extend_from_slice(codec);
    stream.write_all(&header).await.map_err(|e| Error::Protocol(format!("video write: {e}")))
}

pub(crate) async fn write_frame(stream: &mut quinn::SendStream, au: &AccessUnit, at: u64) -> Result<(), Error> {
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(au.data.len() as u32).to_be_bytes());
    header.push(if au.is_keyframe() { FLAG_KEYFRAME } else { 0 });
    header.extend_from_slice(&at.to_be_bytes());
    stream.write_all(&header).await.map_err(|e| Error::Protocol(format!("video write: {e}")))?;
    stream.write_all(&au.data).await.map_err(|e| Error::Protocol(format!("video write: {e}")))
}

/// Sends queued frames to the viewer until the queue is closed or the connection ends.
/// `keyframe_needed` is called after a stall, in case the screen doesn't change again.
pub(crate) async fn send_loop(conn: quinn::Connection, queue: Arc<VideoQueue>, keyframe_needed: impl Fn() + Send + 'static) -> Result<(), Error> {
    let watch = tokio::spawn(watch_for_stalls(conn.clone(), queue.clone(), keyframe_needed));
    let result = send_frames(conn, queue).await;
    watch.abort();
    result
}

/// Marks the path stalled when the viewer has been silent for [`STALL_US`] beyond a round
/// trip while frames are waiting, and recovered when it is heard again.
async fn watch_for_stalls(conn: quinn::Connection, queue: Arc<VideoQueue>, keyframe_needed: impl Fn()) {
    let mut heard = (conn.stats().udp_rx.datagrams, Instant::now());
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    loop {
        tick.tick().await;
        let stats = conn.stats();
        if stats.udp_rx.datagrams != heard.0 {
            heard = (stats.udp_rx.datagrams, Instant::now());
            if queue.stalled(false) {
                keyframe_needed();
            }
        } else if !queue.sending() {
            // Nothing to acknowledge: silence means nothing.
            heard.1 = Instant::now();
        } else if heard.1.elapsed() > std::time::Duration::from_micros(STALL_US) + stats.path.rtt {
            queue.stalled(true);
        }
    }
}

async fn send_frames(conn: quinn::Connection, queue: Arc<VideoQueue>) -> Result<(), Error> {
    let mut current: Option<quinn::SendStream> = None;
    while let Some(item) = queue.pop().await {
        let (au, at) = match item {
            Item::Reset => {
                if let Some(mut s) = current.take() {
                    let _ = s.reset(RESET_GOP.into());
                }
                continue;
            }
            Item::Frame(au, at) => (au, at),
        };
        conn.set_send_window((conn.stats().path.cwnd + SEND_MARGIN).min(MAX_SEND_WINDOW));
        queue.writing(true, au.is_keyframe());
        let write = async {
            if let Some(codec) = &au.codec {
                if let Some(mut s) = current.take() {
                    let _ = s.finish();
                }
                let mut s = conn.open_uni().await?;
                write_gop_header(&mut s, codec).await?;
                current = Some(s);
            }
            match current.as_mut() {
                Some(s) => write_frame(s, &au, at).await,
                None => Ok(()), // a delta with no open GOP: never queued, but harmless
            }
        };
        // A write blocked on flow control is abandoned when the queue overflows; the
        // queued Reset then discards the stream.
        let written = tokio::select! {
            r = write => r,
            _ = queue.reset.notified() => Ok(()),
        };
        queue.writing(false, false);
        written?;
    }
    if let Some(mut s) = current.take() {
        let _ = s.finish();
    }
    Ok(())
}

/// Receives the host's video on a session. Yields only frames whose GOP started with a
/// keyframe the caller has received. A newer GOP that has already arrived supersedes the
/// unread frames of the current one, so a viewer that fell behind jumps to the newest
/// keyframe instead of decoding stale frames.
pub struct VideoReceiver {
    conn: quinn::Connection,
    current: Option<(quinn::RecvStream, Arc<str>, bool)>,
}

async fn read_gop_header(stream: &mut quinn::RecvStream) -> Result<Arc<str>, Error> {
    let mut head = [0u8; 2];
    AsyncReadExt::read_exact(stream, &mut head).await?;
    if head[0] != VIDEO_STREAM_VERSION {
        return Err(Error::Protocol(format!("video stream version {} is not supported", head[0])));
    }
    let mut codec = vec![0; head[1] as usize];
    AsyncReadExt::read_exact(stream, &mut codec).await?;
    let codec = String::from_utf8(codec).map_err(|_| Error::Protocol("codec string is not UTF-8".into()))?;
    Ok(codec.into())
}

/// `Ok(None)` when the stream finished cleanly between frames.
async fn read_frame(stream: &mut quinn::RecvStream, codec: &Arc<str>) -> Result<Option<VideoFrame>, Error> {
    let mut head = [0u8; 13];
    if stream.read(&mut head[..1]).await.map_err(std::io::Error::from)?.is_none() {
        return Ok(None);
    }
    AsyncReadExt::read_exact(stream, &mut head[1..]).await?;
    let len = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
    if len == 0 || len > MAX_ACCESS_UNIT {
        return Err(Error::Protocol(format!("video frame of {len} bytes")));
    }
    if head[4] & !FLAG_KEYFRAME != 0 {
        return Err(Error::Protocol("unknown video frame flags".into()));
    }
    let host_us = u64::from_be_bytes(head[5..13].try_into().unwrap());
    let mut data = vec![0; len];
    AsyncReadExt::read_exact(stream, &mut data).await?;
    Ok(Some(VideoFrame { codec: codec.clone(), keyframe: head[4] & FLAG_KEYFRAME != 0, host_us, data }))
}

impl VideoReceiver {
    pub(crate) fn new(conn: quinn::Connection) -> Self {
        VideoReceiver { conn, current: None }
    }

    async fn accept(&mut self) -> Result<(), Error> {
        let mut stream = self.conn.accept_uni().await.map_err(|e| crate::viewer::explain_close(&self.conn, e.into()))?;
        match read_gop_header(&mut stream).await {
            Ok(codec) => self.current = Some((stream, codec, false)),
            Err(_) => {
                let _ = stream.stop(STOP_SUPERSEDED.into());
            }
        }
        Ok(())
    }

    /// The next decodable frame. Fails only when the connection ends. Not cancel-safe:
    /// a call dropped mid-frame loses the rest of that GOP, so don't race it in a
    /// `select!`; read from a task of its own.
    pub async fn next(&mut self) -> Result<VideoFrame, Error> {
        enum Next {
            Newer(Result<quinn::RecvStream, quinn::ConnectionError>),
            Frame(Result<Option<VideoFrame>, Error>),
        }
        loop {
            let Some((stream, codec, _)) = self.current.as_mut() else {
                self.accept().await?;
                continue;
            };
            let codec = codec.clone();
            let next = tokio::select! {
                biased;
                newer = self.conn.accept_uni() => Next::Newer(newer),
                frame = read_frame(stream, &codec) => Next::Frame(frame),
            };
            match next {
                // A newer GOP supersedes this one, including any partial frame of it.
                Next::Newer(newer) => {
                    self.abandon();
                    let mut stream = newer.map_err(|e| crate::viewer::explain_close(&self.conn, e.into()))?;
                    match read_gop_header(&mut stream).await {
                        Ok(codec) => self.current = Some((stream, codec, false)),
                        Err(_) => {
                            let _ = stream.stop(STOP_SUPERSEDED.into());
                        }
                    }
                }
                Next::Frame(Ok(Some(frame))) => {
                    let started = &mut self.current.as_mut().expect("current stream").2;
                    if *started || frame.keyframe {
                        *started = true;
                        return Ok(frame);
                    }
                    // A GOP must start at a keyframe; ignore the rest of this one.
                    self.abandon();
                }
                // Finished, reset or malformed: wait for the next GOP.
                Next::Frame(_) => self.abandon(),
            }
        }
    }

    fn abandon(&mut self) {
        if let Some((mut old, ..)) = self.current.take() {
            let _ = old.stop(STOP_SUPERSEDED.into());
        }
    }
}
