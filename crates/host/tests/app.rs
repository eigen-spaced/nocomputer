//! End to end on loopback, as the app sees it: the real host binary over its pipes, a
//! viewer pairing through an app-side approval, and input arriving on stdout in order.

#![cfg(not(target_os = "macos"))] // the host identity would be the app's Keychain item

use nocomputer_host::local::{FromApp, ToApp};
use nocomputer_proto::{Button, Identity, Input, KeyState, NoticeKind, Outcome, ScrollUnit, Viewer};
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
    app.write_all(&FromApp::Hello { version: 2 }.encode()).unwrap();
    assert_eq!(next(&frames), (0x81, vec![0, 2]));
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

    let Outcome::Ready(mut session) = viewer.connect(addr, Some(host_fp)).await.unwrap() else { panic!() };
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
