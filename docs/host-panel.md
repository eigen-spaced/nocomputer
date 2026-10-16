# Host panel

Contract v3, proposed 2026-10-16 (v2: 2026-10-15, v1: 2026-10-14). v2 added the paired-devices list; v3 adds receiving files (see [the plan](development-plan.md#file-drop-windows-viewer--mac-inbox)). For one look across platforms (user decision, 2026-10-14), each host app keeps its native shell and shows the same bundled web page as its window: the Swift app on the Mac today, and a Windows host later. The viewer is already one Tauri UI everywhere. A single Tauri app that both connects and shares remains the later upgrade path.

The host panel is one HTML/JS/CSS page, bundled with the app and never fetched from the network. The Mac app shows it in a WKWebView, and a future Windows host will show it in a Tauri webview. The shell (Swift or Rust) owns all state and every decision. The page only renders the state and reports clicks. **Pairing approval stays native:** no web content can approve a device.

## Files and loading
- **Location:** `panel/index.html`, `panel/panel.js`, `panel/panel.css` at the repo root.
  - macOS: `build-macos.sh` copies them into `Contents/Resources/panel`, where codesign seals them.
  - Windows: the future host bundles them the same way.
- **Load:** the Mac app serves the page from a `WKURLSchemeHandler` at `nocomputer-panel://panel/`, which answers only for the three files above. A `file://` page has an opaque origin in WebKit, so CSP `'self'` could not be relied on for it. The navigation policy allows only `index.html` in the main frame and cancels everything else. There is no `window.open` and no web file picker; the inbox folder is chosen in a native dialog the shell opens (see [Receiving files](#receiving-files)). If the web content process dies, the shell reloads the page. The web view has `isInspectable = false` and no default context menu items such as Reload; any menu the page wants goes through actions.
- **CSP:** `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:`. No inline script or style, and no style attributes set from strings.
- **Page:** follows `prefers-color-scheme` and uses plain words.

## Transport
- **Shell → page:** the shell calls `window.panel.receive(msg)` with the message as a structured argument, never spliced into script text.
  - Swift: `callAsyncJavaScript("window.panel.receive(msg)", arguments: ["msg": dict], in: nil, contentWorld: .page)`. Never `evaluateJavaScript` with a built string, because device names come from remote devices.
  - Tauri: the same, through an event payload.
- **Page → shell:** `panelSend(action)`.
  - Swift: `window.webkit.messageHandlers.panel.postMessage(action)`. Accept it only from the main frame (`frameInfo.isMainFrame`) with the origin `nocomputer-panel://panel`. Re-serialize the body with `JSONSerialization` and drop it if it isn't a dictionary or is over 4 KiB measured as JSON.
  - Tauri: `invoke("panel_action", {action})`, with the same checks.
- **Version:** every message carries `v: 3`. A page that gets a newer `v` shows "Update the app" and sends nothing.

## Shell → page messages
1. `{v:3, type:"state", state}` is a full snapshot, sent only when something changes, and in answer to `ready`:
   ```
   state = {
     platform: "macos" | "windows",
     host: { name, fingerprint } | null,          // null while Remote access is off
     remoteAccess: { on, busy, error },            // busy while the child starts or stops; error: text or null
     permissions: [ { id, label, granted, canRequest } ],   // macOS: "screen", "accessibility"; may be empty
     session: { device, deviceFingerprint } | null,          // from host Event::Session / SessionEnded
     pairingNotice: string | null,                 // how the last pairing request ended, as today
     clipboard: { on },
     devices: [ { name, fingerprint, pairedAt, removed } ],  // from host paired_devices; [] while Remote access is off
     files: {
       on,                                         // Receive files; always false while folder is null
       folder: string | null,                      // display path, e.g. "~/Inbox"; never read back from the page
       transfers: [ { id, device, name, size, received, status, reason } ]
     }
   }
   ```
2. `{v:3, type:"status", text}` is today's `statusLine()` (counts only), sent every 0.5 s.
3. `{v:3, type:"progress", id, received}` updates one transfer's byte count, at most every 0.25 s per transfer, so the full state isn't resent for each chunk. An unknown `id` is ignored.
4. `{v:3, type:"log", line}` adds one log line.
   - The shell keeps the last 200 lines in memory and never writes them to disk.
   - On `ready`, the shell sends the state and then the current log, so a reload loses nothing.

## Page → shell actions
- `{v:3, type:"ready"}`
- `{v:3, type:"setRemoteAccess", on}`
- `{v:3, type:"setClipboard", on}`
- `{v:3, type:"requestPermission", id}`, where `id` is `"screen"` or `"accessibility"`; anything else is dropped.
- `{v:3, type:"refreshPermissions"}`
- `{v:3, type:"removeDevice", fingerprint}`
- `{v:3, type:"allowPairingAgain", fingerprint}`
- `{v:3, type:"setReceiveFiles", on}`
- `{v:3, type:"chooseFolder"}`
- `{v:3, type:"cancelTransfer", id}`
- `{v:3, type:"clearTransfers"}`

## Paired devices
- **List:** `devices` holds every device the host lists, in pairing order, removed ones included. `pairedAt` is Unix seconds, and the page formats it locally. The page marks the device whose fingerprint equals `session.deviceFingerprint` as connected, and shows "Remove" for an active device and "Allow pairing again" for a removed one.
- **Remove** revokes the device and closes its connections; its viewer shows that access was revoked. A removed device stays listed and is refused before any pairing prompt, so it can't simply pair again.
- **Allow pairing again** deletes a removed device from the list. It may then ask to pair, which needs the usual native approval.
- **Native confirmation:** like pairing approval, a click in the page alone can't change trust. The shell shows a native confirmation naming the device (control and bidi characters stripped) with its full fingerprint, and acts only on the user's confirmation. A second request while one is open is ignored.
- **Checks:** the shell accepts exactly the keys `{v, type, fingerprint}`, with `fingerprint` a non-empty string of at most 128 characters that matches a device in its current list: `removeDevice` only for an active one, `allowPairingAgain` only for a removed one. The host checks again.
- **Order:** removing the connected device ends its session too. The new list and the session's end can arrive in either order.

## Receiving files
- **Folder:** `chooseFolder` makes the shell open the native folder dialog (macOS: `NSOpenPanel`, folders only, New Folder allowed). The page never supplies a path. The shell keeps the folder in its own settings and passes it to the host over the local link. `setReceiveFiles` with `on: true` is dropped while no folder is set. The host refuses every offer while either is unset.
- **Approval stays native**, like pairing: the shell shows a native dialog, "<device> wants to send <name> (<size>)", with Accept and Decline. Name and device have control and bidi characters stripped. Unanswered after 60 s means declined. One dialog at a time; later offers queue in arrival order, each with its own 60 s. Nothing in the page can accept a file.
- **Transfers:** the shell keeps the last 20 in memory, newest first, and never writes them to disk. `id` is the shell's local number for the transfer, not a protocol value. `name` is the offered name until saved, then the final saved name (for example `a (1).txt`). `size` and `received` are bytes, and the page formats them.
  - `status` is `"waiting"` (dialog open), `"receiving"`, `"saved"`, `"declined"`, `"cancelled"` or `"failed"`.
  - `reason` is null unless `status` is `"declined"`, `"cancelled"` or `"failed"`: `"declined"`, `"timeout"`, `"too_large"`, `"no_space"`, `"bad_name"`, `"checksum"`, `"short"`, `"disk"`, `"disconnected"`, `"cancelled_here"` or `"cancelled_by_viewer"`. The page turns these into plain words; an unknown reason shows as "failed".
- **Checks:** `cancelTransfer` accepts exactly `{v, type, id}`, with `id` an integer matching a `"waiting"` or `"receiving"` transfer. Cancelling a waiting transfer closes its dialog as declined. `clearTransfers` removes finished entries only.
- **Log:** one line per finished transfer, such as "Received a file from latitude" or "A file from latitude failed: not enough space", without the file name.

## Rules
- **The shell validates every action.** Any action whose `v` isn't 3 is dropped, so a stale page can't drive a newer shell. Unknown types, wrong field types and out-of-enum values are dropped and logged by type only.
- **Text only:** the page renders with `textContent` and never uses `innerHTML`. All strings, including errors, notices, device names and log lines, are text.
- **No flicker:** the page leaves a node alone when its text hasn't changed, so the user can select and copy the fingerprint while status updates arrive.
- **Never in any message:** passwords, keys, typed input, clipboard text or file contents. Fingerprints and device names are fine; file names appear only in `files.transfers`.

## Not in v3
- An in-page pairing prompt. Approval stays the native dialog, and changing that is a separate decision.
- Session controls, such as disconnecting the viewer.
- Opening the inbox or a received file from the panel (for example "Show in Finder").
- Sending files from the host to the viewer; that comes with the reverse direction.
