# Initial development plan

## Product baseline

Build a remote desktop that makes terminal and browser work comfortable on Windows while controlling the Mac's existing display. Use one-time pairing for unattended access to an existing user session. Keep networking independent of Tailscale.

Initial host: M3 MacBook Air, 16 GB RAM, macOS Tahoe 26.6.x, lid open, Wi-Fi. Initial viewer: Windows 11, eighth-generation Core i5, 16 GB RAM, 1366x768 display. Resolutions and client capabilities must be negotiated rather than assumed.

Required: keyboard, mouse, scrolling, right-click, fit-to-window, fullscreen, plain-text clipboard, encrypted/authenticated connection, pairing/revocation, locked-session access, and bounded buffering.

Optional initially: native-size/zoom view to make text usable on a small viewer screen. Test readability before deciding whether fit-to-window alone is sufficient.

Defer: audio, file transfer, multiple displays, multiple controllers, discovery, NAT traversal, cloud relay, session recording, logout/reboot access. Reverse-direction control is planned after the initial path works.

## Power and session boundary

V1 may keep the Mac awake while remote access is enabled, while allowing display sleep and screen lock. The user accepted this initial fallback; actual wake from system sleep is still desired later. External power is preferred for planned longer sessions.

A locked user session, a sleeping computer, and a machine awaiting login after reboot are separate states. V1 supports the first if feasibility succeeds. Do not require disabling screen lock, auto-login, FileVault, or OS security protections.

True remote wake needs a separate investigation of hardware/network behavior and possibly an always-on local helper. No helper hardware has been confirmed. An unreachable sleeping host cannot receive commands from its own application.

## Local setup inventory

Observed on 2026-09-30:

| Item | Observed state |
| --- | --- |
| Workspace | Empty before planning files; not initialized as a Git repository |
| macOS | 26.6.2, build 25G83 |
| Architecture | arm64 |
| Selected developer directory | `/Library/Developer/CommandLineTools` |
| Swift | 6.3.3 |
| macOS SDK | 26.5 |
| Rust / Cargo | 1.98.1 |
| FFmpeg | Present at `/opt/homebrew/bin/ffmpeg`; capabilities not checked |

Before the first app prototype:

1. Check whether full Xcode is installed and compatible with this machine. Command Line Tools are selected; full Xcode availability and signing setup are unverified.
2. Create a minimal Swift macOS application with a stable bundle identifier so permissions belong to a reproducible app identity. Document how to build/run it locally.
3. Check Screen Recording and Accessibility permission flows manually. App identity/signing changes may affect permissions; record the exact tested configuration.
4. Establish a way to build and run the Windows viewer on the Windows machine. Confirm its GPU, driver, and decoder capabilities. Do not assume remote execution or Windows CI access exists.
5. Initialize version control during implementation and exclude generated builds, recordings, credentials, and pairing state. License choice can wait; it is not an obstacle to local prototypes.

## Milestone 0: macOS feasibility

Implementation status (2026-09-30): the local Swift/AppKit diagnostic app and bundle build script are implemented. Build and code-signature verification passed with the installed Command Line Tools; full Xcode is absent. The app launched and its process remained running. Three synthetic frame-state tests passed using `bash scripts/test-macos.sh`. Git has been initialized without a commit. See [the manual checklist](macos-feasibility.md) for the interactive checks still required. Lock-screen visibility and input remain unverified; this prototype is not completion of Milestone 0.

Update (2026-10-01): the user confirmed unlocked preview and ordinary keyboard injection. Both lock/unlock trials terminated capture with no capture source, despite granted permissions and keep-awake enabled. The dead-stream lifecycle bug preventing Stop/Start has been addressed; manual recovery verification is next. Fresh capture while locked and an alternative supported capture/filter approach must still be investigated before treating locked access as feasible.

