//! Loopback tests for pairing, authorization and impersonation.

use crate::host::{Approver, Host, HostConfig};
use crate::wire::{read_msg, valid_device_name, write_msg, ErrorCode, Message, MAX_CONTROL_MESSAGE};
use crate::{tls, Error, Fingerprint, Identity, MemoryKeyStore, Outcome, PairedClients, Viewer};
use rustls::sign::CertifiedKey;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

const LOOPBACK: ([u8; 4], u16) = ([127, 0, 0, 1], 0);

struct TestHost {
    host: Arc<Host>,
    addr: SocketAddr,
    prompts: Arc<Mutex<Vec<crate::PairingPrompt>>>,
}

fn approver(answer: bool, prompts: Arc<Mutex<Vec<crate::PairingPrompt>>>) -> Approver {
    Arc::new(move |p| {
        prompts.lock().unwrap().push(p);
        Box::pin(async move { answer })
    })
}

fn start(host: Host, prompts: Arc<Mutex<Vec<crate::PairingPrompt>>>) -> TestHost {
    let host = Arc::new(host);
    let addr = host.local_addr().unwrap();
    let runner = host.clone();
    tokio::spawn(async move { runner.run().await });
    TestHost { host, addr, prompts }
}

fn host_with(identity: &Identity, answer: bool) -> TestHost {
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let host = Host::bind(LOOPBACK.into(), identity, HostConfig::new("test-host"), PairedClients::in_memory(), approver(answer, prompts.clone())).unwrap();
    start(host, prompts)
}

fn viewer() -> Viewer {
    Viewer::new(Identity::generate().unwrap(), "test-viewer").unwrap()
}

/// Presents `cert_of`'s certificate while signing with `key_of`'s key.
fn mismatched(cert_of: &Identity, key_of: &Identity) -> Arc<CertifiedKey> {
    let key = tls::provider().key_provider.load_private_key(key_of.key()).unwrap();
    Arc::new(CertifiedKey::new(vec![cert_of.cert()], key))
}

#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl rustls::server::ResolvesServerCert for Fixed {
    fn resolve(&self, _: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

impl rustls::client::ResolvesClientCert for Fixed {
    fn resolve(&self, _: &[&[u8]], _: &[rustls::SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

async fn pair(viewer: &Viewer, host: &TestHost) -> Fingerprint {
    let Outcome::FirstContact(first) = viewer.connect(host.addr, None).await.unwrap() else { panic!("expected first contact") };
    assert!(!first.already_paired);
    let host_fp = first.host_fingerprint;
    assert!(first.request_pairing().await.unwrap());
    host_fp
}

fn remote_code(result: Result<Outcome, Error>) -> ErrorCode {
    match result {
        Err(Error::Remote { code, .. }) => code,
        Err(e) => panic!("expected a refusal from the host, got {e}"),
        Ok(_) => panic!("expected a refusal from the host, got a connection"),
    }
}

#[tokio::test]
async fn pairs_once_then_reconnects_without_prompt() {
    let host_id = Identity::generate().unwrap();
    let host = host_with(&host_id, true);
    let viewer = viewer();

    let host_fp = pair(&viewer, &host).await;
    assert_eq!(host_fp, host_id.fingerprint());
    {
        let prompts = host.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        // The approved fingerprint is the one that authenticated the TLS connection.
        assert_eq!(prompts[0].device_fingerprint, viewer.fingerprint());
        assert_eq!(prompts[0].host_fingerprint, host_id.fingerprint());
        assert_eq!(prompts[0].device_name, "test-viewer");
    }

    for _ in 0..2 {
        let Outcome::Ready(mut session) = viewer.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!("expected session") };
        assert_eq!(session.host_name, "test-host");
        session.ping().await.unwrap();
        session.close().await;
    }
    assert_eq!(host.prompts.lock().unwrap().len(), 1, "no prompt after pairing");
}

#[tokio::test]
async fn sessions_are_named_from_the_pairing_not_the_hello() {
    let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let mut config = HostConfig::new("test-host");
    config.events = Some(events);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let host = start(Host::bind(LOOPBACK.into(), &Identity::generate().unwrap(), config, PairedClients::in_memory(), approver(true, prompts.clone())).unwrap(), prompts);
    let identity = Identity::generate().unwrap();
    let device = identity.fingerprint();
    let copy = Identity::from_bytes(&identity.to_bytes()).unwrap();
    let host_fp = pair(&Viewer::new(identity, "latitude").unwrap(), &host).await;

    // The same paired device later calls itself something else.
    let renamed = Viewer::new(copy, "someone-else").unwrap();
    let Outcome::Ready(session) = renamed.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!("expected session") };
    session.close().await;
    let mut events = Vec::new();
    while let Ok(e) = seen.try_recv() {
        events.push(e);
    }
    let expected = crate::HostEvent::Session { device, name: "latitude".into(), reported_name: "someone-else".into() };
    assert!(events.contains(&expected), "{events:?}");
}

#[tokio::test]
async fn one_controller_at_a_time() {
    let host = host_with(&Identity::generate().unwrap(), true);
    let (first, second) = (viewer(), viewer());
    let host_fp = pair(&first, &host).await;
    pair(&second, &host).await;

    let Outcome::Ready(mut active) = first.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!() };
    assert_eq!(remote_code(second.connect(host.addr, Some(host_fp)).await), ErrorCode::Busy);
    active.ping().await.unwrap();

    // Closing frees the slot.
    active.close().await;
    let Outcome::Ready(mut active) = second.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!("slot was not freed by close") };
    active.ping().await.unwrap();
    assert_eq!(remote_code(first.connect(host.addr, Some(host_fp)).await), ErrorCode::Busy);

    // So does revocation.
    assert!(host.host.revoke(&second.fingerprint()).unwrap());
    let Outcome::Ready(mut active) = first.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!("slot was not freed by revocation") };
    active.ping().await.unwrap();
}

