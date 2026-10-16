# nocomputer-host

The host process. The Mac capture app spawns it from its bundle with no arguments. The app feeds it encoded video over stdin/stdout, and it serves paired viewers with `nocomputer-proto`. The app keeps the Screen Recording and Accessibility permissions; this process needs only the network and its own key.

## Pipe contract (local-v6)

The contract is specified in `src/local.rs`, with golden vectors in `testdata/local-v6/`.
- **App to host:** Hello, encoded access units, capture-stopped and input-unavailable notices (both forwarded to the viewer), pairing decisions, clipboard text for the viewer, login verdicts, and devices to remove or forget.
- **Host to app:** HelloAck; start/stop stream when a controlling session begins or ends; keyframe requests, rate-limited to one per 500 ms; the viewer's picture size (protocol v5), at most one per 250 ms with the newest kept; display events; input from the controlling session; clipboard text from it; and logins to check.

Input is ordered. Consecutive pointer moves coalesce while the app is busy; buttons, keys and scrolls are never coalesced. A backlog of 1024 frames first sheds moves, then is discarded and replaced by ReleaseAll; a working app never gets there. The host sends ReleaseAll whenever a session starts or ends, before StopStream, so nothing stays held when a viewer goes away. Input contents are never logged; replay mode prints counts by kind only. Clipboard text waits behind control frames and input, and only the newest is kept while the app is busy; it is never logged, and replay mode drops it.

Pairing prompts go to the app with a fresh `request_id`. Only a decision carrying the id of the current prompt resolves it, so a late answer can't approve a later request. The pipe is the only approval path. When the device disconnects or the request times out, the host sends a `pairing_withdrawn` event, and the app closes the prompt.

Right after HelloAck the host sends an `identity` event with its name and fingerprint. The app shows it in its window while Remote access is on, so a user can compare it with a viewer's screen even when no pairing prompt appears (a device the host already knows).

A `paired_devices` event follows it and is sent again after every pairing, removal or forget. It lists every device in pairing order, removed ones included. RemoveDevice revokes a device and closes its connections, and its session ends as usual. A removed device stays listed so it can't simply pair again. ForgetDevice deletes a removed device, and after that it may ask to pair, with the usual approval. The app confirms both with its user before sending them.

Logins (protocol v4) go to the app as VerifyLogin with a fresh `request_id`. The app answers with LoginVerdict: ok only if the username is its own account's short or full name, OpenDirectory finds that record with this process's UID, and `ODRecord.verifyPassword` accepts the password. The proto host applies the delays and runs one check at a time; the app only checks. The host clears its frame buffer after writing, and the app clears the consumed pipe bytes and its copy of the password when done. The String handed to OpenDirectory can't be cleared, so this is best effort. Neither side logs the username or password; the app sees `login_failed` events with a device fingerprint and a count.

Each access unit is checked against its keyframe flag. A keyframe must be an IDR with an in-band SPS and PPS. The WebCodecs codec string (`avc1.PPCCLL`) is taken from that SPS, so a new resolution simply arrives with the next IDR.

stdout carries only local-v6 frames; logs go to stderr. A test fails if stdout printing appears in the source.

Exit codes:
- 0: stdin closed
- 1: startup error (bind, key storage)
- 2: local-v6 version mismatch
- 3: any other local-v6 violation

## Defaults

| | |
| --- | --- |
| Listen | `0.0.0.0:47900` (UDP) |
| Name | the machine name, else `nocomputer host` |
| State | `<app data>/nocomputer/host`, created 0700 on Unix |
| Identity | the Keychain account `host` on macOS, or DPAPI in the state dir on Windows. It is separate from the `nocomputer-pair` identity, so viewers pair with the app host once. |

## Testing without the app

```
nocomputer-host --from-file build/moving-1080p.h264 [--listen ADDR] [--state-dir DIR]
```

This mode replays the file at 60 fps while a viewer is connected. A keyframe request jumps to the next IDR. stdin becomes a y/N console for approving pairing requests and allowing logins; it has no account to check, so the password is not looked at. Pair and watch from another terminal with `nocomputer-pair pair ADDR` and `nocomputer-pair watch ADDR`.

## Status (2026-10-13, Windows)

- 17 tests pass:
  - golden vectors and malformed frames;
  - input coalescing and backlog handling;
  - H.264 checks against the fixture (90 AUs, IDRs at 0 and 60);
  - the binary over real pipes (exit codes 0/2/3, HelloAck bytes);
  - end to end through the app pipe: a viewer pairs through an app-side approval, its input arrives on stdout in order and sanitized, an app notice reaches the viewer, and held input is released before StopStream when the viewer leaves;
  - end to end on loopback: the real binary replays the fixture, and a viewer pairs through the console, receives 120 frames in order from a keyframe at about 60 fps, and gets an IDR soon after requesting one.
- The full suite passed 20 times in a row, and clippy is clean.
- A manual loopback run of `nocomputer-pair watch` received 60 frames per second.
- The replay's frame spacing has gaps of up to 32 ms on Windows, because of timer granularity. Replay pacing only, not the real capture path.

Not yet done:
- input between machines, with the Mac app injecting it.
