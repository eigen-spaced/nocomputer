//! local-v1: the pipe contract between the capture app (parent) and this host process
//! (child), over the child's stdin and stdout.
//!
//! ```text
//! frame: [u32 BE len][u8 type][payload]    len = 1 + payload length, 1..=4 MiB
//!
//! app -> host
//!   0x01 Hello            u16 BE version (= 1); must be the first frame
//!   0x02 AccessUnit       u8 flags (bit0 keyframe, others 0), u64 BE capture_us,
//!                         Annex B bytes (at least 1); a keyframe is an IDR with SPS+PPS
//!   0x03 CaptureStopped   UTF-8 reason, at most 1024 bytes
//!   0x04 PairingDecision  u64 BE request_id, u8 approve (0 or 1); exactly 9 bytes
//! host -> app
//!   0x81 HelloAck         u16 BE version
//!   0x82 StartStream      empty: a controlling session began; start capture, IDR first
//!   0x83 StopStream       empty: no controlling session remains
//!   0x84 RequestKeyframe  empty
//!   0x85 Event            UTF-8 JSON object tagged by "type"; unknown types are ignored
//! ```
//!
//! Anything else is fatal: lengths out of range (checked before allocating), unknown
//! frame types, a payload on an empty type, wrong fixed sizes, invalid UTF-8. The
//! payloads of `testdata/local-v1/*.bin` are the reference encoding for both sides.
//! `capture_us` is the app's monotonic clock and only meaningful within the app.

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const VERSION: u16 = 1;
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub const MAX_REASON: usize = 1024;

const HELLO: u8 = 0x01;
const ACCESS_UNIT: u8 = 0x02;
const CAPTURE_STOPPED: u8 = 0x03;
const PAIRING_DECISION: u8 = 0x04;
const HELLO_ACK: u8 = 0x81;
const START_STREAM: u8 = 0x82;
const STOP_STREAM: u8 = 0x83;
const REQUEST_KEYFRAME: u8 = 0x84;
const EVENT: u8 = 0x85;
const FLAG_KEYFRAME: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromApp {
    Hello { version: u16 },
    AccessUnit { keyframe: bool, capture_us: u64, data: Vec<u8> },
    CaptureStopped { reason: String },
    PairingDecision { request_id: u64, approve: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToApp {
    HelloAck { version: u16 },
    StartStream,
    StopStream,
    RequestKeyframe,
    Event(Event),
}

/// Display-only notices for the app. Fingerprints are full, grouped hex strings.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Answer with `PairingDecision` carrying the same `request_id`. A newer prompt
    /// replaces an older one, and decisions for any other id are ignored.
    PairingPrompt { request_id: u64, device_name: String, device_fingerprint: String, host_fingerprint: String },
    /// `name` is the one approved at pairing.
    Session { name: String, device_fingerprint: String },
    SessionEnded { device_fingerprint: String },
    Paired { name: String, device_fingerprint: String },
    PairingDenied { device_fingerprint: String },
    Rejected { device_fingerprint: Option<String>, reason: String },
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// Clean end of input between frames.
    Eof,
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "local pipe: {e}"),
            Error::Eof => write!(f, "local pipe closed"),
            Error::Protocol(m) => write!(f, "local-v1: {m}"),
        }
    }
}

impl std::error::Error for Error {}

fn bad(message: impl Into<String>) -> Error {
    Error::Protocol(message.into())
}

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&(1 + payload.len() as u32).to_be_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

impl ToApp {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToApp::HelloAck { version } => frame(HELLO_ACK, &version.to_be_bytes()),
            ToApp::StartStream => frame(START_STREAM, &[]),
            ToApp::StopStream => frame(STOP_STREAM, &[]),
            ToApp::RequestKeyframe => frame(REQUEST_KEYFRAME, &[]),
            ToApp::Event(event) => frame(EVENT, &serde_json::to_vec(event).expect("events serialize")),
        }
    }
}

