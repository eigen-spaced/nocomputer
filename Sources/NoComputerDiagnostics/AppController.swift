import AppKit
import ApplicationServices

@MainActor
final class AppController: NSObject, NSApplicationDelegate {
    private var remoteHost: RemoteHost?
    private var window: NSWindow!
    private let permissionLabel = NSTextField(wrappingLabelWithString: "")
    private let remoteAccess = NSButton(checkboxWithTitle: "Remote access (paired viewers only)", target: nil, action: nil)
    private let remoteStatus = NSTextField(wrappingLabelWithString: "Remote access off")
    private let logView = NSTextView()
    private var log: [String] = []
    private var timer: Timer?

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 760, height: 480),
                          styleMask: [.titled, .closable, .resizable, .miniaturizable], backing: .buffered, defer: false)
        window.title = "NoComputer"
        window.center()
        window.isReleasedWhenClosed = false
        let root = NSStackView()
        root.orientation = .vertical
        root.alignment = .leading
        root.spacing = 8
        root.edgeInsets = NSEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)
        window.contentView = root
        root.addArrangedSubview(NSTextField(wrappingLabelWithString:
            "Remote access runs the bundled nocomputer-host and accepts paired viewers only. It keeps the system awake while on. No video, typed input, or clipboard is saved or logged."))
        root.addArrangedSubview(permissionLabel)
        let row = NSStackView()
        row.orientation = .horizontal
        for (title, action) in [
            ("Request Screen Recording", #selector(requestScreen)),
            ("Request Accessibility", #selector(requestAccessibility)),
            ("Refresh permissions", #selector(refreshPermissions))
        ] { row.addArrangedSubview(NSButton(title: title, target: self, action: action)) }
        root.addArrangedSubview(row)
        remoteAccess.target = self
        remoteAccess.action = #selector(toggleRemoteAccess)
        root.addArrangedSubview(remoteAccess)
        root.addArrangedSubview(remoteStatus)
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
        appMenu.addItem(withTitle: "Quit NoComputer", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        item.submenu = appMenu
        NSApplication.shared.mainMenu = menu
        window.makeKeyAndOrderFront(nil)
        NSApplication.shared.activate(ignoringOtherApps: true)
        refreshPermissions()
        timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                if let host = self?.remoteHost { self?.remoteStatus.stringValue = host.statusLine() }
            }
        }
    }

    // In memory only, for the user to read; never written to disk.
    private func record(_ message: String) {
        log.append(String(format: "%.3f  ", ProcessInfo.processInfo.systemUptime) + message)
        if log.count > 200 { log.removeFirst(log.count - 200) }
        logView.string = log.joined(separator: "\n")
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

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationWillTerminate(_ notification: Notification) {
        remoteHost?.disable()
        timer?.invalidate()
    }
}
