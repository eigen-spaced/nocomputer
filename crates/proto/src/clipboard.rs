//! Plain-text clipboard between the controlling viewer and the host, in both directions.
//!
//! Each update travels on its own QUIC bidirectional side stream, so a large paste never
//! waits in front of input on the control stream. The viewer's first bidirectional
//! stream is the control stream; every later one, and every one the host opens, is a
//! side stream:
//!
//! ```text
//! [u8 kind = 1 (clipboard)][u32 BE len][len bytes of UTF-8 text], then FIN
//! ```
//!
//! The text is at most [`MAX_CLIPBOARD`] bytes, contains no U+0000, is not empty, and
//! uses `\n` line endings. The receiver finishes its own half at once and never writes
//! on it. A malformed side stream is stopped and dropped; it does not end the session.
//! Newest wins: a sender resets its unfinished stream when newer text arrives, and a
//! receiver ignores a stream older than the last one it delivered. Neither side logs
//! the text.

use quinn::VarInt;
use std::fmt;
use tokio::sync::watch;

pub const MAX_CLIPBOARD: usize = 1 << 20;
const KIND_CLIPBOARD: u8 = 1;
/// Reset code for a clipboard stream superseded by newer text.
const RESET_SUPERSEDED: u32 = 1;
/// Stop code for a side stream the receiver rejects.
const STOP_REJECTED: u32 = 1;

/// Valid clipboard text. `Debug` shows only the length.
#[derive(Clone, PartialEq, Eq)]
pub struct ClipboardText(String);

impl ClipboardText {
    /// `None` if the text is empty, too long or contains U+0000.
    pub fn new(text: String) -> Option<Self> {
        (!text.is_empty() && text.len() <= MAX_CLIPBOARD && !text.contains('\0')).then_some(ClipboardText(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for ClipboardText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClipboardText({} bytes)", self.0.len())
    }
}

/// Starts the task that sends this side's updates; it ends with the connection or when
/// the returned sender is dropped. Sending never blocks the caller: while a stream is
/// waiting for credit or being written, only the latest text is kept.
pub(crate) fn start_sender(conn: quinn::Connection) -> watch::Sender<Option<ClipboardText>> {
    let (tx, rx) = watch::channel(None);
    tokio::spawn(send_loop(conn, rx));
    tx
}

async fn send_loop(conn: quinn::Connection, mut updates: watch::Receiver<Option<ClipboardText>>) {
    loop {
        // `changed()` marks the value seen, so newer text is taken from the top of the
        // loop rather than waited for again.
        let Some(text) = updates.borrow_and_update().clone() else {
            if updates.changed().await.is_err() {
                return;
            }
            continue;
        };
        let opened = tokio::select! {
            opened = conn.open_bi() => opened,
            changed = updates.changed() => {
                if changed.is_err() {
                    return;
                }
                continue;
            }
        };
        let Ok((mut send, mut recv)) = opened else { return };
        let _ = recv.stop(VarInt::from_u32(0));
        let superseded = tokio::select! {
            _ = write(&mut send, &text) => false,
            changed = updates.changed() => {
                if changed.is_err() {
                    return;
                }
                true
            }
        };
        if superseded {
            let _ = send.reset(VarInt::from_u32(RESET_SUPERSEDED));
            continue;
        }
        if updates.changed().await.is_err() {
            return;
        }
    }
}

async fn write(send: &mut quinn::SendStream, text: &ClipboardText) -> Result<(), quinn::WriteError> {
    let bytes = text.as_str().as_bytes();
    send.write_all(&[KIND_CLIPBOARD]).await?;
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(bytes).await?;
    let _ = send.finish();
    Ok(())
}

/// Accepts side streams until the connection ends, passing each valid update to
/// `deliver` unless a newer one was already delivered. At most as many streams are
/// read at once as QUIC stream credit allows, each at most [`MAX_CLIPBOARD`] bytes.
/// Dropping the future stops the reads in progress too.
pub(crate) async fn receive_loop(conn: quinn::Connection, deliver: impl Fn(ClipboardText)) {
    let mut reads = tokio::task::JoinSet::new();
    let mut newest = None::<u64>;
    loop {
        tokio::select! {
            accepted = conn.accept_bi() => {
                let Ok((send, recv)) = accepted else { return };
                reads.spawn(async move { (recv.id().index(), read(send, recv).await) });
            }
            Some(Ok((index, text))) = reads.join_next() => {
                if let Some(text) = text {
                    if newest.is_none_or(|n| index > n) {
                        newest = Some(index);
                        deliver(text);
                    }
                }
            }
        }
    }
}

async fn read(mut send: quinn::SendStream, mut recv: quinn::RecvStream) -> Option<ClipboardText> {
    let _ = send.finish();
    let text = async {
        let mut kind = [0; 1];
        recv.read_exact(&mut kind).await.ok()?;
        if kind[0] != KIND_CLIPBOARD {
            return None;
        }
        let mut len = [0; 4];
        recv.read_exact(&mut len).await.ok()?;
        let len = u32::from_be_bytes(len) as usize;
        // Checked before allocating.
        if len > MAX_CLIPBOARD {
            return None;
        }
        let mut body = vec![0; len];
        recv.read_exact(&mut body).await.ok()?;
        // Nothing may follow the text.
        recv.read_to_end(0).await.ok()?;
        ClipboardText::new(String::from_utf8(body).ok()?)
    }
    .await;
    if text.is_none() {
        let _ = recv.stop(VarInt::from_u32(STOP_REJECTED));
    }
    text
}
