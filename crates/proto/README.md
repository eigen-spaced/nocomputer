# nocomputer-proto

Transport, device identity, pairing and video for nocomputer, shared by the viewer and host. QUIC (quinn) with TLS 1.3 (rustls, ring provider), mutually authenticated. Input goes from the controlling viewer to the host; plain-text clipboard goes both ways.

## Security model

- **Identity.** Each device has one self-signed ECDSA P-256 certificate (rcgen). Its **fingerprint** is the SHA-256 of the whole certificate (DER), always shown in full as 16 groups of four hex digits. Certificates are never reissued; a new identity means pairing again. Expiry is deliberately not checked, because trust comes from the pin, not from dates or a CA.
- **Verification.** Both sides use custom rustls verifiers that skip CA chains and hostname matching (the TLS server name is the fixed string `nocomputer`), but always verify the TLS 1.3 handshake signature with the presented certificate's key. A peer that presents someone else's certificate without its private key fails the handshake. Tests cover this in both directions, and fail if signature verification is removed.
- **Mutual authentication.** The host requires a client certificate on every connection. It accepts certificates it doesn't know, so new devices can ask to pair, and then authorizes **each connection** against its paired-device list. TLS session tickets, resumption and 0-RTT are disabled, so a connection can never ride on an authorization from before a revocation, and nothing is replayable.
- **Pairing (SSH-style, both directions).**
  1. The viewer connects with no pin and gets a `FirstContact` with the host's fingerprint and name. It never gets a session this way, even if the host already knows the device.
  2. The viewer shows the host fingerprint and its own. The user checks that the host's screen shows the same two values.
  3. The viewer sends `PairRequest`, which carries no identity. The host prompts its local user with the device name, **the fingerprint of the certificate that authenticated this connection**, and the host's own fingerprint.
  4. On approval the host records the device, and the viewer pins the host's fingerprint against the address the user typed.

  There are no short codes; the user compares full fingerprints. One request can be pending at a time host-wide. Unanswered requests time out after 60 s, and a denial or timeout blocks all requests for 10 s. A device that disconnects while waiting withdraws its request: the host drops the `Approver`'s future, which should dismiss the prompt. An unpaired device can only send `Hello` and `PairRequest`; anything else ends the connection.
- **Login (protocol v4).** Pairing proves which device is connecting. After it, every pinned connection must also log in to the account the host runs for (on the Mac, the signed-in user) before it gets anything else (see `src/login.rs`).
  1. `Viewer::connect` with a pin returns `Outcome::Login(LoginPrompt)`. The Busy check runs before this, so nobody types a password only to be refused. Nothing else happens on the connection yet: no video, input, clipboard or session, and no claim on the controller slot. Any other message ends the connection with `bad_request`.
  2. `LoginPrompt::login(username, Password)` sends `{"type":"login","username":...,"password":...}` inside the authenticated TLS connection. The host's `Verifier` checks it. Success gives a `Session`; a busy host still refuses at this point if another device took control meanwhile.
  3. Every failure, whatever its cause (wrong account, wrong password, a verifier error or timeout), gets the same answer: `LoginResult::Failed { retry_after }`. The connection stays open for another attempt. The 5th failure, or 2 minutes without a success, ends it with `login_failed`, and connecting again gives a new prompt.

  **Delay:** after the 1st and 2nd consecutive failures the delay is 1 s, then it doubles (2, 4, 8 s ...) up to 5 min. It's counted per device and host-wide, the longer applies, and a success resets both. The host waits up to 1 s before answering a failure. An attempt before `retry_after` has passed is refused without asking the verifier. Checks run one at a time host-wide, so parallel connections can't outrun the delay. The counts live in memory and reset when the host restarts.

  **Password handling:** `Password` zeroizes on drop and its `Debug` prints `Password(redacted)`. The control-message buffers are zeroized after use. Copies inside serde_json's error paths, QUIC's send buffers and the UI toolkit can't be cleared, so this is best effort. Nothing is stored or logged; `HostEvent::LoginFailed` carries the device and a count only. The pairing check stays: a password alone never admits an unpaired device.
