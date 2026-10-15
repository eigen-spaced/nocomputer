//! nocomputer-host: serves paired viewers. Normally spawned by the capture app with no
//! arguments and fed over stdin/stdout (local-v5, see `local.rs`); stdout carries only
//! local-v5 frames and all logging goes to stderr.
//!
//!   nocomputer-host [--listen ADDR] [--name NAME] [--state-dir DIR] [--from-file X.h264]
//!
//! `--from-file` replays an Annex B file at 60 fps instead of reading the app, and turns
//! stdin into a y/N console for pairing approvals and logins (the password is not
//! checked then), for testing without the app. Input from the viewer is then only
//! counted.
//!
//! Exit codes: 0 when stdin closes, 1 on startup errors, 2 on a local-v5 version
//! mismatch, 3 on any other local-v5 violation.

use nocomputer_host::h264::{self, Kind};
use nocomputer_host::input::InputQueue;
use nocomputer_host::local::{self, Event, FromApp, ToApp};
use nocomputer_proto::{
    load_or_create_identity, platform_keystore, wire::valid_device_name, AccessUnit, Approver, ClipboardText, Host, HostConfig, HostEvent, Notice, NoticeKind,
    PairedClients, Verifier, DEFAULT_PORT,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use zeroize::Zeroizing;

/// At most one keyframe request to the capture side per this interval.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);
/// At most one view size to the capture side per this interval: each restarts the encoder.
const VIEW_SIZE_INTERVAL: Duration = Duration::from_millis(250);
const REPLAY_FRAME: Duration = Duration::from_micros(16_667);

struct Exit(i32, String);

impl Exit {
    fn startup(e: impl std::fmt::Display) -> Self {
        Exit(1, e.to_string())
    }
}

impl From<local::Error> for Exit {
    fn from(e: local::Error) -> Self {
        match e {
            local::Error::Eof => Exit(0, "the app closed the pipe".into()),
            other => Exit(3, other.to_string()),
        }
    }
}

struct Options {
    listen: SocketAddr,
    name: String,
    state_dir: PathBuf,
    replay: Option<PathBuf>,
}

fn machine_name() -> String {
    #[cfg(unix)]
    let raw = {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname NUL-terminates on success.
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let len = buf.iter().position(|&b| b == 0).unwrap_or(0);
        if ok { String::from_utf8_lossy(&buf[..len]).into_owned() } else { String::new() }
    };
    #[cfg(not(unix))]
    let raw = std::env::var("COMPUTERNAME").unwrap_or_default();
    let name: String = raw.trim_end_matches(".local").chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string();
    if valid_device_name(&name) {
        name
    } else {
        "nocomputer host".into()
    }
}

fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.unwrap_or_else(|| PathBuf::from(".")).join("nocomputer").join("host")
}

fn options() -> Result<Options, Exit> {
    let mut args = std::env::args().skip(1);
    let mut o = Options { listen: ([0, 0, 0, 0], DEFAULT_PORT).into(), name: machine_name(), state_dir: default_state_dir(), replay: None };
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| Exit::startup(format!("{flag} needs a value")));
        match flag.as_str() {
            "--listen" => o.listen = value()?.parse().map_err(Exit::startup)?,
            "--name" => o.name = value()?,
            "--state-dir" => o.state_dir = value()?.into(),
            "--from-file" => o.replay = Some(value()?.into()),
            _ => return Err(Exit::startup(format!("unknown argument {flag}; usage: nocomputer-host [--listen ADDR] [--name NAME] [--state-dir DIR] [--from-file X.h264]"))),
        }
    }
    if !valid_device_name(&o.name) {
        return Err(Exit::startup("invalid host name"));
    }
    Ok(o)
}

fn create_state_dir(dir: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// What the capture side is told to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    Start,
    Stop,
    Keyframe,
    /// The controlling viewer's picture size, already coalesced (`VIEW_SIZE_INTERVAL`).
    ViewSize { width: u16, height: u16 },
    /// Release all remote input; sent in order with Start and Stop.
    ReleaseAll,
}

fn fp(f: &nocomputer_proto::Fingerprint) -> String {
    f.to_string()
}

