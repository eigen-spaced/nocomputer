//! The start screen: saved hosts, pairing a new one, and logging in. A login hands its
//! session to `live.rs`.
//!
//! Each step is one command, and the connection waits here between them: `connect_open`
//! connects (pinned if the host is saved), then either `connect_pair` asks the host's user
//! to approve this device or `connect_login` sends the account's username and password.
//! `connect_cancel` ends whatever is open or in progress. Each command takes a
//! [`Ticket`] before its first await; cancelling, or a newer command, makes it stale,
//! and a stale command closes what it got instead of keeping it.
//!
//! The password arrives as a command argument and is moved into a [`Password`] at once,
//! which clears it when dropped. It is never logged, stored, or put in an event or error.
//! The page's copy and the IPC buffer are outside this process's reach.

use crate::live::Live;
use nocomputer_proto::{
    load_or_create_identity, paths, platform_keystore, valid_username, Error, ErrorKind, Fingerprint, FirstContact, KnownHosts, LoginPrompt, LoginResult, Outcome,
    Password, Viewer,
};
use serde::Serialize;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// What the page shows next.
#[derive(Debug, Serialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// A host this device hasn't pinned, or (`saved`) one it pinned that no longer has it
    /// paired, presenting the pinned identity. Both fingerprints must match what the host shows.
    Pair { address: String, host_name: String, host_fingerprint: String, this_fingerprint: String, already_paired: bool, saved: bool },
    /// `failed`: the last attempt was wrong; the next can go after `retry_after_ms`.
    Login { address: String, host_name: String, failed: bool, retry_after_ms: u64 },
    /// The session is up; `live_next` has frames.
    Live,
    /// `connect_cancel` ended it.
    Cancelled,
}

/// An error in plain words, with its kind for the page to act on.
#[derive(Debug, Serialize)]
pub struct Problem {
    kind: &'static str,
    message: String,
    /// For a changed host identity: the pinned and the presented fingerprints.
    expected: Option<String>,
    presented: Option<String>,
}

impl Problem {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Problem { kind, message: message.into(), expected: None, presented: None }
    }
}

/// What went wrong, for the start screen or the status bar.
pub fn explain(e: &Error) -> String {
    problem(e).message
}

fn problem(e: &Error) -> Problem {
    let (kind, message) = match e.kind() {
        ErrorKind::Unreachable => ("unreachable", "Can't reach the host. Check that it's on and that Remote access is turned on.".into()),
        ErrorKind::Disconnected => ("disconnected", "The connection to the host was lost.".into()),
        ErrorKind::HostIdentityChanged => (
            "identity_changed",
            "Warning: the host at this address isn't the one this PC paired with. Its identity has changed, so nothing was sent to it. \
             If you reset the host yourself, forget it below and pair again; otherwise don't."
                .into(),
        ),
        ErrorKind::NotPaired => ("not_paired", "The host no longer has this PC paired. Choose Pair again to ask its user to approve this PC.".into()),
        ErrorKind::Revoked => (
            "revoked",
            "The host's user has removed this PC. To use it again, ask them to choose Allow pairing again on the host, then choose Pair again.".into(),
        ),
        ErrorKind::Busy => ("busy", "Another viewer is connected to the host. Try again when it disconnects.".into()),
        ErrorKind::HostEnded => ("host_ended", "The host ended the session.".into()),
        ErrorKind::Replaced => ("replaced", "This PC connected to the host again from elsewhere, so this session was closed.".into()),
        ErrorKind::VersionMismatch => ("version", "The host and this viewer run different versions. Update the older one.".into()),
        ErrorKind::PairingUnavailable => {
            ("pairing_unavailable", "The host is busy with another pairing request, or one was just declined. Wait a few seconds and try again.".into())
        }
        ErrorKind::LoginFailed => ("login_limit", "Too many failed logins. Connect again to try again.".into()),
        ErrorKind::Other => ("other", format!("Something went wrong: {e}")),
    };
    let mut p = Problem::new(kind, message);
    if let Error::HostIdentityChanged { expected, presented } = e {
        (p.expected, p.presented) = (Some(expected.to_string()), Some(presented.to_string()));
    }
    p
}

