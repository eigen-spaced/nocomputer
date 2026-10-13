import AppKit
import ApplicationServices

@MainActor
final class DiagnosticsController: NSObject, NSApplicationDelegate {
    private let capture = CaptureSession()
    private let sleep = SleepAssertion()
    private let displaySleep = SleepAssertion(kind: .display)
    private let displayWake = DisplayWakeRequest()
    private var remoteHost: RemoteHost?
    private let remoteAccess = NSButton(checkboxWithTitle: "Remote access (paired viewers only; runs nocomputer-host)", target: nil, action: nil)
    private let remoteStatus = NSTextField(wrappingLabelWithString: "Remote access off")
    private var wakeProbeTask: Task<Void, Never>?
    private var wakeProbeFrame: CGImage?
    private var wakeProbeFrameTime: TimeInterval?
    private var inputProbeFrame: CGImage?
    private var inputProbeFrameTime: TimeInterval?
    private var lockInputPosted = false
    private var pendingProbeErasure: [CGEvent]?
    private var probeErasurePosted = false
    private var reviewWindow: NSWindow?
    private var displayDeadline: TimeInterval?
    private var displayOffTask: Task<Void, Never>?
    private var workspaceObservers: [NSObjectProtocol] = []
    private var window: NSWindow!
    private let permissionLabel = NSTextField(wrappingLabelWithString: "")
    private let statusLabel = NSTextField(wrappingLabelWithString: "Capture stopped")
    private let preview = NSImageView()
    private let inputTarget = NSTextView()
    private let logView = NSTextView()
    private let keepAwake = NSButton(checkboxWithTitle: "Keep system awake (display may sleep)", target: nil, action: nil)
    private let encodeVideo = NSButton(checkboxWithTitle: "Encode H.264 (hardware)", target: nil, action: nil)
    private let decodeVideo = NSButton(checkboxWithTitle: "Decode locally for comparison (requires encoding)", target: nil, action: nil)
    private var lastEncoderReport: [String: Any]?
    private var lastDecoderReport: [String: Any]?
    private var decodedWindow: NSWindow?
    private let decodedImage = ComparisonImageView()
    private let colorPolicy = NSButton(checkboxWithTitle: "Use explicit SDR sRGB colors", target: nil, action: nil)
    private let streamWidth = NSPopUpButton(frame: .zero, pullsDown: false)
    private let sourceSelector = NSSegmentedControl(labels: ["Decoded", "Raw source"], trackingMode: .selectOne, target: nil, action: nil)
    private let nativePixels = NSButton(checkboxWithTitle: "1:1 physical pixels", target: nil, action: nil)
    private let freezeComparison = NSButton(checkboxWithTitle: "Freeze matched pair", target: nil, action: nil)
    private var comparisonState: DecoderSnapshot?
    private var lastViewedComparison: [String: Any]?
    private var timer: Timer?
    private var pending: Task<Void, Never>?
    private var busy = false
    private var journal: [[String: String]] = []
    private var previousStatus = ""
    private var lastJournalTime: TimeInterval = 0

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1000, height: 900),
                          styleMask: [.titled, .closable, .resizable, .miniaturizable], backing: .buffered, defer: false)
        window.title = "NoComputer — macOS feasibility diagnostics"
        window.center()
        window.isReleasedWhenClosed = false
        let root = NSStackView()
        root.orientation = .vertical
        root.alignment = .leading
        root.spacing = 8
        root.edgeInsets = NSEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)
        window.contentView = root
        root.addArrangedSubview(NSTextField(wrappingLabelWithString:
            "Diagnostics run locally; only the opt-in Remote access below uses the network, through the bundled nocomputer-host. No saved video. Lock-screen capture and basic input demonstrated on this Mac; remote unlock is not yet tested. Export contains metadata only."))
        root.addArrangedSubview(permissionLabel)
        addRow(to: root, buttons: [
            ("Request Screen Recording", #selector(requestScreen)),
            ("Request Accessibility", #selector(requestAccessibility)),
            ("Refresh permissions", #selector(refreshPermissions))
        ])
        addRow(to: root, buttons: [
            ("Start capture", #selector(startCapture)),
            ("Start in 15 seconds", #selector(scheduleCapture)),
            ("Stop / cancel", #selector(stopCapture)),
            ("Export metadata…", #selector(exportMetadata))
        ])
        keepAwake.target = self
        keepAwake.action = #selector(toggleAwake)
        root.addArrangedSubview(keepAwake)
        root.addArrangedSubview(encodeVideo)
        root.addArrangedSubview(decodeVideo)
        root.addArrangedSubview(colorPolicy)
        streamWidth.addItems(withTitles: ["Stream width: up to 1600 px (balanced)", "Stream width: up to 2560 px (text detail)"])
        root.addArrangedSubview(streamWidth)
        root.addArrangedSubview(NSTextField(wrappingLabelWithString:
            "Encoding, decoding, color, and stream-width changes apply after Stop/Start. Preview stays in memory; no video is saved."))
        addRow(to: root, buttons: [("Open decoded preview", #selector(showDecodedPreview))])
        addRow(to: root, buttons: [
            ("Display off in 15 seconds", #selector(scheduleDisplayOff)),
            ("Cancel display test", #selector(cancelDisplayTest))
        ])
        addRow(to: root, buttons: [
            ("Wake + capture in 60 seconds", #selector(scheduleWakeProbe)),
            ("Review before input", #selector(reviewWakeFrame)),
            ("Review after input", #selector(reviewInputFrame))
        ])
        root.addArrangedSubview(NSTextField(wrappingLabelWithString:
            "Wake test: lock and let the display turn off. After automatic wake, stay locked for 20 seconds. Before/after frames stay in memory for review; export contains no pixels."))
        remoteAccess.target = self
        remoteAccess.action = #selector(toggleRemoteAccess)
        root.addArrangedSubview(remoteAccess)
        root.addArrangedSubview(remoteStatus)
        root.addArrangedSubview(statusLabel)
        preview.imageScaling = .scaleProportionallyUpOrDown
        preview.wantsLayer = true
        preview.layer?.backgroundColor = NSColor.black.cgColor
        preview.heightAnchor.constraint(equalToConstant: 60).isActive = true
        root.addArrangedSubview(preview)
        root.addArrangedSubview(NSTextField(wrappingLabelWithString:
            "Input test targets only the field below while this app is frontmost. It moves/clicks the pointer here, posts a/A, a right-click and a scroll. Do not switch apps during the test."))
        inputTarget.isRichText = false
        inputTarget.font = .monospacedSystemFont(ofSize: 14, weight: .regular)
        inputTarget.string = "Input test target\n"
        let targetScroll = NSScrollView()
        targetScroll.hasVerticalScroller = true
        targetScroll.documentView = inputTarget
        targetScroll.heightAnchor.constraint(equalToConstant: 65).isActive = true
        root.addArrangedSubview(targetScroll)
        addRow(to: root, buttons: [("Run local input test", #selector(inputTest))])
        logView.isEditable = false
        logView.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        let logScroll = NSScrollView()
        logScroll.documentView = logView
        logScroll.hasVerticalScroller = true
        logScroll.heightAnchor.constraint(greaterThanOrEqualToConstant: 100).isActive = true
        root.addArrangedSubview(logScroll)
        for view in root.arrangedSubviews {
            view.widthAnchor.constraint(equalTo: root.widthAnchor, constant: -32).isActive = true
        }
        let menu = NSMenu()
        let item = NSMenuItem()
        menu.addItem(item)
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "Quit NoComputer Diagnostics", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        item.submenu = appMenu
        let diagnosticsItem = NSMenuItem()
        menu.addItem(diagnosticsItem)
        let diagnosticsMenu = NSMenu(title: "Diagnostics")
        for (title, action) in [
            ("Keep display awake for 2 minutes", #selector(holdDisplayAwake)),
            ("Whole-display metadata capture", #selector(startWholeDisplayCapture)),
            ("Schedule legacy lock-input probe…", #selector(scheduleLockInputProbe))
        ] {
            let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
            item.target = self
            diagnosticsMenu.addItem(item)
        }
        diagnosticsItem.submenu = diagnosticsMenu
        NSApplication.shared.mainMenu = menu
        window.makeKeyAndOrderFront(nil)
        NSApplication.shared.activate(ignoringOtherApps: true)
        refreshPermissions()
        record("App started; \(ProcessInfo.processInfo.operatingSystemVersionString)")
        let center = NSWorkspace.shared.notificationCenter
        for (name, message) in [
            (NSWorkspace.screensDidSleepNotification, "Workspace notification: displays slept"),
            (NSWorkspace.screensDidWakeNotification, "Workspace notification: displays woke"),
            (NSWorkspace.willSleepNotification, "Workspace notification: system will sleep"),
            (NSWorkspace.didWakeNotification, "Workspace notification: system woke")
        ] {
            workspaceObservers.append(center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    self?.record(message)
                }
            })
        }
        timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.expireDisplayAssertion()
                self?.refreshCapture()
                if let host = self?.remoteHost { self?.remoteStatus.stringValue = host.statusLine() }
            }
        }
    }

    private func addRow(to root: NSStackView, buttons: [(String, Selector)]) {
        let row = NSStackView()
        row.orientation = .horizontal
        for (title, action) in buttons { row.addArrangedSubview(NSButton(title: title, target: self, action: action)) }
        root.addArrangedSubview(row)
    }

    private func record(_ message: String) {
        let entry = ["uptimeSeconds": String(format: "%.3f", ProcessInfo.processInfo.systemUptime), "event": message]
        journal.append(entry)
        if journal.count > 600 { journal.removeFirst(journal.count - 600) }
        logView.string = journal.suffix(60).map { "\($0["uptimeSeconds"]!)  \($0["event"]!)" }.joined(separator: "\n")
        logView.scrollToEndOfDocument(nil)
    }

    @objc private func refreshPermissions() {
        permissionLabel.stringValue = "Screen Recording: \(CGPreflightScreenCaptureAccess() ? "granted" : "not granted")    Accessibility: \(AXIsProcessTrusted() ? "granted" : "not granted")"
    }

    @objc private func requestScreen() {
        _ = CGRequestScreenCaptureAccess()
        refreshPermissions()
        record("Screen Recording requested; relaunch may be required after granting")
    }

    @objc private func requestAccessibility() {
        let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        _ = AXIsProcessTrustedWithOptions(options)
        refreshPermissions()
        record("Accessibility requested")
    }

    @objc private func startCapture() {
        beginCapture(delay: 0)
    }
    @objc private func scheduleCapture() {
        beginCapture(delay: 15)
    }

    @objc private func startWholeDisplayCapture() {
        guard encodeVideo.state == .off, decodeVideo.state == .off else {
            record("Whole-display metadata capture needs encoding and decoding OFF")
            return
        }
        beginCapture(delay: 0, mode: .wholeDisplay)
    }

    private func beginCapture(delay: UInt64, mode: CaptureSession.FilterMode? = nil) {
        guard !busy, pending == nil, wakeProbeTask == nil else { return }
        capture.retireStoppedStream()
        guard capture.stream == nil else {
            record("Capture is already active; stop it before starting another stream")
            return
        }
        previousStatus = ""
        lastJournalTime = 0
        let selectedMode = mode ?? .excludeApplication
        let selectedEncoding = encodeVideo.state == .on
        let selectedDecoding = decodeVideo.state == .on
        let selectedColor: VideoColorPolicy = colorPolicy.state == .on ? .sRGB : .systemDefault
        let selectedWidth = streamWidth.indexOfSelectedItem == 1 ? 2560 : 1600
        guard !selectedDecoding || (selectedEncoding && selectedMode != .wholeDisplay) else {
            record("Local decoded preview needs encoder ON and whole-display OFF")
            return
        }
        lastEncoderReport = nil
        lastDecoderReport = nil
        decodedImage.image = nil
        comparisonState = nil
        lastViewedComparison = nil
        freezeComparison.state = .off
        record(delay == 0 ? "Starting capture; filter=\(selectedMode.rawValue)" : "Capture scheduled in \(delay) seconds; filter=\(selectedMode.rawValue)")
        pending = Task { [weak self] in
            guard let self else { return }
            defer { self.pending = nil }
            do {
                if delay > 0 { try await Task.sleep(nanoseconds: delay * 1_000_000_000) }
                try Task.checkCancellation()
                self.busy = true
                defer { self.busy = false }
                try await self.capture.start(mode: selectedMode, encodeVideo: selectedEncoding,
                    decodePreview: selectedDecoding, colorPolicy: selectedColor, maximumWidth: selectedWidth)
                self.record("Capture started: \(self.capture.displayDescription)")
                if selectedDecoding { self.showDecodedPreview() }
            } catch is CancellationError {
                self.record("Scheduled capture cancelled")
            } catch {
                let failure = error as NSError
                self.record("Capture failed: \(failure.localizedDescription); domain=\(failure.domain); code=\(failure.code)")
            }
        }
    }

    @objc private func stopCapture() {
        if wakeProbeTask != nil {
            wakeProbeTask?.cancel()
            record("Wake probe cancellation requested; cleanup follows any in-flight capture operation")
            return
        }
        // In-flight start cannot be interrupted safely. Stop is available once
        // it completes; a scheduled delay can be cancelled immediately.
        guard !busy else { record("Capture operation in progress; retry stop when it completes"); return }
        if pending != nil { pending?.cancel(); return }
        busy = true
        Task {
            defer { busy = false }
            do {
                let encoder = capture.output?.encoder
                try await capture.stop()
                if let encoder {
                    lastEncoderReport = encoder.snapshot().metadata
                    record("Final encoder statistics: \(encoder.snapshot().summary)")
                    if let decoder = encoder.decoder {
                        lastDecoderReport = decoder.snapshot().metadata
                        record("Final decoder statistics: \(decoder.snapshot().summary)")
                    }
                }
                preview.image = nil
                decodedImage.image = nil
                comparisonState = nil
                freezeComparison.state = .off
                statusLabel.stringValue = "Capture stopped"
                record("Capture stopped")
            } catch { record("Stop failed: \(error.localizedDescription)") }
        }
    }

    private func refreshCapture() {
        guard let output = capture.output else { return }
        let state = output.snapshot()
        let encoderState = output.encoder?.snapshot()
        let decoderState = output.encoder?.decoder?.snapshot()
        if let encoderState { lastEncoderReport = encoderState.metadata }
        if let decoderState { lastDecoderReport = decoderState.metadata }
        if !busy, capture.retireStoppedStream() {
            record("System ended capture; stopped stream released. Start capture can retry after unlocking.")
        }
        let now = ProcessInfo.processInfo.systemUptime
        let age = state.lastCallback.map { String(format: "%.1f s", now - $0) } ?? "none"
        let completeAge = state.lastComplete.map { String(format: "%.1f s", now - $0) } ?? "none"
        statusLabel.stringValue = "\(state.status) | callbacks \(state.callbacks) | complete \(state.completeFrames) | last callback \(age) | last complete \(completeAge)\n\(capture.displayDescription)"
        if capture.filterMode == .wholeDisplay {
            statusLabel.stringValue += "\nPreview disabled for the whole-display experiment; judge frame counts and errors."
        }
        if let encoderState {
            statusLabel.stringValue += "\n\(encoderState.summary)"
            statusLabel.stringValue += decoderState == nil ? "\nPreview disabled; compressed output discarded." : "\n\(decoderState!.summary)"
        }
        if decoderState?.image != nil, decoderState?.error == nil, encoderState?.error == nil,
           state.sourceVisible, state.stoppedAt == nil,
           let callback = state.lastCallback, now >= callback, now - callback < 2 {
            if freezeComparison.state == .off || comparisonState == nil { comparisonState = decoderState }
            updateComparison()
        } else {
            decodedImage.image = nil
            comparisonState = nil
            freezeComparison.state = .off
        }
        // Never present an old preview as live after capture stalls.
        if let image = state.image, state.previewIsCurrent(at: now) {
            preview.image = NSImage(cgImage: image, size: .zero)
        } else { preview.image = nil }
        let signature = state.status + (state.error ?? "")
        if signature != previousStatus || now - lastJournalTime >= 5 {
            record("\(state.status); callbacks=\(state.callbacks); complete=\(state.completeFrames); callbackAge=\(age); completeAge=\(completeAge); pts=\(state.presentationTime.map(String.init(describing:)) ?? "none")\(state.error.map { "; error=\($0)" } ?? "")\(state.errorDomain.map { "; domain=\($0)" } ?? "")\(state.errorCode.map { "; code=\($0)" } ?? "")\(state.stoppedAt.map { "; stoppedAt=\($0)" } ?? "")")
            if let encoderState { record(encoderState.summary) }
            if let decoderState { record(decoderState.summary) }
            previousStatus = signature
            lastJournalTime = now
        }
    }

    @objc private func showDecodedPreview() {
        guard capture.output?.encoder?.decoder != nil else {
            record("Start capture with encoder and local decoding enabled first")
            return
        }
        if decodedWindow == nil {
            let panel = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1100, height: 720),
                styleMask: [.titled, .closable, .resizable, .miniaturizable], backing: .buffered, defer: false)
            panel.title = "Raw / decoded H.264 — matched frames, 2 Hz diagnostic"
            panel.isReleasedWhenClosed = false
            panel.collectionBehavior = [.fullScreenPrimary]
            let root = NSStackView()
            root.orientation = .vertical
            root.spacing = 8
            let controls = NSStackView()
            sourceSelector.selectedSegment = 0
            sourceSelector.target = self
            sourceSelector.action = #selector(updateComparison)
            nativePixels.target = self
            nativePixels.action = #selector(updateComparison)
            for view in [sourceSelector, nativePixels, freezeComparison] as [NSView] { controls.addArrangedSubview(view) }
            root.addArrangedSubview(controls)
            let scroll = ComparisonScrollView()
            scroll.hasHorizontalScroller = true
            scroll.hasVerticalScroller = true
            scroll.autohidesScrollers = true
            scroll.documentView = decodedImage
            root.addArrangedSubview(scroll)
            scroll.widthAnchor.constraint(equalTo: root.widthAnchor).isActive = true
            scroll.heightAnchor.constraint(greaterThanOrEqualToConstant: 100).isActive = true
            panel.contentView = root
            panel.center()
            decodedWindow = panel
        }
        decodedWindow?.makeKeyAndOrderFront(nil)
    }

    @objc private func updateComparison() {
        decodedImage.nativePixels = nativePixels.state == .on
        decodedImage.image = sourceSelector.selectedSegment == 1 ? comparisonState?.sourceImage : comparisonState?.image
        if let comparisonState {
            var report = comparisonState.comparisonMetadata
            report["frozen"] = freezeComparison.state == .on
            report["nativePixels"] = nativePixels.state == .on
            report["source"] = sourceSelector.selectedSegment == 1 ? "raw source" : "decoded"
            report["backingScale"] = decodedWindow?.backingScaleFactor ?? 1
            lastViewedComparison = report
        }
    }

    @objc private func toggleRemoteAccess() {
        if remoteAccess.state == .on {
            let host = remoteHost ?? RemoteHost(window: window)
            host.onLog = { [weak self] in self?.record($0) }
            host.onExit = { [weak self] in
                self?.remoteAccess.state = .off
                self?.remoteStatus.stringValue = "Remote access off"
            }
            remoteHost = host
            do { try host.enable() } catch {
                record("Remote access failed: \(error.localizedDescription)")
                remoteAccess.state = .off
            }
        } else {
            remoteHost?.disable()
        }
    }

    @objc private func toggleAwake() {
        do {
            if keepAwake.state == .on { try sleep.enable() }
            else {
                let result = sleep.disable()
                if result != 0 { record("Sleep assertion release failed: \(result)") }
            }
        } catch { record(error.localizedDescription) }
        keepAwake.state = sleep.active ? .on : .off
        record("System keep-awake \(sleep.active ? "enabled" : "disabled"); separate display keep-awake is \(displaySleep.active ? "enabled" : "disabled")")
    }

    @objc private func holdDisplayAwake() {
        do {
            try displaySleep.enable()
            displayDeadline = ProcessInfo.processInfo.systemUptime + 120
            record("Display keep-awake enabled for 120 seconds; use Cancel display test to release early")
        } catch { record(error.localizedDescription) }
    }

    private func releaseDisplayAssertion() {
        displayWake.release()
        let result = displaySleep.disable()
        if result == 0 {
            displayDeadline = nil
            record("Display keep-awake released")
        } else { record("Display assertion release failed: \(result)") }
    }

    private func expireDisplayAssertion() {
        guard let deadline = displayDeadline,
              ProcessInfo.processInfo.systemUptime >= deadline else { return }
        releaseDisplayAssertion()
    }

    @objc private func scheduleDisplayOff() {
        guard displayOffTask == nil, wakeProbeTask == nil else { return }
        guard !displaySleep.active else {
            record("Disable display keep-awake before the display-off test")
            return
        }
        guard sleep.active else {
            record("Enable system keep-awake before the display-off test to isolate display sleep")
            return
        }
        record("Display-off test scheduled in 15 seconds; stay unlocked or lock manually for the chosen test")
        displayOffTask = Task { [weak self] in
            guard let self else { return }
            defer { self.displayOffTask = nil }
            do {
                try await Task.sleep(nanoseconds: 15_000_000_000)
                try Task.checkCancellation()
                guard self.sleep.active, !self.displaySleep.active else {
                    self.record("Display-off test cancelled: assertion settings changed")
                    return
                }
                let process = Process()
                process.executableURL = URL(fileURLWithPath: "/usr/bin/pmset")
                process.arguments = ["displaysleepnow"]
                process.standardOutput = FileHandle.nullDevice
                process.standardError = FileHandle.nullDevice
                process.terminationHandler = { [weak self] completed in
                    let code = completed.terminationStatus
                    Task { @MainActor in self?.record("Display-off command exited with status \(code); check workspace notifications and physical display") }
                }
                try process.run()
                self.record("Display-off command issued; no persistent power settings changed")
            } catch is CancellationError {
                self.record("Scheduled display-off test cancelled")
            } catch { self.record("Display-off command failed: \(error.localizedDescription)") }
        }
    }

    @objc private func cancelDisplayTest() {
        displayOffTask?.cancel()
        wakeProbeTask?.cancel()
        displayWake.release()
        if displaySleep.active { releaseDisplayAssertion() }
    }

    @objc private func scheduleWakeProbe() {
        beginWakeProbe(testInput: false)
    }

    @objc private func scheduleLockInputProbe() {
        beginWakeProbe(testInput: true)
    }

    private func beginWakeProbe(testInput: Bool) {
        capture.retireStoppedStream()
        guard wakeProbeTask == nil, pending == nil, !busy, capture.stream == nil,
              displayOffTask == nil else {
            record("Stop capture and cancel scheduled tests before starting the wake probe")
            return
        }
        guard sleep.active, !displaySleep.active, encodeVideo.state == .off, decodeVideo.state == .off else {
            record("Wake probe needs system keep-awake ON; display keep-awake inactive, encoder and local decoding OFF")
            return
        }
        if testInput {
            guard AXIsProcessTrusted() else {
                refreshPermissions()
                record("Lock input probe refused: Accessibility is not granted to this build")
                return
            }
            let alert = NSAlert()
            alert.messageText = "Schedule a test character and Backspace?"
            alert.informativeText = "Lock immediately after scheduling. Stay locked for 20 seconds after automatic wake and do not type a password until then. The app will move the pointer, type one fixed nonsecret character, retain a frame, then press Backspace. It never presses Return or submits a login attempt. Cancel with Stop / cancel."
            alert.addButton(withTitle: "Schedule test")
            alert.addButton(withTitle: "Cancel")
            guard alert.runModal() == .alertFirstButtonReturn else { return }
        }
        wakeProbeFrame = nil
        wakeProbeFrameTime = nil
        inputProbeFrame = nil
        inputProbeFrameTime = nil
        lockInputPosted = false
        probeErasurePosted = false
        reviewWindow?.close()
        record("Wake probe scheduled in 60 seconds; input probe=\(testInput). Lock now; remain locked for 20 seconds after automatic wake. Review images stay in memory only.")
        wakeProbeTask = Task { [weak self] in
            guard let self else { return }
            defer { self.busy = false; self.wakeProbeTask = nil }
            do {
                try await Task.sleep(nanoseconds: 60_000_000_000)
                try Task.checkCancellation()
                guard self.sleep.active, !self.displaySleep.active else {
                    self.record("Wake probe cancelled: power assertion settings changed")
                    return
                }
                let wasAsleep = CGDisplayIsAsleep(CGMainDisplayID()) != 0
                self.record("Wake probe firing; main display asleep=\(wasAsleep)")
                try self.displayWake.request()
                self.record("Public display-wake request succeeded (local activity); this does not prove physical wake or unlock")
                try self.displaySleep.enable()
                self.displayDeadline = ProcessInfo.processInfo.systemUptime + 120
                self.record("Wake probe holds display keep-awake for 120 seconds")
                try await Task.sleep(nanoseconds: 2_000_000_000)
                try Task.checkCancellation()
                self.record("Wake probe display state before capture: asleep=\(CGDisplayIsAsleep(CGMainDisplayID()) != 0)")
                self.busy = true
                try await self.capture.start(mode: .excludeApplication)
                self.busy = false
                try Task.checkCancellation()
                self.previousStatus = ""
                self.record("Wake probe capture started: \(self.capture.displayDescription)")
                try await Task.sleep(nanoseconds: 5_000_000_000)
                try Task.checkCancellation()
                let now = ProcessInfo.processInfo.systemUptime
                if let state = self.capture.output?.snapshot(),
                   let image = state.image, state.previewIsCurrent(at: now) {
                    self.wakeProbeFrame = image
                    self.wakeProbeFrameTime = now
                    self.record("Wake probe retained one recent frame in memory at uptime \(now). Review after unlocking; frame contents are not logged or exported.")
                } else {
                    self.record("Wake probe could not retain a recent frame; inspect capture error/status")
                }
                if testInput {
                    try Task.checkCancellation()
                    guard self.wakeProbeFrame != nil, AXIsProcessTrusted() else {
                        self.record("Lock input probe skipped: no before frame or effective Accessibility permission")
                        return
                    }
                    // Each key pair posts synchronously. Remember erasure before
                    // insertion so cancellation/quit also posts Backspace.
                    let events = try InputProbe.events(displayBounds: CGDisplayBounds(CGMainDisplayID()))
                    self.pendingProbeErasure = events.erasure
                    defer { self.eraseProbeMarker() }
                    for event in events.insertion { event.post(tap: .cghidEventTap) }
                    self.lockInputPosted = true
                    self.record("Lock input probe posted pointer movement and one fixed nonsecret character down/up; no Return. Posting does not prove delivery.")
                    try await Task.sleep(nanoseconds: 3_000_000_000)
                    try Task.checkCancellation()
                    let afterTime = ProcessInfo.processInfo.systemUptime
                    if let state = self.capture.output?.snapshot(), let image = state.image,
                       state.previewIsCurrent(at: afterTime) {
                        self.inputProbeFrame = image
                        self.inputProbeFrameTime = afterTime
                        self.record("Lock input probe retained after-input frame in memory at uptime \(afterTime)")
                    } else { self.record("Lock input probe could not retain after-input frame") }
                }
            } catch {
                self.busy = true
                do { try await self.capture.stop() }
                catch { self.record("Wake probe cleanup stop failed: \(error.localizedDescription)") }
                self.releaseDisplayAssertion()
                if error is CancellationError { self.record("Wake probe cancelled and cleaned up") }
                else {
                    let failure = error as NSError
                    self.record("Wake probe failed: \(failure.localizedDescription); domain=\(failure.domain); code=\(failure.code)")
                }
            }
        }
    }

    @objc private func reviewWakeFrame() {
        guard let image = wakeProbeFrame else {
            record("No wake-test frame retained yet")
            return
        }
        showReview(image: image, title: "Before input — wake-test frame (in memory only)")
    }

    private func eraseProbeMarker() {
        guard let events = pendingProbeErasure else { return }
        pendingProbeErasure = nil
        for event in events { event.post(tap: .cghidEventTap) }
        probeErasurePosted = true
        record("Lock input probe posted Backspace down/up for marker cleanup; no login attempt submitted")
    }

    @objc private func reviewInputFrame() {
        guard let image = inputProbeFrame else {
            record("No after-input frame retained yet")
            return
        }
        showReview(image: image, title: "After test character, before Backspace — in memory only")
    }

    private func showReview(image: CGImage, title: String) {
        reviewWindow?.close()
        let review = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 900, height: 600),
                              styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        review.title = title
        review.isReleasedWhenClosed = false
        let imageView = NSImageView()
        imageView.imageScaling = .scaleProportionallyUpOrDown
        imageView.image = NSImage(cgImage: image, size: .zero)
        review.contentView = imageView
        review.center()
        review.makeKeyAndOrderFront(nil)
        reviewWindow = review
    }

    @objc private func inputTest() {
        guard AXIsProcessTrusted(), window.isKeyWindow,
              NSWorkspace.shared.frontmostApplication?.processIdentifier == ProcessInfo.processInfo.processIdentifier else {
            record("Input test refused: grant Accessibility and keep this window frontmost")
            return
        }
        window.makeFirstResponder(inputTarget)
        inputTarget.string = ""
        let local = inputTarget.convert(NSPoint(x: 30, y: 25), to: nil)
        let cocoaPoint = window.convertPoint(toScreen: local)
        // Cocoa is bottom-up; Quartz global coordinates are top-down from the
        // primary screen. Do not use NSScreen.main (it may be another display).
        let mainHeight = NSScreen.screens.first?.frame.height ?? 0
        let point = CGPoint(x: cocoaPoint.x, y: mainHeight - cocoaPoint.y)
        let source = CGEventSource(stateID: .hidSystemState)
        func mouse(_ type: CGEventType, _ button: CGMouseButton) {
            CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: point, mouseButton: button)?.post(tap: .cghidEventTap)
        }
        mouse(.mouseMoved, .left)
        mouse(.leftMouseDown, .left)
        mouse(.leftMouseUp, .left)
        for flags: CGEventFlags in [[], .maskShift] {
            for down in [true, false] {
                let event = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: down)
                event?.flags = flags
                event?.post(tap: .cghidEventTap)
            }
        }
        CGEvent(scrollWheelEvent2Source: source, units: .line, wheelCount: 1, wheel1: -1, wheel2: 0, wheel3: 0)?.post(tap: .cghidEventTap)
        mouse(.rightMouseDown, .right)
        mouse(.rightMouseUp, .right)
        record("Input events posted; inspect field for aA and pointer/context-menu behavior. Posting is not proof of delivery.")
    }

    @objc private func exportMetadata() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "diagnostics-\(Int(Date().timeIntervalSince1970)).json"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            let report: [String: Any] = [
                "schemaVersion": 12,
                "lastViewedComparison": lastViewedComparison ?? ["available": false],
                "comparisonNativePixels": nativePixels.state == .on,
                "comparisonFrozen": freezeComparison.state == .on,
                "comparisonSource": sourceSelector.selectedSegment == 1 ? "raw source" : "decoded",
                "comparisonPTS": comparisonState?.comparisonPTS.map { String($0) } ?? "none",
                "comparisonBackingScale": decodedWindow?.backingScaleFactor ?? 1,
                "encoder": capture.output?.encoder?.snapshot().metadata ?? lastEncoderReport ?? ["enabled": false],
                "decoder": capture.output?.encoder?.decoder?.snapshot().metadata ?? lastDecoderReport ?? ["enabled": false],
                "os": ProcessInfo.processInfo.operatingSystemVersionString,
                "bundleIdentifier": Bundle.main.bundleIdentifier ?? "unbundled",
                "screenRecordingGranted": CGPreflightScreenCaptureAccess(),
                "accessibilityGranted": AXIsProcessTrusted(),
                "keepAwakeActive": sleep.active,
                "displayKeepAwakeActive": displaySleep.active,
                "captureFilter": capture.filterMode.rawValue,
                "wakeProbeFrameRetained": wakeProbeFrame != nil,
                "wakeProbeFrameRetainedAt": wakeProbeFrameTime.map { String($0) } ?? "none",
                "lockInputPosted": lockInputPosted,
                "probeErasurePosted": probeErasurePosted,
                "afterInputFrameRetained": inputProbeFrame != nil,
                "afterInputFrameRetainedAt": inputProbeFrameTime.map { String($0) } ?? "none",
                "display": capture.displayDescription,
                "events": journal
            ]
            try JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted, .sortedKeys]).write(to: url, options: .atomic)
            record("Metadata exported; no images, typed text, or clipboard included")
        } catch { record("Export failed: \(error.localizedDescription)") }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationWillTerminate(_ notification: Notification) {
        capture.output?.encoder?.close()
        remoteHost?.disable()
        eraseProbeMarker()
        pending?.cancel()
        displayOffTask?.cancel()
        wakeProbeTask?.cancel()
        timer?.invalidate()
        for observer in workspaceObservers { NSWorkspace.shared.notificationCenter.removeObserver(observer) }
        displaySleep.disable()
        displayWake.release()
        sleep.disable()
    }
}
