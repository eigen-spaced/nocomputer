# Agent instructions

## Project purpose

Build an inspectable, locally buildable remote desktop for personal use. The initial direction is a Windows 11 viewer controlling an M3 MacBook Air running macOS Tahoe 26.6.x. Terminal and browser use are the primary workloads. Eventually support a Mac viewer controlling a Windows host.

The workspace contains an initial local macOS diagnostic app, not a working remote desktop. Read `docs/macos-feasibility.md` for its limits and manual verification requirements.

## Agreed requirements

- Control the existing physical desktop; activity remains visible on the host.
- Initially support one display and one controlling client at a time.
- Connect using a supplied hostname or IP over a reachable network. Tailscale is the current network provider, not a protocol dependency. No account service, cloud relay, discovery, or NAT traversal in V1.
- Prioritize readable text, responsive typing/scrolling, correct input, and bounded video delay.
- Support fit-to-window, fullscreen, mouse movement, scrolling, right-click, and plain-text clipboard in both directions.
- Support one-time locally approved pairing and subsequent authenticated, encrypted access without per-session local approval. Provide device revocation.
- Target an existing logged-in macOS session, including its locked state. A timed public display-wake request followed by fresh capture successfully captured the lock screen on this Mac (2026-10-01). The user also confirmed injected pointer movement at the lock screen. Keyboard input at the lock screen works: the password typed through the viewer unlocked the Mac (user-confirmed 2026-10-14).
- For initial planned access, the lid is open and the Mac may stay awake with its display off and session locked. Prefer external power. Use a scoped sleep assertion while remote access is enabled; release it when disabled or the process exits.
- Actual wake from system sleep remains a future requirement to investigate. Do not claim that the app or VPN alone can wake an unreachable sleeping host.
- Defer logout/reboot access, audio, file transfer, multiple monitors, and multiple controllers.
- Both keyboard layouts are US English initially. Preserve Control semantics for terminals; do not guess application-specific shortcut mappings. Command mapping must be explicit and configurable if necessary.
- Current Windows hardware is an eighth-generation Core i5 with 16 GB RAM and a 1366x768 display. Do not hard-code this resolution or assume every future client has identical decoder capabilities.

## Development approach

Read `docs/development-plan.md` before implementation. Update it when evidence changes a decision or completes a milestone.

1. Prove macOS capture and input across lock/unlock and display sleep before committing to the complete architecture.
2. Prove capture/encode and decode/render independently before adding transport.
3. Establish a secure paired connection before enabling remote input over a network.
4. Measure desktop readability and latency on the actual Windows client and internet path.
5. Add convenience and reliability, then reverse-direction support.

Swift is the preferred Mac platform language. ScreenCaptureKit and VideoToolbox are candidates pending feasibility. The viewer is Rust + Tauri 2 (user decision, 2026-10-13). It decodes H.264 with WebCodecs in the system web view (WebView2 on Windows, WKWebView on macOS). Networking, pairing and protocol code are Rust, independent of the UI, so the video surface can move to a native renderer if measured latency requires it. The C++/FFmpeg/SDL3 viewer in `viewer/` is a fallback and comparison, not the main line. Transport and Swift↔Rust integration are not settled. Linux viewer and host support is a later goal; see `docs/development-plan.md`.

Model host and viewer as roles. Define versioned wire contracts independently of language and VPN provider. Keep capture/input and decode/render behind small platform boundaries. Avoid building unused abstractions for future platforms.

## Engineering rules

- Use established cryptographic libraries and protocols. Never implement custom cryptography or disable certificate/peer identity verification for convenience.
- Pairing requires local host approval and authenticated verification, using a standard pairing mechanism. Do not invent a short-code cryptographic protocol.
- Authenticate every media/control path. A tailnet address is not authorization.
- Store private credentials in OS-protected storage. Do not persist or log the host login password, key contents, typed input, or clipboard contents.
- Separate narrowly scoped privileged lifecycle work from user-session capture/input. Do not run the whole application as administrator/root as a shortcut.
- Bound queues and instrument drops. For interframe video, dropping data must respect decoder dependencies and provide keyframe recovery; do not blindly drop arbitrary encoded packets.
- Handle coordinate conversion through the rendered content rectangle, display origin, and macOS point/pixel scaling. Letterbox areas must not generate remote clicks.
- Release held keys/buttons on focus loss and disconnect. Clamp/validate all network input, message sizes, and clipboard sizes.
- Keep stream resolution independent of the physical display resolution. Do not change the host display resolution automatically.
- Use monotonic clocks for local timing. Do not subtract unsynchronized host/client clocks to claim end-to-end latency.
- Verify meaningful behavior with focused tests and manual platform checks. Record hardware, build, settings, and permission state for reproducibility.
- Never report Windows behavior as verified from a Mac-only build. State which checks require the Windows machine.
- Do not change security, sharing, sleep, or VPN settings on the user's machine merely to make a test pass. Explain any necessary configuration and obtain authorization for system changes.
- Preserve existing user changes. Use `apply_patch` for file edits. Do not commit, publish, or choose a license without user direction.
- Do not spawn sub-agents unless explicitly requested by the user or a subsequently applicable instruction.

## Done criteria

The first usable release must pass the scenario in `docs/development-plan.md`: connect over the internet to the locked host with its display off, unlock, comfortably use terminal/browser, exchange text clipboard, and disconnect cleanly.

Treat locked-session control, true system wake, and access after reboot as distinct capabilities. Do not silently substitute one for another.