Subsequent evidence: the third export confirms dead-stream release and successful Stop; the failure is ScreenCaptureKit `-3815`. A brief lock/unlock with the physical display still lit preserves capture according to the user. Added temporary display keep-awake, an explicitly requested delayed display-off command, and workspace sleep/wake notification journaling to compare equal-duration locks with display on/off. These comparisons remain pending; do not attribute the failure solely to locking or solely to display sleep yet.

Comparison completed in `diagnostics-1790871588.json`: capture continued through the approximately 71-second display-kept-awake trial; automatic display sleep in the next trial preceded terminal `-3815` by about 17 ms. No system sleep was reported. Display sleep is now the leading trigger for the tested configuration, while lock-screen visibility/input remain unproven. Next investigate whether a different display capture filter or supported display-wake/session arrangement can meet the display-off requirement. Keeping the display lit is a diagnostic workaround, not a replacement for the agreed display-off requirement.

Next experiment implemented: selectable whole-display filter (no application exclusions, metadata-only output to avoid recursion) and opt-in fresh-stream startup after display wake. Seven synthetic tests pass. Manual filter comparison and wake recovery are pending; no supported continuous display-asleep solution has been established. Separately, ad-hoc signing uses a binary-specific designated requirement and there are no valid signing certificates installed; stable certificate-backed signing is needed to resolve permission persistence across changed builds.

Whole-display comparison completed (`diagnostics-1790877389.json`): `-3815` still follows display sleep by about 15 ms. No application exclusions were used, so changing that filter did not solve the failure. Automatic fresh capture after display wake succeeded in about 1.13 seconds and delivered complete frames. Next investigate a supported display-wake request on an awake but locked host, then capture startup before local authentication; unlock timing and lock-screen pixels have not yet been observed. Older raw exports were moved to Trash at the user's request after preserving their findings in the feasibility notes; the latest raw export remains in the root.

Scheduled wake probe implemented: after a user-selected 60-second delay, request display wake through `IOPMAssertionDeclareUserActivity`, hold the display awake temporarily, start default-filter capture, and retain one recent frame in memory for explicit review after local unlock. Metadata includes display state and timing, but no pixels. Cancellation/quit releases assertions; no credentials or input are injected. Hardware outcome remains pending.

Wake trial `diagnostics-1790878507.json` confirms display wake and fresh capture with complete frames followed by idle callbacks. Review failed because the diagnostic incorrectly expired an unchanged image using complete-frame age. Fixed preview freshness to accept idle callbacks while still rejecting stalls and terminal streams. Repeat pixel review is pending; locked-screen input remains untested.

Successful follow-up `diagnostics-1790878826.json`: display wake at 575012.581, fresh capture startup at 575014.349, and in-memory frame retention at 575019.526942. The user's retained-frame screenshot shows the actual macOS lock screen/password prompt. Lock-screen capture after public display wake is now demonstrated on the target machine. Next test deliberately scoped input at that lock screen without retaining credentials. Milestone 0 remains incomplete until input is verified. No rebuild is needed to record these findings, preserving the currently granted app identity.

Input experiment implemented: explicitly armed timed pointer movement and a single Tab down/up after wake/capture, with in-memory before/after frame review. Eleven synthetic tests pass. No password, printable text, click, or Return is injected; visual verification of lock-screen input delivery remains pending. Existing Accessibility grants may need refresh for the changed ad-hoc build.

Input trial visual result: the user confirmed pointer movement and provided before/after lock-screen frames matching the requested pointer position. Mouse movement injection is verified on this machine. Tab produced no observable focus change, so keyboard delivery remains inconclusive. Next use a deliberately armed fixed nonsecret character plus Backspace without submitting, or verify through the eventual authenticated viewer. OS unlocking should initially use ordinary forwarded input; a separate username/password UI is not required by the product scope.

