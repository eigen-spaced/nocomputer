# nocomputer-proto

Transport, device identity and pairing for nocomputer, shared by the viewer and host. QUIC (quinn) with TLS 1.3 (rustls, ring provider), mutually authenticated. There is no video, input or clipboard yet: an authorized session carries only keepalive pings.

## Security model

- **Identity.** Each device has one self-signed ECDSA P-256 certificate (rcgen). Its **fingerprint** is the SHA-256 of the whole certificate (DER), always shown in full as 16 groups of four hex digits. Certificates are never reissued; a new identity means pairing again. Expiry is deliberately not checked, because trust comes from the pin, not from dates or a CA.
- **Verification.** Both sides use custom rustls verifiers that skip CA chains and hostname matching (the TLS server name is the fixed string `nocomputer`), but always verify the TLS 1.3 handshake signature with the presented certificate's key. A peer that presents someone else's certificate without its private key fails the handshake. Tests cover this in both directions, and fail if signature verification is removed.
- **Mutual authentication.** The host requires a client certificate on every connection. It accepts certificates it doesn't know, so new devices can ask to pair, and then authorizes **each connection** against its paired-device list. TLS session tickets, resumption and 0-RTT are disabled, so a connection can never ride on an authorization from before a revocation, and nothing is replayable.
- **Pairing (SSH-style, both directions).**
  1. The viewer connects with no pin and gets a `FirstContact` with the host's fingerprint and name. It never gets a session this way, even if the host already knows the device.
  2. The viewer shows the host fingerprint and its own. The user checks that the host's screen shows the same two values.
  3. The viewer sends `PairRequest`, which carries no identity. The host prompts its local user with the device name, **the fingerprint of the certificate that authenticated this connection**, and the host's own fingerprint.
  4. On approval the host records the device, and the viewer pins the host's fingerprint against the address the user typed.

  There are no short codes; the user compares full fingerprints. One request can be pending at a time host-wide. Unanswered requests time out after 60 s, and a denial or timeout blocks all requests for 10 s. An unpaired device can only send `Hello` and `PairRequest`; anything else ends the connection.
- **Revocation.** A revoked device stays listed as revoked, so it can't simply pair again. `Host::revoke` closes its live connections immediately. A revocation written by another process (`nocomputer-pair revoke`) applies from the next connection, because the host reads the list from disk on each one.
- **Pinned reconnect.** If a pinned host presents a different certificate, the viewer fails with `HostIdentityChanged { expected, presented }` and connects to nothing.

## Storage

| What | Windows | macOS | Other |
| --- | --- | --- | --- |
| Private identity | DPAPI, current user, app-specific entropy (`identity.dpapi`) | login Keychain item `dev.nocomputer.identity` | refused: no OS store yet |
| Paired devices (host) | `paired-clients.json` | same | same |
| Known hosts (viewer) | `known-hosts.json` | same | same |

The JSON lists are authorization data, not secrets, but editing them grants access. They're written atomically, with mode 0600 on Unix; on Windows they inherit the per-user ACL of `%APPDATA%`. Nothing logs key material; `Identity`'s `Debug` prints only the fingerprint.

On macOS, Keychain access control is bound to the code signature of the program that created the item, so ad-hoc rebuilds may prompt. This is the same stable-signing problem as the TCC grants.

## Wire format

The viewer opens one bidirectional control stream. Each message is a 4-byte big-endian length, then one JSON object tagged by `type` (see `src/wire.rs`). Messages are capped at 64 KiB, checked before allocating. Unknown fields are ignored, for forward compatibility; unknown message types are errors. `Hello` carries `min_version`/`max_version`, and the host refuses ranges that exclude its version with `version_unsupported`. Video and input will use separate binary streams or datagrams, not this channel. ALPN is `nocomputer/1`, and the default port is 47900/UDP.

## Manual test tool

`nocomputer-pair` (in `src/bin`) runs either side. State lives in `NOCOMPUTER_STATE_DIR`, or by default `%APPDATA%\nocomputer`, `~/Library/Application Support/nocomputer` or `~/.config/nocomputer`.

```
nocomputer-pair whoami
nocomputer-pair host [--listen 0.0.0.0:47900] [--name NAME]   # then type: clients | revoke FP | quit
nocomputer-pair pair macbook-air[:47900] [--name NAME]
nocomputer-pair connect macbook-air[:47900] [--count N]
nocomputer-pair clients | revoke FINGERPRINT | forget HOST[:PORT]
```

## Status

Verified on Windows, 2026-10-13, on the target client: all 17 tests pass (`cargo test`, including the DPAPI store); `cargo clippy --all-targets` is clean; and a local `host`/`pair`/`connect` run with separate state folders works.

Not yet verified:
- macOS: build, the Keychain store, and pairing between machines.
- Linux key storage.
- One controlling viewer at a time.
- Video, input and clipboard streams.
- Linking into the Tauri viewer and the Swift host.
