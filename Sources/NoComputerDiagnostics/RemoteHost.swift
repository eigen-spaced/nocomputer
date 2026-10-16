import AppKit
import ApplicationServices
import CoreMedia

// Remote access: runs the bundled nocomputer-host child and captures/encodes the
// display only while it reports a controlling session. Holds a system sleep
// assertion only while remote access is enabled.
@MainActor
final class RemoteHost {
    static let executableName = "nocomputer-host"
    static let retryDelay: UInt64 = 2_000_000_000
    // Display wake takes a fraction of a second; poll for it this often.
    static let wakePollDelay: UInt64 = 500_000_000

    var onLog: ((String) -> Void)?
    var onExit: (() -> Void)?
    // This Mac's name and fingerprint while remote access is on, for comparing with
    // what a viewer shows; nil when off.
    var onIdentity: ((LocalLink.Identity?) -> Void)?
    // How the last pairing request ended, shown in the window; nil clears it.
    var onPairingNotice: ((String?) -> Void)?
    // The viewer in control, or nil.
    var onSession: ((LocalLink.Session?) -> Void)?
    // Every paired device, removed ones included; empty while remote access is off.
    var onPairedDevices: (([LocalLink.PairedDevices.Device]) -> Void)?

    private weak var window: NSWindow?
    private var link: HostLink?
    private let capture = CaptureSession()
    private let sleep = SleepAssertion()
    private let clipboard = ClipboardSync()
    // Plain-text clipboard sharing with the controlling viewer; takes effect at once.
    var clipboardEnabled = true {
        didSet { if clipboardEnabled { if streamWanted { clipboard.start() } } else { clipboard.stop() } }
    }
    private var streamWanted = false
    private var starting = false
    private var waitingForDisplay = false
    private var restartTask: Task<Void, Never>?
    // The controlling viewer's largest 1:1 picture in pixels, or nil for the
    // native size. Applied to every capture start until the stream stops.
    private var viewBox: CGSize?
    private var resizing = false
    private var prompt: (alert: NSAlert, request: LocalLink.PairingPrompt)?
    // The open Remove / Allow-pairing-again confirmation; a second request while
    // one is open is ignored.
    private var deviceConfirmation: NSAlert?
    // Controlling sessions, counted as the child does: a replacing session can
    // start before the one it replaces ends.
    private var sessions = 0 {
        didSet {
            if (oldValue > 0) != (sessions > 0), let request = prompt?.request { showPairingPrompt(request) }
            onSession?(sessions > 0 ? latestSession : nil)
        }
    }
    // The newest session is the one in control: it may start before the one it
    // replaces ends.
    private var latestSession: LocalLink.Session?
    // Input is injected on its own queue, never behind UI work. The injector
    // and the fields below are only touched on this queue.
    private let inputQueue = DispatchQueue(label: "dev.nocomputer.input", qos: .userInteractive)
    // Account checks block on OpenDirectory; one at a time, off the main thread.
    private let accountQueue = DispatchQueue(label: "dev.nocomputer.account-check", qos: .userInitiated)
    nonisolated(unsafe) private var injector: InputInjector?
    nonisolated(unsafe) private var inputDisplay = CGMainDisplayID()
    nonisolated(unsafe) private var trustCheckedAt: TimeInterval = -.infinity
    nonisolated(unsafe) private var trusted = false
    nonisolated(unsafe) private var untrustedDrops = 0
    nonisolated(unsafe) private var timedOutput: CaptureOutput?
    // Wakes the display for remote input. Also only touched on inputQueue.
    nonisolated(unsafe) private let displayWake = DisplayWakeRequest()

    var running: Bool { link != nil }

    init(window: NSWindow?) { self.window = window }

    static func bundledExecutable() -> URL? {
        let url = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/\(executableName)")
        return FileManager.default.isExecutableFile(atPath: url.path) ? url : nil
    }

