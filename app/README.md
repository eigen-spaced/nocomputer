# Tauri viewer

The Rust + Tauri viewer. It plays one local Annex B `.h264` file, or live video from a paired host (through its start screen), through WebCodecs in the system web view (WebView2 on Windows, WKWebView on macOS). Live mode forwards pointer, wheel and keyboard input (see Remote input) and shares plain-text clipboard both ways (see Clipboard). The C++ viewer in `viewer/` stays as the fallback comparison until this proof passes on native Windows.

- `src-tauri/src/annexb.rs` splits the stream into access units (SPS/PPS/SEI kept with their picture) and derives the codec string from the first SPS. Limits: 8 MiB file, 1 MiB per access unit, 10,000 units. The first unit must be an IDR.
- `src-tauri/src/main.rs` reads only the file named on the command line and sends it to the page as raw bytes. With no file, it opens the start screen.
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
nocomputer-viewer.exe                                   # the start screen
nocomputer-viewer.exe --connect HOST[:PORT]             # straight to HOST's login (or pairing)
nocomputer-viewer.exe --connect HOST --via ADDR[:PORT]
nocomputer-viewer.exe --check [--require-hardware] --connect HOST
```

- **Start screen (`connect.rs`):** saved hosts with Connect and Forget, and an address field for a name or IP. The host must have Remote access on.
- **Pairing:** a host this PC hasn't saved goes through pairing first. The screen shows the host's fingerprint and this PC's, which must match what the host shows; then the host's user approves within 60 s. Asking for approval opens a fresh connection and checks the host still has the fingerprint shown, so the pairing screen has no time limit. Cancel withdraws the request, and the host closes its prompt. If the host already has this PC paired (this PC lost its saved copy), checking the host's fingerprint is enough. After approval the host is saved, pinned to that fingerprint.
- **Login (protocol v4):** every connection then needs the host account's username and password; the host checks them (on a Mac, against the signed-in account) and sends nothing else until they pass. A wrong login gets one message whatever was wrong, and the host makes later tries wait longer. After 5 tries, or 2 minutes, the host closes the connection; a timed-out login reconnects and asks again. The password goes to Rust as a command argument and is moved at once into proto's `Password`, which clears it when dropped. It is never saved, logged or put in an event, and the form clears its field as soon as it is sent; the JavaScript string and the IPC buffer can't be cleared. The username is remembered per host in the page's local storage.
- **Errors in plain words:** can't reach the host (off, or Remote access off), lost connection, the host ended the session (Remote access turned off, say), not paired, revoked, another viewer connected, connected again from elsewhere, version mismatch, a pairing request already pending, too many logins. A changed host identity is a warning with both fingerprints, and nothing is sent to that host.
- **Disconnect:** the status-bar button ends the session and returns to the start screen. A session that ends any other way also returns there, saying why, instead of freezing on the last frame.
- **Stream size (protocol v5):** the page tells the host the canvas's exact size in device pixels (`devicePixelContentBoxSize`), at the start and after each resize, fullscreen or fit/native change, debounced 200 ms. The host scales its screen once, straight to that size, and fit mode draws a frame within 4 px of the canvas 1:1 without smoothing. Native mode asks for the host's native size (`VIEW_SIZE_NATIVE`), which the host caps. A keyframe whose SPS differs from the last one gets a fresh decoder; frames before the resized keyframe are scaled as before.
- **Identity:** the viewer uses the same identity and known-hosts file as `nocomputer-pair` (`%APPDATA%\nocomputer`, or `NOCOMPUTER_STATE_DIR`).
- **`--via`:** dials another address for the same paired host, for example its LAN address when Tailscale is relaying. The pin still comes from HOST's known-hosts entry, so the host at ADDR must present that same identity or the connection is refused.
- **Delivery:** after login, `connect.rs` hands the session to `live.rs`. The page pulls frames with a long-poll invoke (the pull path from the IPC bench) as raw bytes.
- **Dropping:** if the page or decoder falls behind, frames are dropped up to the next keyframe and one is requested. After a decoder error the page waits for a keyframe.
- **Timestamps:** decoder timestamps are the host's `host_us`.
- **No reordering:** each keyframe's SPS is rewritten to say frames are never reordered (`sps.rs`). VideoToolbox's SPS has a VUI without `bitstream_restriction`, so Chromium's decoder held several frames before output, and on a quiet screen, where frames come only on change, each waited for later ones.
- **Exit:** the session is closed on exit, so the host frees its single-controller slot at once.
- **Check mode:** `--check` decodes the first 300 frames after login and prints a JSON summary: frames, keyframes, drops on each side, size, and the largest gap between host timestamps.

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
- **Pointer off the window:** a key pressed while the pointer is not over the viewer window (or is over a window in front of it) stays with Windows, even with the viewer in front, so Windows shortcuts such as Win+Shift+S work by moving the pointer away. Keys already held when the pointer leaves still finish on the host, so none sticks. Keys that reach the page this way don't work the browser (no reload, print or find); Alt+F4 still closes the viewer.
- **Viewer shortcuts:** Ctrl+Alt+Shift with F (fullscreen), N (native), H (status), R (reset) or the arrows (pan). These never reach the host.
- **Release:** held keys and buttons are released when the window loses the foreground, on disconnect, and by the host at every session start and end.
- **Not capturable:** Ctrl+Alt+Del and Win+L are handled by Windows before any hook and always act locally.
- **Status bar:** drops, decode errors, keyframe age, arrival jitter (the worst difference over 2 s between an arrival gap on this PC and the matching capture gap on the host; each gap is on one clock), the deepest decode queue over 2 s, the keyboard state (and why it is off: window not in front, no session, helper not running), and key timing. Key timing starts when a key-down is read from the hook: *send* ends when it is written to the connection, *key→frame* when the next video frame arrives, and *shown* when that frame leaves the decoder (avg/max over the last 32). Each part is on one monotonic clock; the Rust-to-page hop (about 3 ms) is not counted. The next frame is the key's echo only if nothing else on the screen changes, so measure typing into a still window with the pointer parked.
- **Foreground:** a WinEvent hook reports the window coming to the front, and a 250 ms poll of `GetForegroundWindow` backs it up; in one run the events never arrived and the keyboard stayed off.

Recorded 2026-10-13 against the Mac host (user-confirmed): pointer, clicks, dragging and typing work, and video keeps updating while dragging. Over Tailscale the path was relayed through DERP (Dubai, about 30 ms) although both machines were on the same Wi-Fi, and typing visibly lagged. With `--via` to the Mac's LAN address the user found it "definitely" snappier. The direct-path failure is not yet diagnosed.

Recorded 2026-10-14 (user-confirmed): with the viewer in front and the pointer off its window, Win+Shift+S opens the Windows screenshot tool; with the pointer over the window, typing goes to the host as before. Tailscale showed a direct path (12 ms) that day.

Key timing over Tailscale (DERP), 2026-10-13, typing into a still Terminal on the Mac, viewer numbers in ms as avg/max:

| | key→frame | shown | n |
| --- | --- | --- | --- |
| Before the SPS rewrite (about 4 fps on a quiet screen) | 52 / 249 | 573 / 1509 | 39 |
| After | 56 / 164 | 58 / 166 | 89 |

*send* stayed at most 0.2 ms. After the rewrite, a frame is decoded about 2 ms after it arrives; the user called typing "blazing fast". The Mac side at the same time (its own clock, 60 fps): key injected → next captured frame 40.5 / 62.2 ms, encode 15.1 / 24.9 ms. The two sides' "next frame" can be different frames, so these don't add up exactly.

## Clipboard

Live mode shares plain text both ways on the authenticated connection (protocol v3 side streams), while the **Clipboard** switch in the status bar is on. It is on by default; the choice is saved in the page's local storage, and the page sets it before the session starts, so a saved "off" holds from the start. `--check` never shares.

- **Windows side (`src/clipboard.rs`):** a message-only window on its own thread is registered with `AddClipboardFormatListener` only while a session is up and the switch is on, and does every clipboard read and write.
- **Sent:** what is on the clipboard when sharing starts, then each copy, with `\r\n` made `\n` and the text cut at its first NUL. Nothing is sent for copies with no text, text that is empty or over 1 MiB, text equal to the last sent or received, or copies other apps mark private: `ExcludeClipboardContentFromMonitorProcessing`, `Clipboard Viewer Ignore`, or `CanIncludeInClipboardHistory` / `CanUploadToCloudClipboard` holding anything but 1 (password managers set these).
- **Received:** the host's text is put on the clipboard with `\n` made `\r\n` and `CanUploadToCloudClipboard` = 0, so Windows doesn't sync it to the cloud clipboard; it can still appear in local clipboard history (Win+V). Text arriving while the switch is off is dropped. The write is recognised by its clipboard sequence number and not sent back.
- **Never logged:** the status bar shows only counts: sent, received, skipped, and failed (the clipboard stayed busy for 200 ms, or a write failed).
- **Live test** (`118d6c6`, user-confirmed 2026-10-14, against the Mac host): multi-line text Windows→Mac, ü and emoji Mac→Windows, password-manager copies skipped on both sides, and the switch off on each side. Text from the Mac appears in local Win+V history.
- **Test against the real clipboard** (it overwrites the clipboard and puts its text back): `cargo test -- --ignored clipboard`.

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
