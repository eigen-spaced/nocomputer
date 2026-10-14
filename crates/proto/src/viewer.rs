//! Viewer side: connects to a pinned host and logs in, or makes first contact to pair.

use crate::clipboard::{self, ClipboardText};
use crate::identity::{Fingerprint, Identity, SERVER_NAME};
use crate::input::{Input, Notice};
use crate::login::Password;
use crate::tls;
use crate::video::VideoReceiver;
use crate::wire::{read_msg, valid_device_name, write_msg, Message, PROTOCOL_VERSION};
use crate::{Error, ErrorCode};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::timeout;

const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// Longer than the host's approval timeout, so the host answers first.
const PAIR_RESULT_TIMEOUT: Duration = Duration::from_secs(90);
/// Longer than the host's check and answer delay together.
const LOGIN_RESULT_TIMEOUT: Duration = Duration::from_secs(45);

pub struct Viewer {
    identity: Identity,
    name: String,
    #[cfg(test)]
    pub(crate) client_cert_override: Option<Arc<dyn rustls::client::ResolvesClientCert>>,
}

pub enum Outcome {
    /// Pinned host, and it has this device paired; log in next.
    Login(LoginPrompt),
    /// No pin was given. Nothing is trusted yet; see [`FirstContact`].
    FirstContact(FirstContact),
}

struct Channel {
    // The endpoint must outlive its connection.
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl Channel {
    /// Waits briefly so the close reaches the host even if the process exits next;
    /// otherwise the host only notices at the idle timeout.
    async fn close(self) {
        self.conn.close(0u32.into(), b"");
        let _ = timeout(Duration::from_secs(1), self.endpoint.wait_idle()).await;
    }
}

/// An unverified host. Show `host_fingerprint` and this device's fingerprint to the
/// user, who must confirm the host displays the same values before calling
/// [`FirstContact::request_pairing`] and pinning the host.
pub struct FirstContact {
    pub host_fingerprint: Fingerprint,
    pub host_name: String,
    /// The host already has this device paired (for example after the viewer lost its
    /// known-hosts file); confirming the fingerprint is then enough.
    pub already_paired: bool,
    channel: Channel,
}

impl FirstContact {
    /// Waits for the host's user to approve. Returns whether this device is now paired.
    pub async fn request_pairing(mut self) -> Result<bool, Error> {
        if self.already_paired {
            self.channel.close().await;
            return Ok(true);
        }
        write_msg(&mut self.channel.send, &Message::PairRequest).await?;
        let reply = timeout(PAIR_RESULT_TIMEOUT, read_msg(&mut self.channel.recv)).await.map_err(|_| Error::Timeout("pairing approval"));
        self.channel.close().await;
        match reply?? {
            Message::PairResult { approved } => Ok(approved),
            Message::Error { code, message } => Err(Error::Remote { code, message }),
            other => Err(Error::Protocol(format!("unexpected reply to pairing request: {other:?}"))),
        }
    }

