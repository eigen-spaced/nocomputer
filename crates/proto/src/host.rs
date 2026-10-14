//! Host side: accepts connections, authorizes each one against the paired-device list,
//! logs the paired device's user in to the host's account, and runs locally approved
//! pairing for unknown devices.

use crate::clipboard::{self, ClipboardText};
use crate::identity::{Fingerprint, Identity};
use crate::input::{Input, Notice, MAX_NOTICE};
use crate::login::{valid_username, LoginAttempt, LoginDelay, LoginGate};
use crate::tls;
use crate::trust::{ClientStatus, PairedClients};
use crate::video::{self, AccessUnit, Pushed, VideoQueue};
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
/// Under the Mac app's 3 s wait before it terminates the host.
const CLOSE_FLUSH: Duration = Duration::from_secs(1);
const LOGIN_ATTEMPTS: usize = 5;
/// A verifier that takes longer is treated as a failure.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// What the host's user is asked to approve. The device fingerprint is the one that
/// authenticated the connection, never a value the device sent in a message.
#[derive(Debug, Clone)]
pub struct PairingPrompt {
    pub device_name: String,
    pub device_fingerprint: Fingerprint,
    pub host_fingerprint: Fingerprint,
}

/// Asks the local user; resolves to true only on explicit approval. The future is
/// dropped if the device disconnects first, so dropping it should withdraw the prompt.
pub type Approver = Arc<dyn Fn(PairingPrompt) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Checks a login against the account the host runs for; resolves to true only if the
/// username names that account and the password is its password. Any other account,
/// and any error, is false. Must not log or keep the password. The host serializes
/// calls and applies the delays; the verifier only checks.
pub type Verifier = Arc<dyn Fn(LoginAttempt) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// `name` is the one approved at pairing. `reported_name` is what the device calls
    /// itself in this connection's `Hello`: unverified, so never show it as identity.
    Session { device: Fingerprint, name: String, reported_name: String },
    SessionEnded { device: Fingerprint },
    Paired { device: Fingerprint, name: String },
    PairingDenied { device: Fingerprint },
    /// A login from a paired device failed; `failures` is the device's count since its
    /// last success. Never carries the username or password.
    LoginFailed { device: Fingerprint, failures: u32 },
    Rejected { device: Option<Fingerprint>, reason: String },
    /// The capture side should encode a keyframe soon: a session started, the viewer
    /// asked, or the host dropped frames for a slow viewer. Rate-limit before acting.
    KeyframeNeeded,
    /// From the controlling session, sanitized (see [`Input::sanitized`]). Inject it
    /// promptly, and release everything held when the session ends.
    Input(Input),
    /// Clipboard text from the controlling session. Never log it.
    Clipboard(ClipboardText),
}

pub struct HostConfig {
    pub name: String,
    pub approval_timeout: Duration,
    /// After a denied or unanswered request, all pairing requests are refused this long.
    pub deny_cooldown: Duration,
    pub login_delay: LoginDelay,
    /// From `Welcome` until the login must have succeeded.
    pub login_timeout: Duration,
    pub events: Option<tokio::sync::mpsc::UnboundedSender<HostEvent>>,
}

impl HostConfig {
    pub fn new(name: impl Into<String>) -> Self {
        HostConfig { name: name.into(), approval_timeout: Duration::from_secs(60), deny_cooldown: Duration::from_secs(10), login_delay: LoginDelay::default(),
            login_timeout: Duration::from_secs(120),
            events: None,
        }
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
    verifier: Verifier,
    gate: Mutex<PairingGate>,
    logins: Mutex<LoginGate>,
    /// One login check at a time host-wide, so parallel attempts can't outrun the delay.
    verifying: tokio::sync::Mutex<()>,
    live: Mutex<HashMap<Fingerprint, Vec<quinn::Connection>>>,
    /// The one controlling session.
    controller: Mutex<Option<(Fingerprint, quinn::Connection)>>,
    /// The controlling session's outputs, keyed by its connection's stable id.
    outputs: Mutex<Option<Outputs>>,
}

struct Outputs {
    id: usize,
    video: Arc<VideoQueue>,
    notices: tokio::sync::mpsc::UnboundedSender<Notice>,
    clipboard: tokio::sync::watch::Sender<Option<ClipboardText>>,
}

pub struct Host {
    endpoint: quinn::Endpoint,
    inner: Arc<Inner>,
}

impl Host {
    pub fn bind(addr: SocketAddr, identity: &Identity, config: HostConfig, clients: PairedClients, approver: Approver, verifier: Verifier) -> Result<Self, Error> {
        let tls = tls::server_builder()?.with_single_cert(vec![identity.cert()], identity.key())?;
        Self::bind_with(addr, tls, identity.fingerprint(), config, clients, approver, verifier)
    }

