//! local-v4: the pipe contract between the capture app (parent) and this host process
//! (child), over the child's stdin and stdout. v2 added input (0x86-0x8A) and
//! InputUnavailable (0x05); v3 added Clipboard (0x06, 0x8B); v4 added the account
//! check (0x8C, 0x07) and the identity, pairing_withdrawn and login_failed events. Only the
//! current version is spoken.
//!
//! ```text
//! frame: [u32 BE len][u8 type][payload]    len = 1 + payload length, 1..=4 MiB
//!
//! app -> host
//!   0x01 Hello            u16 BE version (= 4); must be the first frame
//!   0x02 AccessUnit       u8 flags (bit0 keyframe, others 0), u64 BE capture_us,
//!                         Annex B bytes (at least 1); a keyframe is an IDR with SPS+PPS
//!   0x03 CaptureStopped   UTF-8 reason, at most 1024 bytes
//!   0x04 PairingDecision  u64 BE request_id, u8 approve (0 or 1); exactly 9 bytes
//!   0x05 InputUnavailable UTF-8 reason, at most 1024 bytes: input can't be injected
//!   0x06 Clipboard        UTF-8 text for the controlling viewer: 1 byte to 1 MiB, no
//!                         U+0000; dropped when no session is running
//!   0x07 LoginVerdict     u64 BE request_id, u8 ok (0 or 1); exactly 9 bytes
//! host -> app
//!   0x81 HelloAck         u16 BE version
//!   0x82 StartStream      empty: a controlling session began; start capture, IDR first
//!   0x83 StopStream       empty: no controlling session remains
//!   0x84 RequestKeyframe  empty
//!   0x85 Event            UTF-8 JSON object tagged by "type"; unknown types are ignored
//!   0x86 Pointer          u16 BE x, u16 BE y: 0..=65535 across the captured display,
//!                         edges inclusive
//!   0x87 Button           u8 button (0 left, 1 right, 2 middle, 3 back, 4 forward),
//!                         u8 down (0 or 1); at the last pointer position
//!   0x88 Scroll           i32 BE dx, i32 BE dy, u8 unit (0 pixel, 1 line); positive
//!                         dy scrolls toward the end of the document (DOM sign);
//!                         at most 10000 pixels or 100 lines per axis
//!   0x89 Key              u16 BE USB HID usage (keyboard page 0x07), u8 state
//!                         (0 up, 1 down, 2 repeat); unknown usages are the app's to drop
//!   0x8A ReleaseAll       empty: release every key and button the remote holds
//!   0x8B Clipboard        UTF-8 text from the controlling viewer, as 0x06; only the
//!                         newest is kept while the app is busy
//!   0x8C VerifyLogin      u64 BE request_id, u8 username length (1..=255), username,
//!                         password (the rest, 1..=1024 bytes); both UTF-8, no
//!                         control characters in the username and no U+0000 in the
//!                         password. Answer with LoginVerdict: ok only if the username
//!                         names the account the app runs as and the password is that
//!                         account's. At most one is outstanding; a newer request
//!                         replaces it, and a verdict for any other id is ignored.
//! ```
//!
//! Input comes only from the controlling session, in order. Pointer moves may be
//! coalesced; buttons, keys and scrolls never are. The host sends ReleaseAll when a
//! controlling session starts or ends (before StopStream) and when it discards an input
//! backlog. Neither side logs input or clipboard contents, or the username or password
//! of a VerifyLogin; both clear their copies of the password when done.
//!
//! Anything else is fatal: lengths out of range (checked before allocating), unknown
//! frame types, a payload on an empty type, wrong fixed sizes, invalid UTF-8. The
//! payloads of `testdata/local-v4/*.bin` are the reference encoding for both sides.
//! `capture_us` is the app's monotonic clock and only meaningful within the app.

use nocomputer_proto::{valid_username, Button, ClipboardText, Input, KeyState, Password, ScrollUnit, MAX_SCROLL_LINES, MAX_SCROLL_PIXELS};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const VERSION: u16 = 4;
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub const MAX_REASON: usize = 1024;