- **Revocation.** A revoked device stays listed as revoked, so it can't simply pair again. `Host::revoke` closes its live connections immediately. A revocation written by another process (`nocomputer-pair revoke`) applies from the next connection, because the host reads the list from disk on each one.
- **One controller.** The host serves one session at a time; a different paired device is refused with `busy` until that session ends, closes or is revoked. The same device connecting again (for example after a network change, before the old connection times out) takes over, and its old session ends with `replaced`. Pairing connections don't count. A session whose network silently dropped holds the slot until the 30 s idle timeout.
- **Pinned reconnect.** If a pinned host presents a different certificate, the viewer fails with `HostIdentityChanged { expected, presented }` and connects to nothing.

## Storage

| What | Windows | macOS | Other |
| --- | --- | --- | --- |
| Private identity | DPAPI, current user, app-specific entropy (`identity.dpapi`) | login Keychain item `dev.nocomputer.identity` | refused: no OS store yet |
| Paired devices (host) | `paired-clients.json` | same | same |
| Known hosts (viewer) | `known-hosts.json` | same | same |

The JSON lists are authorization data, not secrets, but editing them grants access. They're written atomically, with mode 0600 on Unix; on Windows they inherit the per-user ACL of `%APPDATA%`. Nothing logs key material; `Identity`'s `Debug` prints only the fingerprint.

On macOS, Keychain access control is bound to the code signature of the program that created the item, so ad-hoc rebuilds may prompt. This is the same stable-signing problem as the TCC grants.

## Errors

`Error::kind()` sorts errors for display: `Unreachable` (no answer, for example with Remote access off), `Disconnected` (lost after connecting, including at the 2-minute login limit: connect again and ask for the password again), `HostIdentityChanged` (show both fingerprints as a warning; never offer to pair again from there), `NotPaired`, `Revoked`, `Busy` (another device is in control), `Replaced` (this device connected again from elsewhere; this session was closed), `HostEnded` (the host stopped sharing, for example when its user turned Remote access off; return to the start screen), `VersionMismatch`, `PairingUnavailable`, `LoginFailed` (too many attempts on one connection), and `Other`. A single wrong login is `LoginResult::Failed`, not an error.

## Wire format

The viewer opens one bidirectional control stream. Each message is a 4-byte big-endian length, then one JSON object tagged by `type` (see `src/wire.rs`). Messages are capped at 64 KiB, checked before allocating. Unknown fields are ignored, for forward compatibility; unknown message types are errors. `Hello` carries `min_version`/`max_version`, and the host refuses ranges that exclude its version with `version_unsupported`; the protocol version is 4 (3 added clipboard side streams; 4 added the login). Video and clipboard text have their own streams. Input and notices are messages on this channel, so they stay ordered with the session. ALPN is `nocomputer/1`, and the default port is 47900/UDP.

## Video

The host sends H.264 to the controlling session on QUIC unidirectional streams, one per GOP. Each stream starts with a header carrying the codec string, then frames of the form `[u32 len][u8 flags][u64 host_us][Annex B]` (see `src/video.rs`). Only the first frame on a stream is a keyframe.
- **Host:** `Host::send_access_unit` never blocks. Up to 8 frames queue for the viewer. On overflow the host resets the current stream, drops everything up to the next keyframe, and emits `HostEvent::KeyframeNeeded`. It also emits that event when a session starts and when the viewer calls `Session::request_keyframe`.
- **Viewer:** `Session::video()` yields only frames whose GOP keyframe it has delivered. A reset or truncated stream is discarded, partial frame included. A stream that doesn't start at a keyframe is ignored. A newer GOP that has already arrived supersedes unread frames of the current one.
- **Timestamps:** `host_us` is the host's monotonic clock, for differences between frames only.

## Input