    pub(crate) fn bind_with(
        addr: SocketAddr,
        tls: rustls::ServerConfig,
        fingerprint: Fingerprint,
        config: HostConfig,
        clients: PairedClients,
        approver: Approver,
        verifier: Verifier,
    ) -> Result<Self, Error> {
        if !valid_device_name(&config.name) {
            return Err(Error::Protocol("invalid host name".into()));
        }
        let endpoint = quinn::Endpoint::server(tls::finish_server(tls)?, addr)?;
        let inner = Inner {
            fingerprint,
            config,
            clients: Mutex::new(clients),
            approver,
            verifier,
            gate: Mutex::default(),
            logins: Mutex::default(),
            verifying: tokio::sync::Mutex::default(),
            live: Mutex::default(),
            controller: Mutex::default(),
            outputs: Mutex::default(),
        };
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
            conn.close(crate::CLOSE_REVOKED.into(), b"revoked");
        }
        Ok(listed)
    }

    /// Closes every connection with [`crate::CLOSE_HOST_ENDED`] and waits, briefly, for
    /// the close to reach the viewers, so they report the end instead of timing out.
    pub async fn close(&self) {
        self.endpoint.close(crate::CLOSE_HOST_ENDED.into(), b"host ended the session");
        let _ = timeout(CLOSE_FLUSH, self.endpoint.wait_idle()).await;
    }

    /// Queues a frame for the controlling session, if any. Never blocks: when the viewer
    /// falls behind, frames are dropped up to the next keyframe and
    /// [`HostEvent::KeyframeNeeded`] is emitted.
    pub fn send_access_unit(&self, au: AccessUnit) {
        let queue = self.inner.outputs.lock().unwrap().as_ref().map(|o| o.video.clone());
        if let Some(queue) = queue {
            if let Pushed::NeedKeyframe = queue.push(au) {
                self.inner.emit(HostEvent::KeyframeNeeded);
            }
        }
    }

    /// Tells the controlling session, if any. Messages are cut to [`MAX_NOTICE`] bytes.
    pub fn notice(&self, mut notice: Notice) {
        if notice.message.len() > MAX_NOTICE {
            let mut end = MAX_NOTICE;
            while !notice.message.is_char_boundary(end) {
                end -= 1;
            }
            notice.message.truncate(end);
        }
        if let Some(o) = self.inner.outputs.lock().unwrap().as_ref() {
            let _ = o.notices.send(notice);
        }
    }