Keyboard follow-up implemented: replace the Tab probe with a fixed nonsecret marker, retain the intermediate frame, and erase with Backspace without pressing Return. Prebuild erasure before insertion and clean it up on cancellation/failure/normal quit. Twelve synthetic tests pass; hardware result pending.

Keyboard follow-up verified (`diagnostics-1790880448.json`, 2026-10-01): display wake at 576602.685, fresh capture at 576604.569, marker posting at 576609.912, intermediate frame retention at 576612.954497125, and Backspace posting at 576612.956. Both effective permissions are granted. The user's before/after screenshots show the lock screen changing from the Touch ID/password prompt to a password field containing one masked character. Ordinary injected keyboard input at the locked session is demonstrated, alongside previously verified pointer movement and capture. Backspace was posted, but visual erasure and actual password submission/unlock are not yet verified. The core locked-session feasibility gate is passed for this machine; remaining interaction/lifecycle checks are not automatically complete. Next implement the independent local capture/encode path in Milestone 1, without network input or credential handling.

Build a small local diagnostic app before networking or a production UI.

- Capture the physical display using ScreenCaptureKit and show frame status/timestamps locally.
- Verify input injection with a controlled local test: movement, buttons, scrolling, ordinary keys, and modifiers. Avoid logging typed text.
- Test capture and input separately across unlocked session, explicit screen lock, display off, and unlock. Repeat after idle time; immediate lock/unlock alone is insufficient.
- Determine what a new connection could capture when the session is already locked, rather than only testing a stream started while unlocked.
- Establish whether a user-session app suffices or whether a supported helper/service arrangement is needed. Administrator permissions are acceptable to the user, but are not evidence that protected-screen access will work.
- Prototype scoped sleep prevention and verify release when remote access is disabled or the app exits. Test true system sleep separately so it is not confused with display sleep.

Deliverable: a reproducible build, a manual state matrix, and a conclusion about supported lock-screen capture/input. If a public supported approach cannot satisfy locked access, report the limitation and revise the plan with the user before expanding the implementation.

## Milestone 1: video path and readability

Initial encoding diagnostic implemented (2026-10-01): opt-in ScreenCaptureKit pixel buffers passed directly to VideoToolbox without an application-side pixel copy. Requires hardware H.264, real-time encoding, no frame reordering, Main auto-level, 8 Mbit/s requested average bitrate, 30 fps expected rate, and a 60-frame/two-second keyframe interval. The application admits at most three outstanding frames and drops new raw frames before encoding under pressure. Encoded samples are measured and discarded, never saved or sent. Preview copies are disabled during this test; dimensions are rounded down to even numbers without changing the physical display. Metadata reports sizes, keyframes, pending/drop counts, hardware selection, parameter-set/NAL framing, and monotonic submission-to-callback timing (not end-to-end latency).

The real M3 hardware encoder passed the opt-in synthetic-source test. Its optional `MaxFrameDelayCount` property returned `kVTPropertyNotSupportedErr`; this is recorded rather than silently claimed as applied. The independent application-side three-frame cap remains enforced. Live desktop encoding and decode/render/readability are still pending. This does not complete Milestone 1 or prove zero-copy inside the codec: the BGRA source may require internal conversion.

Live desktop trial verified (`diagnostics-1790881393.json`, 2026-10-01): hardware H.264 at 1600×1034 for approximately 104.57 seconds, 1761 submitted/encoded frames, 50,348,442 compressed bytes, 53 keyframes, no raw/encoder drops or encoder error, peak one outstanding frame, and zero pending at clean Stop. Mean submission-to-callback time was 11.21 ms; maximum 26.04 ms. Format reported two parameter sets and four-byte NAL lengths. Derived average compressed payload rate was approximately 3.85 Mbit/s across the whole run including idle periods; requested average bitrate remains 8 Mbit/s, not a measured constant rate. This verifies local desktop encoding without buildup in this workload, not decode correctness, readable text, constant 30 fps, or end-to-end latency. Next prove independent decoding/rendering and encoder recreation; Windows checks still require the actual client.