    #[cfg(test)]
    pub(crate) fn channel_mut(&mut self) -> (&mut quinn::SendStream, &mut quinn::RecvStream) {
        (&mut self.channel.send, &mut self.channel.recv)
    }
}

/// A pinned host that has this device paired, waiting for its account's username and
/// password. Nothing else happens on the connection until a login succeeds. The host
/// closes it after 5 failed attempts or 2 minutes.
pub struct LoginPrompt {
    pub host_fingerprint: Fingerprint,
    pub host_name: String,
    channel: Channel,
}

pub enum LoginResult {
    Session(Session),
    /// Wrong username or password, or the attempt came before `retry_after` of the
    /// previous failure had passed. The host doesn't say which; neither should the
    /// viewer. Try again on `prompt` once `retry_after` has passed.
    Failed { retry_after: Duration, prompt: LoginPrompt },
}

impl LoginPrompt {
    /// Sends the login. `Err` ends the connection; [`crate::ErrorKind::LoginFailed`]
    /// means too many attempts on it, and connecting again gives a new prompt.
    pub async fn login(mut self, username: &str, password: Password) -> Result<LoginResult, Error> {
        let login = Message::Login { username: username.to_string(), password };
        let sent = write_msg(&mut self.channel.send, &login).await;
        drop(login);
        if let Err(e) = sent {
            return Err(explain_close(&self.channel.conn, e));
        }
        let reply = match timeout(LOGIN_RESULT_TIMEOUT, read_msg(&mut self.channel.recv)).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => return Err(explain_close(&self.channel.conn, e)),
            Err(_) => {
                self.channel.close().await;
                return Err(Error::Timeout("login result"));
            }
        };
        match reply {
            Message::LoginResult { ok: true, .. } => Ok(LoginResult::Session(Session::new(self.host_fingerprint, self.host_name, self.channel))),
            Message::LoginResult { ok: false, retry_after_ms } => {
                // Bounded, so a host can't make the viewer wait forever.
                let retry_after = Duration::from_millis(retry_after_ms.min(3_600_000));
                Ok(LoginResult::Failed { retry_after, prompt: self })
            }
            Message::Error { code, message } => {
                self.channel.close().await;
                Err(Error::Remote { code, message })
            }
            other => {
                self.channel.close().await;
                Err(Error::Protocol(format!("unexpected reply to login: {}", message_type(&other))))
            }
        }
    }

    /// Gives up without logging in.
    pub async fn close(self) {
        self.channel.close().await;
    }

    #[cfg(test)]
    pub(crate) fn channel_mut(&mut self) -> (&mut quinn::SendStream, &mut quinn::RecvStream) {
        (&mut self.channel.send, &mut self.channel.recv)
    }
}

/// Only the message type: replies are never echoed in full.
fn message_type(message: &Message) -> String {
    serde_json::to_value(message).ok().and_then(|v| v.get("type")?.as_str().map(str::to_string)).unwrap_or_default()
}

/// Notices held for the caller before newer ones are dropped.
const NOTICES: usize = 16;

/// An authenticated, authorized connection to a pinned host.
pub struct Session {
    pub host_fingerprint: Fingerprint,
    pub host_name: String,
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    /// Replies from the host, read by a task so that reading is never cut short.
    replies: mpsc::Receiver<Message>,
    notices: mpsc::Receiver<Notice>,
    clipboard_out: tokio::sync::watch::Sender<Option<ClipboardText>>,
    clipboard_in: tokio::sync::watch::Receiver<Option<ClipboardText>>,
    seq: u64,
}

impl Session {
    fn new(host_fingerprint: Fingerprint, host_name: String, channel: Channel) -> Self {
        let Channel { endpoint, conn, send, mut recv } = channel;
        let (replies_tx, replies) = mpsc::channel(1);
        let (notices_tx, notices) = mpsc::channel(NOTICES);
        tokio::spawn(async move {
            while let Ok(m) = read_msg(&mut recv).await {
                let delivered = match m {
                    Message::Notice(n) => !matches!(notices_tx.try_send(n), Err(mpsc::error::TrySendError::Closed(_))),
                    other => replies_tx.send(other).await.is_ok(),
                };
                if !delivered {
                    break;
                }
            }
        });
        let clipboard_out = clipboard::start_sender(conn.clone());
        let (clipboard_tx, clipboard_in) = tokio::sync::watch::channel(None);
        let side_streams = conn.clone();
        tokio::spawn(async move {
            // Ends with the connection, or when the session is dropped.
            tokio::select! {
                _ = clipboard::receive_loop(side_streams, |text| {
                    clipboard_tx.send_replace(Some(text));
                }) => {}
                _ = clipboard_tx.closed() => {}
            }
        });
        Session { host_fingerprint, host_name, endpoint, conn, send, replies, notices, clipboard_out, clipboard_in, seq: 0 }
    }