/// Turns host events into capture commands (with keyframe requests rate-limited and view
/// sizes coalesced), input, clipboard text and display events. Input held by a session is
/// released when any session starts or ends.
async fn route_events(
    mut events: mpsc::UnboundedReceiver<HostEvent>,
    capture: mpsc::UnboundedSender<Capture>,
    input: Arc<InputQueue>,
    clipboard: watch::Sender<Option<ClipboardText>>,
    notices: mpsc::UnboundedSender<Event>,
) {
    let release = || {
        input.clear();
        let _ = capture.send(Capture::ReleaseAll);
    };
    let mut sessions = 0usize;
    let mut last_keyframe: Option<Instant> = None;
    let mut deferred: Option<Instant> = None;
    // A stream start already begins with a keyframe; the request that follows a new
    // session's start is redundant.
    let mut skip_next_keyframe = false;
    // Each view size restarts the encoder: send at most one per interval, the newest.
    let mut last_view_size: Option<Instant> = None;
    let mut pending_view_size: Option<(Instant, u16, u16)> = None;
    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Some(e) => e,
                None => return,
            },
            _ = tokio::time::sleep_until(deferred.unwrap_or_else(Instant::now)), if deferred.is_some() => {
                deferred = None;
                last_keyframe = Some(Instant::now());
                let _ = capture.send(Capture::Keyframe);
                continue;
            }
            _ = tokio::time::sleep_until(pending_view_size.map_or_else(Instant::now, |p| p.0)), if pending_view_size.is_some() => {
                if let Some((_, width, height)) = pending_view_size.take() {
                    last_view_size = Some(Instant::now());
                    let _ = capture.send(Capture::ViewSize { width, height });
                }
                continue;
            }
        };
        match event {
            HostEvent::Session { device, name, .. } => {
                release();
                sessions += 1;
                if sessions == 1 {
                    let _ = capture.send(Capture::Start);
                    last_keyframe = Some(Instant::now());
                    skip_next_keyframe = true;
                }
                let _ = notices.send(Event::Session { name, device_fingerprint: fp(&device) });
            }
            HostEvent::SessionEnded { device } => {
                release();
                sessions = sessions.saturating_sub(1);
                if sessions == 0 {
                    deferred = None;
                    (last_view_size, pending_view_size) = (None, None);
                    let _ = capture.send(Capture::Stop);
                }
                let _ = notices.send(Event::SessionEnded { device_fingerprint: fp(&device) });
            }
            HostEvent::KeyframeNeeded if std::mem::take(&mut skip_next_keyframe) => {}
            HostEvent::KeyframeNeeded if sessions > 0 => {
                let now = Instant::now();
                match last_keyframe {
                    Some(t) if now < t + KEYFRAME_INTERVAL => {
                        deferred.get_or_insert(t + KEYFRAME_INTERVAL);
                    }
                    _ => {
                        last_keyframe = Some(now);
                        let _ = capture.send(Capture::Keyframe);
                    }
                }
            }
            HostEvent::KeyframeNeeded => {}
            HostEvent::ViewSize { width, height } if sessions > 0 => {
                let now = Instant::now();
                match last_view_size {
                    Some(t) if now < t + VIEW_SIZE_INTERVAL => pending_view_size = Some((t + VIEW_SIZE_INTERVAL, width, height)),
                    _ => {
                        last_view_size = Some(now);
                        pending_view_size = None;
                        let _ = capture.send(Capture::ViewSize { width, height });
                    }
                }
            }
            HostEvent::ViewSize { .. } => {}
            HostEvent::Input(i) if sessions > 0 => {
                if let Some(frame) = ToApp::from_input(i) {
                    input.push(frame);
                }
            }
            HostEvent::Input(_) => {}
            HostEvent::Clipboard(text) if sessions > 0 => {
                clipboard.send_replace(Some(text));
            }
            HostEvent::Clipboard(_) => {}
            HostEvent::Paired { device, name } => {
                let _ = notices.send(Event::Paired { name, device_fingerprint: fp(&device) });
            }
            HostEvent::PairingDenied { device } => {
                let _ = notices.send(Event::PairingDenied { device_fingerprint: fp(&device) });
            }
            HostEvent::Rejected { device, reason } => {
                let _ = notices.send(Event::Rejected { device_fingerprint: device.as_ref().map(fp), reason });
            }
            HostEvent::LoginFailed { device, failures } => {
                let _ = notices.send(Event::LoginFailed { device_fingerprint: fp(&device), failures });
            }
        }
    }
}

type Pending = Arc<Mutex<Option<(u64, oneshot::Sender<bool>)>>>;