#[tokio::test]
async fn same_device_reconnecting_replaces_its_session() {
    let host = host_with(&Identity::generate().unwrap(), true);
    let viewer = viewer();
    let host_fp = pair(&viewer, &host).await;
    let Outcome::Ready(mut old) = viewer.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!() };
    old.ping().await.unwrap();

    let Outcome::Ready(mut new) = viewer.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!("same device was refused") };
    new.ping().await.unwrap();
    match old.ping().await {
        Err(Error::Remote { code: ErrorCode::Replaced, .. }) => {}
        other => panic!("old session must end as replaced, got {other:?}"),
    }
    // The old session's end must not free the slot the new one holds.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let other = Viewer::new(Identity::generate().unwrap(), "other").unwrap();
    pair(&other, &host).await;
    assert_eq!(remote_code(other.connect(host.addr, Some(host_fp)).await), ErrorCode::Busy);
    new.ping().await.unwrap();
}

#[tokio::test]
async fn first_contact_never_yields_a_session_even_when_paired() {
    let host = host_with(&Identity::generate().unwrap(), true);
    let viewer = viewer();
    pair(&viewer, &host).await;
    let Outcome::FirstContact(first) = viewer.connect(host.addr, None).await.unwrap() else { panic!("unpinned connect must not be ready") };
    assert!(first.already_paired);
}

#[tokio::test]
async fn denial_is_not_paired_and_starts_cooldown() {
    let host = host_with(&Identity::generate().unwrap(), false);
    let viewer = viewer();
    let Outcome::FirstContact(first) = viewer.connect(host.addr, None).await.unwrap() else { panic!() };
    let host_fp = first.host_fingerprint;
    assert!(!first.request_pairing().await.unwrap());
    assert_eq!(remote_code(viewer.connect(host.addr, Some(host_fp)).await), ErrorCode::NotPaired);

    let Outcome::FirstContact(again) = viewer.connect(host.addr, None).await.unwrap() else { panic!() };
    match again.request_pairing().await {
        Err(Error::Remote { code: ErrorCode::PairingUnavailable, .. }) => {}
        other => panic!("expected cooldown refusal, got {:?}", other.map_err(|e| e.to_string())),
    }
    assert_eq!(host.prompts.lock().unwrap().len(), 1, "no prompt during cooldown");
}

