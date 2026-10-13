# nocomputer-host

The host process. The Mac capture app spawns it from its bundle with no arguments. The app feeds it encoded video over stdin/stdout, and it serves paired viewers with `nocomputer-proto`. The app keeps the Screen Recording and Accessibility permissions; this process needs only the network and its own key.

## Pipe contract (local-v1)

The contract is specified in `src/local.rs`, with golden vectors in `testdata/local-v1/`.
- **App to host:** Hello, encoded access units, capture-stopped notices, and pairing decisions.
- **Host to app:** HelloAck; start/stop stream when a controlling session begins or ends; keyframe requests, rate-limited to one per 500 ms; and display events.

Pairing prompts go to the app with a fresh `request_id`. Only a decision carrying the id of the current prompt resolves it, so a late answer can't approve a later request. The pipe is the only approval path.

Each access unit is checked against its keyframe flag. A keyframe must be an IDR with an in-band SPS and PPS. The WebCodecs codec string (`avc1.PPCCLL`) is taken from that SPS, so a new resolution simply arrives with the next IDR.

stdout carries only local-v1 frames; logs go to stderr. A test fails if stdout printing appears in the source.

Exit codes:
- 0: stdin closed
- 1: startup error (bind, key storage)
- 2: local-v1 version mismatch
- 3: any other local-v1 violation

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

This mode replays the file at 60 fps while a viewer is connected. A keyframe request jumps to the next IDR. stdin becomes a y/N console for approving pairing requests. Pair and watch from another terminal with `nocomputer-pair pair ADDR` and `nocomputer-pair watch ADDR`.

## Status (2026-10-13, Windows)

- 13 tests pass:
  - golden vectors and malformed frames;
  - H.264 checks against the fixture (90 AUs, IDRs at 0 and 60);
  - the binary over real pipes (exit codes 0/2/3, HelloAck bytes);
  - end to end on loopback: the real binary replays the fixture, and a viewer pairs through the console, receives 120 frames in order from a keyframe at about 60 fps, and gets an IDR soon after requesting one.
- The full suite passed 20 times in a row, and clippy is clean.
- A manual loopback run of `nocomputer-pair watch` received 60 frames per second.
- The replay's frame spacing has gaps of up to 32 ms on Windows, because of timer granularity. Replay pacing only, not the real capture path.

Not yet done:
- the run with the Mac app;
- telling the viewer when capture stops;
- the viewer app (decode and display).