/// Clears a request that ends unanswered: the proto host dropped its future because the
/// device left or the request timed out.
struct Unanswered {
    pending: Pending,
    request_id: u64,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for Unanswered {
    fn drop(&mut self) {
        let mut slot = self.pending.lock().unwrap();
        if slot.as_ref().is_some_and(|(id, _)| *id == self.request_id) {
            *slot = None;
            drop(slot);
            if let Some(f) = self.on_drop.take() {
                f();
            }
        }
    }
}

/// Pairing approvals go to the app as prompts with a fresh id; only a decision carrying
/// the id of the current prompt resolves it. A prompt left unanswered is withdrawn.
fn app_approver(pending: Pending, notices: mpsc::UnboundedSender<Event>) -> Approver {
    let next_id = Arc::new(AtomicU64::new(1));
    Arc::new(move |p| {
        let (tx, rx) = oneshot::channel();
        let request_id = next_id.fetch_add(1, Ordering::Relaxed);
        *pending.lock().unwrap() = Some((request_id, tx));
        let _ = notices.send(Event::PairingPrompt {
            request_id,
            device_name: p.device_name,
            device_fingerprint: fp(&p.device_fingerprint),
            host_fingerprint: fp(&p.host_fingerprint),
        });
        let withdraw = notices.clone();
        let guard = Unanswered {
            pending: pending.clone(),
            request_id,
            on_drop: Some(Box::new(move || {
                let _ = withdraw.send(Event::PairingWithdrawn { request_id });
            })),
        };
        Box::pin(async move {
            let _guard = guard;
            rx.await.unwrap_or(false)
        })
    })
}

/// Logins go to the app, which checks them against the account it runs as. The host
/// serializes checks, so one is outstanding at a time; only a verdict carrying its id
/// resolves it.
fn app_verifier(pending: Pending, requests: mpsc::UnboundedSender<ToApp>) -> Verifier {
    let next_id = Arc::new(AtomicU64::new(1));
    Arc::new(move |attempt| {
        let (tx, rx) = oneshot::channel();
        let request_id = next_id.fetch_add(1, Ordering::Relaxed);
        *pending.lock().unwrap() = Some((request_id, tx));
        // The proto host checks these bounds before calling; never send anything else.
        let sent = nocomputer_proto::valid_username(&attempt.username)
            && requests.send(ToApp::VerifyLogin { request_id, username: attempt.username, password: attempt.password }).is_ok();
        let guard = Unanswered { pending: pending.clone(), request_id, on_drop: None };
        Box::pin(async move {
            let _guard = guard;
            sent && rx.await.unwrap_or(false)
        })
    })
}

/// Replay mode has no account to check: the console allows each login, and the password
/// is dropped unread.
fn console_verifier(pending: Pending) -> Verifier {
    Arc::new(move |attempt| {
        let (tx, rx) = oneshot::channel();
        *pending.lock().unwrap() = Some((0, tx));
        eprintln!();
        eprintln!("Login from device {} (replay mode: the password is not checked). Allow? [y/N]", attempt.device);
        Box::pin(async move { rx.await.unwrap_or(false) })
    })
}

fn console_approver(pending: Pending) -> Approver {
    Arc::new(move |p| {
        let (tx, rx) = oneshot::channel();
        // Pending before the prompt shows, so an answer can't arrive first and be lost.
        *pending.lock().unwrap() = Some((0, tx));
        eprintln!();
        eprintln!("Pairing request from device \"{}\"", p.device_name);
        eprintln!("  device fingerprint: {}", p.device_fingerprint);
        eprintln!("  this host:          {}", p.host_fingerprint);
        eprintln!("Check that the device shows exactly these two fingerprints. Approve? [y/N]");
        Box::pin(async move { rx.await.unwrap_or(false) })
    })
}

fn submit(host: &Host, keyframe: bool, data: Vec<u8>) -> Result<(), String> {
    match h264::classify(&data, keyframe)? {
        Kind::Keyframe { codec } => host.send_access_unit(AccessUnit::keyframe(codec, data)),
        Kind::Delta => host.send_access_unit(AccessUnit::delta(data)),
    }
    Ok(())
}

async fn handshake(stdin: &mut tokio::io::Stdin) -> Result<(), Exit> {
    match FromApp::read(stdin).await? {
        FromApp::Hello { version: local::VERSION } => Ok(()),
        FromApp::Hello { version } => Err(Exit(2, format!("the app speaks local-v{version}; this host speaks local-v{}", local::VERSION))),
        other => Err(Exit(3, format!("expected Hello first, got {other:?}"))),
    }
}

async fn run() -> Result<(), Exit> {
    let o = options()?;
    // The app's Hello is checked before touching key storage or the network.
    let mut stdin = tokio::io::stdin();
    if o.replay.is_none() {
        handshake(&mut stdin).await?;
    }
    create_state_dir(&o.state_dir).map_err(|e| Exit::startup(format!("state directory {}: {e}", o.state_dir.display())))?;
    let store = platform_keystore(&o.state_dir, "host").map_err(Exit::startup)?;
    let identity = load_or_create_identity(store.as_ref()).map_err(Exit::startup)?;

    let (events_tx, events) = mpsc::unbounded_channel();
    let (capture_tx, capture) = mpsc::unbounded_channel();
    let (notices_tx, notices) = mpsc::unbounded_channel();
    let input = Arc::new(InputQueue::default());
    let (clipboard_tx, clipboard) = watch::channel(None);
    let pending: Pending = Arc::default();
    let logins: Pending = Arc::default();
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let (approver, verifier) = match o.replay {
        // One console answers both; only one question is open at a time.
        Some(_) => (console_approver(pending.clone()), console_verifier(pending.clone())),
        None => (app_approver(pending.clone(), notices_tx.clone()), app_verifier(logins.clone(), requests_tx)),
    };
    let mut config = HostConfig::new(o.name.clone());
    config.events = Some(events_tx);
    let clients = PairedClients::at(o.state_dir.join("paired-clients.json"));
    let host = Arc::new(Host::bind(o.listen, &identity, config, clients, approver, verifier).map_err(Exit::startup)?);
    eprintln!("nocomputer-host: \"{}\" listening on {}", o.name, host.local_addr().map_err(Exit::startup)?);
    eprintln!("nocomputer-host: this host is {}", host.fingerprint());
    let _ = notices_tx.send(Event::Identity { name: o.name.clone(), host_fingerprint: fp(&host.fingerprint()) });
    tokio::spawn(route_events(events, capture_tx, input.clone(), clipboard_tx, notices_tx));
    let runner = host.clone();
    tokio::spawn(async move { runner.run().await });

    let result = match o.replay {
        Some(path) => replay(&host, &path, capture, input, notices, pending).await,
        None => serve_app(&host, stdin, capture, input, clipboard, notices, Questions { pairing: pending, logins, login_requests: requests }).await,
    };
    host.close().await;
    result
}

/// What the app is asked: the pairing prompt and the login check awaiting an answer, and
/// VerifyLogin frames to send.
struct Questions {
    pairing: Pending,
    logins: Pending,
    login_requests: mpsc::UnboundedReceiver<ToApp>,
}

/// Normal mode: the app on the other end of stdin/stdout.
async fn serve_app(
    host: &Arc<Host>,
    mut stdin: tokio::io::Stdin,
    mut capture: mpsc::UnboundedReceiver<Capture>,
    input: Arc<InputQueue>,
    mut clipboard: watch::Receiver<Option<ClipboardText>>,
    mut notices: mpsc::UnboundedReceiver<Event>,
    questions: Questions,
) -> Result<(), Exit> {
    let Questions { pairing: pending, logins, login_requests: mut requests } = questions;
    // One writer owns stdout, so frames never interleave. Control frames go first; input
    // is ordered among itself; clipboard text waits behind both, and only the newest is
    // kept while it waits. On shutdown it writes the control frames already queued, then
    // stops.
    let (out_tx, mut out) = mpsc::unbounded_channel::<ToApp>();
    let (stop_tx, mut stop) = oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        loop {
            let message = tokio::select! {
                biased;
                Some(m) = out.recv() => m,
                m = input.pop() => m,
                Ok(()) = clipboard.changed() => match clipboard.borrow_and_update().clone() {
                    Some(text) => ToApp::Clipboard(text),
                    None => continue,
                },
                _ = &mut stop => {
                    while let Ok(m) = out.try_recv() {
                        if stdout.write_all(&Zeroizing::new(m.encode())).await.is_err() {
                            break;
                        }
                    }
                    let _ = stdout.flush().await;
                    return;
                }
            };
            // Cleared after writing, since a VerifyLogin frame holds a password.
            let frame = Zeroizing::new(message.encode());
            if stdout.write_all(&frame).await.is_err() || stdout.flush().await.is_err() {
                return;
            }
        }
    });
    let _ = out_tx.send(ToApp::HelloAck { version: local::VERSION });
    let forward = out_tx.clone();
    tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                Some(c) = capture.recv() => match c {
                    Capture::Start => ToApp::StartStream,
                    Capture::Stop => ToApp::StopStream,
                    Capture::Keyframe => ToApp::RequestKeyframe,
                    Capture::ViewSize { width, height } => ToApp::ViewSize { width, height },
                    Capture::ReleaseAll => ToApp::ReleaseAll,
                },
                Some(e) = notices.recv() => ToApp::Event(e),
                Some(r) = requests.recv() => r,
                else => return,
            };
            if forward.send(message).is_err() {
                return;
            }
        }
    });

    let result = loop {
        match FromApp::read(&mut stdin).await {
            Ok(FromApp::AccessUnit { keyframe, data, .. }) => {
                if let Err(e) = submit(host, keyframe, data) {
                    break Err(Exit(3, format!("access unit: {e}")));
                }
            }
            Ok(FromApp::CaptureStopped { reason }) => {
                eprintln!("nocomputer-host: capture stopped: {reason}");
                host.notice(Notice { kind: NoticeKind::CaptureStopped, message: reason });
            }
            Ok(FromApp::InputUnavailable { reason }) => {
                eprintln!("nocomputer-host: input unavailable: {reason}");
                host.notice(Notice { kind: NoticeKind::InputUnavailable, message: reason });
            }
            Ok(FromApp::Clipboard(text)) => host.send_clipboard(text),
            Ok(FromApp::PairingDecision { request_id, approve }) => {
                let mut slot = pending.lock().unwrap();
                if slot.as_ref().is_some_and(|(id, _)| *id == request_id) {
                    let (_, tx) = slot.take().unwrap();
                    let _ = tx.send(approve);
                } else {
                    eprintln!("nocomputer-host: ignoring a decision for pairing request {request_id}, which is not pending");
                }
            }
            Ok(FromApp::LoginVerdict { request_id, ok }) => {
                let mut slot = logins.lock().unwrap();
                if slot.as_ref().is_some_and(|(id, _)| *id == request_id) {
                    let (_, tx) = slot.take().unwrap();
                    let _ = tx.send(ok);
                }
            }
            Ok(FromApp::Hello { .. }) => break Err(Exit(3, "Hello after the handshake".into())),
            Err(e) => break Err(e.into()),
        }
    };
    drop(out_tx);
    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    match result {
        Err(Exit(0, _)) => Ok(()),
        other => other,
    }
}