#[tokio::test]
async fn revocation_closes_live_session_and_blocks_reconnect_and_repair() {
    let host = host_with(&Identity::generate().unwrap(), true);
    let viewer = viewer();
    let host_fp = pair(&viewer, &host).await;
    let Outcome::Ready(mut session) = viewer.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!() };
    session.ping().await.unwrap();

    assert!(host.host.revoke(&viewer.fingerprint()).unwrap());
    match session.ping().await {
        Err(Error::Remote { code: ErrorCode::Revoked, .. }) => {}
        other => panic!("live session must end as revoked, got {other:?}"),
    }

    assert_eq!(remote_code(viewer.connect(host.addr, Some(host_fp)).await), ErrorCode::Revoked);
    assert_eq!(remote_code(viewer.connect(host.addr, None).await), ErrorCode::Revoked);
    assert_eq!(host.prompts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn different_host_key_is_reported_as_identity_change() {
    let real = Identity::generate().unwrap();
    let real_host = host_with(&real, true);
    let viewer = viewer();
    let pinned = pair(&viewer, &real_host).await;

    let impostor = host_with(&Identity::generate().unwrap(), true);
    match viewer.connect(impostor.addr, Some(pinned)).await {
        Err(Error::HostIdentityChanged { expected, presented }) => {
            assert_eq!(expected, pinned);
            assert_ne!(presented, pinned);
        }
        Err(e) => panic!("expected identity change, got {e}"),
        Ok(_) => panic!("connected to an impostor"),
    }
    assert!(impostor.prompts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn host_certificate_without_its_key_fails_the_handshake() {
    // The impostor presents the real host's (public) certificate, so the pin matches,
    // but cannot sign with the real key: only the handshake signature check stops it.
    let real = Identity::generate().unwrap();
    let real_host = host_with(&real, true);
    let viewer = viewer();
    let pinned = pair(&viewer, &real_host).await;

    let attacker_key = Identity::generate().unwrap();
    let tls = tls::server_builder().unwrap().with_cert_resolver(Arc::new(Fixed(mismatched(&real, &attacker_key))));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let impostor = Host::bind_with(LOOPBACK.into(), tls, real.fingerprint(), HostConfig::new("impostor"), PairedClients::in_memory(), approver(true, prompts.clone())).unwrap();
    let impostor = start(impostor, prompts);

    for pin in [Some(pinned), None] {
        match viewer.connect(impostor.addr, pin).await {
            Err(Error::Connection(_)) => {}
            Err(e) => panic!("expected a handshake failure, got {e}"),
            Ok(_) => panic!("accepted a certificate without its key"),
        }
    }
}

#[tokio::test]
async fn client_certificate_without_its_key_fails_the_handshake() {
    let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let mut config = HostConfig::new("test-host");
    config.events = Some(events);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let host_id = Identity::generate().unwrap();
    let host = start(Host::bind(LOOPBACK.into(), &host_id, config, PairedClients::in_memory(), approver(true, prompts.clone())).unwrap(), prompts);
    let paired_id = Identity::generate().unwrap();
    let paired_copy = Identity::from_bytes(&paired_id.to_bytes()).unwrap();
    let paired = Viewer::new(paired_id, "paired").unwrap();
    let host_fp = pair(&paired, &host).await;

    // Another device replays the paired device's certificate with its own key.
    let thief_key = Identity::generate().unwrap();
    let mut thief = Viewer::new(Identity::generate().unwrap(), "thief").unwrap();
    thief.client_cert_override = Some(Arc::new(Fixed(mismatched(&paired_copy, &thief_key))));
    // In QUIC the client finishes its side of the handshake before the host has checked
    // the client's signature, so the thief sees the connection drop rather than a TLS error.
    match thief.connect(host.addr, Some(host_fp)).await {
        Err(Error::Connection(_) | Error::Io(_)) => {}
        Err(e) => panic!("expected the host to drop the connection, got {e}"),
        Ok(_) => panic!("host accepted a certificate without its key"),
    }
    assert_eq!(host.prompts.lock().unwrap().len(), 1);
    // The host saw the pairing and one session-free pairing connection, then a failed
    // handshake: never a session for the thief.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let mut events = Vec::new();
    while let Ok(e) = seen.try_recv() {
        events.push(e);
    }
    assert!(!events.iter().any(|e| matches!(e, crate::HostEvent::Session { .. })), "{events:?}");
    assert!(
        events.iter().any(|e| matches!(e, crate::HostEvent::Rejected { device: None, reason } if reason.starts_with("handshake failed"))),
        "{events:?}"
    );
}

#[tokio::test]
async fn unpaired_device_can_only_ask_to_pair() {
    let host = host_with(&Identity::generate().unwrap(), true);
    let viewer = viewer();
    let Outcome::FirstContact(mut first) = viewer.connect(host.addr, None).await.unwrap() else { panic!() };
    let (send, recv) = first.channel_mut();
    write_msg(send, &Message::Ping { seq: 1 }).await.unwrap();
    match read_msg(recv).await.unwrap() {
        Message::Error { code: ErrorCode::NotPaired, .. } => {}
        other => panic!("expected not_paired, got {other:?}"),
    }
    assert!(host.prompts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn version_mismatch_is_refused() {
    let host = host_with(&Identity::generate().unwrap(), true);
    let id = Identity::generate().unwrap();
    let tls = tls::client_builder(None, Arc::default()).unwrap().with_client_auth_cert(vec![id.cert()], id.key()).unwrap();
    let endpoint = quinn::Endpoint::client(LOOPBACK.into()).unwrap();
    let conn = endpoint.connect_with(tls::finish_client(tls).unwrap(), host.addr, crate::SERVER_NAME).unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_msg(&mut send, &Message::Hello { min_version: crate::PROTOCOL_VERSION + 1, max_version: crate::PROTOCOL_VERSION + 2, device_name: "future".into() }).await.unwrap();
    match read_msg(&mut recv).await.unwrap() {
        Message::Error { code: ErrorCode::VersionUnsupported, .. } => {}
        other => panic!("expected version_unsupported, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_control_message_is_rejected_before_allocation() {
    let (mut a, mut b) = tokio::io::duplex(64);
    use tokio::io::AsyncWriteExt;
    a.write_all(&((MAX_CONTROL_MESSAGE as u32) + 1).to_be_bytes()).await.unwrap();
    assert!(matches!(read_msg(&mut b).await, Err(Error::Protocol(_))));

    let (mut a, mut b) = tokio::io::duplex(1024);
    let msg = Message::Welcome { version: 1, host_name: "h".into(), paired: true };
    write_msg(&mut a, &msg).await.unwrap();
    assert_eq!(read_msg(&mut b).await.unwrap(), msg);
}

#[test]
fn unknown_fields_are_tolerated_but_unknown_types_are_not() {
    let with_extra: Message = serde_json::from_str(r#"{"type":"ping","seq":3,"later":true}"#).unwrap();
    assert_eq!(with_extra, Message::Ping { seq: 3 });
    assert!(serde_json::from_str::<Message>(r#"{"type":"teleport"}"#).is_err());
    let code: Message = serde_json::from_str(r#"{"type":"error","code":"from_the_future","message":""}"#).unwrap();
    assert_eq!(code, Message::Error { code: ErrorCode::Unknown, message: String::new() });
}

#[test]
fn device_names_cannot_disguise_prompts() {
    assert!(valid_device_name("Sunny's Latitude"));
    assert!(valid_device_name("Ünïcode ok"));
    for bad in ["", " padded", "line\nbreak", "tab\there", "rtl\u{202E}gnp.exe", "zero\u{200B}width", &"x".repeat(65)] {
        assert!(!valid_device_name(bad), "{bad:?} should be rejected");
    }
}

#[test]
fn fingerprints_display_in_full_and_parse_back() {
    let id = Identity::generate().unwrap();
    let fp = id.fingerprint();
    let shown = fp.to_string();
    assert_eq!(shown.len(), 16 * 4 + 15);
    assert_eq!(shown.parse::<Fingerprint>().unwrap(), fp);
    assert_eq!(shown.replace(' ', ":").to_lowercase().parse::<Fingerprint>().unwrap(), fp);
    assert!("ABCD".parse::<Fingerprint>().is_err());
    assert!(!format!("{id:?}").contains("PRIVATE"));
}

#[test]
fn identity_round_trips_and_rejects_mismatched_keys() {
    let store = MemoryKeyStore::default();
    let first = crate::load_or_create_identity(&store).unwrap();
    let again = crate::load_or_create_identity(&store).unwrap();
    assert_eq!(first.fingerprint(), again.fingerprint());

    // First identity's certificate with another identity's key.
    let other = Identity::generate().unwrap();
    let key = other.key().secret_der().to_vec();
    let mut spliced = b"NCID1".to_vec();
    for part in [first.cert().as_ref(), &key[..]] {
        spliced.extend_from_slice(&(part.len() as u32).to_be_bytes());
        spliced.extend_from_slice(part);
    }
    assert!(matches!(Identity::from_bytes(&spliced), Err(Error::BadIdentity)));
    assert!(matches!(Identity::from_bytes(b"garbage"), Err(Error::BadIdentity)));
}

#[test]
fn paired_clients_persist_and_revocation_sticks() {
    let dir = std::env::temp_dir().join(format!("nocomputer-proto-test-{}", std::process::id()));
    let path = dir.join("paired-clients.json");
    let fp = Identity::generate().unwrap().fingerprint();
    {
        let mut clients = PairedClients::at(&path);
        assert_eq!(clients.status(&fp), crate::ClientStatus::Unknown);
        clients.add(fp, "laptop").unwrap();
        assert!(clients.add(fp, "again").is_err());
    }
    let mut reopened = PairedClients::at(&path);
    assert_eq!(reopened.status(&fp), crate::ClientStatus::Paired { name: "laptop".into() });
    assert!(reopened.revoke(&fp).unwrap());
    assert_eq!(PairedClients::at(&path).status(&fp), crate::ClientStatus::Revoked);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn known_hosts_pin_by_address() {
    let dir = std::env::temp_dir().join(format!("nocomputer-proto-hosts-{}", std::process::id()));
    let hosts = crate::KnownHosts::at(dir.join("known-hosts.json"));
    let fp = Identity::generate().unwrap().fingerprint();
    hosts.add("MacBook-Air:47900", fp, "mac").unwrap();
    assert_eq!(hosts.get("macbook-air:47900").unwrap().unwrap().fingerprint, fp);
    assert!(hosts.forget("macbook-air:47900").unwrap());
    assert!(hosts.get("macbook-air:47900").unwrap().is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(windows)]
#[test]
fn dpapi_store_round_trips_and_does_not_store_plaintext() {
    use crate::KeyStore;
    let dir = std::env::temp_dir().join(format!("nocomputer-proto-dpapi-{}", std::process::id()));
    let path = dir.join("identity.dpapi");
    let store = crate::keystore::dpapi::DpapiKeyStore::new(path.clone());
    let id = crate::load_or_create_identity(&store).unwrap();
    let on_disk = std::fs::read(&path).unwrap();
    let key = id.key().secret_der().to_vec();
    assert!(!on_disk.windows(key.len()).any(|w| w == key), "private key stored in plaintext");
    assert_eq!(Identity::from_bytes(&store.load().unwrap().unwrap()).unwrap().fingerprint(), id.fingerprint());
    std::fs::remove_dir_all(&dir).unwrap();
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::HostEvent>) -> Vec<crate::HostEvent> {
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    events
}

#[tokio::test]
async fn video_reaches_the_controller_in_order() {
    let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let mut config = HostConfig::new("test-host");
    config.events = Some(events);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let host = start(Host::bind(LOOPBACK.into(), &Identity::generate().unwrap(), config, PairedClients::in_memory(), approver(true, prompts.clone())).unwrap(), prompts);
    let viewer = viewer();
    let host_fp = pair(&viewer, &host).await;
    let Outcome::Ready(mut session) = viewer.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!() };
    session.ping().await.unwrap(); // the session loop, and so its video queue, is running
    assert!(drain(&mut seen).contains(&crate::HostEvent::KeyframeNeeded), "a new session needs a keyframe");

    // Deltas before the first keyframe can't be decoded and are never sent.
    host.host.send_access_unit(crate::AccessUnit::delta(vec![9]));
    host.host.send_access_unit(crate::AccessUnit::keyframe("avc1.4D0028", vec![1, 1]));
    for i in 2..5u8 {
        host.host.send_access_unit(crate::AccessUnit::delta(vec![i]));
    }

    let mut video = session.video();
    let mut got = Vec::new();
    let mut last_us = 0;
    for i in 0..6 {
        if i == 4 {
            // Sent once the first GOP is read: a newer GOP that has already arrived
            // supersedes unread frames of the current one.
            host.host.send_access_unit(crate::AccessUnit::keyframe("avc1.640028", vec![5, 5]));
            host.host.send_access_unit(crate::AccessUnit::delta(vec![6]));
        }
        let f = tokio::time::timeout(std::time::Duration::from_secs(5), video.next()).await.unwrap().unwrap();
        assert!(f.host_us >= last_us);
        last_us = f.host_us;
        got.push((f.codec.to_string(), f.keyframe, f.data));
    }
    let expected = vec![
        ("avc1.4D0028".to_string(), true, vec![1, 1]),
        ("avc1.4D0028".to_string(), false, vec![2]),
        ("avc1.4D0028".to_string(), false, vec![3]),
        ("avc1.4D0028".to_string(), false, vec![4]),
        ("avc1.640028".to_string(), true, vec![5, 5]),
        ("avc1.640028".to_string(), false, vec![6]),
    ];
    assert_eq!(got, expected);

    session.request_keyframe().await.unwrap();
    session.ping().await.unwrap();
    assert!(drain(&mut seen).contains(&crate::HostEvent::KeyframeNeeded), "viewer request must reach the capture side");
}

#[test]
fn slow_viewer_drops_to_the_next_keyframe() {
    use crate::video::{Pushed, VideoQueue, QUEUE_FRAMES};
    let queue = VideoQueue::new();
    assert_eq!(queue.push(crate::AccessUnit::delta(vec![0])), Pushed::Dropped, "nothing before the first keyframe");
    assert_eq!(queue.push(crate::AccessUnit::keyframe("avc1.4D0028", vec![1])), Pushed::Queued);
    for _ in 1..QUEUE_FRAMES {
        assert_eq!(queue.push(crate::AccessUnit::delta(vec![2])), Pushed::Queued);
    }
    // Full: the GOP is abandoned and a keyframe requested once.
    assert_eq!(queue.push(crate::AccessUnit::delta(vec![3])), Pushed::NeedKeyframe);
    assert_eq!(queue.push(crate::AccessUnit::delta(vec![4])), Pushed::Dropped);
    assert_eq!(queue.push(crate::AccessUnit::keyframe("avc1.4D0028", vec![5])), Pushed::Queued);
    assert_eq!(queue.push(crate::AccessUnit::delta(vec![6])), Pushed::Queued);
}

/// A raw host and viewer connection, so a test can write malformed or partial streams.
async fn raw_pair() -> (quinn::Connection, quinn::Connection, quinn::Endpoint, quinn::Endpoint) {
    let host_id = Identity::generate().unwrap();
    let tls = tls::server_builder().unwrap().with_single_cert(vec![host_id.cert()], host_id.key()).unwrap();
    let server = quinn::Endpoint::server(tls::finish_server(tls).unwrap(), LOOPBACK.into()).unwrap();
    let viewer_id = Identity::generate().unwrap();
    let tls = tls::client_builder(Some(host_id.fingerprint()), Arc::default()).unwrap().with_client_auth_cert(vec![viewer_id.cert()], viewer_id.key()).unwrap();
    let client = quinn::Endpoint::client(LOOPBACK.into()).unwrap();
    let connecting = client.connect_with(tls::finish_client(tls).unwrap(), server.local_addr().unwrap(), crate::SERVER_NAME).unwrap();
    let (accepted, connected) = tokio::join!(async { server.accept().await.unwrap().await.unwrap() }, connecting);
    (accepted, connected.unwrap(), server, client)
}

#[tokio::test]
async fn a_newer_gop_supersedes_unread_frames() {
    use crate::video::{write_frame, write_gop_header};
    let (host, viewer, _s, _c) = raw_pair().await;
    let mut video = crate::VideoReceiver::new(viewer);
    for (n, codec) in [(1u8, "avc1.4D0028"), (5, "avc1.640028")] {
        let mut gop = host.open_uni().await.unwrap();
        write_gop_header(&mut gop, codec).await.unwrap();
        write_frame(&mut gop, &crate::AccessUnit::keyframe(codec, vec![n]), 0).await.unwrap();
        write_frame(&mut gop, &crate::AccessUnit::delta(vec![n + 1]), 0).await.unwrap();
        gop.finish().unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Both GOPs are waiting; the viewer jumps to the newest keyframe.
    let first = video.next().await.unwrap();
    assert_eq!((first.keyframe, first.data, &*first.codec), (true, vec![5], "avc1.640028"));
    assert_eq!(video.next().await.unwrap().data, vec![6]);
}

#[tokio::test]
async fn reset_gop_never_reaches_the_decoder_without_its_keyframe() {
    use crate::video::{write_frame, write_gop_header};
    let (host, viewer, _s, _c) = raw_pair().await;
    let mut video = crate::VideoReceiver::new(viewer);
    let at = |n: u64| n * 1000;

    let mut gop = host.open_uni().await.unwrap();
    write_gop_header(&mut gop, "avc1.4D0028").await.unwrap();
    write_frame(&mut gop, &crate::AccessUnit::keyframe("avc1.4D0028", vec![1]), at(1)).await.unwrap();
    write_frame(&mut gop, &crate::AccessUnit::delta(vec![2]), at(2)).await.unwrap();
    assert_eq!(video.next().await.unwrap().data, vec![1]);
    assert_eq!(video.next().await.unwrap().data, vec![2]);

    // Half a frame, then the host abandons the GOP.
    let mut partial = 100u32.to_be_bytes().to_vec();
    partial.push(0);
    partial.extend_from_slice(&at(3).to_be_bytes());
    partial.extend_from_slice(&[3; 10]);
    gop.write_all(&partial).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    gop.reset(crate::video::RESET_GOP.into()).unwrap();

    // A stream that doesn't start at a keyframe is ignored whole.
    let mut orphan = host.open_uni().await.unwrap();
    write_gop_header(&mut orphan, "avc1.4D0028").await.unwrap();
    write_frame(&mut orphan, &crate::AccessUnit::delta(vec![4]), at(4)).await.unwrap();
    orphan.finish().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut next = host.open_uni().await.unwrap();
    write_gop_header(&mut next, "avc1.4D0028").await.unwrap();
    write_frame(&mut next, &crate::AccessUnit::keyframe("avc1.4D0028", vec![5]), at(5)).await.unwrap();
    write_frame(&mut next, &crate::AccessUnit::delta(vec![6]), at(6)).await.unwrap();

    let after: Vec<_> = [video.next().await.unwrap(), video.next().await.unwrap()].into_iter().map(|f| (f.keyframe, f.data)).collect();
    assert_eq!(after, vec![(true, vec![5]), (false, vec![6])]);
}

#[test]
fn input_and_notices_have_a_fixed_json_shape() {
    use crate::{Button, Input, KeyState, Notice, NoticeKind, ScrollUnit};
    let cases = [
        (Message::Input(Input::PointerMove { x: 0, y: 65535 }), r#"{"type":"input","kind":"pointer_move","x":0,"y":65535}"#),
        (Message::Input(Input::PointerButton { button: Button::Right, down: true }), r#"{"type":"input","kind":"pointer_button","button":"right","down":true}"#),
        (Message::Input(Input::Scroll { dx: 0, dy: -120, unit: ScrollUnit::Pixel }), r#"{"type":"input","kind":"scroll","dx":0,"dy":-120,"unit":"pixel"}"#),
        (Message::Input(Input::Key { usage: 0xE3, state: KeyState::Repeat }), r#"{"type":"input","kind":"key","usage":227,"state":"repeat"}"#),
        (Message::Input(Input::ReleaseAll), r#"{"type":"input","kind":"release_all"}"#),
        (
            Message::Notice(Notice { kind: NoticeKind::InputUnavailable, message: "no access".into() }),
            r#"{"type":"notice","kind":"input_unavailable","message":"no access"}"#,
        ),
    ];
    for (msg, json) in cases {
        assert_eq!(serde_json::to_string(&msg).unwrap(), json);
        assert_eq!(serde_json::from_str::<Message>(json).unwrap(), msg);
    }
    let future: Message = serde_json::from_str(r#"{"type":"input","kind":"pointer_button","button":"thumb","down":true}"#).unwrap();
    assert_eq!(future, Message::Input(Input::PointerButton { button: Button::Unknown, down: true }));
    assert!(serde_json::from_str::<Message>(r#"{"type":"input","kind":"pointer_move","x":70000,"y":0}"#).is_err());
}

#[test]
fn the_host_sanitizes_input() {
    use crate::{Button, Input, ScrollUnit};
    assert_eq!(Input::PointerButton { button: Button::Unknown, down: true }.sanitized(), None);
    assert_eq!(Input::Scroll { dx: i32::MIN, dy: 5, unit: ScrollUnit::Line }.sanitized(), Some(Input::Scroll { dx: -100, dy: 5, unit: ScrollUnit::Line }));
    assert_eq!(Input::Scroll { dx: 0, dy: 99_999, unit: ScrollUnit::Pixel }.sanitized(), Some(Input::Scroll { dx: 0, dy: 10_000, unit: ScrollUnit::Pixel }));
    assert_eq!(Input::Scroll { dx: 0, dy: 0, unit: ScrollUnit::Pixel }.sanitized(), None);
}

#[tokio::test]
async fn input_reaches_the_host_in_order_and_notices_reach_the_viewer() {
    use crate::{Button, HostEvent, Input, KeyState, Notice, NoticeKind};
    let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let mut config = HostConfig::new("test-host");
    config.events = Some(events);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let host = start(Host::bind(LOOPBACK.into(), &Identity::generate().unwrap(), config, PairedClients::in_memory(), approver(true, prompts.clone())).unwrap(), prompts);
    let viewer = viewer();
    let host_fp = pair(&viewer, &host).await;

    // No session yet: nobody to tell.
    host.host.notice(Notice { kind: NoticeKind::CaptureStopped, message: "nobody hears this".into() });
    let Outcome::Ready(mut session) = viewer.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!() };
    let sent = [
        Input::PointerMove { x: 1, y: 2 },
        Input::PointerButton { button: Button::Left, down: true },
        Input::PointerButton { button: Button::Unknown, down: true },
        Input::Key { usage: 0x04, state: KeyState::Down },
        Input::Key { usage: 0x04, state: KeyState::Repeat },
        Input::Key { usage: 0x04, state: KeyState::Up },
        Input::PointerButton { button: Button::Left, down: false },
        Input::ReleaseAll,
    ];
    for input in sent {
        session.input(input).await.unwrap();
    }
    session.ping().await.unwrap(); // everything before the ping has been handled
    let inputs: Vec<Input> = drain(&mut seen).into_iter().filter_map(|e| if let HostEvent::Input(i) = e { Some(i) } else { None }).collect();
    let expected: Vec<Input> = sent.into_iter().filter(|i| !matches!(i, Input::PointerButton { button: Button::Unknown, .. })).collect();
    assert_eq!(inputs, expected);

    let long = "é".repeat(crate::MAX_NOTICE);
    host.host.notice(Notice { kind: NoticeKind::InputUnavailable, message: long });
    let notice = tokio::time::timeout(std::time::Duration::from_secs(5), session.notice()).await.unwrap().unwrap();
    assert_eq!(notice.kind, NoticeKind::InputUnavailable);
    assert_eq!(notice.message, "é".repeat(crate::MAX_NOTICE / 2), "cut to the byte limit on a character boundary");
    // A notice between a ping and its pong doesn't disturb the ping.
    host.host.notice(Notice { kind: NoticeKind::CaptureStopped, message: "display asleep".into() });
    session.ping().await.unwrap();
    assert_eq!(session.notice().await.unwrap().kind, NoticeKind::CaptureStopped);
    session.close().await;
}

#[test]
fn clipboard_text_is_bounded_and_never_shown() {
    use crate::{ClipboardText, MAX_CLIPBOARD};
    assert!(ClipboardText::new(String::new()).is_none());
    assert!(ClipboardText::new("a\0b".into()).is_none());
    assert!(ClipboardText::new("x".repeat(MAX_CLIPBOARD + 1)).is_none());
    let max = ClipboardText::new("x".repeat(MAX_CLIPBOARD)).unwrap();
    assert_eq!(max.as_str().len(), MAX_CLIPBOARD);
    let secret = ClipboardText::new("hunter2".into()).unwrap();
    assert_eq!(format!("{secret:?}"), "ClipboardText(7 bytes)");
    assert_eq!(format!("{:?}", crate::HostEvent::Clipboard(secret)), "Clipboard(ClipboardText(7 bytes))");
}

#[tokio::test]
async fn clipboard_crosses_both_ways_beside_input() {
    use crate::{ClipboardText, HostEvent, Input, KeyState, MAX_CLIPBOARD};
    use std::time::Duration;
    let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let mut config = HostConfig::new("test-host");
    config.events = Some(events);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let host = start(Host::bind(LOOPBACK.into(), &Identity::generate().unwrap(), config, PairedClients::in_memory(), approver(true, prompts.clone())).unwrap(), prompts);
    let viewer = viewer();
    let host_fp = pair(&viewer, &host).await;
    // No session yet: dropped.
    host.host.send_clipboard(ClipboardText::new("nobody".into()).unwrap());
    let Outcome::Ready(mut session) = viewer.connect(host.addr, Some(host_fp)).await.unwrap() else { panic!() };
    session.ping().await.unwrap();

    // A full-size paste does not hold up a key behind it.
    let big = ClipboardText::new("é".repeat(MAX_CLIPBOARD / 2)).unwrap();
    session.send_clipboard(big.clone());
    session.input(Input::Key { usage: 0x04, state: KeyState::Down }).await.unwrap();
    session.ping().await.unwrap();
    let mut got = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while got.is_none() && tokio::time::Instant::now() < deadline {
        for event in drain(&mut seen) {
            if let HostEvent::Clipboard(text) = event {
                got = Some(text);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(got, Some(big));

    // Back to back: the last one is what the viewer ends up with.
    for n in 0..20 {
        host.host.send_clipboard(ClipboardText::new(format!("update {n}")).unwrap());
    }
    let mut last = None;
    while let Ok(Some(text)) = tokio::time::timeout(Duration::from_millis(500), session.clipboard()).await {
        last = Some(text);
    }
    assert_eq!(last.map(ClipboardText::into_string).as_deref(), Some("update 19"));

    // A receiver waits beside notice() in one select!, and starts after what was read.
    let mut receiver = session.clipboard_receiver();
    host.host.send_clipboard(ClipboardText::new("beside notices".into()).unwrap());
    let text = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = session.notice() => None,
            text = receiver.next() => text,
        }
    });
    assert_eq!(text.await.unwrap().map(ClipboardText::into_string).as_deref(), Some("beside notices"));
    session.close().await;
}

#[tokio::test]
async fn malformed_or_stale_clipboard_streams_are_dropped() {
    use std::time::Duration;
    let (host_conn, viewer_conn, _server, _client) = raw_pair().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(crate::clipboard::receive_loop(host_conn.clone(), move |text| {
        let _ = tx.send(text.into_string());
    }));
    async fn side(conn: &quinn::Connection, bytes: &[u8], finish: bool) -> quinn::SendStream {
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        send.write_all(bytes).await.unwrap();
        if finish {
            send.finish().unwrap();
        }
        send
    }
    fn clip(text: &[u8]) -> Vec<u8> {
        let mut v = vec![1];
        v.extend((text.len() as u32).to_be_bytes());
        v.extend(text);
        v
    }
    let mut bad = vec![
        [vec![2], (2u32).to_be_bytes().to_vec(), b"hi".to_vec()].concat(), // unknown kind
        [vec![1], ((crate::MAX_CLIPBOARD + 1) as u32).to_be_bytes().to_vec()].concat(), // too long
        clip(b"a\0b"),
        clip(b""),
        clip(&[0xff, 0xfe]),
        [clip(b"ok"), b"trailing".to_vec()].concat(),
    ];
    bad.push(clip(b"short")[..6].to_vec()); // truncated
    for bytes in &bad {
        side(&viewer_conn, bytes, true).await;
    }
    // An older stream that finishes after a newer one is stale.
    let mut older = side(&viewer_conn, &clip(b"older")[..3], false).await;
    side(&viewer_conn, &clip(b"newer"), true).await;
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().as_deref(), Some("newer"));
    older.write_all(&clip(b"older")[3..]).await.unwrap();
    older.finish().unwrap();
    side(&viewer_conn, &clip(b"latest"), true).await;
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().as_deref(), Some("latest"));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn clipboard_sent_while_an_earlier_one_waits_still_arrives() {
    use std::time::Duration;
    let (host_conn, viewer_conn, _server, _client) = raw_pair().await;
    fn clip(text: &[u8]) -> Vec<u8> {
        let mut v = vec![1];
        v.extend((text.len() as u32).to_be_bytes());
        v.extend(text);
        v
    }
    // Take all four streams of credit, so the sender's first open has to wait.
    let mut held = Vec::new();
    for _ in 0..4 {
        let (mut send, recv) = viewer_conn.open_bi().await.unwrap();
        send.write_all(&clip(b"held")[..1]).await.unwrap();
        held.push((send, recv));
    }
    let updates = crate::clipboard::start_sender(viewer_conn.clone());
    updates.send_replace(crate::ClipboardText::new("A".into()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    updates.send_replace(crate::ClipboardText::new("B".into()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(crate::clipboard::receive_loop(host_conn.clone(), move |text| {
        let _ = tx.send(text.into_string());
    }));
    for (send, _) in &mut held {
        send.write_all(&clip(b"held")[1..]).await.unwrap();
        send.finish().unwrap();
    }
    let mut last = None;
    while let Ok(Some(text)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        last = Some(text);
    }
    assert_eq!(last.as_deref(), Some("B"));
}
