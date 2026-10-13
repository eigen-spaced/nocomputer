# local-v1 golden vectors

Reference encodings of every local-v1 frame. Each file is one complete frame: length, type and payload. The specification is the module doc of `src/local.rs`. Both sides test against these bytes: the Rust host in `local::tests`, and the Swift app in its own tests.

| File | Frame |
| --- | --- |
| `01-hello.bin` | Hello, version 1 |
| `02-access-unit-key.bin` | AccessUnit, keyframe, capture_us `0x0102030405060708`, SPS + PPS + IDR (3- and 4-byte start codes) |
| `02-access-unit-delta.bin` | AccessUnit, not a keyframe, one non-IDR slice |
| `03-capture-stopped.bin` | CaptureStopped with a non-ASCII UTF-8 reason |
| `04-pairing-decision-approve.bin` | PairingDecision, request_id `0x0A0B0C0D0E0F1011`, approve |
| `04-pairing-decision-deny.bin` | PairingDecision, request_id 7, deny |
| `81-hello-ack.bin` | HelloAck, version 1 |
| `82-start-stream.bin`, `83-stop-stream.bin`, `84-request-keyframe.bin` | empty frames |
| `85-event-pairing-prompt.bin`, `85-event-session.bin` | Event JSON |

The Annex B payloads are syntactically minimal stand-ins, not decodable video. Regenerate with `NOCOMPUTER_WRITE_GOLDEN=1 cargo test golden`, and only for a deliberate change to the contract. Any change means a new contract version.