    /// Sends clipboard text to the controlling session, if any. Never blocks; if earlier
    /// text is still on its way, only the newest is kept.
    pub fn send_clipboard(&self, text: ClipboardText) {
        if let Some(o) = self.inner.outputs.lock().unwrap().as_ref() {
            o.clipboard.send_replace(Some(text));
        }
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
            ClientStatus::Paired { name } => {
                // Checked before the login too, so nobody types a password only to be
                // told the host is busy.
                if self.controlled_by_other(device) {
                    return refuse(&mut send, ErrorCode::Busy, "another device is connected to this host").await;
                }
                write_msg(&mut send, &Message::Welcome { version: PROTOCOL_VERSION, host_name: self.config.name.clone(), paired: true }).await?;
                // Live from here, so revocation also ends a connection waiting to log in.
                self.live.lock().unwrap().entry(device).or_default().push(conn.clone());
                let result = async {
                    self.login(&mut send, &mut recv, device).await?;
                    if !self.claim_control(device, conn) {
                        return refuse(&mut send, ErrorCode::Busy, "another device is connected to this host").await;
                    }
                    if let Err(e) = write_msg(&mut send, &Message::LoginResult { ok: true, retry_after_ms: 0 }).await {
                        self.release_control(conn);
                        return Err(e);
                    }
                    let result = self.controlled(conn, device, name, device_name, &mut send, recv).await;
                    self.release_control(conn);
                    result
                }
                .await;
                if let Some(conns) = self.live.lock().unwrap().get_mut(&device) {
                    conns.retain(|c| c.stable_id() != conn.stable_id());
                }
                result
            }
            ClientStatus::Unknown => {
                write_msg(&mut send, &Message::Welcome { version: PROTOCOL_VERSION, host_name: self.config.name.clone(), paired: false }).await?;
                // An unpaired device may only ask to pair; anything else ends the connection.
                match timeout(PAIR_REQUEST_TIMEOUT, read_msg(&mut recv)).await {
                    Ok(Ok(Message::PairRequest)) => self.pair(conn, &mut send, device, device_name).await,
                    Ok(Ok(_)) => refuse(&mut send, ErrorCode::NotPaired, "pair with this host first").await,
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(Error::Timeout("pairing request")),
                }
            }
        }
    }

    /// The controlling session, with outputs to it, until the viewer leaves.
    async fn controlled(
        &self,
        conn: &quinn::Connection,
        device: Fingerprint,
        name: String,
        reported_name: String,
        send: &mut quinn::SendStream,
        recv: quinn::RecvStream,
    ) -> Result<(), Error> {
        let queue = VideoQueue::new();
        let (notices_tx, notices) = tokio::sync::mpsc::unbounded_channel();
        self.emit(HostEvent::Session { device, name, reported_name });
        let clipboard = clipboard::start_sender(conn.clone());
        *self.outputs.lock().unwrap() = Some(Outputs { id: conn.stable_id(), video: queue.clone(), notices: notices_tx, clipboard });
        tokio::spawn(video::send_loop(conn.clone(), queue.clone()));
        let events = self.config.events.clone();
        let side_streams = tokio::spawn(clipboard::receive_loop(conn.clone(), move |text| {
            if let Some(tx) = &events {
                let _ = tx.send(HostEvent::Clipboard(text));
            }
        }));
        self.emit(HostEvent::KeyframeNeeded);
        let result = self.session(send, recv, notices).await;
        side_streams.abort();
        self.emit(HostEvent::SessionEnded { device });
        queue.close();
        let mut outputs = self.outputs.lock().unwrap();
        if outputs.as_ref().is_some_and(|o| o.id == conn.stable_id()) {
            *outputs = None;
        }
        result
    }

    /// Reads `Login` messages until one succeeds; the caller answers that one once it
    /// has control. Every failure, whatever its cause, gets the same answer.
    async fn login(&self, send: &mut quinn::SendStream, recv: &mut quinn::RecvStream, device: Fingerprint) -> Result<(), Error> {
        let deadline = tokio::time::Instant::now() + self.config.login_timeout;
        for attempt in 1..=LOGIN_ATTEMPTS {
            let message = tokio::time::timeout_at(deadline, read_msg(recv)).await.map_err(|_| Error::Timeout("login"))??;
            let Message::Login { username, password } = message else {
                return refuse(send, ErrorCode::BadRequest, "log in first").await;
            };
            let retry_after = {
                let _turn = self.verifying.lock().await;
                let waiting = self.logins.lock().unwrap().wait(&device, Instant::now());
                match waiting {
                    // Refused unchecked, and not counted.
                    Some(wait) => wait,
                    None => {
                        let attempt = LoginAttempt { device, username, password };
                        let checkable = valid_username(&attempt.username) && attempt.password.is_valid();
                        let ok = checkable && timeout(VERIFY_TIMEOUT, (self.verifier)(attempt)).await.unwrap_or(false);
                        if ok {
                            self.logins.lock().unwrap().succeed(&device);
                            return Ok(());
                        }
                        let (failures, wait) = self.logins.lock().unwrap().fail(device, &self.config.login_delay, Instant::now());
                        self.emit(HostEvent::LoginFailed { device, failures });
                        // Answering only after the shortest delay slows a viewer that
                        // ignores retry_after.
                        let answer_after = wait.min(self.config.login_delay.base);
                        tokio::time::sleep(answer_after).await;
                        wait - answer_after
                    }
                }
            };
            if attempt == LOGIN_ATTEMPTS {
                break;
            }
            let retry_after_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);
            write_msg(send, &Message::LoginResult { ok: false, retry_after_ms }).await?;
        }
        refuse(send, ErrorCode::LoginFailed, "too many failed logins; connect again to retry").await
    }

    fn controlled_by_other(&self, device: Fingerprint) -> bool {
        let slot = self.controller.lock().unwrap();
        slot.as_ref().is_some_and(|(holder, conn)| *holder != device && conn.close_reason().is_none())
    }

    /// One controlling session at a time. A closed session frees the slot at once,
    /// including one closed by revocation. The same device connecting again (for
    /// example after a network change, before its old connection times out) replaces
    /// its old session; any other device is refused until the slot is free.
    fn claim_control(&self, device: Fingerprint, conn: &quinn::Connection) -> bool {
        let mut slot = self.controller.lock().unwrap();
        if let Some((holder, old)) = slot.as_ref() {
            if old.close_reason().is_none() {
                if *holder != device {
                    return false;
                }
                old.close(crate::CLOSE_REPLACED.into(), b"replaced");
            }
        }
        *slot = Some((device, conn.clone()));
        true
    }

    fn release_control(&self, conn: &quinn::Connection) {
        let mut slot = self.controller.lock().unwrap();
        if slot.as_ref().is_some_and(|(_, c)| c.stable_id() == conn.stable_id()) {
            *slot = None;
        }
    }

    async fn pair(&self, conn: &quinn::Connection, send: &mut quinn::SendStream, device: Fingerprint, device_name: String) -> Result<(), Error> {
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
        // A device that leaves withdraws its request: the approver's future is dropped.
        let approved = tokio::select! {
            approved = timeout(self.config.approval_timeout, (self.approver)(prompt)) => approved.unwrap_or(false),
            _ = conn.closed() => false,
        };
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

/// Messages read ahead from the controller. A full buffer stops reading, which pushes
/// back on the viewer through QUIC flow control.
const READ_AHEAD: usize = 64;

impl Inner {
    /// Authorized control loop: keepalive pings, keyframe requests and input from the
    /// viewer; notices to it.
    async fn session(
        &self,
        send: &mut quinn::SendStream,
        mut recv: quinn::RecvStream,
        mut notices: tokio::sync::mpsc::UnboundedReceiver<Notice>,
    ) -> Result<(), Error> {
        // Reading happens in its own task: a read cut short by a notice would lose data.
        let (tx, mut messages) = tokio::sync::mpsc::channel(READ_AHEAD);
        let reader = tokio::spawn(async move {
            while let Ok(m) = read_msg(&mut recv).await {
                if tx.send(m).await.is_err() {
                    break;
                }
            }
        });
        let result = loop {
            tokio::select! {
                m = messages.recv() => match m {
                    Some(Message::Ping { seq }) => write_msg(send, &Message::Pong { seq }).await?,
                    Some(Message::RequestKeyframe) => self.emit(HostEvent::KeyframeNeeded),
                    Some(Message::Input(input)) => {
                        if let Some(input) = input.sanitized() {
                            self.emit(HostEvent::Input(input));
                        }
                    }
                    Some(_) => break refuse(send, ErrorCode::BadRequest, "unexpected message").await,
                    // The viewer finished its stream or closed the connection.
                    None => break Ok(()),
                },
                Some(notice) = notices.recv() => write_msg(send, &Message::Notice(notice)).await?,
            }
        };
        reader.abort();
        result
    }
}