enum Stage {
    Idle,
    Pairing { address: String, contact: FirstContact },
    Login { address: String, prompt: LoginPrompt },
}

/// The latest command. A cancel that comes before the command reaches its first wait
/// still counts: `Notify` keeps the permit.
#[derive(Clone)]
struct Ticket {
    id: u64,
    cancel: Arc<Notify>,
}

pub struct Connector {
    stage: Mutex<Stage>,
    ticket: Mutex<Ticket>,
    /// `--via`: (address, where to dial instead), still pinned to the address's identity.
    via: Option<(String, String)>,
}

impl Connector {
    pub fn new(via: Option<(String, String)>) -> Self {
        let via = via.map(|(a, v)| (paths::with_default_port(&a), paths::with_default_port(&v)));
        Connector { stage: Mutex::new(Stage::Idle), ticket: Mutex::new(Ticket { id: 0, cancel: Default::default() }), via }
    }

    /// Makes every earlier command stale; call before a command's first await.
    fn ticket(&self) -> Ticket {
        let mut t = self.ticket.lock().unwrap();
        *t = Ticket { id: t.id + 1, cancel: Default::default() };
        t.clone()
    }

    fn is_current(&self, t: &Ticket) -> bool {
        self.ticket.lock().unwrap().id == t.id
    }

    /// Stores the stage for the next command, or closes it if `t` went stale.
    async fn keep(&self, t: &Ticket, stage: Stage) -> bool {
        let kept = {
            let mut current = self.stage.lock().unwrap();
            let fresh = self.ticket.lock().unwrap().id == t.id;
            if fresh {
                *current = stage;
                None
            } else {
                Some(stage)
            }
        };
        match kept {
            None => true,
            Some(stale) => {
                close(stale).await;
                false
            }
        }
    }

    fn take(&self) -> Stage {
        std::mem::replace(&mut *self.stage.lock().unwrap(), Stage::Idle)
    }

    /// Runs `f` until it finishes, or until `t` is cancelled or stale; dropping `f`
    /// closes whatever connection it holds.
    async fn cancellable<T>(&self, t: &Ticket, f: impl Future<Output = T>) -> Option<T> {
        if !self.is_current(t) {
            return None;
        }
        tokio::select! {
            out = f => Some(out),
            _ = t.cancel.notified() => None,
        }
    }

    /// Connects to `address`, pinned to `pin` if given. `None` if cancelled or stale.
    async fn dial(&self, t: &Ticket, address: &str, pin: Option<Fingerprint>) -> Result<Option<(Outcome, Viewer)>, Problem> {
        let dial = match &self.via {
            Some((a, via)) if a == address => via.clone(),
            _ => address.to_string(),
        };
        let viewer = viewer()?;
        let connect = async {
            let addr = paths::resolve(&dial).await.map_err(|e| Problem::new("unreachable", format!("Can't find {dial}: {e}")))?;
            viewer.connect(addr, pin).await.map_err(|e| problem(&e))
        };
        match self.cancellable(t, connect).await {
            Some(outcome) => Ok(Some((outcome?, viewer))),
            None => Ok(None),
        }
    }

    async fn open(&self, t: &Ticket, address: &str) -> Result<Step, Problem> {
        let pinned = known_hosts().get(address).map_err(|e| problem(&e))?;
        let Some((outcome, viewer)) = self.dial(t, address, pinned.map(|h| h.fingerprint)).await? else { return Ok(Step::Cancelled) };
        let (step, stage) = match outcome {
            Outcome::Login(prompt) => {
                let step = Step::Login { address: address.to_string(), host_name: prompt.host_name.clone(), failed: false, retry_after_ms: 0 };
                (step, Stage::Login { address: address.to_string(), prompt })
            }
            Outcome::FirstContact(contact) => {
                let step = Step::Pair {
                    address: address.to_string(),
                    host_name: contact.host_name.clone(),
                    host_fingerprint: contact.host_fingerprint.to_string(),
                    this_fingerprint: viewer.fingerprint().to_string(),
                    already_paired: contact.already_paired,
                    saved: false,
                };
                (step, Stage::Pairing { address: address.to_string(), contact })
            }
        };
        Ok(if self.keep(t, stage).await { step } else { Step::Cancelled })
    }
}

