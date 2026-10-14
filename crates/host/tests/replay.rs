//! End to end on loopback: the real host binary replaying the fixture, and a viewer
//! built on nocomputer-proto pairing with it, logging in through its console, and
//! receiving video.

#![cfg(not(target_os = "macos"))] // the host identity would be the app's Keychain item

use nocomputer_proto::{Identity, LoginResult, Outcome, Password, Viewer};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the host even when an assertion fails, so it can't outlive the test.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewer_pairs_and_receives_replayed_video() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let state = std::env::temp_dir().join(format!("nocomputer-host-replay-{}", std::process::id()));
    let mut child = Child(Command::new(env!("CARGO_BIN_EXE_nocomputer-host"))
        .args(["--listen", "127.0.0.1:0", "--name", "replay-host", "--from-file"])
        .arg(root.join("build/moving-1080p.h264"))
        .arg("--state-dir")
        .arg(&state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap());
    let mut console = child.0.stdin.take().unwrap();
    let (lines_tx, lines) = std::sync::mpsc::channel::<String>();
    let stderr = child.0.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = lines_tx.send(line);
        }
    });
    let wait_for = |needle: &str| -> String {
        loop {
            let line = lines.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("host never printed {needle:?}"));
            if line.contains(needle) {
                return line;
            }
        }
    };
    let listening = wait_for("listening on ");
    let addr: std::net::SocketAddr = listening.rsplit(' ').next().unwrap().parse().unwrap();

    let viewer = Viewer::new(Identity::generate().unwrap(), "replay-viewer").unwrap();
    let Outcome::FirstContact(first) = viewer.connect(addr, None).await.unwrap() else { panic!() };
    let host_fp = first.host_fingerprint;
    let pairing = tokio::spawn(first.request_pairing());
    // The approval prompt names both fingerprints; approve it on the console.
    wait_for(&viewer.fingerprint().to_string());
    console.write_all(b"y\n").unwrap();
    assert!(pairing.await.unwrap().unwrap());

    // Replay mode has no account to check: the console allows the login.
    let Outcome::Login(prompt) = viewer.connect(addr, Some(host_fp)).await.unwrap() else { panic!() };
    let login = tokio::spawn(prompt.login("replay-user", Password::new("unchecked".into()).unwrap()));
    wait_for("Allow? [y/N]");
    console.write_all(b"y\n").unwrap();
    let LoginResult::Session(mut session) = login.await.unwrap().unwrap() else { panic!("the allowed login failed") };
    session.ping().await.unwrap();
    let mut video = session.video();
    let started = Instant::now();
    let mut frames = Vec::new();
    while frames.len() < 120 {
        frames.push(tokio::time::timeout(Duration::from_secs(5), video.next()).await.unwrap().unwrap());
    }
    let elapsed = started.elapsed();
    assert!(frames[0].keyframe, "the stream starts at a keyframe");
    assert!(frames.iter().all(|f| &*f.codec == "avc1.4D0028"));
    // Fixture keyframes are at 0 and 60 of 90, looping: 0, 60, 90, ...
    let keyframes: Vec<usize> = frames.iter().enumerate().filter(|(_, f)| f.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keyframes, vec![0, 60, 90], "replay order");
    // 120 frames at 60 fps: about two seconds.
    assert!(elapsed > Duration::from_millis(1700) && elapsed < Duration::from_millis(3500), "{elapsed:?}");

    // A keyframe request makes the replay jump to the next IDR.
    session.request_keyframe().await.unwrap();
    let mut until_key = 0;
    while !video.next().await.unwrap().keyframe {
        until_key += 1;
    }
    assert!(until_key < 40, "keyframe arrived after {until_key} frames");

    session.close().await;
    drop(console);
    drop(child);
    let _ = std::fs::remove_dir_all(&state);
}
