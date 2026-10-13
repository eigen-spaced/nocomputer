# Tauri viewer

The Rust + Tauri viewer. It plays one local Annex B `.h264` file, or live video from a paired host (`--connect`), through WebCodecs in the system web view (WebView2 on Windows, WKWebView on macOS). Live mode forwards pointer, wheel and keyboard input (see Remote input); there is no clipboard yet. The C++ viewer in `viewer/` stays as the fallback comparison until this proof passes on native Windows.

- `src-tauri/src/annexb.rs` splits the stream into access units (SPS/PPS/SEI kept with their picture) and derives the codec string from the first SPS. Limits: 8 MiB file, 1 MiB per access unit, 10,000 units. The first unit must be an IDR.
- `src-tauri/src/main.rs` reads only the file named on the command line and sends it to the page as raw bytes.
- `ui/main.js` decodes with no `description` (Annex B, in-band parameter sets), keeps at most 4 chunks queued in the decoder, and closes every `VideoFrame`.

## Build and check (native Windows)

Prerequisites: VS 2022 Build Tools (C++ workload and Windows SDK), the Rust MSVC toolchain via rustup, and WebView2 (included with Windows 11). Run from `app/src-tauri` in a normal (not WSL) shell:

```
cargo test
cargo build
target\debug\nocomputer-viewer.exe --check --require-hardware ..\..\build\synthetic-srgb.h264
target\debug\nocomputer-viewer.exe --check --software ..\..\build\synthetic-srgb.h264
target\debug\nocomputer-viewer.exe ..\..\build\synthetic-srgb.h264
```