fn viewer() -> Result<Viewer, Problem> {
    let state = paths::default_state_dir();
    let identity = platform_keystore(&state, "device").and_then(|s| load_or_create_identity(s.as_ref())).map_err(|e| problem(&e))?;
    Viewer::new(identity, device_name()).map_err(|e| problem(&e))
}

fn device_name() -> String {
    let raw = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "nocomputer viewer".into());
    raw.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string()
}

fn known_hosts() -> KnownHosts {
    KnownHosts::at(paths::default_state_dir().join("known-hosts.json"))
}

#[derive(Debug, Serialize)]
pub struct SavedHost {
    address: String,
    name: String,
    fingerprint: String,
}

#[tauri::command]
pub fn connect_hosts() -> Result<Vec<SavedHost>, Problem> {
    let hosts = known_hosts().list().map_err(|e| problem(&e))?;
    Ok(hosts.into_iter().map(|h| SavedHost { address: h.address, name: h.name, fingerprint: h.fingerprint.to_string() }).collect())
}

#[tauri::command]
pub fn connect_forget(address: String) -> Result<(), Problem> {
    known_hosts().forget(&address).map(|_| ()).map_err(|e| problem(&e))
}

/// Connects to `address` (a name or IP, with or without a port), ending anything open.
#[tauri::command]
pub async fn connect_open(connector: tauri::State<'_, Connector>, address: String) -> Result<Step, Problem> {
    let t = connector.ticket();
    let address = address.trim();
    if address.is_empty() || address.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Problem::new("bad_input", "Enter the host's name or IP address."));
    }
    close(connector.take()).await;
    connector.open(&t, &paths::with_default_port(address)).await
}

/// Starts pairing again with a saved host that no longer has this device paired. The
/// host must still present the saved identity; the saved entry is replaced only once
/// `connect_pair` gets the host's approval.
#[tauri::command]
pub async fn connect_pair_again(connector: tauri::State<'_, Connector>, address: String) -> Result<Step, Problem> {
    let t = connector.ticket();
    close(connector.take()).await;
    let address = paths::with_default_port(address.trim());
    let Some(saved) = known_hosts().get(&address).map_err(|e| problem(&e))? else {
        return connector.open(&t, &address).await;
    };
    let Some((outcome, viewer)) = connector.dial(&t, &address, None).await? else { return Ok(Step::Cancelled) };
    let Outcome::FirstContact(contact) = outcome else {
        return Err(Problem::new("other", "The host answered unexpectedly; connect again."));
    };
    if contact.host_fingerprint != saved.fingerprint {
        let presented = contact.host_fingerprint;
        drop(contact);
        return Err(problem(&Error::HostIdentityChanged { expected: saved.fingerprint, presented }));
    }
    let step = Step::Pair {
        address: address.clone(),
        host_name: contact.host_name.clone(),
        host_fingerprint: contact.host_fingerprint.to_string(),
        this_fingerprint: viewer.fingerprint().to_string(),
        already_paired: contact.already_paired,
        saved: true,
    };
    Ok(if connector.keep(&t, Stage::Pairing { address, contact }).await { step } else { Step::Cancelled })
}