/// Test mode: replays a file as if the app were capturing, with a console for approvals.
async fn replay(
    host: &Arc<Host>,
    path: &std::path::Path,
    mut capture: mpsc::UnboundedReceiver<Capture>,
    input: Arc<InputQueue>,
    mut notices: mpsc::UnboundedReceiver<Event>,
    pending: Pending,
) -> Result<(), Exit> {
    let data = std::fs::read(path).map_err(|e| Exit::startup(format!("{}: {e}", path.display())))?;
    let units: Vec<(bool, Vec<u8>)> = h264::access_units(&data)
        .into_iter()
        .map(|u| (h264::nal_units(u).iter().any(|n| n.kind() == 5), u.to_vec()))
        .collect();
    let keyframes: Vec<usize> = units.iter().enumerate().filter(|(_, (k, _))| *k).map(|(i, _)| i).collect();
    if keyframes.is_empty() {
        return Err(Exit::startup(format!("{} has no IDR", path.display())));
    }
    for (i, (k, u)) in units.iter().enumerate() {
        h264::classify(u, *k).map_err(|e| Exit::startup(format!("{} access unit {i}: {e}", path.display())))?;
    }
    eprintln!("nocomputer-host: replaying {} ({} access units, keyframes at {keyframes:?}) while a viewer is connected", path.display(), units.len());

    tokio::spawn(async move {
        while let Some(e) = notices.recv().await {
            eprintln!("nocomputer-host: {}", serde_json::to_string(&e).unwrap_or_default());
        }
    });
    tokio::spawn(count_input(input));
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some((_, tx)) = pending.lock().unwrap().take() {
                let _ = tx.send(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"));
            }
        }
    });

    let mut next = keyframes[0];
    let mut streaming = false;
    let mut tick = tokio::time::interval(REPLAY_FRAME);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            command = capture.recv() => match command {
                None => return Ok(()),
                Some(Capture::Start) => {
                    streaming = true;
                    next = keyframes[0];
                }
                Some(Capture::Stop) => streaming = false,
                // A recording has one size.
                Some(Capture::ReleaseAll | Capture::ViewSize { .. }) => {}
                // Like a forced IDR: jump to the next keyframe in the file.
                Some(Capture::Keyframe) => next = keyframes.iter().copied().find(|&k| k >= next).unwrap_or(keyframes[0]),
            },
            _ = tick.tick(), if streaming => {
                let (keyframe, unit) = &units[next];
                submit(host, *keyframe, unit.clone()).map_err(|e| Exit(3, e))?;
                next = (next + 1) % units.len();
            }
        }
    }
}

