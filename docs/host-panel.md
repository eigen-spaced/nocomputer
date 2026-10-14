# Host panel

Contract v1, agreed 2026-10-14. For one look across platforms (user decision, 2026-10-14), each host app keeps its native shell and shows the same bundled web page as its window: the Swift app on the Mac today, and a Windows host later. The viewer is already one Tauri UI everywhere. A single Tauri app that both connects and shares remains the later upgrade path.

The host panel is one HTML/JS/CSS page, bundled with the app and never fetched from the network. The Mac app shows it in a WKWebView, and a future Windows host will show it in a Tauri webview. The shell (Swift or Rust) owns all state and every decision. The page only renders the state and reports clicks. **Pairing approval stays native:** no web content can approve a device.

## Files and loading
- **Location:** `panel/index.html`, `panel/panel.js`, `panel/panel.css` at the repo root.
  - macOS: `build-macos.sh` copies them into `Contents/Resources/panel`, where codesign seals them.
  - Windows: the future host bundles them the same way.
- **Load:** `loadFileURL(index.html, allowingReadAccessTo:` the panel dir only`)`. The navigation policy allows that first load and cancels everything else. There is no `window.open` and no file picker. If the web content process dies, the shell reloads the page. The web view has `isInspectable = false` and no default context menu items such as Reload; any menu the page wants goes through actions.
- **CSP:** `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:`. No inline script or style, and no style attributes set from strings.
- **Page:** follows `prefers-color-scheme` and uses plain words.

## Transport
- **Shell → page:** the shell calls `window.panel.receive(msg)` with the message as a structured argument, never spliced into script text.
  - Swift: `callAsyncJavaScript("window.panel.receive(msg)", arguments: ["msg": dict], in: nil, contentWorld: .page)`. Never `evaluateJavaScript` with a built string, because device names come from remote devices.
  - Tauri: the same, through an event payload.
- **Page → shell:** `panelSend(action)`.
  - Swift: `window.webkit.messageHandlers.panel.postMessage(action)`. Accept it only from the main frame (`frameInfo.isMainFrame`) and from a `file://` URL inside the bundle's panel dir. Re-serialize the body with `JSONSerialization` and drop it if it isn't a dictionary or is over 4 KiB measured as JSON.
  - Tauri: `invoke("panel_action", {action})`, with the same checks.
- **Version:** every message carries `v: 1`. A page that gets a newer `v` shows "Update the app" and sends nothing.

## Shell → page messages
1. `{v:1, type:"state", state}` is a full snapshot, sent only when something changes, and in answer to `ready`:
   ```
   state = {
     platform: "macos" | "windows",
     host: { name, fingerprint } | null,          // null while Remote access is off
     remoteAccess: { on, busy, error },            // busy while the child starts or stops; error: text or null
     permissions: [ { id, label, granted, canRequest } ],   // macOS: "screen", "accessibility"; may be empty
     session: { device, deviceFingerprint } | null,          // from host Event::Session / SessionEnded
     pairingNotice: string | null,                 // how the last pairing request ended, as today
     clipboard: { on }
   }
   ```
2. `{v:1, type:"status", text}` is today's `statusLine()` (counts only), sent every 0.5 s.
3. `{v:1, type:"log", line}` adds one log line.
   - The shell keeps the last 200 lines in memory and never writes them to disk.
   - On `ready`, the shell sends the state and then the current log, so a reload loses nothing.

## Page → shell actions
- `{v:1, type:"ready"}`
- `{v:1, type:"setRemoteAccess", on}`
- `{v:1, type:"setClipboard", on}`
- `{v:1, type:"requestPermission", id}`, where `id` is `"screen"` or `"accessibility"`; anything else is dropped.
- `{v:1, type:"refreshPermissions"}`

## Rules
- **The shell validates every action.** Any action whose `v` isn't 1 is dropped, so a stale page can't drive a newer shell. Unknown types, wrong field types and out-of-enum values are dropped and logged by type only.
- **Text only:** the page renders with `textContent` and never uses `innerHTML`. All strings, including errors, notices, device names and log lines, are text.
- **No flicker:** the page leaves a node alone when its text hasn't changed, so the user can select and copy the fingerprint while status updates arrive.
- **Never in any message:** passwords, keys, typed input or clipboard text. Fingerprints and device names are fine.

## Not in v1
- An in-page pairing prompt. Approval stays the native dialog, and changing that is a separate decision.
- A paired-devices list with Remove. This comes first in v2. It needs a list/revoke local command, or the shell reading the file.
- Session controls, such as disconnecting the viewer.
