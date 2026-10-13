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
pub mod paths;
mod tls;
mod trust;
mod video;
mod viewer;
pub mod wire;

#[cfg(test)]
mod tests;

pub use clipboard::{ClipboardText, MAX_CLIPBOARD};
pub use host::{Approver, Host, HostConfig, HostEvent, PairingPrompt};
pub use identity::{Fingerprint, Identity, SERVER_NAME};
pub use input::{Button, Input, KeyState, Notice, NoticeKind, ScrollUnit, MAX_NOTICE, MAX_SCROLL_LINES, MAX_SCROLL_PIXELS};
pub use keystore::{load_or_create_identity, platform_keystore, KeyStore, MemoryKeyStore};
pub use tls::ALPN;
pub use trust::{ClientStatus, KnownHost, KnownHosts, PairedClient, PairedClients};
pub use video::{AccessUnit, VideoFrame, VideoReceiver, MAX_ACCESS_UNIT, VIDEO_STREAM_VERSION};
pub use viewer::{FirstContact, Outcome, Session, Viewer};
pub use wire::{ErrorCode, PROTOCOL_VERSION};

pub const DEFAULT_PORT: u16 = 47900;

/// QUIC application close code the host uses when it revokes a live session.
pub(crate) const CLOSE_REVOKED: u32 = 0x10;
/// ... and when a newer session from the same device takes over.
pub(crate) const CLOSE_REPLACED: u32 = 0x11;

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