The controlling viewer sends `{"type":"input","kind":...}` messages (see `src/input.rs`):
- `pointer_move{x,y}`: 0..=65535 across the video picture, edges inclusive. The viewer never sends positions in the letterbox.
- `pointer_button{button,down}`: left, right, middle, back or forward, at the last position.
- `scroll{dx,dy,unit}`: pixel or line, with the DOM sign (positive `dy` scrolls toward the end of the document).
- `key{usage,state}`: a USB HID keyboard usage, never a character, so the layout stays the host's concern. `state` is up, down or repeat. Any modifier remapping (which key sends Command) happens on the viewer.
- `release_all`.

The host drops unknown buttons, clamps scrolls to ±10000 px or ±100 lines, and emits each one as `HostEvent::Input`. Only one session exists at a time, so input comes only from the controller. The host's capture side reports problems through `Host::notice`, which reaches the viewer as `{"type":"notice","kind":"input_unavailable"|"capture_stopped","message":...}` (`Session::notice`). Neither side logs input contents.

## Clipboard

Plain text only, in both directions, between the controlling session and the host (see `src/clipboard.rs`). Each update goes on its own QUIC bidirectional side stream, so a large paste never waits in front of input. The viewer's first bidirectional stream is the control stream; any later one, and any the host opens, is a side stream: `[u8 kind = 1][u32 BE len][UTF-8 text]`, then FIN.
- **Limits:** 1 byte to 1 MiB (`MAX_CLIPBOARD`), no U+0000, `\n` line endings. The receiver checks the length before allocating. A malformed side stream is stopped and dropped without ending the session.
- **Newest wins:** `Session::send_clipboard` and `Host::send_clipboard` never block; while a stream waits for credit or is being written, only the latest text is kept, and an unfinished older stream is reset. The receiver ignores a stream older than the last one it delivered. `Session::clipboard` is cancel-safe and returns only the newest unread text.
- **Never shown:** `ClipboardText`'s `Debug` prints only its length, and so does `HostEvent::Clipboard`.
- **Policy is the apps':** when to send, what to skip (password-manager markers), echo suppression and the on/off switch live in the host app and the viewer, not here.

## Manual test tool

`nocomputer-pair` (in `src/bin`) runs either side. `connect` and `watch` ask for the host account's username and a password, which is read without echo. Its `host` has no account to check, so its console allows or refuses each login, without seeing the password. State lives in `NOCOMPUTER_STATE_DIR`, or by default `%APPDATA%\nocomputer`, `~/Library/Application Support/nocomputer` or `~/.config/nocomputer`.

```
nocomputer-pair whoami
nocomputer-pair host [--listen 0.0.0.0:47900] [--name NAME]   # then type: clients | revoke FP | quit
nocomputer-pair pair macbook-air[:47900] [--name NAME]
nocomputer-pair connect macbook-air[:47900] [--count N] [--name NAME]
nocomputer-pair watch macbook-air[:47900] [--seconds N]           # video statistics
nocomputer-pair clients | revoke FINGERPRINT | forget HOST[:PORT]
```

## Status

Verified on Windows, 2026-10-13, on the target client: all 27 tests pass (`cargo test`, including the DPAPI store); `cargo clippy --all-targets` is clean; and a local `host`/`pair`/`connect` run with separate state folders works.

Verified between machines, 2026-10-13: the Windows client paired with the MacBook Air host over the tailnet, with matching full fingerprints on both screens and approval on the Mac. Pinned reconnects then worked without prompting, with control-channel pings of 26–28 ms. Revoking the device on the Mac closed a live session from Windows, and the next connection was refused as revoked. The Mac side (build, Keychain store, tests) was run by the Mac session. Details are in `docs/development-plan.md`, Milestone 2.

Session names come from the pairing record. The name a device sends in `Hello` is reported separately (`HostEvent::Session::reported_name`) and is unverified.

Not yet verified:
- Linux key storage.
- Video between machines (the loopback path is tested here and in `crates/host`).
- Input between machines.
- Clipboard between machines.
- Linking into the Tauri viewer and the Swift host.
- The login (v4) between machines, and the Mac's account check.