    pub async fn ping(&mut self) -> Result<Duration, Error> {
        self.seq += 1;
        let started = Instant::now();
        write_msg(&mut self.send, &Message::Ping { seq: self.seq }).await.map_err(|e| self.explain_close(e))?;
        let reply = timeout(REPLY_TIMEOUT, self.replies.recv()).await.map_err(|_| Error::Timeout("pong"))?;
        match reply {
            Some(Message::Pong { seq }) if seq == self.seq => Ok(started.elapsed()),
            Some(Message::Error { code, message }) => Err(Error::Remote { code, message }),
            Some(other) => Err(Error::Protocol(format!("unexpected reply to ping: {other:?}"))),
            None => Err(self.ended()),
        }
    }

    fn explain_close(&self, error: Error) -> Error {
        explain_close(&self.conn, error)
    }

    /// Why the control stream ended.
    fn ended(&self) -> Error {
        let error = match self.conn.close_reason() {
            Some(reason) => reason.into(),
            None => Error::Protocol("the host ended the session".into()),
        };
        self.explain_close(error)
    }

    /// Asks the host for a keyframe, for example after a decoder error. No reply.
    pub async fn request_keyframe(&mut self) -> Result<(), Error> {
        write_msg(&mut self.send, &Message::RequestKeyframe).await.map_err(|e| self.explain_close(e))
    }

    /// Sends input to the host, in order. No reply.
    pub async fn input(&mut self, input: Input) -> Result<(), Error> {
        write_msg(&mut self.send, &Message::Input(input)).await.map_err(|e| self.explain_close(e))
    }

    /// The next notice from the host; `None` once the session has ended. Cancel-safe.
    pub async fn notice(&mut self) -> Option<Notice> {
        self.notices.recv().await
    }

    /// Sends clipboard text to the host. Never blocks; if earlier text is still on its
    /// way, only the newest is kept.
    pub fn send_clipboard(&self, text: ClipboardText) {
        self.clipboard_out.send_replace(Some(text));
    }

    /// The host's newest clipboard text since the last call; `None` once the session
    /// has ended. Cancel-safe. Updates that arrive while the caller is busy are
    /// replaced, never queued.
    pub async fn clipboard(&mut self) -> Option<ClipboardText> {
        next_clipboard(&mut self.clipboard_in).await
    }

    /// The same as [`Session::clipboard`], but without borrowing the session, so it can
    /// wait in one `select!` with [`Session::notice`]. Starts after what `clipboard` has
    /// already returned. Holds the session's connection open until dropped, if the
    /// session is dropped without `close()`.
    pub fn clipboard_receiver(&self) -> ClipboardReceiver {
        ClipboardReceiver(self.clipboard_in.clone())
    }

    /// The host's video. Use it from a separate task; it shares the connection.
    pub fn video(&self) -> VideoReceiver {
        VideoReceiver::new(self.conn.clone())
    }

    pub async fn close(self) {
        self.conn.close(0u32.into(), b"");
        let _ = timeout(Duration::from_secs(1), self.endpoint.wait_idle()).await;
    }
}

/// The host's clipboard text; see [`Session::clipboard_receiver`].
pub struct ClipboardReceiver(tokio::sync::watch::Receiver<Option<ClipboardText>>);

impl ClipboardReceiver {
    /// The host's newest clipboard text since the last call; `None` once the session
    /// has ended. Cancel-safe.
    pub async fn next(&mut self) -> Option<ClipboardText> {
        next_clipboard(&mut self.0).await
    }
}

async fn next_clipboard(updates: &mut tokio::sync::watch::Receiver<Option<ClipboardText>>) -> Option<ClipboardText> {
    loop {
        updates.changed().await.ok()?;
        if let Some(text) = updates.borrow_and_update().clone() {
            return Some(text);
        }
    }
}

