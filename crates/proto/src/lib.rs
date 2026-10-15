//! nocomputer transport, device identity and pairing.
//!
//! QUIC (quinn) with TLS 1.3 (rustls), mutually authenticated with self-signed
//! certificates pinned by SHA-256 fingerprint. A viewer pairs with a host once,
//! SSH-style: both sides show both full fingerprints, the user compares them, and the
//! host's user approves locally. Afterwards the host authorizes every connection against
//! its paired-device list, which supports revocation. See `README.md` for the flow.

mod clipboard;
mod host;
mod identity;
mod input;
mod keystore;
mod login;
pub mod paths;
mod tls;
mod trust;
mod video;
mod viewer;
pub mod wire;

#[cfg(test)]
mod tests;

pub use clipboard::{ClipboardText, MAX_CLIPBOARD};
pub use host::{Approver, Host, HostConfig, HostEvent, PairingPrompt, Verifier};
pub use identity::{Fingerprint, Identity, SERVER_NAME};
pub use input::{Button, Input, KeyState, Notice, NoticeKind, ScrollUnit, MAX_NOTICE, MAX_SCROLL_LINES, MAX_SCROLL_PIXELS};
pub use login::{valid_username, LoginAttempt, LoginDelay, Password, MAX_PASSWORD, MAX_USERNAME};
pub use keystore::{load_or_create_identity, platform_keystore, KeyStore, MemoryKeyStore};
pub use tls::ALPN;
pub use trust::{ClientStatus, KnownHost, KnownHosts, PairedClient, PairedClients};
pub use video::{AccessUnit, VideoFrame, VideoReceiver, MAX_ACCESS_UNIT, VIDEO_STREAM_VERSION};
pub use viewer::{ClipboardReceiver, FirstContact, LoginPrompt, LoginResult, Outcome, Session, Viewer};
pub use wire::{ErrorCode, PROTOCOL_VERSION, VIEW_SIZE_NATIVE};

pub const DEFAULT_PORT: u16 = 47900;

/// QUIC application close code the host uses when it revokes a live session.
pub(crate) const CLOSE_REVOKED: u32 = 0x10;
/// ... and when a newer session from the same device takes over.
pub(crate) const CLOSE_REPLACED: u32 = 0x11;
/// ... and when the host stops sharing, for example when its user turns remote access off.
pub(crate) const CLOSE_HOST_ENDED: u32 = 0x12;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("message encoding: {0}")]
    Json(#[from] serde_json::Error),
    #[error("host identity changed: pinned {expected}, host presented {presented}")]
    HostIdentityChanged { expected: Fingerprint, presented: Fingerprint },
    #[error("host refused ({code:?}): {message}")]
    Remote { code: ErrorCode, message: String },
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("key storage: {0}")]
    KeyStore(String),
    #[error("invalid fingerprint: expected 64 hex digits")]
    BadFingerprint,
    #[error("stored identity is invalid")]
    BadIdentity,
}

/// What went wrong, in the terms a viewer shows its user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No answer from the address: the host is off, unreachable, or not listening
    /// (for example with Remote access turned off).
    Unreachable,
    /// The connection was lost or closed after it was made.
    Disconnected,
    /// The host presented a different key than the pinned one. Show it as a warning
    /// with both fingerprints; never offer to pair again from here.
    HostIdentityChanged,
    /// The host doesn't have this device paired.
    NotPaired,
    /// The host's user revoked this device.
    Revoked,
    /// Another device is in control of the host.
    Busy,
    /// This device connected again from elsewhere; this session was closed.
    Replaced,
    /// The host stopped sharing and closed the connection; not a network fault.
    HostEnded,
    /// The host and viewer speak different protocol versions; update the older one.
    VersionMismatch,
    /// Another pairing request is pending on the host, or one was just denied.
    PairingUnavailable,
    /// Too many failed logins on one connection; connect again to retry. A single
    /// failure is [`LoginResult::Failed`], not an error.
    LoginFailed,
    Other,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Connect(_) | Error::Timeout(_) => ErrorKind::Unreachable,
            Error::Connection(quinn::ConnectionError::TimedOut) => ErrorKind::Unreachable,
            Error::Connection(_) => ErrorKind::Disconnected,
            // A control-stream read or write after the host closed the connection or
            // its stream, for example at the 2-minute login limit.
            Error::Io(e) if matches!(e.kind(), std::io::ErrorKind::NotConnected | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof) => {
                ErrorKind::Disconnected
            }
            Error::HostIdentityChanged { .. } => ErrorKind::HostIdentityChanged,
            Error::Remote { code, .. } => match code {
                ErrorCode::VersionUnsupported => ErrorKind::VersionMismatch,
                ErrorCode::NotPaired => ErrorKind::NotPaired,
                ErrorCode::Revoked => ErrorKind::Revoked,
                ErrorCode::PairingUnavailable => ErrorKind::PairingUnavailable,
                ErrorCode::Busy => ErrorKind::Busy,
                ErrorCode::Replaced => ErrorKind::Replaced,
                ErrorCode::HostEnded => ErrorKind::HostEnded,
                ErrorCode::LoginFailed => ErrorKind::LoginFailed,
                ErrorCode::BadRequest | ErrorCode::Unknown => ErrorKind::Other,
            },
            _ => ErrorKind::Other,
        }
    }
}