On user request, the six superseded root exports through `diagnostics-1790880448.json` were moved to the user's Trash after their findings were preserved in the feasibility notes. The newest encoding export remains in the project root.

Candidate Mac path: ScreenCaptureKit pixel buffers to VideoToolbox low-latency H.264 encoding. Start with 30 fps as a configurable baseline; avoid frame reordering and unnecessary copies. Validate actual encoder settings rather than assuming flags guarantee latency.

Local decode/readability diagnostic added (2026-10-01): optional VideoToolbox hardware decoder consumes every encoded sample with its format description in memory. Synchronous decode backpressures encoder callbacks, and encoder admission remains bounded until downstream decode finishes; no encoded-frame queue or arbitrary encoded drops. This deliberately simple loop is a Mac-only correctness aid, not a Windows renderer or transport validation. An independent resizable/fullscreen-capable AppKit preview shows decoded pixels via a copied CGImage at 2 Hz, with aspect-preserving fit. Source capture excludes all diagnostic windows to prevent recursion. Blank/suspended/stalled/failed capture, codec errors, and Stop clear the visible image; idle static desktops keep the existing image. Metadata exports contain decoder counts/dimensions/hardware selection and monotonic decode submission-to-callback timing, excluding preview conversion. Synthetic hardware round trips verify red/blue pattern channel order and orientation, equal encoded/decoded counts, session recreation, and image release on closure. Live desktop text/colour review remains pending, as do Windows decoding/rendering and internet measurements. The decision to add a Mac-only loop before the Windows decoder isolates codec correctness without claiming client-platform coverage.

First decode a sample on Windows, then render into a resizable window with aspect-preserving scaling. Evaluate native Windows decoding/rendering and library-based alternatives against build complexity and measured results before selecting one.

Live visual feedback (2026-10-01): the user confirms the decoded desktop looks good overall, but colours appear somewhat washed out and text could be sharper. Their screenshot includes a screenshot-selection overlay; do not attribute that overlay to the app or infer exact contrast from it. No new loopback metadata export was found (the root still contains only the previous encoder-only report). Live decode is visually demonstrated, but decoder timing/counts and Stop/Start recovery are not yet confirmed by an export. Next quality investigation should compare uncompressed and decoded images at identical scale, inspect colour attachments/format metadata through capture/encode/decode, and compare native-size versus fit-to-window presentation before adding sharpening. The current path sets no explicit end-to-end colour policy; that is a hypothesis to investigate, not a proven cause. Windows quality remains untested.

Follow-up export reviewed (2026-10-02, `diagnostics-1790882543.json`): hardware encoding and decoding of 1600×1034 succeeded with 3720 encoded frames and 3720 decoded frames, no codec errors, 98 keyframes, zero encoder drops, one raw frame dropped before encoding, peak three outstanding frames, and zero outstanding at the export snapshot. Mean decode time was 3.065 ms, maximum 8.134 ms; mean encode time was 11.970 ms with a 102.694 ms maximum observed early in startup. These are separate local stage timings, not end-to-end latency; the synchronous decode/preview path can backpressure encoding. The single raw drop respected the admission bound and did not prevent subsequent decode. This export has one capture startup and no Stop/restart events, so live recreation/cleanup is still not verified by this report. Quality issues remain as reported; next work is the controlled source/decoded colour and scaling comparison, not sharpening by assumption.

Quality diagnostics implemented (2026-10-02): raw/decoded review images now come from the same encoder submission, sampled at 2 Hz with bounded retention. A freeze control permits switching between that exact pair. Fit versus one-stream-pixel-per-physical-viewer-pixel modes isolate display resampling; native mode provides scrolling rather than scaling a large image down. Exports include known colour attachments at source, compressed format, and decode output, image colour spaces, comparison PTS, backing scale, and selected view. No pixels are exported. Optional explicit SDR sRGB capture plus BT.709 primaries/matrix and sRGB encoder transfer can be compared against the unchanged default path. It is not a confirmed fix for the user's reported washout. No sharpening, forced host resolution, or production rendering change was introduced. Diagnostic raw-image copies happen before the encode timer and can affect throughput; stage timings still do not establish end-to-end latency.