/// A stream error after the host closed the connection says only "connection lost";
/// report a revocation or takeover as such.
pub(crate) fn explain_close(conn: &quinn::Connection, error: Error) -> Error {
    let Some(quinn::ConnectionError::ApplicationClosed(close)) = conn.close_reason() else { return error };
    let (code, message) = match u32::try_from(close.error_code.into_inner()) {
        Ok(crate::CLOSE_REVOKED) => (ErrorCode::Revoked, "the host revoked this device's access"),
        Ok(crate::CLOSE_REPLACED) => (ErrorCode::Replaced, "a newer session from this device took over"),
        _ => return error,
    };
    Error::Remote { code, message: message.into() }
}

impl Viewer {
    pub fn new(identity: Identity, name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        if !valid_device_name(&name) {
            return Err(Error::Protocol("invalid device name".into()));
        }
        Ok(Viewer {
            identity,
            name,
            #[cfg(test)]
            client_cert_override: None,
        })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.identity.fingerprint()
    }

    /// With `pinned`, fails with [`Error::HostIdentityChanged`] if the host presents any
    /// other certificate. Without, returns [`Outcome::FirstContact`] even if the host
    /// already knows this device.
    pub async fn connect(&self, addr: SocketAddr, pinned: Option<Fingerprint>) -> Result<Outcome, Error> {
        let presented = Arc::new(Mutex::new(None));
        let builder = tls::client_builder(pinned, presented.clone())?;
        #[cfg(test)]
        let tls = match &self.client_cert_override {
            Some(resolver) => builder.with_client_cert_resolver(resolver.clone()),
            None => builder.with_client_auth_cert(vec![self.identity.cert()], self.identity.key())?,
        };
        #[cfg(not(test))]
        let tls = builder.with_client_auth_cert(vec![self.identity.cert()], self.identity.key())?;

        let bind: SocketAddr = if addr.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, 0).into() };
        let endpoint = quinn::Endpoint::client(bind)?;
        let connecting = endpoint.connect_with(tls::finish_client(tls)?, addr, SERVER_NAME)?;
        let conn = match connecting.await {
            Ok(c) => c,
            Err(e) => {
                let seen = *presented.lock().unwrap();
                return Err(match (pinned, seen) {
                    (Some(expected), Some(presented)) if expected != presented => Error::HostIdentityChanged { expected, presented },
                    _ => e.into(),
                });
            }
        };
        let host_fingerprint = tls::peer_fingerprint(&conn).ok_or_else(|| Error::Protocol("host presented no certificate".into()))?;
        if let Some(expected) = pinned {
            if expected != host_fingerprint {
                conn.close(0u32.into(), b"");
                return Err(Error::HostIdentityChanged { expected, presented: host_fingerprint });
            }
        }

        let (mut send, mut recv) = conn.open_bi().await?;
        let hello = Message::Hello { min_version: PROTOCOL_VERSION, max_version: PROTOCOL_VERSION, device_name: self.name.clone() };
        write_msg(&mut send, &hello).await?;
        let reply = timeout(REPLY_TIMEOUT, read_msg(&mut recv)).await.map_err(|_| Error::Timeout("welcome"))??;
        let channel = Channel { endpoint, conn, send, recv };
        let (host_name, paired) = match reply {
            Message::Welcome { version, host_name, paired } if version == PROTOCOL_VERSION && valid_device_name(&host_name) => (host_name, paired),
            Message::Error { code, message } => {
                channel.close().await;
                return Err(Error::Remote { code, message });
            }
            other => {
                channel.close().await;
                return Err(Error::Protocol(format!("unexpected welcome: {other:?}")));
            }
        };
        match pinned {
            None => Ok(Outcome::FirstContact(FirstContact { host_fingerprint, host_name, already_paired: paired, channel })),
            Some(_) if paired => Ok(Outcome::Login(LoginPrompt { host_fingerprint, host_name, channel })),
            Some(_) => {
                channel.close().await;
                Err(Error::Remote { code: ErrorCode::NotPaired, message: "the host does not have this device paired; pair again".into() })
            }
        }
    }
}