    func enable() throws {
        guard link == nil else { return }
        guard let executable = Self.bundledExecutable() else {
            throw NSError(domain: "NoComputer.RemoteHost", code: 1, userInfo: [NSLocalizedDescriptionKey:
                "\(Self.executableName) is not in the app bundle; build with scripts/build-macos.sh"])
        }
        let link = HostLink(executable: executable)
        link.onStartStream = { [weak self] in self?.sessionStarted() }
        link.onStopStream = { [weak self] in self?.stopStream(reason: "no controlling session") }
        link.onRequestKeyframe = { [weak self] in self?.capture.requestKeyframe() }
        link.onEvent = { [weak self] in self?.event($0) }
        link.onClipboard = { [weak self] in self?.clipboard.apply($0) }
        link.onViewSize = { [weak self] in self?.viewSizeChanged(width: $0, height: $1) }
        clipboard.onLocalText = { [weak link] in link?.sendClipboard($0) }
        link.onExit = { [weak self] in self?.exited($0) }
        link.onInput = { [weak self] input in self?.inputQueue.async { self?.inject(input) } }
        link.onVerifyLogin = { [weak link, accountQueue] request in
            accountQueue.async {
                let ok = AccountCheck.verify(username: request.username, password: request.password)
                link?.loginVerdict(requestID: request.requestID, ok: ok)
            }
        }
        try sleep.enable()
        do { try link.start() } catch { sleep.disable(); throw error }
        self.link = link
        log("Remote access enabled; system sleep assertion held")
    }

    func disable() {
        guard let link else { return }
        // Synchronous: this also runs at app termination.
        inputQueue.sync { injector?.releaseAll() }
        stopStream(reason: "remote access disabled")
        link.stop()
    }

    func statusLine() -> String {
        guard let link else { return "Remote access off" }
        let state = link.snapshot()
        let input = inputQueue.sync { (injector?.state ?? InputSnapshot(), untrustedDrops, displayWake.failures) }
        var line = "Remote access on; streaming=\(capture.stream != nil ? "yes" : "no"); sent=\(state.accessUnits) (keyframes \(state.keyframes), \(state.bytes) bytes); pipe drops=\(state.pressureDrops)+\(state.dependentDrops) dependent; peak queued=\(state.peakQueued)"
        if let e = capture.output?.encoder?.snapshot() {
            let average = e.encoded > 0 ? e.totalMilliseconds / Double(e.encoded) : 0
            line += String(format: "; encoder raw drops=%d; encode avg=%.1f ms max=%.1f ms", e.rawDrops, average, e.maxMilliseconds)
        }
        if let c = capture.output?.snapshot(), c.keyToFrameCount > 0 {
            let average = c.keyToFrameTotalMilliseconds / Double(c.keyToFrameCount)
            line += String(format: "; key→frame avg=%.1f ms max=%.1f ms (n=%d)", average, c.keyToFrameMaxMilliseconds, c.keyToFrameCount)
        }
        // Counts only; no key identities.
        line += "\nInput: pointer=\(input.0.pointerEvents) buttons=\(input.0.buttonEvents) scroll=\(input.0.scrollEvents) keys=\(input.0.keyEvents) unmapped=\(input.0.unmappedKeys) releaseAll=\(input.0.releaseAlls)"
        if input.1 > 0 { line += " dropped without Accessibility=\(input.1)" }
        if input.2 > 0 { line += " display wake failures=\(input.2)" }
        // Counts only; never the text.
        let c = clipboard.state
        line += "\nClipboard: \(clipboardEnabled ? "on" : "off"); sent=\(c.sent) received=\(c.applied) skipped secret=\(c.skippedSecret) over 1 MiB=\(c.skippedLarge) not text=\(c.skippedNonText)"
        return line
    }

    private func sessionStarted() {
        // Every session gets the notice: a viewer that connects later never
        // saw an earlier one.
        if !AXIsProcessTrusted() { link?.inputUnavailable(reason: "Accessibility is not granted to the host app") }
        if clipboardEnabled { clipboard.start() }
        // Capture cannot start, and ends with -3815, while the display sleeps.
        inputQueue.async { self.displayWake.requestIfDue() }
        startStream()
    }

