//! Runs the real binary over its pipes, as the app does.

use nocomputer_host::local::{FromApp, VERSION};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn golden(name: &str) -> Vec<u8> {
    std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/local-v6").join(format!("{name}.bin"))).unwrap()
}

/// Feeds `input`, closes stdin, and returns (exit code, stdout).
fn run(input: &[u8]) -> (i32, Vec<u8>) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let state = std::env::temp_dir().join(format!("nocomputer-host-test-{}-{n}", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_nocomputer-host"))
        .args(["--listen", "127.0.0.1:0", "--state-dir"])
        .arg(&state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    let code = child.wait().unwrap().code().unwrap();
    let _ = std::fs::remove_dir_all(&state);
    (code, out)
}

#[test]
fn closed_pipe_exits_cleanly() {
    assert_eq!(run(&[]), (0, vec![]));
}

#[test]
fn version_mismatch_exits_2_without_output() {
    for version in [1, VERSION - 1, VERSION + 1] {
        assert_eq!(run(&FromApp::Hello { version }.encode()), (2, vec![]), "version {version}");
    }
}

#[test]
fn anything_but_hello_first_exits_3() {
    assert_eq!(run(&golden("82-start-stream")).0, 3);
    assert_eq!(run(&FromApp::CaptureStopped { reason: "x".into() }.encode()).0, 3);
    assert_eq!(run(&[0, 0, 0, 0]).0, 3);
}

// These load the host identity. On macOS that is the login Keychain item the app uses,
// so they run only where the store lives in the test's own state directory.
#[cfg(not(target_os = "macos"))]
mod with_identity {
    use super::*;
    use nocomputer_host::local::ToApp;

    /// The event frames after HelloAck, as JSON.
    fn events_after_ack(out: &[u8]) -> Vec<serde_json::Value> {
        let ack = golden("81-hello-ack");
        assert_eq!(ack, ToApp::HelloAck { version: VERSION }.encode());
        assert!(out.starts_with(&ack), "{out:?}");
        let mut rest = &out[ack.len()..];
        let mut events = Vec::new();
        while !rest.is_empty() {
            let len = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
            assert_eq!(rest[4], 0x85, "only events follow HelloAck");
            events.push(serde_json::from_slice(&rest[5..4 + len]).unwrap());
            rest = &rest[4 + len..];
        }
        events
    }

    #[test]
    fn hello_is_acknowledged_then_eof_exits_cleanly() {
        let (code, out) = run(&golden("01-hello"));
        assert_eq!(code, 0);
        // HelloAck, the identity, the (empty) paired devices, then nothing.
        let events = events_after_ack(&out);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["type"], "identity");
        assert_eq!(events[1], serde_json::json!({"type": "paired_devices", "devices": []}));
    }

    #[test]
    fn removing_or_forgetting_an_unlisted_device_answers_with_the_list() {
        let mut input = golden("01-hello");
        input.extend(golden("08-remove-device"));
        input.extend(golden("09-forget-device"));
        let (code, out) = run(&input);
        assert_eq!(code, 0);
        let events = events_after_ack(&out);
        assert_eq!(events.len(), 4, "{events:?}");
        for list in &events[1..] {
            assert_eq!(list, &serde_json::json!({"type": "paired_devices", "devices": []}));
        }
    }

    #[test]
    fn access_unit_not_matching_its_flag_exits_3() {
        let mut input = golden("01-hello");
        let delta = FromApp::read_sync(&golden("02-access-unit-delta"));
        let FromApp::AccessUnit { capture_us, data, .. } = delta else { unreachable!() };
        input.extend(FromApp::AccessUnit { keyframe: true, capture_us, data }.encode());
        assert_eq!(run(&input).0, 3);
    }

    #[test]
    fn well_formed_frames_are_accepted() {
        let mut input = golden("01-hello");
        for name in ["02-access-unit-key", "02-access-unit-delta", "03-capture-stopped", "04-pairing-decision-deny", "05-input-unavailable", "07-login-verdict-refused"] {
            input.extend(golden(name));
        }
        assert_eq!(run(&input).0, 0);
    }
}