/// Replay mode has nothing to inject into: report how much input arrived, by kind only,
/// at most once a second.
async fn count_input(input: Arc<InputQueue>) {
    let mut counts = [0u64; 5];
    let mut last_report = Instant::now();
    loop {
        let kind = match input.pop().await {
            ToApp::Pointer { .. } => 0,
            ToApp::Button { .. } => 1,
            ToApp::Scroll { .. } => 2,
            ToApp::Key { .. } => 3,
            _ => 4,
        };
        counts[kind] += 1;
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            let [moves, buttons, scrolls, keys, releases] = counts;
            eprintln!("nocomputer-host: input so far: {moves} moves, {buttons} buttons, {scrolls} scrolls, {keys} keys, {releases} release-alls");
        }
    }
}

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(()) => 0,
        Err(Exit(code, message)) => {
            eprintln!("nocomputer-host: {message}");
            code
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn next(rx: &mut mpsc::UnboundedReceiver<Capture>, within: Duration) -> Option<Capture> {
        tokio::time::timeout(within, rx.recv()).await.ok().flatten()
    }

    #[tokio::test]
    async fn view_sizes_are_coalesced_and_the_newest_wins() {
        let (events, rx) = mpsc::unbounded_channel();
        let (capture, mut commands) = mpsc::unbounded_channel();
        let (clipboard, _clipboard_rx) = watch::channel(None);
        let (notices, _notices_rx) = mpsc::unbounded_channel();
        tokio::spawn(route_events(rx, capture, Arc::new(InputQueue::default()), clipboard, notices));
        let device: nocomputer_proto::Fingerprint = "00".repeat(32).parse().unwrap();
        let short = Duration::from_millis(100);

        events.send(HostEvent::ViewSize { width: 1, height: 1 }).unwrap(); // no session: dropped
        events.send(HostEvent::Session { device, name: "v".into(), reported_name: "v".into() }).unwrap();
        assert_eq!(next(&mut commands, short).await, Some(Capture::ReleaseAll));
        assert_eq!(next(&mut commands, short).await, Some(Capture::Start));
        for width in [1366, 1200, 1000] {
            events.send(HostEvent::ViewSize { width, height: 700 }).unwrap();
        }
        assert_eq!(next(&mut commands, short).await, Some(Capture::ViewSize { width: 1366, height: 700 }), "the first goes at once");
        assert_eq!(next(&mut commands, short).await, None, "the rest wait for the interval");
        assert_eq!(next(&mut commands, VIEW_SIZE_INTERVAL).await, Some(Capture::ViewSize { width: 1000, height: 700 }), "only the newest");
        assert_eq!(next(&mut commands, VIEW_SIZE_INTERVAL * 2).await, None);

        events.send(HostEvent::ViewSize { width: 800, height: 600 }).unwrap();
        assert_eq!(next(&mut commands, short).await, Some(Capture::ViewSize { width: 800, height: 600 }), "after a quiet interval, at once");
        events.send(HostEvent::ViewSize { width: 640, height: 480 }).unwrap();
        events.send(HostEvent::SessionEnded { device }).unwrap();
        assert_eq!(next(&mut commands, short).await, Some(Capture::ReleaseAll));
        assert_eq!(next(&mut commands, short).await, Some(Capture::Stop));
        assert_eq!(next(&mut commands, VIEW_SIZE_INTERVAL * 2).await, None, "a size still waiting is forgotten with the session");
    }

    /// stdout belongs to local-v5 frames.
    #[test]
    fn nothing_prints_to_stdout() {
        for (name, source) in [("main.rs", include_str!("main.rs")), ("local.rs", include_str!("local.rs")), ("h264.rs", include_str!("h264.rs")), ("input.rs", include_str!("input.rs"))] {
            let code = source.split("#[cfg(test)]").next().unwrap();
            for mac in [concat!("print", "ln!"), concat!("print", "!"), concat!("dbg", "!")] {
                let stdout = code.match_indices(mac).any(|(i, _)| !code[..i].ends_with('e'));
                assert!(!stdout, "{name} uses {mac}");
            }
        }
    }
}