impl FromApp {
    /// The app's side of the contract; here for tests and the golden vectors.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            FromApp::Hello { version } => frame(HELLO, &version.to_be_bytes()),
            FromApp::AccessUnit { keyframe, capture_us, data } => {
                let mut payload = vec![if *keyframe { FLAG_KEYFRAME } else { 0 }];
                payload.extend_from_slice(&capture_us.to_be_bytes());
                payload.extend_from_slice(data);
                frame(ACCESS_UNIT, &payload)
            }
            FromApp::CaptureStopped { reason } => frame(CAPTURE_STOPPED, reason.as_bytes()),
            FromApp::PairingDecision { request_id, approve } => {
                let mut payload = request_id.to_be_bytes().to_vec();
                payload.push(*approve as u8);
                frame(PAIRING_DECISION, &payload)
            }
        }
    }

    fn decode(kind: u8, payload: Vec<u8>) -> Result<Self, Error> {
        match kind {
            HELLO => {
                let version: [u8; 2] = payload.as_slice().try_into().map_err(|_| bad("Hello must be 2 bytes"))?;
                Ok(FromApp::Hello { version: u16::from_be_bytes(version) })
            }
            ACCESS_UNIT => {
                if payload.len() < 10 {
                    return Err(bad("AccessUnit needs flags, capture_us and at least one byte"));
                }
                if payload[0] & !FLAG_KEYFRAME != 0 {
                    return Err(bad("AccessUnit has unknown flags"));
                }
                let capture_us = u64::from_be_bytes(payload[1..9].try_into().unwrap());
                Ok(FromApp::AccessUnit { keyframe: payload[0] & FLAG_KEYFRAME != 0, capture_us, data: payload[9..].to_vec() })
            }
            CAPTURE_STOPPED => {
                if payload.len() > MAX_REASON {
                    return Err(bad("CaptureStopped reason exceeds 1024 bytes"));
                }
                let reason = String::from_utf8(payload).map_err(|_| bad("CaptureStopped reason is not UTF-8"))?;
                Ok(FromApp::CaptureStopped { reason })
            }
            PAIRING_DECISION => {
                if payload.len() != 9 {
                    return Err(bad("PairingDecision must be 9 bytes"));
                }
                let approve = match payload[8] {
                    0 => false,
                    1 => true,
                    _ => return Err(bad("PairingDecision approve must be 0 or 1")),
                };
                Ok(FromApp::PairingDecision { request_id: u64::from_be_bytes(payload[..8].try_into().unwrap()), approve })
            }
            other => Err(bad(format!("unknown frame type 0x{other:02x}"))),
        }
    }

    /// Decodes one complete frame from a buffer; for tests and tools.
    pub fn read_sync(bytes: &[u8]) -> Self {
        let body = &bytes[4..];
        Self::decode(body[0], body[1..].to_vec()).expect("a valid frame")
    }

    /// Reads one frame. [`Error::Eof`] only at a frame boundary; EOF inside a frame is
    /// an I/O error.
    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self, Error> {
        let mut len = [0u8; 4];
        match r.read(&mut len[..1]).await.map_err(Error::Io)? {
            0 => return Err(Error::Eof),
            _ => r.read_exact(&mut len[1..]).await.map_err(Error::Io)?,
        };
        let len = u32::from_be_bytes(len) as usize;
        // Checked before allocating.
        if len == 0 || len > MAX_FRAME {
            return Err(bad(format!("frame length {len} is out of range")));
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).await.map_err(Error::Io)?;
        let kind = body.remove(0);
        Self::decode(kind, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS_PPS_IDR: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x4D, 0x00, 0x28, 0xAA, // SPS
        0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80, // PPS
        0, 0, 1, 0x65, 0x88, 0x84, 0x00, // IDR slice
    ];
    const DELTA: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A, 0x02, 0x10];

    fn from_app() -> Vec<(&'static str, FromApp)> {
        vec![
            ("01-hello", FromApp::Hello { version: 1 }),
            ("02-access-unit-key", FromApp::AccessUnit { keyframe: true, capture_us: 0x0102030405060708, data: SPS_PPS_IDR.to_vec() }),
            ("02-access-unit-delta", FromApp::AccessUnit { keyframe: false, capture_us: 0x0102030405064E20, data: DELTA.to_vec() }),
            ("03-capture-stopped", FromApp::CaptureStopped { reason: "display disconnected – ü".into() }),
            ("04-pairing-decision-approve", FromApp::PairingDecision { request_id: 0x0A0B0C0D0E0F1011, approve: true }),
            ("04-pairing-decision-deny", FromApp::PairingDecision { request_id: 7, approve: false }),
        ]
    }

    fn to_app() -> Vec<(&'static str, ToApp)> {
        vec![
            ("81-hello-ack", ToApp::HelloAck { version: 1 }),
            ("82-start-stream", ToApp::StartStream),
            ("83-stop-stream", ToApp::StopStream),
            ("84-request-keyframe", ToApp::RequestKeyframe),
            (
                "85-event-pairing-prompt",
                ToApp::Event(Event::PairingPrompt {
                    request_id: 7,
                    device_name: "latitude".into(),
                    device_fingerprint: "1111 2222 3333 4444 5555 6666 7777 8888 9999 AAAA BBBB CCCC DDDD EEEE FFFF 0000".into(),
                    host_fingerprint: "0102 0304 0506 0708 090A 0B0C 0D0E 0F10 1112 1314 1516 1718 191A 1B1C 1D1E 1F20".into(),
                }),
            ),
            ("85-event-session", ToApp::Event(Event::Session { name: "latitude".into(), device_fingerprint: "1111 2222 3333 4444 5555 6666 7777 8888 9999 AAAA BBBB CCCC DDDD EEEE FFFF 0000".into() })),
        ]
    }

    fn golden(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/local-v1").join(format!("{name}.bin"))
    }

    /// Compares against the checked-in vectors; NOCOMPUTER_WRITE_GOLDEN=1 rewrites them.
    fn check(name: &str, bytes: &[u8]) {
        if std::env::var_os("NOCOMPUTER_WRITE_GOLDEN").is_some() {
            std::fs::write(golden(name), bytes).unwrap();
        }
        let expected = std::fs::read(golden(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(bytes, expected.as_slice(), "{name} differs from its golden vector");
    }

    #[tokio::test]
    async fn golden_vectors_round_trip() {
        for (name, message) in from_app() {
            let bytes = message.encode();
            check(name, &bytes);
            assert_eq!(FromApp::read(&mut bytes.as_slice()).await.unwrap(), message, "{name}");
        }
        for (name, message) in to_app() {
            check(name, &message.encode());
        }
    }

    async fn read(bytes: &[u8]) -> Result<FromApp, Error> {
        FromApp::read(&mut &bytes[..]).await
    }

    #[tokio::test]
    async fn malformed_frames_are_fatal() {
        assert!(matches!(read(&[]).await, Err(Error::Eof)));
        assert!(matches!(read(&[0, 0, 0]).await, Err(Error::Io(_))), "EOF inside a frame");
        assert!(matches!(read(&[0, 0, 0, 0]).await, Err(Error::Protocol(_))), "zero length");
        assert!(matches!(read(&[0, 0x40, 0, 1, 2]).await, Err(Error::Protocol(_))), "over 4 MiB, before allocating");
        assert!(matches!(read(&[0, 0, 0, 1, 0x7F]).await, Err(Error::Protocol(_))), "unknown type");
        assert!(matches!(read(&[0, 0, 0, 1, 0x81]).await, Err(Error::Protocol(_))), "host-bound type");
        assert!(matches!(read(&[0, 0, 0, 2, 0x01, 1]).await, Err(Error::Protocol(_))), "short Hello");
        assert!(matches!(read(&[0, 0, 0, 10, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0]).await, Err(Error::Protocol(_))), "empty AU");
        assert!(matches!(read(&[0, 0, 0, 11, 0x02, 2, 0, 0, 0, 0, 0, 0, 0, 0, 9]).await, Err(Error::Protocol(_))), "unknown AU flag");
        assert!(matches!(read(&[0, 0, 0, 3, 0x03, 0xC3, 0x28]).await, Err(Error::Protocol(_))), "invalid UTF-8");
        assert!(matches!(read(&[0, 0, 0, 10, 0x04, 0, 0, 0, 0, 0, 0, 0, 1, 2]).await, Err(Error::Protocol(_))), "approve not 0/1");
        let mut long = vec![0, 0, 0x04, 0x02, 0x03];
        long.extend(std::iter::repeat_n(b'a', 1025));
        assert!(matches!(read(&long).await, Err(Error::Protocol(_))), "reason over 1024 bytes");
    }
}
