//! End to end on loopback, as the app sees it: the real host binary over its pipes, a
//! viewer pairing through an app-side approval, logging in through the app's account
//! check, input arriving on stdout in order, and clipboard text in both directions.

#![cfg(not(target_os = "macos"))] // the host identity would be the app's Keychain item

use nocomputer_host::local::{FromApp, ToApp, VERSION};
use nocomputer_proto::{Button, ClipboardText, Identity, Input, KeyState, LoginResult, NoticeKind, Outcome, Password, ScrollUnit, Viewer};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Kills the host even when an assertion fails, so it can't outlive the test.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One host-to-app frame as raw (type, payload).
fn read_frames(mut stdout: impl Read + Send + 'static) -> Receiver<(u8, Vec<u8>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || loop {
        let mut len = [0u8; 4];
        if stdout.read_exact(&mut len).is_err() {
            return;
        }
        let mut body = vec![0; u32::from_be_bytes(len) as usize];
        if stdout.read_exact(&mut body).is_err() || tx.send((body[0], body[1..].to_vec())).is_err() {
            return;
        }
    });
    rx
}

fn next(frames: &Receiver<(u8, Vec<u8>)>) -> (u8, Vec<u8>) {
    frames.recv_timeout(Duration::from_secs(10)).expect("a frame from the host")
}

/// The next frame that isn't a display event or keyframe request.
fn next_command(frames: &Receiver<(u8, Vec<u8>)>) -> Vec<u8> {
    loop {
        let (kind, payload) = next(frames);
        if kind != 0x85 && kind != 0x84 {
            let mut frame = (1 + payload.len() as u32).to_be_bytes().to_vec();
            frame.push(kind);
            frame.extend(payload);
            return frame;
        }
    }
}

