//! Host side: accepts connections, authorizes each one against the paired-device list,
//! and runs locally approved pairing for unknown devices.

use crate::identity::{Fingerprint, Identity};
use crate::tls;
use crate::trust::{ClientStatus, PairedClients};
use crate::wire::{read_msg, valid_device_name, write_msg, ErrorCode, Message, PROTOCOL_VERSION};
use crate::Error;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::timeout;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const PAIR_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_GRACE: Duration = Duration::from_secs(5);
const CLOSE_REVOKED: u32 = 0x10;

/// What the host's user is asked to approve. The device fingerprint is the one that
/// authenticated the connection, never a value the device sent in a message.
#[derive(Debug, Clone)]
pub struct PairingPrompt {
    pub device_name: String,
    pub device_fingerprint: Fingerprint,
    pub host_fingerprint: Fingerprint,
}

/// Asks the local user; resolves to true only on explicit approval.
pub type Approver = Arc<dyn Fn(PairingPrompt) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    Session { device: Fingerprint, name: String },
    SessionEnded { device: Fingerprint },
    Paired { device: Fingerprint, name: String },
    PairingDenied { device: Fingerprint },
    Rejected { device: Option<Fingerprint>, reason: String },
}

pub struct HostConfig {
    pub name: String,
    pub approval_timeout: Duration,
    /// After a denied or unanswered request, all pairing requests are refused this long.
    pub deny_cooldown: Duration,
    pub events: Option<tokio::sync::mpsc::UnboundedSender<HostEvent>>,
}

impl HostConfig {
    pub fn new(name: impl Into<String>) -> Self {
        HostConfig { name: name.into(), approval_timeout: Duration::from_secs(60), deny_cooldown: Duration::from_secs(10), events: None }
    }
}

#[derive(Default)]
struct PairingGate {
    pending: bool,
    cooldown_until: Option<Instant>,
}

struct Inner {
    fingerprint: Fingerprint,
    config: HostConfig,
    clients: Mutex<PairedClients>,
    approver: Approver,
    gate: Mutex<PairingGate>,
    live: Mutex<HashMap<Fingerprint, Vec<quinn::Connection>>>,
}

pub struct Host {
    endpoint: quinn::Endpoint,
    inner: Arc<Inner>,
}

impl Host {
    pub fn bind(addr: SocketAddr, identity: &Identity, config: HostConfig, clients: PairedClients, approver: Approver) -> Result<Self, Error> {
        let tls = tls::server_builder()?.with_single_cert(vec![identity.cert()], identity.key())?;
        Self::bind_with(addr, tls, identity.fingerprint(), config, clients, approver)
    }

    pub(crate) fn bind_with(
        addr: SocketAddr,
        tls: rustls::ServerConfig,
        fingerprint: Fingerprint,
        config: HostConfig,
        clients: PairedClients,
        approver: Approver,
    ) -> Result<Self, Error> {
        if !valid_device_name(&config.name) {
            return Err(Error::Protocol("invalid host name".into()));
        }
        let endpoint = quinn::Endpoint::server(tls::finish_server(tls)?, addr)?;
        let inner = Inner { fingerprint, config, clients: Mutex::new(clients), approver, gate: Mutex::default(), live: Mutex::default() };
        Ok(Host { endpoint, inner: Arc::new(inner) })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.inner.fingerprint
    }

    /// Accepts connections until [`Host::close`].
    pub async fn run(&self) {
        while let Some(incoming) = self.endpoint.accept().await {
            let inner = self.inner.clone();
            tokio::spawn(async move { inner.handle(incoming).await });
        }
    }

    pub fn paired_clients(&self) -> Result<Vec<crate::PairedClient>, Error> {
        self.inner.clients.lock().unwrap().list()
    }

    /// Marks the device revoked and closes its live connections.
    pub fn revoke(&self, device: &Fingerprint) -> Result<bool, Error> {
        let listed = self.inner.clients.lock().unwrap().revoke(device)?;
        for conn in self.inner.live.lock().unwrap().remove(device).unwrap_or_default() {
            conn.close(CLOSE_REVOKED.into(), b"revoked");
        }
        Ok(listed)
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"host closing");
    }
}

impl Inner {
    fn emit(&self, event: HostEvent) {
        if let Some(tx) = &self.config.events {
            let _ = tx.send(event);
        }
    }