Twenty-five tests passed with hardware and synthetic-fixture generation explicitly enabled. The sRGB grayscale source `[0,16,32,64,96,128,160,192,224,255]` decoded as `[0,16,31,64,95,128,159,192,223,255]`, at most one code value different. Red/blue tests still verify orientation/channel order and matched-pair retention/release; geometry tests cover physical-pixel scaling and letterboxing. An independent FFmpeg 9.0.2 software H.264 decode consumed all 30 frames of a generated 320×240 synthetic fixture without errors. ffprobe reports Main/yuv420p, limited video range, BT.709 primaries/matrix, and IEC 61966-2-1 transfer. This verifies basic elementary-stream interoperability on the Mac, not Windows hardware decoding, text readability, or the future wire protocol. Live default-versus-sRGB/native comparison and Stop/Start checks require user interaction; the Windows decoder trial requires the actual Windows machine. Do not mark Milestone 1 complete until those checks are done.

Use terminal output, small browser text, scrolling, and window movement as workloads. Evaluate compression, color/chroma effects on text, and fit-to-window readability. Test hardware decoding and define a fallback policy where needed.

UI cleanup (2026-10-02, user request): only four main-window checkboxes remain: system keep-awake, encoding, local decoding, and explicit SDR sRGB. The completed filter/restart/lock-input feasibility checkboxes were removed. Automatic restart-on-display-wake was retired; the verified explicit wake/fresh-capture path remains available. Occasional whole-display metadata capture, temporary display keep-awake, and the explicitly confirmed legacy lock-input probe are accessible as actions in the Diagnostics menu rather than persistent main-window toggles. Existing permission rules and scoped assertion cleanup are unchanged. No new locked-session capability or Windows behavior is claimed by this cleanup.

Latest visual/export review (2026-10-02, `diagnostics-1790966488.json`): the user reports raw default/sRGB colours agree and resemble the desktop, while decoded colours remain washed out. The provided screenshots use different magnifications, so do not quantify compression-related text softness from them. Both local runs stopped cleanly, and a fresh sRGB codec/capture session followed the default run. Default: 1721 encoded/decoded, one pre-encode raw drop, no codec errors; sRGB: 2363 encoded/decoded, no drops/errors, mean/max encode 8.878/50.616 ms and decode 2.702/10.960 ms. Both reached but did not exceed three outstanding frames and ended with zero pending. Live Stop/recreation is now evidenced, but colour quality and Windows behavior remain open.

The sRGB export reports BT.709 primaries/matrix, IEC_sRGB transfer, and limited compressed range; decoded attachments preserve those tags. Synthetic grayscale tests with either explicit transfer attachments or only an sRGB source colour space both differ by at most one code value. Thus the synthetic checks do not reproduce the live washout, and no contrast/gamma workaround is justified yet. Persist last rendered-image colour-space metadata after Stop (images still released) and add aggregate 32×32 sRGB comparison measurements without exporting samples. These are diagnostics, not a perceptual quality score; preview conversion/measurement adds sampled work outside codec stage timing.

Add an optional 2560-wide text-detail stream alongside the unchanged 1600-wide default. Derive source physical size from the filter's content rectangle in points and point-to-pixel scale, preserving aspect ratio and even codec dimensions without changing the host display or upscaling a smaller source. Geometry and a synthetic 2560×1656 hardware encode/decode smoke test cover the new option; live text, bitrate and performance checks remain pending. Capture downscaling can discard small text detail that a native-pixel preview cannot restore. Next compare frozen raw/decoded pairs at identical viewing mode, including the higher stream width, and review the new aggregate measurements before selecting a colour correction or production renderer.