/// Asks the host's user to approve this device, then connects again pinned to it.
#[tauri::command]
pub async fn connect_pair(connector: tauri::State<'_, Connector>) -> Result<Step, Problem> {
    let t = connector.ticket();
    let Stage::Pairing { address, contact } = connector.take() else {
        return Err(Problem::new("other", "Nothing is waiting to pair; connect again."));
    };
    let (fingerprint, name) = (contact.host_fingerprint, contact.host_name.clone());
    // The host gives an unpaired device 30 s to ask, and the user may have taken longer
    // to compare fingerprints: ask on a fresh connection, from the same host.
    drop(contact);
    let Some((outcome, _)) = connector.dial(&t, &address, None).await? else { return Ok(Step::Cancelled) };
    let contact = match outcome {
        Outcome::FirstContact(c) if c.host_fingerprint == fingerprint => c,
        _ => return Err(Problem::new("identity_changed", "The host's fingerprint changed after you checked it, so nothing was asked. Connect again and compare it once more.")),
    };
    let Some(approved) = connector.cancellable(&t, contact.request_pairing()).await else { return Ok(Step::Cancelled) };
    if !approved.map_err(|e| problem(&e))? {
        return Err(Problem::new("declined", "The host didn't approve this PC: the request was declined, or no one answered in time."));
    }
    known_hosts().add(&address, fingerprint, &name).map_err(|e| problem(&e))?;
    connector.open(&t, &address).await
}

#[tauri::command]
pub async fn connect_login(connector: tauri::State<'_, Connector>, live: tauri::State<'_, Live>, username: String, password: String) -> Result<Step, Problem> {
    // First, so it is cleared on every path below.
    let password = Password::new(password);
    let t = connector.ticket();
    let Stage::Login { address, prompt } = connector.take() else {
        return Err(Problem::new("other", "Nothing is waiting for a login; connect again."));
    };
    let username = username.trim();
    let password = match (password, valid_username(username)) {
        (Some(password), true) => password,
        (password, valid) => {
            drop(password);
            connector.keep(&t, Stage::Login { address, prompt }).await;
            let message = if valid { "Enter the account's password." } else { "Enter the host account's username." };
            return Err(Problem::new("bad_input", message));
        }
    };
    let host_name = prompt.host_name.clone();
    let Some(result) = connector.cancellable(&t, prompt.login(username, password)).await else { return Ok(Step::Cancelled) };
    match result.map_err(|e| problem(&e))? {
        LoginResult::Session(session) if connector.is_current(&t) => {
            live.begin(session, address);
            Ok(Step::Live)
        }
        LoginResult::Session(session) => {
            session.close().await;
            Ok(Step::Cancelled)
        }
        LoginResult::Failed { retry_after, prompt } => {
            if !connector.keep(&t, Stage::Login { address: address.clone(), prompt }).await {
                return Ok(Step::Cancelled);
            }
            Ok(Step::Login { address, host_name, failed: true, retry_after_ms: retry_after.as_millis() as u64 })
        }
    }
}

/// Ends the step in progress, or the connection waiting between steps.
#[tauri::command]
pub async fn connect_cancel(connector: tauri::State<'_, Connector>) -> Result<(), ()> {
    let stale = {
        let mut t = connector.ticket.lock().unwrap();
        let next = Ticket { id: t.id + 1, cancel: Default::default() };
        std::mem::replace(&mut *t, next)
    };
    stale.cancel.notify_one();
    close(connector.take()).await;
    Ok(())
}

async fn close(stage: Stage) {
    match stage {
        Stage::Login { prompt, .. } => prompt.close().await,
        Stage::Pairing { contact, .. } => drop(contact),
        Stage::Idle => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<T>(f: impl Future<Output = T>) -> T {
        tauri::async_runtime::block_on(f)
    }

    #[test]
    fn a_newer_command_or_a_cancel_makes_a_command_stale() {
        let c = Connector::new(None);
        let first = c.ticket();
        let second = c.ticket();
        assert!(!c.is_current(&first) && c.is_current(&second));
        assert_eq!(block_on(c.cancellable(&first, async { 1 })), None, "a stale command doesn't run on");
        assert!(!block_on(c.keep(&first, Stage::Idle)), "nor keep what it got");
        assert!(block_on(c.keep(&second, Stage::Idle)));
        assert_eq!(block_on(c.cancellable(&second, async { 2 })), Some(2));
    }

    #[test]
    fn a_cancel_before_the_wait_still_counts() {
        let c = Connector::new(None);
        let t = c.ticket();
        t.cancel.notify_one(); // as connect_cancel does, before the command reaches select!
        assert_eq!(block_on(c.cancellable(&t, std::future::pending::<()>())), None);
    }
}
