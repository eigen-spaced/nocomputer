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

    var onLog: ((String) -> Void)?
    var onExit: (() -> Void)?
    // This Mac's name and fingerprint while remote access is on, for comparing with
    // what a viewer shows; nil when off.
    var onIdentity: ((LocalLink.Identity?) -> Void)?

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
    private var restartTask: Task<Void, Never>?
    private var prompt: (alert: NSAlert, requestID: UInt64)?
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
        let input = inputQueue.sync { (injector?.state ?? InputSnapshot(), untrustedDrops) }
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
        startStream()
    }

    private func startStream() {
        streamWanted = true
        restartTask?.cancel()
        guard capture.stream == nil, !starting, let link else { return }
        starting = true
        link.expectKeyframe()
        Task {
            defer { starting = false }
            do {
                try await capture.start(sampleConsumer: { [weak link] in link?.submit($0) },
                    onStop: { reason in DispatchQueue.main.async { [weak self] in self?.captureEnded(reason) } })
                if !streamWanted { await stopCapture() }
                let output = capture.output
                if let display = capture.displayID { inputQueue.async { self.inputDisplay = display; self.timedOutput = output } }
                log("Streaming: \(capture.displayDescription)")
            } catch {
                captureEnded(error.localizedDescription)
            }
        }
    }

    private func stopStream(reason: String) {
        releaseInput()
        clipboard.stop()
        streamWanted = false
        restartTask?.cancel()
        Task { await stopCapture() }
        log("Stream stopped: \(reason)")
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
        restartTask?.cancel()
        restartTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: Self.retryDelay)
            guard !Task.isCancelled, let self, self.streamWanted else { return }
            self.startStream()
        }
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
        case "pairing_withdrawn":
            guard let withdrawn = try? JSONDecoder().decode(LocalLink.PairingWithdrawn.self, from: json),
                  prompt?.requestID == withdrawn.requestID else { return }
            dismissPrompt()
            log("Pairing request withdrawn: the device left or the request timed out")
        case let type?:
            // Display only. The JSON carries names and fingerprints, never keys.
            log("Host event \(type): \(String(decoding: json, as: UTF8.self))")
        case nil:
            log("Ignored untyped host event")
        }
    }

    // Deny is the default button; approving needs a deliberate click. Only one
    // request is pending in the child at a time, so a newer prompt replaces this one.
    private func showPairingPrompt(_ request: LocalLink.PairingPrompt) {
        dismissPrompt()
        let alert = NSAlert()
        alert.messageText = "Allow \"\(request.deviceName)\" to control this Mac?"
        alert.informativeText = """
            Only approve if the viewer shows these same fingerprints.

            Device:
            \(request.deviceFingerprint)

            This Mac:
            \(request.hostFingerprint)
            """
        alert.addButton(withTitle: "Deny")
        alert.addButton(withTitle: "Approve")
        prompt = (alert, request.requestID)
        let decide: (NSApplication.ModalResponse) -> Void = { [weak self] response in
            guard let self, self.prompt?.requestID == request.requestID else { return }
            self.prompt = nil
            let approve = response == .alertSecondButtonReturn
            self.link?.decide(requestID: request.requestID, approve: approve)
            self.log("Pairing \(approve ? "approved" : "denied"): \(request.deviceName)")
        }
        if let window { alert.beginSheetModal(for: window, completionHandler: decide) }
        else { decide(alert.runModal()) }
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
        streamWanted = false
        restartTask?.cancel()
        Task { await stopCapture() }
        link = nil
        onIdentity?(nil)
        sleep.disable()
        log("Remote access off: \(reason); sleep assertion released")
        onExit?()
    }

    private func releaseInput() { inputQueue.async { self.injector?.releaseAll() } }

    // On inputQueue. Without Accessibility, posted events are silently
    // discarded by the system, so drop and count them instead.
    nonisolated private func inject(_ input: RemoteInput) {
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