Validation of this update: all 29 tests passed with hardware and synthetic-fixture generation enabled, including the higher-resolution round trip and aggregate comparison checks. The diagnostic bundle was rebuilt and reopened. Effective privacy permissions and live colour/text quality still need manual verification after the changed ad-hoc build.

Deliverable: independent capture/encode and decode/render prototypes, documented media framing requirements, and a usable quality baseline on the actual client.

## Immediate next checkpoint (2026-10-02)

The user accepts current video quality as sufficient for a PoC and prefers to defer colour tuning. This is a Mac-local video-path checkpoint, not completion of Milestone 1 on the actual Windows viewer or of the usable-release criteria. Keep existing colour/bitrate defaults and the optional 2560 text-detail setting; do not add sharpening or speculative gamma/contrast changes before measuring the actual viewer.

Latest trial (`diagnostics-1790967412.json`): the user reports definite improvement at 2560 width/native viewing. Hardware encoded/decoded 1531 frames at 2560×1656, with no codec errors, one pre-encode raw drop, zero encoder drops, peak three outstanding and zero pending at clean Stop. Encode mean/max 19.749/74.189 ms; decode 4.181/11.761 ms. Those are local stage timings, not total delay. Last sampled comparison mean absolute RGB difference is 0.998 code values, signed shift +0.661 and dark shift +0.284; no sampled source transparency. This is only the last sampled pair, not necessarily the frozen screenshot pair, and cannot dismiss the earlier subjective washout. Source/decoded CGImages both report DeviceRGB despite sRGB capture/compressed/decode attachments. A tentative explicit preview-output colour change was set aside without changing the proven visual path. Schema 12 now retains a separate last-viewed comparison record (PTS, view settings, colour metadata and aggregate measurements, no images), including across Stop, to avoid conflating a frozen review with the newest decoder sample.

Next sequence:

1. Build a minimal Windows decode/render prototype using only the generated synthetic H.264 fixture. Select its implementation against reproducible Windows build requirements; Mac-only checks cannot establish Windows hardware decoding. No live networking or remote input yet.
2. On the actual Windows client, verify decoder hardware/fallback behavior, orientation/colour, resizing, fit/native viewing and fullscreen. Record build/driver/capability information; do not hard-code the current 1366×768 display.
3. Prove a mutually authenticated paired connection, credential storage and revocation on both platforms. Then carry bounded live video with explicit framing and keyframe recovery.
4. Add authenticated input/clipboard and test the locked-host internet scenario. Revisit colour/readability and bitrate on that real end-to-end path.

## Windows viewer checkpoint: Rust + Tauri (2026-10-13)

The user chose Rust + Tauri 2 for the viewer instead of the C++/FFmpeg/SDL3 prototype, with the goal of a NoMachine alternative. The offline prototype in `app/` (see `app/README.md`) plays one bounded local Annex B file through WebCodecs. It has no networking, input or clipboard.

