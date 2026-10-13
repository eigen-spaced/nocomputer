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
    assert!(session.ping().await.is_err(), "live session must end on revocation");

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
    write_msg(&mut send, &Message::Hello { min_version: 2, max_version: 3, device_name: "future".into() }).await.unwrap();
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
    assert_eq!(reopened.status(&fp), crate::ClientStatus::Paired);
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