    private func startStream() {
        streamWanted = true
        restartTask?.cancel()
        guard capture.stream == nil, !starting, let link else { return }
        if CGDisplayIsAsleep(CGMainDisplayID()) != 0 {
            // Woken by the session start or the viewer's next input.
            if !waitingForDisplay { log("Display asleep; capture waits for it to wake") }
            waitingForDisplay = true
            scheduleStart(after: Self.wakePollDelay)
            return
        }
        waitingForDisplay = false
        starting = true
        link.expectKeyframe()
        Task {
            defer { starting = false }
            do {
                try await capture.start(box: viewBox, sampleConsumer: { [weak link] in link?.submit($0) },
                    onStop: { reason in DispatchQueue.main.async { [weak self] in self?.captureEnded(reason) } })
                if !streamWanted { await stopCapture() }
                let output = capture.output
                if let display = capture.displayID { inputQueue.async { self.inputDisplay = display; self.timedOutput = output } }
                log("Streaming: \(capture.displayDescription)")
                // A size that arrived while starting.
                applyViewBox()
            } catch {
                captureEnded(error.localizedDescription)
            }
        }
    }

    private func stopStream(reason: String) {
        releaseInput()
        clipboard.stop()
        streamWanted = false
        waitingForDisplay = false
        viewBox = nil
        restartTask?.cancel()
        Task { await stopCapture() }
        log("Stream stopped: \(reason)")
    }

    private func viewSizeChanged(width: UInt16, height: UInt16) {
        let native = width == UInt16.max && height == UInt16.max
        viewBox = native ? nil : CGSize(width: Int(width), height: Int(height))
        applyViewBox()
    }

    // One resize at a time; the newest box wins. A capture start reads the box itself.
    private func applyViewBox() {
        guard streamWanted, !resizing, !starting, capture.stream != nil else { return }
        resizing = true
        let box = viewBox
        Task {
            do {
                if let description = try await capture.resize(box: box) { log("Resized: \(description)") }
            } catch {
                // The stream may be half changed; a fresh one starts at the new size.
                log("Resize failed, restarting capture: \(error.localizedDescription)")
                await stopCapture()
                if streamWanted { startStream() }
            }
            resizing = false
            if box != viewBox { applyViewBox() }
        }
    }

    private func stopCapture() async {
        do { try await capture.stop() } catch { log("Capture stop failed: \(error.localizedDescription)") }
    }

    // The system ended capture (e.g. around lock or display changes). Tell the
    // child, then retry with a fresh stream while a controller still wants video.
    private func captureEnded(_ reason: String) {
        link?.captureStopped(reason: reason)
        log("Capture ended: \(reason)")
        capture.retireStoppedStream()
        guard streamWanted else { return }
        scheduleStart(after: Self.retryDelay)
    }

