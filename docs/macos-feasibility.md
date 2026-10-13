# macOS diagnostic prototype

> **2026-10-13:** the diagnostic controls described below (local capture and preview, decoded comparison, color and width options, display-off and wake probes, lock-input probe, local input test, metadata export) were removed from the app. It now has only the permission buttons, **Remote access**, its status line, and an in-memory log. The findings below still stand. To repeat a procedure, build `d03ac78`, the last commit with those controls. The display-wake request (`DisplayWakeRequest`) is kept for a viewer connecting while the display is off.

## Build and launch

```sh
bash scripts/build-macos.sh
open "build/NoComputer Diagnostics.app"
```

The package has no external dependencies. The current app builds with Command Line Tools, without full Xcode. The stable bundle identifier is `dev.nocomputer.diagnostics`. Local builds sign with the local `nocomputer Local Development` identity when present (see [Development signing](#development-signing-and-stale-permissions)), otherwise ad-hoc. `NOCOMPUTER_SIGN_IDENTITY` overrides both. Do not reset privacy permissions indiscriminately.

Launch does not start capture, request permissions, post input, or prevent sleep. Use the buttons to request Screen Recording and Accessibility when ready. macOS may require granting access in System Settings and relaunching. Refresh permissions after changing them.

Run the synthetic frame-state checks with `bash scripts/test-macos.sh`. They exercise idle, blank, and suspended capture output without requesting screen permissions or capturing the desktop. The script supplies framework and runtime paths for Swift Testing when using Command Line Tools.

The default preview excludes this app to avoid recursive capture. It is scaled to at most 1600 pixels wide and refreshed at 2 Hz; capture requests 30 fps and reports frame metadata. Preview conversion intentionally copies pixels and is not representative of the future encoder path. Images remain in memory. The whole-display experiment disables preview conversion and retains only metadata. Export writes only bounded status metadata, permission state, and display configuration to a location you choose.

## Controls

Current main-window checkboxes (2026-10-02): **Keep system awake**, **Encode H.264 (hardware)**, **Decode locally for comparison**, and **Use explicit SDR sRGB colors**. The old filter/restart/lock-input checkboxes have been retired. Advanced repeat tests are menu actions under **Diagnostics**. Earlier experimental procedures below describe the historical UI; use the corresponding menu action where still available. Automatic restart-on-display-wake has been removed, so that historical comparison is not currently exposed.

- **Start capture:** enumerate the current main display and begin capture immediately.
- **Start in 15 seconds:** start a fresh capture after a delay, giving time to lock manually. Do not grant initial permissions during this test; establish them first.
- **Stop / cancel:** stop an active capture or cancel a scheduled start. During asynchronous startup/shutdown, wait for completion before retrying.
- **Keep system awake:** explicitly hold a user-idle system-sleep assertion. Does not prevent display sleep or manual sleep. Disabled initially; released when unchecked or the process exits.
- **Diagnostics → Keep display awake for 2 minutes:** a separate, temporary user-idle display-sleep assertion, released after 120 seconds or on Cancel display test/quit. Verify physically that the display stays on; an assertion is not proof that the lock screen honors it.
- **Diagnostics → Whole-display metadata capture:** start a whole-display stream without application exclusions. Requires encoding/decoding OFF. Preview conversion is disabled to avoid recursion; a blank preview is intentional. Stop and use ordinary Start capture to return to the application-exclusion filter.
- **Display off in 15 seconds:** requires system keep-awake enabled and display keep-awake disabled. After the delay, runs `pmset displaysleepnow`, which requests display sleep without changing persistent settings. It does not lock the session itself; manually lock before it fires. Existing password-after-display-sleep settings may also lock the session.
- **Cancel display test:** cancel the pending display-off request and release the temporary display assertion. A command already issued cannot be cancelled; wake the display locally.
- **Wake + capture in 60 seconds:** explicitly schedule a public display-wake request, a temporary display keep-awake assertion, and fresh capture two seconds afterward. Five seconds after capture starts, retain one recent frame in memory for inspection after local unlock. Stop/cancel or Cancel display test cancels the probe and cleans up its capture/assertions. Requires stopped capture, system keep-awake ON, and the other experiment options OFF.
- **Diagnostics → Schedule legacy lock-input probe…:** explicit repeat of the completed fixed-character experiment. Requires effective Accessibility permission and confirmation at scheduling. After retaining the before frame, posts a mouse move to display fraction (0.75, 0.45) and one fixed nonsecret `a` down/up. Retains an after frame three seconds later, then posts Backspace down/up. No click, Return, password, or login attempt is sent. Event posting alone is not proof of delivery.
- **Review before input / Review after input:** show the retained frames in a local window after unlocking. Frames are never exported or written to disk and are replaced on the next wake probe. Their retention times, not contents, appear in metadata. A capture-only probe has no after-input frame.
- **Run local input test:** requires Accessibility and the diagnostic window to be frontmost. Posts pointer movement, left-click, `a`/`A`, one scroll, and right-click in its own test field. Every posted key/button has a matching release. Inspect delivery manually; successful posting is not proof of OS acceptance.
- **Export metadata:** save the most recent 600 journal entries. Journal times are monotonic process-independent uptime seconds; frame presentation timestamps are reported separately. These are not end-to-end latency measurements.
- **Encode H.264 (hardware):** select while stopped, then Start capture. Sends complete frames' original pixel buffers to VideoToolbox; output is counted and discarded unless local decoding is enabled. Requires hardware encoding and reports failures rather than falling back to software. Stop/start to change; leave OFF for retained-frame wake/input tests.
- **Decode locally for comparison:** requires encoding ON and ordinary application-exclusion capture. Consumes encoded samples in a hardware decoder, retaining matched comparison images in memory. Changes apply only after Stop/Start. No recording, network, or input forwarding. Leave OFF for wake/input probes.
- **Open decoded preview:** opens a separate aspect-preserving decoded-image window after loopback capture starts. Resize it or use its green fullscreen control to inspect text. The title identifies its 2 Hz diagnostic refresh; it is not a smoothness/latency viewer.
- **Explicit SDR sRGB color path:** optional, initially OFF. Stop/start to compare it with system/display defaults. Requests sRGB capture output and explicit BT.709 primaries/matrix plus sRGB encoder transfer; source/format/decoded colour metadata is exported. This is an experiment, not a confirmed fix for washed-out colours. No host display setting changes.

The preview clears on blank/suspended/stopped output, or after two seconds without a callback. Idle callbacks keep the existing image valid on static desktops. Judge statuses, callback/complete ages, and subsequent recovery together.

## Manual test matrix

Verified on 2026-09-30: debug build, bundle signing/verification, launch with process still running, and three synthetic capture-state tests. No Screen Recording/Accessibility grant, live desktop capture, injected input, or power assertion was exercised by the agent.

All hardware/permission/locked-state results below are **pending**, not inferred from compilation. Record OS build, signing identity, power source, sleep settings, and the exported metadata for each run. Do not export screenshots or passwords.

User observations on 2026-10-01 supersede the pending status for these checks:

- Unlocked desktop preview works; both Screen Recording and Accessibility are granted in both exported reports.
- The local input field received `aA` in the user's screenshot. Mouse/context-menu/scroll delivery has not been separately confirmed.
- Two lock/unlock experiments ended capture with “Failed to find any displays or windows to capture.” The second report (`diagnostics-1790870350.json`) recorded 1334 callbacks and 1082 complete frames before termination, with the keep-awake assertion enabled. This establishes failure of the tested capture path across lock, not impossibility of all lock-screen capture approaches.
- Stop then failed with “Failed to stop a stream that is already stopped or does not exist.” The prototype retained that dead stream and blocked Start. Recovery handling has now been changed to release terminal streams, retain the failure metadata until restart/stop, and treat the specific already-stopped error as successful cleanup. Other stop failures still surface normally.
- The updated app requires a manual unlock-and-restart check. Fresh capture while already locked and display-sleep recovery remain untested.

Later observations the same day: `diagnostics-1790870933.json` confirms termination with ScreenCaptureKit domain error `-3815` (no capture source), followed by release of the dead stream and a successful Stop. The user separately confirmed a brief lock/unlock without display power-off leaves capture working. Long lock duration and display power-off remain confounded; the assertion/notification controls below are intended to separate them.

Comparison result (`diagnostics-1790871588.json`, 2026-10-01): the display-assertion trial ran from uptime 572509.716 to manual Stop at 572580.447 (about 71 seconds), with complete frames still arriving near its end and no terminal error. In the second trial, with only system keep-awake, automatic display sleep was reported at 572620.798 and the stream's terminal `-3815` callback at 572620.8147 (about 17 ms later). Displays woke at 572652.379, but the terminated stream did not resume. Stop succeeded. No system-sleep notification appears during either trial. The user did not use the scheduled display-off button; automatic display sleep supplied the event instead. This strongly associates failure with display sleep in this configuration; it does not establish lock-screen pixel visibility, or prove that every ScreenCaptureKit configuration behaves the same way.

## Compare lock duration and display sleep

Use the same one-minute locked interval for both tests. Keep the lid open and system keep-awake enabled throughout. Export metadata after each test; no changes to password requirements or system sleep settings are needed.

1. **Locked, display lit:** stop/start capture, enable **Keep display awake for 2 minutes**, and lock manually with Control+Command+Q. Verify the display stays lit for 60 seconds, then unlock and export. If it turns off anyway, this is not a valid display-lit control; report that rather than assuming the checkbox kept it on.
2. **Locked, display off:** stop/start capture, disable display keep-awake, and select **Display off in 15 seconds**. Lock manually before the delay expires. Once the display goes off, leave it locked for 60 seconds, then wake/unlock locally and export. Restart capture manually if it stopped.

The journal now includes public workspace display-sleep/wake and system-sleep/wake notifications. Correlate these with the capture error's monotonic `stoppedAt` time; notification delivery timing is observational evidence, not a guaranteed exact hardware transition timestamp.

If only the display-off trial fails, investigate display sleep handling next. If the display-lit one-minute trial also fails, investigate locked-session duration/context instead. Neither trial proves lock-screen pixel visibility or input acceptance.

| Scenario | Procedure | Evidence required |
| --- | --- | --- |
| Permission absent | Start before granting Screen Recording | Clear failure or permission flow; no crash |
| Unlocked capture | Grant/relaunch, start; move a browser window | Complete frames, updated preview, monotonic counts |
| Local input | Grant Accessibility; run test while app is frontmost | Field receives `aA`; pointer, right-click, and scroll observed |
| Active capture across lock | Enable keep-awake; start capture; lock with Control+Command+Q; wait at least 60 seconds; unlock locally | Export status/age transitions and confirm recovery; independently observe whether lock-screen pixels were captured |
| Fresh capture while locked | Stop; schedule start; lock before 15 seconds; wait; unlock locally | Did enumeration/start succeed while locked? Which statuses arrived? A successful start alone does not prove lock-screen visibility |
| Display sleep | Enable keep-awake; start capture; let display turn off; wake locally | Distinguish blank/stalled frames from error; confirm recovery |
| Assertion lifecycle | Inspect `pmset -g assertions` before/after checkbox changes and after quitting | Named assertion appears only while enabled and is gone after exit |
| Capture error/restart | Stop/start repeatedly; test denial/revocation when convenient | No overlapping streams, stable recovery or actionable error |
| Permission identity | Quit, rebuild and relaunch | Record whether existing grants persist |

Frame metadata cannot identify what pixels a locked-session stream contains. This app does not persist images or provide a viewer on another device during lock. The explicitly scheduled wake probe can retain one frame in memory for later review. Its contents and the user's observation that they remained locked must be considered together; frame counters alone do not prove locked control.

The ordinary local input test intentionally refuses to target another app. The separately armed timed lock-input probe posts pointer movement, one fixed nonsecret marker, and Backspace to the active session. It does not programmatically verify that the user actually locked the session; the operator must lock and remain locked during the test. This prototype never retains or automatically enters login credentials. Password submission remains untested.

True system sleep/wake, remote access, and Windows decoding/rendering are not implemented here. Local hardware encoding/decoding is an opt-in diagnostic, not a working remote desktop.

## Local H.264 encoding test

Hardware result (2026-10-01, `diagnostics-1790881393.json`): capture started at uptime 577506.597 and final encoder statistics were recorded at 577611.168 (approximately 104.57 seconds). Hardware encoding of the 1600×1034 stream succeeded: 1761 submitted and encoded frames, 50,348,442 bytes, 53 keyframes, zero raw or encoder drops, no encoder error, peak one frame outstanding, and zero pending at successful Stop. Mean encode submission-to-callback time was 11.211 ms; maximum 26.040 ms. The format reported two parameter sets with four-byte AVCC NAL lengths. Both permissions were granted; neither power assertion was active. Payload averaged approximately 3.85 Mbit/s across the whole run, including idle intervals. This is not an end-to-end latency, constant-frame-rate, readability, or decoding result. A second start/stop is not present in this export, so encoder recreation with live desktop input remains a manual check.

1. Refresh effective Screen Recording permission if necessary after the rebuild. Accessibility is not required for encoding.
2. Stop capture; disable whole-display and wake/input experiments. Select **H.264 hardware encoder test**, then **Start capture**. Stay unlocked for this first test; no sleep-setting changes are needed.
3. For 30–60 seconds, use a terminal/browser: scroll, move windows, and leave the desktop still briefly. The preview is intentionally blank. Confirm `hardware=yes`, growing encoded/byte counts during changes, keyframes, pending at most three, and no encoder error. Idle desktop callbacks need not produce new encoded frames.
4. Export metadata, then Stop. Export again if final stopped statistics are desired; the app retains the last encoder report after Stop. Start/Stop once more to check encoder recreation and cleanup.

Settings: H.264 Main auto-level, no frame reordering, real-time, 8 Mbit/s requested average bitrate, expected 30 fps, keyframe interval at most 60 frames/two seconds. These are starting values, not measured quality guarantees. Stream dimensions are even and independent of host display resolution. The current M3 encoder does not support the optional `MaxFrameDelayCount` property; export records this. Application-level outstanding-frame admission is separately capped at three. Raw-frame drops do not damage H.264 dependencies; no encoded packet dropping is implemented.

Timing is local monotonic submission-to-callback time, including codec queueing. The application does not copy source pixels for the encoder, but VideoToolbox may internally convert BGRA. Compressed data, parameter-set bytes, and desktop pixels are not exported. Samples use AVCC length-prefixed NAL units with parameter sets in the format description; the eventual decoder must receive both. Decode correctness, text quality, Windows capability, and internet latency are not inferred from byte counts.

Run standard tests with `bash scripts/test-macos.sh`. Opt in to actual hardware encoding of generated black frames (no screen capture or privacy requests) with `NOCOMPUTER_TEST_HARDWARE=1 bash scripts/test-macos.sh`. These checks verify hardware selection, nonempty H.264 samples, keyframe/format metadata, bounded pending work, invalid-source rejection, and idempotent closure. Live desktop behavior still needs the manual test above.

## Whole-display filter and recovery experiment

The application-exclusion filter could introduce a dependency on the user session's app context. This is a hypothesis, not an established cause. The next comparison uses an empty window-exclusion list to capture the display without an application filter.

1. Enable system keep-awake, disable display keep-awake, select **Whole-display filter experiment**, and start capture. Confirm complete-frame counts increase; there is intentionally no preview.
2. Enable **Restart failed capture when display wakes**. Lock and let the display turn off as before, then wake/unlock after a minute.
3. Export metadata. Check whether `-3815` still occurs at display sleep, and whether a fresh stream starts after display wake. This can test a recovery policy even if continuous capture fails.
4. Stop capture, uncheck whole-display mode, then start normally to check visible preview recovery.

Apple documents the display/window-exclusion filter in [its capture pipeline discussion](https://developer-rno.apple.com/videos/play/wwdc2022/10155/). An [Apple staff forum response](https://developer.apple.com/forums/thread/827059) says window screenshots with `SCScreenshotManager` do not work while the display is asleep. That response concerns a different API path and does not establish the behavior of our display stream, but it argues against assuming a filter change will solve display-off capture.

## Whole-display experiment result

`diagnostics-1790877389.json` (2026-10-01) records the whole-display filter, both permissions granted, system keep-awake active, and restart-on-wake enabled. Display sleep was reported at uptime 573569.036; the stream terminated with `-3815` at 573569.050529, about 15 ms later. Removing application exclusions therefore did not prevent the observed display-sleep failure. This comparison argues against application exclusions being its cause; it does not establish a general OS limitation for all capture arrangements.

Displays woke at 573603.936. A fresh stream started at 573605.068 (about 1.13 seconds later) and delivered complete frames, including 10 complete frames at 573605.401 and 37 at 573608.901. Stop succeeded afterward. Automatic post-wake recovery is confirmed for this trial; capture while the display is off and lock-screen pixel visibility remain unverified/unsupported by this path. No precise unlock timestamp was recorded, so this does not establish whether restart happened before or after authentication.

The earlier `diagnostics-1790871021.json` also confirms successful manual Stop, Start, delivery, and Stop after the lifecycle fix. Findings from the five older reports are preserved in this document. On user request they were moved out of the project root to the user's Trash; only the newest report remains in the root.

Next investigate an explicitly enabled display-wake request while the user session remains locked, followed by fresh capture. Waking the display at connection time could meet access to an initially display-off host without claiming streaming while the display sleeps. Do not confuse waking a display on an awake system with waking the system itself.

## Scheduled wake while locked

Successful trial `diagnostics-1790878826.json` (2026-10-01): the display was asleep at uptime 575012.255, wake was reported at 575012.581, fresh capture started at 575014.349, and the probe retained a frame at 575019.526942. Both permissions were granted. The user shared a screenshot of the retained-frame review showing the macOS lock screen with the password/Touch ID prompt. Together, this verifies public display wake and fresh lock-screen capture on this machine while the system stays awake. It does not verify injected input/password entry, true system wake, or login after reboot. No screenshot was copied into the repository.

This validates a candidate connection lifecycle: keep the host system reachable during explicitly enabled remote availability; on authenticated connection request display wake, acquire a display assertion for the active session, and start fresh capture. Release session display assertions on disconnect so the display may sleep again. Security/transport and actual remote input are not implemented yet.

Trial `diagnostics-1790878507.json`: after permissions were refreshed, the timed probe successfully woke the display and started capture at uptime 574709.554. Complete frames arrived before the expected local unlock interval, reaching 26 before mostly idle output. However, the original probe incorrectly rejected its retained preview at 574714.888 based on complete-frame age. It treated an unchanged screen as stale despite ongoing idle callbacks. The preview/review check now uses callback freshness and terminal state; blank/suspended frames still clear the image. A repeat is needed to inspect pixels. This trial confirms wake and fresh-frame delivery, not the contents of those frames.

Hardware-verified for the successful trial above. [Apple's documented user-activity API](https://developer.apple.com/documentation/iokit/1557127-iopmassertiondeclareuseractivity) requests display power-on without special privileges. This diagnostic uses the local-user activity type to request visible display wake; no keyboard/mouse event or password is injected. It does not wake a sleeping/unreachable system and does not unlock a user session.

1. Stop capture. Enable **Keep system awake**. Leave **Keep display awake**, **Whole-display filter experiment**, and **Restart failed capture when display wakes** OFF.
2. Click **Wake + capture in 60 seconds**, then lock immediately with Control+Command+Q. Let the display turn off normally. Do not touch the keyboard/trackpad or unlock it.
3. At about 60 seconds the app should request display wake. Observe whether the password/lock screen remains visible. Wait at least 20 seconds after the automatic wake before unlocking locally, giving the app time to start capture and retain frames.
4. Unlock, choose **Review before input**, and report what the retained image shows: lock screen, your desktop, black/blank, or no frame. Export metadata to the project root. If you touched input/unlocked early, record that so the retained image is not misattributed.
5. Stop capture and cancel the display test afterward. The temporary display assertion also expires after 120 seconds and is released on quit.

The journal records whether the main display was asleep when the request fired, the request's result, display state before capture, startup outcome, and retention time. Workspace notifications independently record display wake. These observations establish distinct facts: request accepted, physical display woke, stream started, and lock-screen pixels captured. None proves password-input injection; that remains a separate test.

## Lock-screen input probe

User result (2026-10-01): before/after review screenshots show the cursor moving from near center to the requested display fraction (0.75, 0.45), and the user confirms movement without local input. Injected pointer movement at the lock screen is verified. The lock-screen/password prompt looks unchanged after Tab; this is inconclusive for keyboard acceptance because Tab is navigation rather than a printable password character. No metadata from this input trial has yet been supplied. No password submission or authentication was tested.

Next keyboard test should send a fixed nonsecret character followed by Backspace, with no Return, and compare the intervening password-entry UI. Application pairing and OS screen unlocking remain separate operations. Avoid implementing a custom OS login credential dialog as a shortcut before verifying normal input forwarding.

Character probe result (`diagnostics-1790880448.json`, 2026-10-01): both effective permissions are granted. Displays woke at uptime 576602.685; fresh capture started at 576604.569. The before frame was retained at 576609.8997040417, marker events were posted at 576609.912, and the intermediate frame was retained at 576612.954497125. The user's screenshots show the actual lock screen before input and a password field containing one masked character afterward. This demonstrates ordinary injected keyboard input reaching the locked session on the target machine. Backspace cleanup was posted at 576612.956 (`probeErasurePosted=true`), but these two frames do not independently verify visual erasure. No Return, password submission, or successful authentication was tested. No screenshots were copied into the repository.

Together with earlier pointer movement and display-wake capture trials, this passes the core locked-session feasibility gate for the tested user-session app. It does not establish every input gesture, assertion cleanup, permission persistence, true system wake, or access after reboot. Proceed to independent capture/encoding; actual remote unlock remains an acceptance check after secure pairing and input forwarding exist.

Twelve synthetic tests passed, including display-coordinate mapping, rejection of invalid bounds, matched marker/Backspace down/up sequences without inherited modifiers, and absence of Return. Automated tests construct events but never post them.

1. Ensure Screen Recording and Accessibility are effectively granted to this build. Confirm normal preview works if permissions had to be refreshed, then Stop.
2. Enable system keep-awake and **Wake probe: test character, then Backspace**. Leave display keep-awake, whole-display mode, and restart-on-wake OFF.
3. Click **Wake + capture in 60 seconds**, confirm **Schedule test**, and lock immediately. Do not touch input until at least 20 seconds after automatic wake.
4. Observe whether the password entry UI appears or contains a masked marker, then whether Backspace erases it. Unlock locally afterward, once cleanup has run.
5. Compare **Review before input** and **Review after input**; the second frame is deliberately retained before Backspace. Export metadata and report visual/physical delivery separately from the log's statement that events were posted. No visible change is inconclusive unless permissions and input target are established.

Stop/cancel cancels the pending probe and cleans up capture/assertions. Once a key pair starts, it always includes key-up. Marker erasure is prebuilt before insertion and posted after frame retention, on task cancellation/failure, or on normal app quit. A force-kill/crash cannot guarantee cleanup; if needed clear the field locally. No secret input or authentication is automated. Do not touch input or type your password before the scheduled test/cleanup finishes.

## Local decoded visual review

User feedback (2026-10-01): live decoded desktop looks good overall; colours are somewhat washed out and text could be sharper. The supplied screenshot contains a screenshot-selection overlay, which is not an app defect. No new loopback export was available when this feedback was recorded. Visual delivery is demonstrated, but objective decoder statistics and live Stop/Start confirmation remain pending. Do not call the quality baseline complete: compare raw/decoded frames at identical scale, native-size versus fit presentation, and colour metadata before deciding whether bitrate, stream resolution, colour handling, or a sharpening filter needs to change.

Follow-up metadata (`diagnostics-1790882543.json`, reviewed 2026-10-02) confirms 3720 encoded and 3720 decoded frames at 1600×1034 with both hardware paths selected and no codec errors. Decode mean/max: 3.065/8.134 ms; encode mean/max: 11.970/102.694 ms. The maximum encode spike appears in the early startup journal. One raw frame was dropped before encoding, zero encoder drops were reported, and peak outstanding work reached but did not exceed the three-frame cap. Outstanding work was zero at the export snapshot. There is only one capture start and no Stop/final-statistics/restart event in the report, so it does not confirm Stop/Start cleanup. These local timings exclude UI presentation and cannot establish end-to-end latency. Colour and text quality still need controlled comparison rather than inference from successful frame counts.

This adds an optional Mac-only loop: capture → hardware H.264 encode → hardware H.264 decode → one in-memory review image. The decoder consumes the original compressed sample and format description, including parameter sets; this does not test serialization or transport. The displayed image comes from the decoder, not the uncompressed capture preview. All diagnostic application windows are excluded from capture.

1. Refresh effective Screen Recording permission if this build needs it. Stop capture; do not run the separate Diagnostics-menu feasibility actions during this review.
2. Enable **Encode H.264 (hardware)** and **Decode locally for comparison**, then Start capture. A separate decoded preview opens automatically; **Open decoded preview** reopens it if closed.
3. Resize the window or use its green fullscreen control. Inspect terminal/browser small text, coloured text, window edges, and scrolling. Compare against the original desktop at similar scale. Fit-to-window scaling and the current 1600-wide stream may make small text harder to read; do not assume compression is the only cause.
4. Leave a document still briefly and confirm its image stays visible. Move/scroll content and confirm it updates. The deliberate 2 Hz preview will look choppy; it is not a production renderer or latency test. No mouse or keyboard events in this window are forwarded.
5. Stop and confirm the decoded image clears. Start again with both options enabled and confirm a fresh decoded image appears. Stop, export metadata to the root, and report readability/colour/cropping issues separately from counters.

Every encoded frame is decoded; only decoded-image conversion/display is sampled at 2 Hz. Decode runs synchronously and encoder admission counts a frame until downstream decode completes, bounding outstanding work at three without discarding interframe dependencies. This can influence encoder throughput and is not the final client pipeline. Decoder submission-to-callback timing excludes the subsequent CGImage conversion and UI polling; it is not total loop latency. No files or network streams contain pixels. Stop/terminal capture failure releases codec sessions and clears the decoded preview. Blank/suspended/stalled source states and codec errors also suppress display; idle callbacks allow unchanged valid images to remain.

Nineteen tests passed with hardware tests explicitly enabled. The synthetic red/blue pattern round trip verifies decoder hardware selection, output dimensions, orientation/channel order, equal encoded/decoded counts, independent session recreation, and image release after closure. Invalid uncompressed samples are rejected before hardware setup. Live desktop visual quality and Windows decode/render remain pending.

Synchronous mode follows [Apple's decode-frame contract](https://developer.apple.com/documentation/videotoolbox/vtdecompressionsessiondecodeframe%28_%3Asamplebuffer%3Aflags%3Ainfoflagsout%3Aoutputhandler%3A%29): both asynchronous and temporal-processing flags are off, so output completes before the decode call returns. [Hardware decoding is required](https://developer.apple.com/documentation/VideoToolbox/kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder) rather than silently falling back to software. This is a diagnostic policy, not a universal requirement for future viewer hardware.

## Matched comparison and native pixels

The review window now switches between **Decoded** and **Raw source** for a matched frame pair. The raw image is copied from the original captured pixel buffer for that encoder submission, not a separately timed capture. This adds bounded diagnostic work before encode timing; it is not a production zero-copy path. Only image pairs and a bounded set of at most three in-flight raw previews are retained; no desktop image/video files are saved.

1. Enable encoder and local decoding, with the explicit sRGB option initially OFF. Start and show terminal/browser text and a few familiar colours.
2. Select **Freeze matched pair**. Switch **Decoded / Raw source** to compare the exact same frame at the exact same scale. If both are similarly soft, that implicates capture/view scaling rather than only compression; this comparison alone cannot identify every cause.
3. Select **1:1 physical pixels**. One stream pixel now maps to one display backing pixel, including on Retina displays. Scroll if the image is larger than the viewport. This is not one pixel per macOS logical point, and can look physically smaller than fit mode. Compare both views again.
4. Stop. Enable **Explicit SDR sRGB color path** and Start again. Unfreeze, then freeze a new pair and repeat the comparison. Compare raw/decoded contrast, not the screenshot tool's dimming/selection overlay. Export each trial's metadata if results differ.
5. Stop/Start and confirm image clearing/recreation. The next platform checkpoint still needs the Windows machine; Mac-only tests do not establish its decoder/rendering capabilities.

Metadata schema 11 records only allowlisted colour fields (primaries, transfer, matrix, buffer pixel format/colour-space name, compressed-format range), rendered-image colour-space names, paired PTS, viewing mode, backing scale, and aggregate colour differences. Last rendered-image colour-space names and aggregates survive Stop; image objects do not. The comparison internally renders a 32×32 sRGB 8-bit grid and exports only mean absolute/signed RGB differences, dark-region mean shift, and a sampled source-transparency flag. These are not a perceptual quality score or proof of a particular colour bug. No arbitrary attachments, pixel samples, credentials, or clipboard data are exported. The explicit path does not alter the display, sleep, security, or sharing settings. [Apple documents that unspecified capture colour space follows the display](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/colorspacename); explicit sRGB is therefore a controlled alternative, not a finding that defaults were necessarily wrong.

Twenty-five tests passed with hardware/fixture checks enabled. The synthetic sRGB gray ramp differed by at most one 8-bit code value after the hardware round trip. Native-pixel/fit geometry and matched-pair retention/release are covered. Subjective live colour/sharpness remains a manual check; no sharpening filter has been added.

Schema 12 additionally retains `lastViewedComparison` separately from the latest decoder statistics: paired PTS, colour-space names, aggregate measurements and view settings only. This record survives Stop without retaining either image, so a frozen review is not confused with a later decoded frame. On 2026-10-02 the user accepted the improved 2560-wide/native result as sufficient for a PoC and requested moving forward; colour tuning is deferred until Windows/end-to-end testing.

## Synthetic elementary-stream interoperability

Latest live review (`diagnostics-1790966488.json`, 2026-10-02): raw default and sRGB colours look alike and closer to the physical desktop; decoded colours still look washed out according to the user. Both sessions stopped cleanly and the second recreated capture/codec sessions. Default encoded/decoded 1721 frames; explicit sRGB encoded/decoded 2363, with no codec errors in either run. The sRGB compressed format is limited-range with BT.709 primaries/matrix and IEC_sRGB transfer, preserved in decoded buffer attachments. Missing source transfer attachments alone did not reproduce the issue in synthetic grayscale checks: tests with/without those attachments both stayed within one code value. Colour cause remains unresolved. Actual remote authentication/unlock and Windows rendering remain untested.

For sharper-text investigation, select **Stream width: up to 2560 px (text detail)** before Start. Default remains 1600; changing width requires Stop/Start. Source dimensions use capture-filter points and backing scale, not assumed client resolution. This does not change the Mac's display settings. Freeze a new pair and compare Raw/Decoded with the same 1:1 toggle state; then check fit mode separately. More source detail may help text but increases bandwidth/work. Higher-resolution live quality/performance still needs this manual trial; synthetic hardware testing is not a substitute. Export after Stop retains the aggregate diagnostics and colour-space names, without screen pixels.

For an independent decoder check, an opt-in test writes only a generated 320×240 colour gradient as Annex-B H.264. It cannot capture the screen. It converts the encoder's AVCC NAL lengths to start codes and includes parameter sets before keyframes, with bounded sizes and overwrite refusal. This fixture is not the future wire protocol.

Example (choose a new output path in an existing temporary directory):

```sh
NOCOMPUTER_TEST_HARDWARE=1 NOCOMPUTER_FIXTURE_PATH=/absolute/new/path/synthetic-srgb.h264 bash scripts/test-macos.sh
ffprobe -v error -f h264 -count_frames -show_streams /absolute/new/path/synthetic-srgb.h264
ffmpeg -v error -hwaccel none -c:v h264 -f h264 -i /absolute/new/path/synthetic-srgb.h264 -f null -
```

On 2026-10-02, FFmpeg 9.0.2 software decoding consumed all 30 synthetic frames without errors. ffprobe identified H.264 Main/yuv420p, limited range, BT.709 matrix/primaries, and IEC 61966-2-1 transfer. The generated fixture is in `build/synthetic-srgb.h264` (ignored build artifact). It may be copied to Windows for a decoder smoke test, but playback there has not been performed or verified. No desktop content appears in the fixture.

A second opt-in test writes a moving 1920×1080 fixture (`NOCOMPUTER_MOVING_FIXTURE_PATH`) through the same encoder settings: 90 frames at 30 fps, each with a large three-digit frame-number band and a white bar that crosses the frame once over the clip. The 60-frame keyframe interval yields more than one IDR, each preceded by in-band SPS/PPS. Generated 2026-10-13 as `build/moving-1080p.h264`: 252,466 bytes, SHA-256 `05f8e07a8397bf15cee8f4563ea2753f12d6d9c8dcbe316d375462ee67ca33a7`, IDRs at frames 0 and 60, 88 P-frames, no B-frames, Main level 4.0 (`avc1.4D0028`), limited range, BT.709 primaries/matrix, IEC 61966-2-1 transfer. FFmpeg read all 90 frames; frames 0, 61 and 89 decoded with the expected numbers and bar positions. Its frame content is static apart from the band and bar, so it exercises frame updates, decode order and native-view panning, not desktop text quality.

## Development signing and stale permissions

Report cleanup (2026-10-01): six superseded exports were moved from the project root to `~/.Trash/` on user request: `diagnostics-1790877389.json`, `diagnostics-1790878012.json`, `diagnostics-1790878295.json`, `diagnostics-1790878507.json`, `diagnostics-1790878826.json`, and `diagnostics-1790880448.json`. They remain recoverable. Findings are recorded above; the newest encoding export remains in the root. For completeness, `1790878012` recorded absent effective permissions and wake-probe startup failure `-3801`; `1790878295` recorded ordinary capture with effective permissions, rejection of scheduling a wake probe while capture was active, and later display-sleep termination `-3815`. Neither report established additional locked-session capabilities.

On 2026-10-01 the user reported Accessibility appearing enabled in Settings but requiring removing/re-adding the app after builds. `codesign -d -r-` confirmed this app's designated requirement is a binary-specific `cdhash`; `security find-identity -v -p codesigning` found no valid signing identities. A changed ad-hoc build can therefore cease to match the previous permission grant even with the same bundle ID and path. This is consistent with the report; it is not evidence that screen permissions caused the display-sleep error in trials whose exports reported grants as active.

For now, check effective permission status in the app after rebuilding. If stale, quit it, remove/re-add only this app in the relevant privacy pane, grant permission, and reopen. Do not reset unrelated apps or the whole privacy database.

The durable development fix is to use the same certificate-backed code-signing identity across builds. Do not weaken the designated requirement to only a bundle ID to avoid reauthorization. Apple explains how [compatible designated requirements share privacy access](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements).

Stable local signing (2026-10-13, user-approved): a self-signed code-signing certificate, `nocomputer Local Development` (RSA 2048, critical `codeSigning` extended key usage, valid to 2036-10-10, certificate SHA-1 `670422BFC61AA432B9E8101BB7DBA7EE83A71EB7`), was created with `/usr/bin/openssl` and imported into the login keychain with access limited to `/usr/bin/codesign`. The temporary key and PKCS#12 files were deleted after import. It is not marked trusted; codesign does not need trust to sign, so `security find-identity -v` lists it as `CSSMERR_TP_NOT_TRUSTED`, and no system trust settings were changed. The first signing required the user to allow codesign access to the key. `scripts/build-macos.sh` uses `NOCOMPUTER_SIGN_IDENTITY` if set, otherwise this identity if present, otherwise ad-hoc. The designated requirement is now `identifier "dev.nocomputer.diagnostics" and certificate leaf = H"670422bf…1eb7"`, and it is unchanged across rebuilds. Existing Screen Recording/Accessibility grants were made against the old ad-hoc `cdhash`, so they need re-granting once for this signature. Verified 2026-10-13: after re-granting both once for the new signature, a rebuild with a changed binary (CDHash `f3129ad3…` → `91b92f59…`, same designated requirement) kept Screen Recording and Accessibility granted, as the user confirmed in the app without touching System Settings. The certificate is local to this Mac and is not a distribution identity. To recreate it elsewhere, generate an equivalent certificate; never commit the key.

## Remote access keyframe interval

The removed diagnostics kept the Milestone 1 interval: an IDR at least every 60 frames or 2 s. Remote access asks for IDRs on demand (stream start, viewer recovery, pipe pressure), so its encoder uses a 20 s safety interval instead (`VideoEncoder.remoteKeyframeSeconds`).

Measured 2026-10-13 with `NOCOMPUTER_MEASURE_KEYFRAMES=1 scripts/test-macos.sh --filter KeyframeInterval`: 30 s at 1600x1034, 30 fps, 8 Mbit/s target, on synthetic dense terminal text with one line being typed. A live desktop will differ.

| Interval | Keyframes | Mean keyframe | Mean delta | Rate | Keyframe share of bytes |
| --- | --- | --- | --- | --- | --- |
| 2 s | 15 | 883 KB | 0.66 KB | 3.69 Mbit/s | 96% |
| 10 s | 3 | 754 KB | 0.91 KB | 0.82 Mbit/s | 73% |
| 20 s | 2 | 659 KB | 1.28 KB | 0.66 Mbit/s | 53% |
| 30 s | 1 | 367 KB | 1.66 KB | 0.50 Mbit/s | 20% |

On mostly static text, keyframes dominate the bitrate, and each one is a burst of roughly 0.7 MB. Deltas grow a little at longer intervals as rate control spends the freed bits on refinement. The 30 s row holds only the first keyframe of the run, so it is not directly comparable. The live run before this change (2 s interval) averaged about 1.8 Mbit/s. The live rate after the change still needs measuring on a comparable session.

## API references

- [Apple SCFrameStatus](https://developer.apple.com/documentation/screencapturekit/scframestatus): complete/idle/blank/suspended frame status interpretation.
- [Apple ScreenCaptureKit pipeline discussion](https://developer-rno.apple.com/videos/wwdc2022/10155/): buffer retention and queue-depth effects.
- [Apple power assertion types](https://developer.apple.com/documentation/iokit/iopmlib_h/iopmassertiontypes): user-idle system sleep assertion.
- [Apple CGEvent posting](https://developer.apple.com/documentation/coregraphics/cgevent/post%28tap%3A%29): Quartz event posting interface.