`--check` prints one JSON summary line (no pixel data) and exits 0 on pass. A pass needs every chunk decoded with no errors, a constant frame size, and `outputInOrder` (frame *i* carries chunk *i*'s timestamp; the fixtures have no B-frames). Use `cargo build --release` and `target\release\...` for timing; the page files are compiled into the executable, so rebuild after editing `ui/`.

| Fixture | Source | Expected |
| --- | --- | --- |
| `build/synthetic-srgb.h264` | 30 identical 320x240 frames | `avc1.4D001E`, 30 frames, 320x240 |
| `build/moving-1080p.h264` (sha256 `05f8e07a…ca33a7`) | Mac host's VideoToolbox encoder: Main, level 4.0, 8 Mbit/s, real-time, no B-frames, 60-frame GOP, IDRs at 0 and 60 each preceded by SPS/PPS/SEI | `avc1.4D0028`, 90 frames, 1920x1080; each frame shows its number 000-089 and a white bar moving left to right |

Recorded 2026-10-13 on the Windows target client: both fixtures pass in all three modes, with `outputInOrder` true. Visible playback of `moving-1080p.h264` (user-confirmed): the counter runs 000-089 in order with the bar moving; in native view the frame overflows the 1366x768 window, the arrows pan and **R** resets; frame 089 is held at EOF.

## Live video

```
nocomputer-pair pair HOST            # once: compare fingerprints, approve on the host
nocomputer-viewer.exe --connect HOST[:PORT]
nocomputer-viewer.exe --connect HOST --via ADDR[:PORT]
nocomputer-viewer.exe --check [--require-hardware] --connect HOST
```

- **Identity:** the viewer uses the same identity and known-hosts file as `nocomputer-pair` (`%APPDATA%\nocomputer`, or `NOCOMPUTER_STATE_DIR`), and connects only to a pinned host. Pairing in the app comes later.
- **`--via`:** dials another address for the same paired host, for example its LAN address when Tailscale is relaying. The pin still comes from HOST's known-hosts entry, so the host at ADDR must present that same identity or the connection is refused.
- **Delivery:** `live.rs` connects when the page asks for its first frame. The page pulls frames with a long-poll invoke (the pull path from the IPC bench) as raw bytes.
- **Dropping:** if the page or decoder falls behind, frames are dropped up to the next keyframe and one is requested. After a decoder error the page waits for a keyframe.
- **Timestamps:** decoder timestamps are the host's `host_us`.
- **Exit:** the session is closed on exit, so the host frees its single-controller slot at once.
- **Check mode:** `--check` decodes the first 300 frames and prints a JSON summary: frames, keyframes, drops on each side, size, and the largest gap between host timestamps.

Loopback check on the target client, 2026-10-13:
- **Setup:** `nocomputer-host --from-file build/moving-1080p.h264`, paired via `nocomputer-pair`.
- **Default and `--require-hardware`:** 305 frames received and decoded at 1920x1080, `avc1.4D0028`, with no decode errors and no drops on either side. 300 frames took 5.1–5.2 s, which is 60 fps pacing. The largest host gap was 32 ms (the replay timer's granularity on Windows).

## Remote input

Live mode sends input on the authenticated control stream. Nothing typed is logged; the status bar shows only a count of keys sent.

- **Pointer, buttons and wheel** come from the page. Positions are sent only inside the picture (not over the letterbox), at most once per animation frame. The right-click menu, middle-click autoscroll and back/forward navigation are suppressed so those buttons reach the host.
- **Keyboard (Windows):** keys are read with a low-level keyboard hook (`WH_KEYBOARD_LL`), which sees every key typed on this PC while it is installed. It is installed only while the viewer window is in the foreground in live mode, and removed otherwise. Windows does not call a low-level hook owned by the process whose WebView2 window is in the foreground, so the hook runs in a helper copy of the viewer (`nocomputer-viewer --keyboard-helper`). The viewer starts it, tells it when the window gains or loses the foreground (a WinEvent hook) and receives key events from it over pipes. The helper exits with the viewer.
- **Keys are physical:** scan codes become USB HID usages, so the host's keyboard layout decides the characters. Keys with no mapping (media keys) stay local.
- **Command key:** the status bar selects which key sends Command: Win (default) or right Alt. The choice is saved in the page's local storage.
- **Windowed:** Ctrl, Shift and Alt combinations go to the host, except Alt+Tab, Alt+Esc, Ctrl+Esc and Alt+F4, which stay with Windows. **Fullscreen:** every mapped key goes to the host.
- **Viewer shortcuts:** Ctrl+Alt+Shift with F (fullscreen), N (native), H (status), R (reset) or the arrows (pan). These never reach the host.
- **Release:** held keys and buttons are released when the window loses the foreground, on disconnect, and by the host at every session start and end.
- **Not capturable:** Ctrl+Alt+Del and Win+L are handled by Windows before any hook and always act locally.
- **Status bar:** drops, decode errors, keyframe age, arrival jitter (the worst difference over 2 s between an arrival gap on this PC and the matching capture gap on the host; each gap is on one clock), the deepest decode queue over 2 s, and the keyboard state.

Recorded 2026-10-13 against the Mac host (user-confirmed): pointer, clicks, dragging and typing work, and video keeps updating while dragging. Over Tailscale the path was relayed through DERP (Dubai, about 30 ms) although both machines were on the same Wi-Fi, and typing visibly lagged. With `--via` to the Mac's LAN address the user found it "definitely" snappier. The direct-path failure is not yet diagnosed.

## What counts as hardware-decode evidence

`--require-hardware` requests WebCodecs `prefer-hardware`, which Chromium treats as hardware-only. A pass is necessary but not sufficient evidence on its own, because it is a request rather than a per-frame report. The DevTools Media panel does not list WebCodecs decoders in WebView2, so use Windows' own accounting instead. Each GPU engine records running time per process, and the video-decode engine is fixed-function decode hardware.

Run `tools/gpu-decode-evidence.ps1` from `app/src-tauri` (paste its contents if the execution policy blocks the file). It plays the fixture visibly in each mode and adds up `\GPU Engine(*engtype_VideoDecode)\Running Time` over the viewer and all its descendant `msedgewebview2.exe` processes. That WebView2 instance is private to this app and loads one page, so nothing else in those processes decodes video during a run.

- Pass: every hardware run above 0 and attributed to the WebView2 `--type=gpu-process`; every software run exactly 0.
- Recorded 2026-10-13 on the target client (Dell Latitude 5400, 1366x768, i5-8265U, 16 GB, UHD 620 driver 31.0.101.2137, Windows 11 Pro build 26300, WebView2 Chromium 154): hardware 12.1 / 12.4 / 11.7 ms, all in gpu-process; software 0.0 / 0.0 / 0.0 ms.

The synthetic fixture's 30 frames are identical; `moving-1080p.h264` covers frame updates, decode order and a mid-stream IDR.

macOS (reported by the Mac agent on 2026-10-13, not verified from Windows): MacBook Air M3, macOS 26.6.2, WebKit 21624.5.1.11.3. Both fixtures pass `--check` in all three modes. During playback the viewer's own `com.apple.WebKit.GPU` process opens an `AppleAVD` (fixed-function decoder) client, seen with `tools/avd-decode-evidence.sh` (samples `ioreg -r -c AppleAVD -l` before, during and after playback; no sudo). WKWebView ignores `hardwareAcceleration`: `--software` also decodes in hardware, so macOS has no software control and software-mode runs there are not a software-decode test.

Record the Windows build, GPU and driver, WebView2/Chromium version (the `userAgent` field of the summary), and both summaries. The macOS WKWebView check is a separate step and is not covered by a Windows pass.

## Visible playback checklist

Same as the C++ viewer: correct gradient and channel order; it updates while playing; resizing keeps the aspect ratio with black letterboxing; **N** shows one video pixel per screen pixel; the arrow keys pan an oversized native view and **R** resets the pan; **F** toggles borderless fullscreen and **Esc** exits it, with the monitor resolution unchanged; the last frame is held at EOF; **H** hides the status line.

Recorded 2026-10-13 on the same target client: gradient and channel order correct (the hard edge 256 px from the left is the fixture's green channel wrapping, `G = x % 256`); letterboxing correct; last frame held; **N** shows the frame at exactly 320x240, centred; arrows and **R** do nothing at this size, as expected, since nothing overflows; **F**/**Esc** borderless fullscreen with no flicker or mode change; **H** works. Updating while playing and panning were covered later with `moving-1080p.h264` (see Build and check).

## IPC latency bench

`target\release\nocomputer-viewer.exe --bench-ipc` measures how long video-sized messages take to get from Rust into the page at 60 fps, 600 messages per phase. Latency runs from when a message is due to when the page has it as an `ArrayBuffer`, on a shared wall clock whose offset the page calibrates first. Both ends use monotonic clocks (Rust `Instant`, page `performance.now()`); the offset between the two processes is measured NTP-style from the fastest of 200 `invoke` round trips, and the reported error bound is half that round trip. No wall clock is involved, and this is one machine, not a host/client subtraction. Pass a fixture (`--bench-ipc ..\..\build\moving-1080p.h264`) to also decode and draw it in a loop at 60 fps during every phase. `pull` is a long-poll: the page requests message *n* as soon as it has *n*-1, and the command holds the request until *n* is due.  It covers the IPC hop only: decoding and display come on top, and a native viewer would also pay a smaller copy. `push` is a Tauri `Channel`, where payloads over 1 KiB are announced by `eval` and then fetched, an extra round trip. `pull` is one `invoke` per message, answered when the message is due.

Recorded 2026-10-13 on the Windows target client. Values are ms, the range across runs; clock error ±0.85 to ±1.3 ms. "Idle" is 5 runs with no decode load (the first 3 used wall clocks, the last 2 monotonic clocks). "Load" is 3 runs with `moving-1080p.h264` decoded and drawn at 60 fps throughout (about 3,590 frames per run, no errors).

| Size | pull idle p50 / p99 | pull load p50 / p99 | pull load > 16.7 ms (of 600) | push load p50 / p99 |
| --- | --- | --- | --- | --- |
| 32 KiB (typical 1080p delta frame) | 2.4-2.8 / 3.7-4.0 | 3.0-3.1 / 5.7-9.6 | 0-1 | 5.1-5.5 / 10.5-13.5 |
| 128 KiB (busy delta frame) | 4.0-4.8 / 5.6-6.8 | 4.3-5.3 / 6.9-8.6 | 0 | 7.5-7.7 / 13.5-15.9 |
| 512 KiB (keyframe) | 8.6-9.8 / 11.7-16.3 | 10.9-11.3 / 16.8-42.0 | 6-21 | 12.6-13.5 / 21.4-24.6 |

Pull beats push at every size, idle and under load. A live stream should pull, keeping one request outstanding while the previous frame decodes. Under load, ordinary frames still arrive in about 3 ms; the cost shows in the tail, mostly for keyframe-sized messages, which are rare at the host's settings (8 Mbit/s, 2 s GOP). A 512 KiB message every frame at 60 fps is a deliberate worst case.
