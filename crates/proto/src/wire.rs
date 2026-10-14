//! Control-channel messages: a 4-byte big-endian length, then one JSON object.
//!
//! Unknown fields are ignored so later versions can add fields; unknown message types
//! are errors. Video uses its own streams (`video.rs`); input and notices are messages
//! here, so they stay ordered with the rest of the session (`input.rs`).

use crate::input::{Input, Notice};
use crate::login::Password;
use crate::Error;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

/// 2 added input and notices; 3 added clipboard side streams (`clipboard.rs`); 4 added
/// the account login after `Welcome` for a paired device (`login.rs`).
pub const PROTOCOL_VERSION: u16 = 4;
pub const MAX_CONTROL_MESSAGE: usize = 64 * 1024;
pub const MAX_DEVICE_NAME_CHARS: usize = 64;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Viewer -> host, first message on the control stream.
    Hello { min_version: u16, max_version: u16, device_name: String },
    /// Host -> viewer reply to an acceptable Hello.
    Welcome { version: u16, host_name: String, paired: bool },
    /// Viewer -> host, only from a device the host does not know. Carries no identity:
    /// the host pairs the certificate that authenticated this connection.
    PairRequest,
    PairResult { approved: bool },
    /// Viewer -> host, after `Welcome { paired: true }`, until a `LoginResult` with `ok`.
    Login { username: String, password: Password },
    /// Host -> viewer. On failure, the next attempt is refused unchecked until
    /// `retry_after_ms` has passed.
    LoginResult { ok: bool, retry_after_ms: u64 },
    Ping { seq: u64 },
    /// Viewer -> host, in a session: the decoder needs a keyframe. No reply.
    RequestKeyframe,
    Pong { seq: u64 },
    /// Viewer -> host, controlling session only. No reply.
    Input(Input),
    /// Host -> viewer, at any time in a session. No reply.
    Notice(Notice),
    Error { code: ErrorCode, message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionUnsupported,
    NotPaired,
    Revoked,
    /// Another pairing request is pending, or one was just denied.
    PairingUnavailable,
    BadRequest,
    /// Another device is in control of the host.
    Busy,
    /// A newer session from the same device took over this one.
    Replaced,
    /// Too many failed logins on this connection; connect again to retry.
    LoginFailed,
    #[serde(other)]
    Unknown,
}

/// Room for the largest login message, so encoding it never reallocates and leaves
/// copies of the password behind.
const LOGIN_CAPACITY: usize = 16 * 1024;

pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &Message) -> Result<(), Error> {
    let capacity = if matches!(msg, Message::Login { .. }) { LOGIN_CAPACITY } else { 128 };
    let mut body = Zeroizing::new(Vec::with_capacity(capacity));
    serde_json::to_writer(&mut *body, msg)?;
    if body.len() > MAX_CONTROL_MESSAGE {
        return Err(Error::Protocol("control message too large to send".into()));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    Ok(())
}

pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<Message, Error> {
    let mut len = [0; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    // Checked before allocating.
    if len > MAX_CONTROL_MESSAGE {
        return Err(Error::Protocol(format!("control message of {len} bytes exceeds {MAX_CONTROL_MESSAGE}")));
    }
    let mut body = Zeroizing::new(vec![0; len]);
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Names appear in approval prompts, so no control characters (which could fake
/// prompt lines), no invisible or bidirectional formatting characters (which could
/// disguise the name), and no leading/trailing whitespace.
pub fn valid_device_name(name: &str) -> bool {
    let chars = name.chars().count();
    (1..=MAX_DEVICE_NAME_CHARS).contains(&chars) && name.trim() == name && !name.chars().any(|c| c.is_control() || is_format_char(c))
}

fn is_format_char(c: char) -> bool {
    matches!(c, '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}