On the actual client (Dell Latitude 5400, i5-8265U, UHD 620 driver 31.0.101.2137, 1366×768, Windows 11 Pro build 26300, WebView2/Chromium 154), both fixtures decode completely and in order in default, hardware-required and software modes: `build/synthetic-srgb.h264` (30 frames, 320×240), and `build/moving-1080p.h264` (90 frames, 1920×1080, produced by the Mac host's VideoToolbox encoder with an IDR in mid-stream). The per-process GPU VideoDecode engine counter shows hardware decode, with a software-mode control at zero. The user confirmed colour and orientation, letterboxing, the moving frame counter, native view with panning, borderless fullscreen without a mode change, and the held last frame.

The Mac agent reports the same `--check` results in WKWebView on the M3, where the viewer's GPU process opens an AppleAVD decoder client. WebKit ignores the software preference, so macOS has no software control.

Rust-to-page IPC on the client, measured with monotonic clocks and a calibrated offset, adds about 3 ms (p50) for a 32 KiB frame when the page is fetching frames while decoding and drawing 1080p at 60 fps. Keyframe-sized messages (512 KiB) have p99 tails up to 42 ms. Pull (one long-poll invoke per frame) beats a Tauri Channel at every size. Tauri passes this pre-transport gate; on the live path, watch IDR sizes at 2560 width and cap or chunk keyframes before blaming decode.

This completes next-sequence steps 1–2 for the fixtures. It does not complete Milestone 1: live desktop readability and end-to-end delay on the Windows client need transport.

Before remote input, the viewer must disable WebView2 browser accelerator keys, hook Win and Alt+Tab natively, and release held keys on blur.

Next is step 3: a Rust protocol crate (QUIC via quinn, and rustls with pinned mutual certificates that still verify handshake signatures), SSH-style full-fingerprint approval on the host, DPAPI/Keychain storage, and revocation. Then comes live video over pull IPC with keyframe recovery. Linking Rust into the Swift host changes code signing and can drop TCC grants, so plan stable signing first.

## Milestone 2: paired secure connection

Offline viewer prototype implemented (2026-10-02): C++17 with FFmpeg and SDL3, provisionally chosen to exercise a common decode/render path on the Mac while keeping Windows hardware negotiation behind FFmpeg's D3D11VA interface. This is not a final language/renderer choice. CMake and a baseline-pinned vcpkg manifest support a Windows x64 build; `scripts/build-windows.ps1` and [the Windows checklist](windows-viewer.md) document prerequisites and verification. No Windows toolchain, executable, GPU/driver or runtime is available/verified in this workspace.

The executable accepts only one bounded local H.264 fixture, decodes synchronously with no application video queue, and uploads converted BGRA pixels to a resizable SDL window. Fit, native renderer-output pixels, clamped pan and fullscreen are implemented; controls are local only. Required-hardware mode fails rather than claiming hardware success from device creation. Default Windows mode reports a software fallback if initial D3D11 availability fails; runtime negotiation/decode failures remain errors requiring an explicit software retry. Hardware frames are copied to CPU memory for rendering in this prototype; it is not zero-copy or the final latency architecture. No transport, pairing, clipboard or remote input was added.

Mac verification: clean C++ compilation; six CTest checks passed (geometry, URL rejection, malformed H.264 rejection, 30-frame software decoding, dummy software texture upload, and rejection of Windows-required hardware mode on a non-Windows platform). An additional real Metal-renderer hidden-window smoke test decoded/uploaded all 30 frames successfully. FFmpeg version 9.0.2, SDL3 3.4.16, AppleClang 21.0.0. The generated fixture reports 320×240, limited range, BT.709 matrix/primaries and IEC sRGB transfer. These smoke checks do not verify visible Windows presentation, fullscreen behavior, hardware decode, or desktop readability. Actual Windows validation is the next platform gate before live transport integration.

Choose an established secure transport after testing practical integration on both platforms. QUIC, TCP, and datagrams are options, not commitments. Reliable streams alone do not eliminate stale-video buffering.

Define a small versioned protocol covering capability negotiation, display geometry, codec configuration, frame boundaries, input, clipboard, session lifecycle, keyframe requests, and telemetry. Bound message sizes and queues. Define decoder recovery when video is dropped or lost.

Implement locally approved pairing with verified peer identities, OS-protected credential storage, and revocation. Verify rejection of unknown/revoked clients and host impersonation. Encrypt and authenticate every channel. Do not introduce remotely injectable input until authorization works.

Deliverable: authenticated connections by hostname/IP with a documented pairing flow and bounded video transport.

## Milestone 3: desktop interaction

- Forward keyboard events with explicit modifier mappings. Preserve terminal Control shortcuts. Provide an intentional mapping for Command and a local escape/release shortcut.
- Transform pointer coordinates through the displayed image rectangle; handle letterboxing, scaling, and macOS display points correctly.
- Implement scrolling, left/right buttons, drag behavior, fit-to-window, and fullscreen.
- Synchronize plain text clipboard without feedback loops, with bounded payloads and a user-visible disable option.
- Release held input on focus loss/disconnect and enforce one controlling session.

Deliverable: a complete session for terminal/browser work with no stuck keys or pointer alignment errors.

## Milestone 4: reliability and measurement

Measure capture/encode, receive/decode/render, queue age, drops, bitrate, and network round-trip time. Use a physical/camera measurement or a defined clock synchronization method for end-to-end latency; unsynchronized timestamps are insufficient.

Test network interruption, reconnect, permission denial/revocation, display sleep, host lock during a session, and viewer resize/fullscreen transitions. Restore decoding with a fresh keyframe after interruption. Prefer reducing video load to accumulating stale frames.

For unattended usability, verify host startup at user login, visible remote-access status, paired-device revocation, and disabling remote access. Verify the app does not leave sleep prevention active after shutdown.

Deliverable: documented supported states, reproducible measurements, and completion of the acceptance scenario.

## First release acceptance scenario

1. Pair Windows and Mac locally once.
2. Leave the Mac logged in but locked, lid open, display off, and system awake under explicit remote-access mode.
3. Connect from Windows over the user's actual internet/VPN path without local approval.
4. View the lock screen and enter the Mac password without storing it in the app.
5. Read/edit terminal text, interrupt a command using Control+C, browse and scroll, use right-click, resize/fullscreen, and copy plain text in both directions.
6. Disconnect during held input and verify all keys/buttons are released. Reconnect successfully and verify disabling remote access prevents further sessions.

Record subjective usability as well as measurements. A fast stream with unreadable text does not pass. Set numeric performance targets after measuring a local baseline and the real internet path.

## Later extension

Add Windows capture/input hosting and Mac decode/render viewing while reusing protocol and security contracts. Keep platform code separate without forcing it into a single language.

Linux, both viewer and host (user goal, 2026-10-13): start only after the Windows viewer and Mac host work end to end. The Rust core (transport, pairing, protocol) is intended to be shared unchanged. Each Linux side needs its own platform proof first:

- Viewer: Tauri on Linux uses WebKitGTK, not Chromium. Its WebCodecs H.264 decoding depends on GStreamer and its installed plugins, and is unproven for this project. First run the existing `app/` decode checks on both fixtures. WSLg can show whether decoding works at all; hardware decoding needs a real Linux machine with a GPU. If WebKitGTK fails, keep Tauri for the UI and replace only the Linux video surface with a native one (for example wgpu with GStreamer/VA-API decode).
- Host: under Wayland, capture and input injection go through the xdg-desktop-portal ScreenCast and RemoteDesktop portals, which need user consent, with frames delivered over PipeWire. Encoding uses VA-API. Prove consent persistence, locked-session behaviour and input before claiming support. Do not assume X11.

Investigate real sleep/wake support using the available home network and confirmed hardware. Investigate logout/reboot access separately, including VPN availability before user login. Neither is implied by successful locked-session access.

## References informing scope

- [NoMachine guide](https://knowledgebase.nomachine.com/DT10R00170): describes unlocking a remote locked screen using the logged-in user's password; does not establish feasibility for our implementation.
- [Apple sleep settings](https://support.apple.com/en-nz/guide/mac-help/-mchlp2995/mac): distinguishes configurable sleep and network wake behavior.
- [Tailscale wake guide](https://tailscale.com/blog/wake-on-lan-tailscale-upsnap): describes a separate awake local device for wake signaling.
- [Tailscale unattended access](https://tailscale.com/docs/how-to/run-unattended): documents platform-specific availability outside a user login session. Recheck when implementing reboot access.