const HELLO: u8 = 0x01;
const ACCESS_UNIT: u8 = 0x02;
const CAPTURE_STOPPED: u8 = 0x03;
const PAIRING_DECISION: u8 = 0x04;
const INPUT_UNAVAILABLE: u8 = 0x05;
const CLIPBOARD: u8 = 0x06;
const LOGIN_VERDICT: u8 = 0x07;
const HELLO_ACK: u8 = 0x81;
const START_STREAM: u8 = 0x82;
const STOP_STREAM: u8 = 0x83;
const REQUEST_KEYFRAME: u8 = 0x84;
const EVENT: u8 = 0x85;
const POINTER: u8 = 0x86;
const BUTTON: u8 = 0x87;
const SCROLL: u8 = 0x88;
const KEY: u8 = 0x89;
const RELEASE_ALL: u8 = 0x8A;
const CLIPBOARD_TO_APP: u8 = 0x8B;
const VERIFY_LOGIN: u8 = 0x8C;
const FLAG_KEYFRAME: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromApp {
    Hello { version: u16 },
    AccessUnit { keyframe: bool, capture_us: u64, data: Vec<u8> },
    CaptureStopped { reason: String },
    PairingDecision { request_id: u64, approve: bool },
    InputUnavailable { reason: String },
    /// `Debug` shows only the length.
    Clipboard(ClipboardText),
    LoginVerdict { request_id: u64, ok: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToApp {
    HelloAck { version: u16 },
    StartStream,
    StopStream,
    RequestKeyframe,
    Event(Event),
    Pointer { x: u16, y: u16 },
    Button { button: u8, down: bool },
    Scroll { dx: i32, dy: i32, unit: u8 },
    Key { usage: u16, state: u8 },
    ReleaseAll,
    Clipboard(ClipboardText),
    /// `Debug` hides the password. Built only from a valid username and password.
    VerifyLogin { request_id: u64, username: String, password: Password },
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
    /// Sent once after HelloAck: this host's name and fingerprint, for the app to show
    /// so its user can compare it with what a viewer shows.
    Identity { name: String, host_fingerprint: String },
    /// The device behind this prompt disconnected, or the request timed out; close the
    /// prompt. No decision is needed.
    PairingWithdrawn { request_id: u64 },
    /// A paired device's login failed; `failures` is its count since its last success.
    LoginFailed { device_fingerprint: String, failures: u32 },
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
            Error::Protocol(m) => write!(f, "local-v4: {m}"),
        }
    }
}

impl std::error::Error for Error {}

fn bad(message: impl Into<String>) -> Error {
    Error::Protocol(message.into())
}

fn reason(frame: &str, payload: Vec<u8>) -> Result<String, Error> {
    if payload.len() > MAX_REASON {
        return Err(bad(format!("{frame} reason exceeds 1024 bytes")));
    }
    String::from_utf8(payload).map_err(|_| bad(format!("{frame} reason is not UTF-8")))
}

