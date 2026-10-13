import AppKit
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

    private weak var window: NSWindow?
    private var link: HostLink?
    private let capture = CaptureSession()
    private let sleep = SleepAssertion(kind: .system)
    private var streamWanted = false
    private var starting = false
    private var restartTask: Task<Void, Never>?
    private var prompt: (alert: NSAlert, requestID: UInt64)?

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
        link.onStartStream = { [weak self] in self?.startStream() }
        link.onStopStream = { [weak self] in self?.stopStream(reason: "no controlling session") }
        link.onRequestKeyframe = { [weak self] in self?.capture.output?.encoder?.requestKeyframe() }
        link.onEvent = { [weak self] in self?.event($0) }
        link.onExit = { [weak self] in self?.exited($0) }
        try sleep.enable()
        do { try link.start() } catch { sleep.disable(); throw error }
        self.link = link
        log("Remote access enabled; system sleep assertion held")
    }

    func disable() {
        guard let link else { return }
        stopStream(reason: "remote access disabled")
        link.stop()
    }

    func statusLine() -> String {
        guard let link else { return "Remote access off" }
        let state = link.snapshot()
        let encoder = capture.output?.encoder?.snapshot()
        return "Remote access on; streaming=\(capture.stream != nil ? "yes" : "no"); sent=\(state.accessUnits) (keyframes \(state.keyframes), \(state.bytes) bytes); pipe drops=\(state.pressureDrops)+\(state.dependentDrops) dependent; peak queued=\(state.peakQueued)"
            + (encoder.map { "; encoder raw drops=\($0.rawDrops)" } ?? "")
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
                // The whole display, including this app: the remote user sees
                // exactly what is on screen.
                try await capture.start(mode: .wholeDisplay, encodeVideo: true, colorPolicy: .sRGB,
                    maximumWidth: 1600, keyframeSeconds: VideoEncoder.remoteKeyframeSeconds, sampleConsumer: { [weak link] in link?.submit($0) },
                    onStop: { reason in DispatchQueue.main.async { [weak self] in self?.captureEnded(reason) } })
                if !streamWanted { await stopCapture() }
                log("Streaming: \(capture.displayDescription)")
            } catch {
                captureEnded(error.localizedDescription)
            }
        }
    }

    private func stopStream(reason: String) {
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
    }

    private func exited(_ reason: String) {
        dismissPrompt()
        streamWanted = false
        restartTask?.cancel()
        Task { await stopCapture() }
        link = nil
        sleep.disable()
        log("Remote access off: \(reason); sleep assertion released")
        onExit?()
    }

    private func log(_ message: String) { onLog?(message) }
}
