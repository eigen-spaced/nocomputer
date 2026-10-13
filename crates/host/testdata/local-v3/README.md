# local-v3 golden vectors

Reference encodings of every local-v3 frame. Each file is one complete frame: length, type and payload. The specification is the module doc of `src/local.rs`. Both sides test against these bytes: the Rust host in `local::tests`, and the Swift app in its own tests.

| File | Frame |
| --- | --- |
| `01-hello.bin` | Hello, version 3 |
| `02-access-unit-key.bin` | AccessUnit, keyframe, capture_us `0x0102030405060708`, SPS + PPS + IDR (3- and 4-byte start codes) |
| `02-access-unit-delta.bin` | AccessUnit, not a keyframe, one non-IDR slice |
| `03-capture-stopped.bin` | CaptureStopped with a non-ASCII UTF-8 reason |
| `04-pairing-decision-approve.bin` | PairingDecision, request_id `0x0A0B0C0D0E0F1011`, approve |
| `04-pairing-decision-deny.bin` | PairingDecision, request_id 7, deny |
| `05-input-unavailable.bin` | InputUnavailable with a non-ASCII UTF-8 reason |
| `06-clipboard.bin` | Clipboard, app to host: two lines, non-ASCII and an emoji |
| `81-hello-ack.bin` | HelloAck, version 3 |
| `82-start-stream.bin`, `83-stop-stream.bin`, `84-request-keyframe.bin` | empty frames |
| `85-event-pairing-prompt.bin`, `85-event-session.bin` | Event JSON |
| `86-pointer.bin` | Pointer x `0x1234`, y `0xFFFF` |
| `87-button-left-down.bin`, `87-button-forward-up.bin` | Button 0 down, button 4 up |
| `88-scroll-pixel.bin` | Scroll dx -3, dy 120, pixels |
| `88-scroll-line.bin` | Scroll dx 0, dy -100, lines |
| `89-key-down.bin`, `89-key-up.bin`, `89-key-repeat.bin` | Key usage `0x04` down, `0xE3` up, `0x28` repeat |
| `8a-release-all.bin` | ReleaseAll |
| `8b-clipboard.bin` | Clipboard, host to app: two lines, non-ASCII and an emoji |

The Annex B payloads are syntactically minimal stand-ins, not decodable video. Regenerate with `NOCOMPUTER_WRITE_GOLDEN=1 cargo test golden`, and only for a deliberate change to the contract. Any change means a new contract version.
