//! Viewer side: connects to a pinned host, or makes first contact to pair.

use crate::identity::{Fingerprint, Identity, SERVER_NAME};
use crate::tls;
use crate::video::VideoReceiver;
use crate::wire::{read_msg, valid_device_name, write_msg, Message, PROTOCOL_VERSION};
use crate::{Error, ErrorCode};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::timeout;

const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// Longer than the host's approval timeout, so the host answers first.
const PAIR_RESULT_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Viewer {
    identity: Identity,
    name: String,
    #[cfg(test)]
    pub(crate) client_cert_override: Option<Arc<dyn rustls::client::ResolvesClientCert>>,
}

pub enum Outcome {
    /// Pinned host, and it has this device paired.
    Ready(Session),
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

/// An authenticated, authorized connection to a pinned host.
pub struct Session {
    pub host_fingerprint: Fingerprint,
    pub host_name: String,
    channel: Channel,
    seq: u64,
}

impl Session {
    pub async fn ping(&mut self) -> Result<Duration, Error> {
        self.seq += 1;
        let started = Instant::now();
        let reply = async {
            write_msg(&mut self.channel.send, &Message::Ping { seq: self.seq }).await?;
            timeout(REPLY_TIMEOUT, read_msg(&mut self.channel.recv)).await.map_err(|_| Error::Timeout("pong"))?
        };
        match reply.await.map_err(|e| self.explain_close(e))? {
            Message::Pong { seq } if seq == self.seq => Ok(started.elapsed()),
            Message::Error { code, message } => Err(Error::Remote { code, message }),
            other => Err(Error::Protocol(format!("unexpected reply to ping: {other:?}"))),
        }
    }

    fn explain_close(&self, error: Error) -> Error {
        explain_close(&self.channel.conn, error)
    }

    /// Asks the host for a keyframe, for example after a decoder error. No reply.
    pub async fn request_keyframe(&mut self) -> Result<(), Error> {
        write_msg(&mut self.channel.send, &Message::RequestKeyframe).await.map_err(|e| self.explain_close(e))
    }

    /// The host's video. Use it from a separate task; it shares the connection.
    pub fn video(&self) -> VideoReceiver {
        VideoReceiver::new(self.channel.conn.clone())
    }

    pub async fn close(self) {
        self.channel.close().await;
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
            Some(_) if paired => Ok(Outcome::Ready(Session { host_fingerprint, host_name, channel, seq: 0 })),
            Some(_) => {
                channel.close().await;
                Err(Error::Remote { code: ErrorCode::NotPaired, message: "the host does not have this device paired; pair again".into() })
            }
        }
    }
}