/// The next VerifyLogin frame, as (request_id, username, password).
fn verify_login(frames: &Receiver<(u8, Vec<u8>)>) -> (u64, String, String) {
    let frame = next_command(frames);
    assert_eq!(frame[4], 0x8C, "expected VerifyLogin");
    let payload = &frame[5..];
    let request_id = u64::from_be_bytes(payload[..8].try_into().unwrap());
    let len = payload[8] as usize;
    let username = String::from_utf8(payload[9..9 + len].to_vec()).unwrap();
    let password = String::from_utf8(payload[9 + len..].to_vec()).unwrap();
    (request_id, username, password)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_reaches_the_app_in_order_and_is_released_at_session_end() {
    let state = std::env::temp_dir().join(format!("nocomputer-host-app-{}", std::process::id()));
    let mut child = Child(
        Command::new(env!("CARGO_BIN_EXE_nocomputer-host"))
            .args(["--listen", "127.0.0.1:0", "--name", "app-host", "--state-dir"])
            .arg(&state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut app = child.0.stdin.take().unwrap();
    let frames = read_frames(child.0.stdout.take().unwrap());
    let stderr = BufReader::new(child.0.stderr.take().unwrap());
    app.write_all(&FromApp::Hello { version: VERSION }.encode()).unwrap();
    assert_eq!(next(&frames), (0x81, VERSION.to_be_bytes().to_vec()));
    // Then the host's identity, for the app to show.
    let (kind, identity) = next(&frames);
    assert_eq!(kind, 0x85);
    let identity: serde_json::Value = serde_json::from_slice(&identity).unwrap();
    assert_eq!((identity["type"].as_str(), identity["name"].as_str()), (Some("identity"), Some("app-host")));
    // Keep draining stderr: the host must never block or fail on its logging.
    let (addr_tx, addr) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if let Some((_, a)) = line.split_once("listening on ") {
                let _ = addr_tx.send(a.parse::<std::net::SocketAddr>().unwrap());
            }
        }
    });
    let addr = addr.recv_timeout(Duration::from_secs(10)).expect("the listen address");

    // Pair, approving the prompt from the app side.
    let viewer = Viewer::new(Identity::generate().unwrap(), "app-viewer").unwrap();
    let Outcome::FirstContact(first) = viewer.connect(addr, None).await.unwrap() else { panic!() };
    let host_fp = first.host_fingerprint;
    let pairing = tokio::spawn(first.request_pairing());
    let (kind, prompt) = next(&frames);
    assert_eq!(kind, 0x85);
    let prompt: serde_json::Value = serde_json::from_slice(&prompt).unwrap();
    assert_eq!(prompt["type"], "pairing_prompt");
    assert_eq!(prompt["device_fingerprint"], viewer.fingerprint().to_string());
    let request_id = prompt["request_id"].as_u64().unwrap();
    app.write_all(&FromApp::PairingDecision { request_id, approve: true }.encode()).unwrap();
    assert!(pairing.await.unwrap().unwrap());

    // Log in: the app refuses the first password and accepts the second.
    let Outcome::Login(prompt) = viewer.connect(addr, Some(host_fp)).await.unwrap() else { panic!() };
    let attempt = tokio::spawn(prompt.login("app-user", Password::new("wrong".into()).unwrap()));
    let (request_id, username, password) = verify_login(&frames);
    assert_eq!((username.as_str(), password.as_str()), ("app-user", "wrong"));
    app.write_all(&FromApp::LoginVerdict { request_id, ok: false }.encode()).unwrap();
    let LoginResult::Failed { retry_after, prompt } = attempt.await.unwrap().unwrap() else { panic!("a refused login succeeded") };
    tokio::time::sleep(retry_after).await;
    let attempt = tokio::spawn(prompt.login("app-user", Password::new("right – ü".into()).unwrap()));
    let (request_id, _, password) = verify_login(&frames);
    assert_eq!(password, "right – ü");
    // A verdict for another request is ignored.
    app.write_all(&FromApp::LoginVerdict { request_id: request_id + 100, ok: true }.encode()).unwrap();
    app.write_all(&FromApp::LoginVerdict { request_id, ok: true }.encode()).unwrap();
    let LoginResult::Session(mut session) = attempt.await.unwrap().unwrap() else { panic!("an accepted login failed") };
    assert_eq!(next_command(&frames), ToApp::ReleaseAll.encode(), "a session starts with nothing held");
    assert_eq!(next_command(&frames), ToApp::StartStream.encode());

    let sent = [
        Input::PointerMove { x: 100, y: 200 },
        Input::PointerButton { button: Button::Left, down: true },
        Input::PointerButton { button: Button::Unknown, down: true },
        Input::PointerButton { button: Button::Left, down: false },
        Input::Scroll { dx: 0, dy: 1_000_000, unit: ScrollUnit::Pixel },
        Input::Key { usage: 0xE3, state: KeyState::Down },
        Input::Key { usage: 0x19, state: KeyState::Down },
        Input::Key { usage: 0x19, state: KeyState::Repeat },
        Input::Key { usage: 0x19, state: KeyState::Up },
        Input::Key { usage: 0xE3, state: KeyState::Up },
    ];
    for input in sent {
        session.input(input).await.unwrap();
    }
    let expected = [
        ToApp::Pointer { x: 100, y: 200 },
        ToApp::Button { button: 0, down: true },
        ToApp::Button { button: 0, down: false },
        ToApp::Scroll { dx: 0, dy: 10_000, unit: 0 },
        ToApp::Key { usage: 0xE3, state: 1 },
        ToApp::Key { usage: 0x19, state: 1 },
        ToApp::Key { usage: 0x19, state: 2 },
        ToApp::Key { usage: 0x19, state: 0 },
        ToApp::Key { usage: 0xE3, state: 0 },
    ];
    for frame in expected {
        assert_eq!(next_command(&frames), frame.encode());
    }

    // The app's notices reach the viewer.
    app.write_all(&FromApp::InputUnavailable { reason: "Accessibility is off".into() }.encode()).unwrap();
    let notice = tokio::time::timeout(Duration::from_secs(5), session.notice()).await.unwrap().unwrap();
    assert_eq!((notice.kind, notice.message.as_str()), (NoticeKind::InputUnavailable, "Accessibility is off"));

    // Clipboard text crosses both ways.
    let text = ClipboardText::new("from the viewer\n– ü".into()).unwrap();
    session.send_clipboard(text.clone());
    assert_eq!(next_command(&frames), ToApp::Clipboard(text).encode());
    let text = ClipboardText::new("from the app".into()).unwrap();
    app.write_all(&FromApp::Clipboard(text.clone()).encode()).unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), session.clipboard()).await.unwrap(), Some(text));

    // Held input is released before the stream stops, even if the viewer never let go.
    session.input(Input::Key { usage: 0xE1, state: KeyState::Down }).await.unwrap();
    assert_eq!(next_command(&frames), ToApp::Key { usage: 0xE1, state: 1 }.encode());
    session.close().await;
    assert_eq!(next_command(&frames), ToApp::ReleaseAll.encode());
    assert_eq!(next_command(&frames), ToApp::StopStream.encode());

    drop(app);
    assert_eq!(child.0.wait().unwrap().code(), Some(0));
    let _ = std::fs::remove_dir_all(&state);
}