    async fn handle(self: Arc<Self>, incoming: quinn::Incoming) {
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => return self.emit(HostEvent::Rejected { device: None, reason: format!("handshake failed: {e}") }),
        };
        let Some(device) = tls::peer_fingerprint(&conn) else {
            conn.close(0u32.into(), b"");
            return self.emit(HostEvent::Rejected { device: None, reason: "no client certificate".into() });
        };
        if let Err(e) = self.serve(&conn, device).await {
            self.emit(HostEvent::Rejected { device: Some(device), reason: e.to_string() });
        }
        // Let the peer read our last message and close first.
        let _ = timeout(CLOSE_GRACE, conn.closed()).await;
        conn.close(0u32.into(), b"");
    }

    async fn serve(&self, conn: &quinn::Connection, device: Fingerprint) -> Result<(), Error> {
        let (mut send, mut recv) = timeout(HELLO_TIMEOUT, conn.accept_bi()).await.map_err(|_| Error::Timeout("control stream"))??;
        let hello = timeout(HELLO_TIMEOUT, read_msg(&mut recv)).await.map_err(|_| Error::Timeout("hello"))??;
        let Message::Hello { min_version, max_version, device_name } = hello else {
            return refuse(&mut send, ErrorCode::BadRequest, "expected hello").await;
        };
        if !(min_version..=max_version).contains(&PROTOCOL_VERSION) {
            let message = format!("host speaks version {PROTOCOL_VERSION}; device offered {min_version}-{max_version}");
            return refuse(&mut send, ErrorCode::VersionUnsupported, &message).await;
        }
        if !valid_device_name(&device_name) {
            return refuse(&mut send, ErrorCode::BadRequest, "invalid device name").await;
        }

        // Authorized afresh on every connection; there is no session resumption.
        let status = self.clients.lock().unwrap().status(&device);
        match status {
            ClientStatus::Revoked => refuse(&mut send, ErrorCode::Revoked, "this device's access was revoked").await,
            ClientStatus::Paired => {
                write_msg(&mut send, &Message::Welcome { version: PROTOCOL_VERSION, host_name: self.config.name.clone(), paired: true }).await?;
                self.live.lock().unwrap().entry(device).or_default().push(conn.clone());
                self.emit(HostEvent::Session { device, name: device_name });
                let result = session(&mut send, &mut recv).await;
                if let Some(conns) = self.live.lock().unwrap().get_mut(&device) {
                    conns.retain(|c| c.stable_id() != conn.stable_id());
                }
                self.emit(HostEvent::SessionEnded { device });
                result
            }
            ClientStatus::Unknown => {
                write_msg(&mut send, &Message::Welcome { version: PROTOCOL_VERSION, host_name: self.config.name.clone(), paired: false }).await?;
                // An unpaired device may only ask to pair; anything else ends the connection.
                match timeout(PAIR_REQUEST_TIMEOUT, read_msg(&mut recv)).await {
                    Ok(Ok(Message::PairRequest)) => self.pair(&mut send, device, device_name).await,
                    Ok(Ok(_)) => refuse(&mut send, ErrorCode::NotPaired, "pair with this host first").await,
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(Error::Timeout("pairing request")),
                }
            }
        }
    }

    async fn pair(&self, send: &mut quinn::SendStream, device: Fingerprint, device_name: String) -> Result<(), Error> {
        let available = {
            let mut gate = self.gate.lock().unwrap();
            let cooling = gate.cooldown_until.is_some_and(|t| Instant::now() < t);
            let available = !gate.pending && !cooling;
            gate.pending |= available;
            available
        };
        if !available {
            return refuse(send, ErrorCode::PairingUnavailable, "pairing is busy or cooling down; try again shortly").await;
        }
        let prompt = PairingPrompt { device_name: device_name.clone(), device_fingerprint: device, host_fingerprint: self.fingerprint };
        let approved = timeout(self.config.approval_timeout, (self.approver)(prompt)).await.unwrap_or(false);
        let recorded = approved && self.clients.lock().unwrap().add(device, &device_name).is_ok();
        {
            let mut gate = self.gate.lock().unwrap();
            gate.pending = false;
            if !recorded {
                gate.cooldown_until = Some(Instant::now() + self.config.deny_cooldown);
            }
        }
        write_msg(send, &Message::PairResult { approved: recorded }).await?;
        let _ = send.finish();
        self.emit(if recorded { HostEvent::Paired { device, name: device_name } } else { HostEvent::PairingDenied { device } });
        Ok(())
    }
}

async fn refuse(send: &mut quinn::SendStream, code: ErrorCode, message: &str) -> Result<(), Error> {
    write_msg(send, &Message::Error { code, message: message.to_string() }).await?;
    let _ = send.finish();
    Err(Error::Remote { code, message: message.to_string() })
}

/// Authorized control loop. Only keepalive pings exist so far.
async fn session(send: &mut quinn::SendStream, recv: &mut quinn::RecvStream) -> Result<(), Error> {
    loop {
        match read_msg(recv).await {
            Ok(Message::Ping { seq }) => write_msg(send, &Message::Pong { seq }).await?,
            Ok(_) => return refuse(send, ErrorCode::BadRequest, "unexpected message").await,
            // The viewer finished its stream or closed the connection.
            Err(_) => return Ok(()),
        }
    }
}