fn id_and_flag(frame: &str, payload: &[u8]) -> Result<(u64, bool), Error> {
    if payload.len() != 9 {
        return Err(bad(format!("{frame} must be 9 bytes")));
    }
    let flag = match payload[8] {
        0 => false,
        1 => true,
        _ => return Err(bad(format!("{frame} flag must be 0 or 1"))),
    };
    Ok((u64::from_be_bytes(payload[..8].try_into().unwrap()), flag))
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
            ToApp::Pointer { x, y } => frame(POINTER, &[x.to_be_bytes(), y.to_be_bytes()].concat()),
            ToApp::Button { button, down } => frame(BUTTON, &[*button, *down as u8]),
            ToApp::Scroll { dx, dy, unit } => frame(SCROLL, &[&dx.to_be_bytes()[..], &dy.to_be_bytes(), &[*unit]].concat()),
            ToApp::Key { usage, state } => frame(KEY, &[&usage.to_be_bytes()[..], &[*state]].concat()),
            ToApp::ReleaseAll => frame(RELEASE_ALL, &[]),
            ToApp::Clipboard(text) => frame(CLIPBOARD_TO_APP, text.as_str().as_bytes()),
            ToApp::VerifyLogin { request_id, username, password } => {
                debug_assert!(valid_username(username));
                let password = password.expose().as_bytes();
                let mut out = Vec::with_capacity(5 + 9 + username.len() + password.len());
                out.extend_from_slice(&(1 + 9 + username.len() as u32 + password.len() as u32).to_be_bytes());
                out.push(VERIFY_LOGIN);
                out.extend_from_slice(&request_id.to_be_bytes());
                out.push(username.len() as u8);
                out.extend_from_slice(username.as_bytes());
                out.extend_from_slice(password);
                out
            }
        }
    }

    /// The frame for sanitized input from the controlling session; `None` for input the
    /// app has no encoding for.
    pub fn from_input(input: Input) -> Option<ToApp> {
        Some(match input {
            Input::PointerMove { x, y } => ToApp::Pointer { x, y },
            Input::PointerButton { button, down } => {
                let button = match button {
                    Button::Left => 0,
                    Button::Right => 1,
                    Button::Middle => 2,
                    Button::Back => 3,
                    Button::Forward => 4,
                    Button::Unknown => return None,
                };
                ToApp::Button { button, down }
            }
            Input::Scroll { dx, dy, unit } => {
                let (dx, dy, unit) = match unit {
                    ScrollUnit::Pixel => (dx.clamp(-MAX_SCROLL_PIXELS, MAX_SCROLL_PIXELS), dy.clamp(-MAX_SCROLL_PIXELS, MAX_SCROLL_PIXELS), 0),
                    ScrollUnit::Line => (dx.clamp(-MAX_SCROLL_LINES, MAX_SCROLL_LINES), dy.clamp(-MAX_SCROLL_LINES, MAX_SCROLL_LINES), 1),
                };
                ToApp::Scroll { dx, dy, unit }
            }
            Input::Key { usage, state } => {
                let state = match state {
                    KeyState::Up => 0,
                    KeyState::Down => 1,
                    KeyState::Repeat => 2,
                };
                ToApp::Key { usage, state }
            }
            Input::ReleaseAll => ToApp::ReleaseAll,
        })
    }

    /// Input frames, which the host may coalesce or discard under backlog.
    pub fn is_input(&self) -> bool {
        matches!(self, ToApp::Pointer { .. } | ToApp::Button { .. } | ToApp::Scroll { .. } | ToApp::Key { .. } | ToApp::ReleaseAll)
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
            FromApp::InputUnavailable { reason } => frame(INPUT_UNAVAILABLE, reason.as_bytes()),
            FromApp::Clipboard(text) => frame(CLIPBOARD, text.as_str().as_bytes()),
            FromApp::PairingDecision { request_id, approve } => {
                let mut payload = request_id.to_be_bytes().to_vec();
                payload.push(*approve as u8);
                frame(PAIRING_DECISION, &payload)
            }
            FromApp::LoginVerdict { request_id, ok } => {
                let mut payload = request_id.to_be_bytes().to_vec();
                payload.push(*ok as u8);
                frame(LOGIN_VERDICT, &payload)
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
            CAPTURE_STOPPED => Ok(FromApp::CaptureStopped { reason: reason("CaptureStopped", payload)? }),
            INPUT_UNAVAILABLE => Ok(FromApp::InputUnavailable { reason: reason("InputUnavailable", payload)? }),
            CLIPBOARD => {
                let text = String::from_utf8(payload).map_err(|_| bad("Clipboard is not UTF-8"))?;
                Ok(FromApp::Clipboard(ClipboardText::new(text).ok_or_else(|| bad("Clipboard is empty, over 1 MiB or contains NUL"))?))
            }
            PAIRING_DECISION => {
                let (request_id, approve) = id_and_flag("PairingDecision", &payload)?;
                Ok(FromApp::PairingDecision { request_id, approve })
            }
            LOGIN_VERDICT => {
                let (request_id, ok) = id_and_flag("LoginVerdict", &payload)?;
                Ok(FromApp::LoginVerdict { request_id, ok })
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
            ("01-hello", FromApp::Hello { version: 4 }),
            ("02-access-unit-key", FromApp::AccessUnit { keyframe: true, capture_us: 0x0102030405060708, data: SPS_PPS_IDR.to_vec() }),
            ("02-access-unit-delta", FromApp::AccessUnit { keyframe: false, capture_us: 0x0102030405064E20, data: DELTA.to_vec() }),
            ("03-capture-stopped", FromApp::CaptureStopped { reason: "display disconnected – ü".into() }),
            ("04-pairing-decision-approve", FromApp::PairingDecision { request_id: 0x0A0B0C0D0E0F1011, approve: true }),
            ("04-pairing-decision-deny", FromApp::PairingDecision { request_id: 7, approve: false }),
            ("05-input-unavailable", FromApp::InputUnavailable { reason: "Accessibility not granted – ü".into() }),
            ("06-clipboard", FromApp::Clipboard(ClipboardText::new("line one\nzwei – ü 🙂".into()).unwrap())),
            ("07-login-verdict-ok", FromApp::LoginVerdict { request_id: 0x0A0B0C0D0E0F1011, ok: true }),
            ("07-login-verdict-refused", FromApp::LoginVerdict { request_id: 7, ok: false }),
        ]
    }

    fn to_app() -> Vec<(&'static str, ToApp)> {
        vec![
            ("81-hello-ack", ToApp::HelloAck { version: 4 }),
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
            ("86-pointer", ToApp::Pointer { x: 0x1234, y: 0xFFFF }),
            ("87-button-left-down", ToApp::Button { button: 0, down: true }),
            ("87-button-forward-up", ToApp::Button { button: 4, down: false }),
            ("88-scroll-pixel", ToApp::Scroll { dx: -3, dy: 120, unit: 0 }),
            ("88-scroll-line", ToApp::Scroll { dx: 0, dy: -100, unit: 1 }),
            ("89-key-down", ToApp::Key { usage: 0x0004, state: 1 }),
            ("89-key-up", ToApp::Key { usage: 0x00E3, state: 0 }),
            ("89-key-repeat", ToApp::Key { usage: 0x0028, state: 2 }),
            ("8a-release-all", ToApp::ReleaseAll),
            ("8b-clipboard", ToApp::Clipboard(ClipboardText::new("from the viewer\n– ü 🙂".into()).unwrap())),
            (
                "8c-verify-login",
                ToApp::VerifyLogin { request_id: 9, username: "Jürgen".into(), password: Password::new("correct horse – ü 🙂".into()).unwrap() },
            ),
            ("85-event-pairing-withdrawn", ToApp::Event(Event::PairingWithdrawn { request_id: 7 })),
            (
                "85-event-identity",
                ToApp::Event(Event::Identity {
                    name: "MacBook Air".into(),
                    host_fingerprint: "0102 0304 0506 0708 090A 0B0C 0D0E 0F10 1112 1314 1516 1718 191A 1B1C 1D1E 1F20".into(),
                }),
            ),
        ]
    }

    fn golden(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/local-v4").join(format!("{name}.bin"))
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

    #[test]
    fn input_maps_to_frames() {
        use nocomputer_proto::{Button, Input, KeyState, ScrollUnit};
        let cases = [
            (Input::PointerMove { x: 1, y: 2 }, Some(ToApp::Pointer { x: 1, y: 2 })),
            (Input::PointerButton { button: Button::Middle, down: true }, Some(ToApp::Button { button: 2, down: true })),
            (Input::PointerButton { button: Button::Unknown, down: true }, None),
            (Input::Scroll { dx: 50_000, dy: -1, unit: ScrollUnit::Pixel }, Some(ToApp::Scroll { dx: 10_000, dy: -1, unit: 0 })),
            (Input::Scroll { dx: 0, dy: -500, unit: ScrollUnit::Line }, Some(ToApp::Scroll { dx: 0, dy: -100, unit: 1 })),
            (Input::Key { usage: 0xE3, state: KeyState::Repeat }, Some(ToApp::Key { usage: 0xE3, state: 2 })),
            (Input::ReleaseAll, Some(ToApp::ReleaseAll)),
        ];
        for (input, frame) in cases {
            assert_eq!(ToApp::from_input(input), frame, "{input:?}");
        }
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
        assert!(matches!(read(&[0, 0, 0, 10, 0x07, 0, 0, 0, 0, 0, 0, 0, 1, 2]).await, Err(Error::Protocol(_))), "ok not 0/1");
        assert!(matches!(read(&[0, 0, 0, 9, 0x07, 0, 0, 0, 0, 0, 0, 0, 1]).await, Err(Error::Protocol(_))), "short LoginVerdict");
        assert!(matches!(read(&[0, 0, 0, 1, 0x8C]).await, Err(Error::Protocol(_))), "app-bound VerifyLogin");
        let mut long = vec![0, 0, 0x04, 0x02, 0x03];
        long.extend(std::iter::repeat_n(b'a', 1025));
        assert!(matches!(read(&long).await, Err(Error::Protocol(_))), "reason over 1024 bytes");
        long[4] = 0x05;
        assert!(matches!(read(&long).await, Err(Error::Protocol(_))), "InputUnavailable reason over 1024 bytes");
        assert!(matches!(read(&[0, 0, 0, 1, 0x86]).await, Err(Error::Protocol(_))), "app-bound input type");
        assert!(matches!(read(&[0, 0, 0, 1, 0x06]).await, Err(Error::Protocol(_))), "empty clipboard");
        assert!(matches!(read(&[0, 0, 0, 4, 0x06, b'a', 0, b'b']).await, Err(Error::Protocol(_))), "clipboard with NUL");
        assert!(matches!(read(&[0, 0, 0, 3, 0x06, 0xC3, 0x28]).await, Err(Error::Protocol(_))), "clipboard not UTF-8");
        let mut big = ((nocomputer_proto::MAX_CLIPBOARD + 2) as u32).to_be_bytes().to_vec();
        big.push(0x06);
        big.extend(std::iter::repeat_n(b'a', nocomputer_proto::MAX_CLIPBOARD + 1));
        assert!(matches!(read(&big).await, Err(Error::Protocol(_))), "clipboard over 1 MiB");
        big.truncate(big.len() - 1);
        big[..4].copy_from_slice(&((nocomputer_proto::MAX_CLIPBOARD + 1) as u32).to_be_bytes());
        assert!(matches!(read(&big).await, Ok(FromApp::Clipboard(_))), "clipboard of exactly 1 MiB");
        assert_eq!(format!("{:?}", read(&big).await.unwrap()), "Clipboard(ClipboardText(1048576 bytes))");
    }
}

#[cfg(test)]
mod login_tests {
    use super::*;

    #[test]
    fn verify_login_never_shows_the_password() {
        let message = ToApp::VerifyLogin { request_id: 1, username: "u".into(), password: Password::new("hunter2".into()).unwrap() };
        assert!(!format!("{message:?}").contains("hunter2"));
    }
}