    private func scheduleStart(after delay: UInt64) {
        restartTask?.cancel()
        restartTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: delay)
            guard !Task.isCancelled, let self, self.streamWanted else { return }
            self.startStream()
        }
    }

    // Remote input declared activity, which wakes a sleeping display. If capture
    // ended while it slept, try again soon rather than after the full retry delay.
    private func inputDeclaredActivity() {
        guard streamWanted, capture.stream == nil, !starting else { return }
        scheduleStart(after: Self.wakePollDelay)
    }

    private func event(_ json: Data) {
        switch LocalLink.eventType(json) {
        case "pairing_prompt":
            guard let request = try? JSONDecoder().decode(LocalLink.PairingPrompt.self, from: json) else {
                log("Ignored malformed pairing prompt")
                return
            }
            showPairingPrompt(request)
        case "identity":
            guard let identity = try? JSONDecoder().decode(LocalLink.Identity.self, from: json) else {
                log("Ignored malformed identity")
                return
            }
            onIdentity?(identity)
        case "paired_devices":
            guard let list = try? JSONDecoder().decode(LocalLink.PairedDevices.self, from: json) else {
                log("Ignored malformed paired devices")
                return
            }
            onPairedDevices?(list.devices)
        case "pairing_withdrawn":
            guard let withdrawn = try? JSONDecoder().decode(LocalLink.PairingWithdrawn.self, from: json),
                  let current = prompt, current.request.requestID == withdrawn.requestID else { return }
            dismissPrompt()
            onPairingNotice?("Pairing request from \"\(current.request.deviceName)\" closed: the device cancelled or 60 s passed. Nothing was approved.")
            log("Pairing request withdrawn: the device left or the request timed out")
        case "session", "session_ended":
            let started = LocalLink.eventType(json) == "session"
            if started { latestSession = try? JSONDecoder().decode(LocalLink.Session.self, from: json) }
            sessions = max(0, sessions + (started ? 1 : -1))
            log("Host event \(LocalLink.eventType(json) ?? ""): \(String(decoding: json, as: UTF8.self))")
        case let type?:
            // Display only. The JSON carries names and fingerprints, never keys.
            log("Host event \(type): \(String(decoding: json, as: UTF8.self))")
        case nil:
            log("Ignored untyped host event")
        }
    }

    // Deny is the default button; approving needs a deliberate click. Only one
    // request is pending in the child at a time, so a newer prompt replaces this one.
    // While a viewer is in control, a click may be the viewer's rather than someone
    // at this Mac, so Approve is disabled; the prompt is redrawn when that changes.
    private func showPairingPrompt(_ request: LocalLink.PairingPrompt) {
        dismissPrompt()
        onPairingNotice?(nil)
        let controlled = sessions > 0
        let alert = NSAlert()
        alert.messageText = "Allow \"\(Self.displayName(request.deviceName))\" to control this Mac?"
        alert.informativeText = (controlled ? "Disconnect the viewer first to approve a new device.\n\n" : "") + """
            Only approve if the viewer shows these same fingerprints.

            Device:
            \(request.deviceFingerprint)

            This Mac:
            \(request.hostFingerprint)
            """
        alert.addButton(withTitle: "Deny")
        alert.addButton(withTitle: "Approve").isEnabled = !controlled
        prompt = (alert, request)
        // Matched by dialog, not request id: a redrawn prompt reuses the id, and the
        // dialog it replaced must not answer for it.
        let decide: (NSApplication.ModalResponse) -> Void = { [weak self, weak alert] response in
            guard let self, let alert, self.prompt?.alert === alert else { return }
            self.prompt = nil
            let approve = response == .alertSecondButtonReturn && self.sessions == 0
            self.link?.decide(requestID: request.requestID, approve: approve)
            self.log("Pairing \(approve ? "approved" : "denied"): \(request.deviceName)")
            self.onPairingNotice?("Pairing request from \"\(request.deviceName)\" \(approve ? "approved" : "denied").")
        }
        if let window { alert.beginSheetModal(for: window, completionHandler: decide) }
        else { decide(alert.runModal()) }
    }

    // Removing ends the device's session at once and blocks it; allowing it to
    // pair again only lets it ask, and asking still needs approval here. Either is
    // confirmed in a native dialog, so the page alone can't change who may connect.
    func confirmRemove(_ device: LocalLink.PairedDevices.Device) {
        confirm(device, remove: true)
    }

    func confirmAllowPairingAgain(_ device: LocalLink.PairedDevices.Device) {
        confirm(device, remove: false)
    }

    private func confirm(_ device: LocalLink.PairedDevices.Device, remove: Bool) {
        guard link != nil, deviceConfirmation == nil else { return }
        let name = Self.displayName(device.name)
        let alert = NSAlert()
        if remove {
            alert.messageText = "Remove \"\(name)\"?"
            alert.informativeText = """
                It is disconnected now and can't connect again until you allow it to pair again.

                Device:
                \(device.deviceFingerprint)
                """
        } else {
            alert.messageText = "Allow \"\(name)\" to pair again?"
            alert.informativeText = """
                It can then ask to pair, and you will be asked to approve it here.

                Device:
                \(device.deviceFingerprint)
                """
        }
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: remove ? "Remove" : "Allow").hasDestructiveAction = remove
        deviceConfirmation = alert
        let finish: (NSApplication.ModalResponse) -> Void = { [weak self, weak alert] response in
            guard let self, let alert, self.deviceConfirmation === alert else { return }
            self.deviceConfirmation = nil
            guard response == .alertSecondButtonReturn, let link = self.link else { return }
            if remove { link.removeDevice(fingerprint: device.deviceFingerprint) }
            else { link.forgetDevice(fingerprint: device.deviceFingerprint) }
            self.log(remove ? "Removed device \(name)" : "Allowed \(name) to pair again")
        }
        if let window { alert.beginSheetModal(for: window, completionHandler: finish) }
        else { finish(alert.runModal()) }
    }

    // A remote device chose this name: drop control, format (bidi) and line
    // separator characters before showing it in a dialog, and keep it short.
    nonisolated static func displayName(_ name: String) -> String {
        let hidden: Set<Unicode.GeneralCategory> = [.control, .format, .lineSeparator, .paragraphSeparator]
        var scalars = String.UnicodeScalarView()
        scalars.append(contentsOf: name.unicodeScalars.filter { !hidden.contains($0.properties.generalCategory) })
        let shown = String(scalars).trimmingCharacters(in: .whitespaces)
        if shown.isEmpty { return "unnamed device" }
        return shown.count > 64 ? String(shown.prefix(64)) + "…" : shown
    }

    private func dismissConfirmation() {
        guard let alert = deviceConfirmation else { return }
        deviceConfirmation = nil
        if let sheetParent = alert.window.sheetParent { sheetParent.endSheet(alert.window) }
        else if NSApp.modalWindow == alert.window { NSApp.abortModal() }
    }

    private func dismissPrompt() {
        guard let (alert, _) = prompt else { return }
        prompt = nil
        if let sheetParent = alert.window.sheetParent { sheetParent.endSheet(alert.window) }
        else if NSApp.modalWindow == alert.window { NSApp.abortModal() }
    }

    private func exited(_ reason: String) {
        releaseInput()
        clipboard.stop()
        dismissPrompt()
        dismissConfirmation()
        streamWanted = false
        restartTask?.cancel()
        Task { await stopCapture() }
        link = nil
        sessions = 0
        onIdentity?(nil)
        onPairingNotice?(nil)
        onPairedDevices?([])
        sleep.disable()
        log("Remote access off: \(reason); sleep assertion released")
        onExit?()
    }

    // Also lets the display sleep again on its normal idle timer.
    private func releaseInput() {
        inputQueue.async {
            self.injector?.releaseAll()
            self.displayWake.release()
        }
    }

    // On inputQueue. Without Accessibility, posted events are silently
    // discarded by the system, so drop and count them instead.
    nonisolated private func inject(_ input: RemoteInput) {
        // Any viewer input counts as activity, as a local user's would, even
        // when Accessibility is missing and the input itself is dropped.
        if displayWake.requestIfDue() { Task { @MainActor in self.inputDeclaredActivity() } }
        let now = ProcessInfo.processInfo.systemUptime
        if now - trustCheckedAt > 1 {
            trustCheckedAt = now
            trusted = AXIsProcessTrusted()
        }
        guard trusted else { untrustedDrops += 1; return }
        let bounds = CGDisplayBounds(inputDisplay)
        if let injector { injector.update(displayBounds: bounds) } else { injector = InputInjector(displayBounds: bounds) }
        injector?.handle(input)
        if case .key(_, .down) = input { timedOutput?.markKeyDown() }
    }

    private func log(_ message: String) { onLog?(message) }
}
