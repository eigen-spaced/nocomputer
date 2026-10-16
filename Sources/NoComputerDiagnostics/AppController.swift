import AppKit
import ApplicationServices

@MainActor
final class AppController: NSObject, NSApplicationDelegate {
    private var remoteHost: RemoteHost?
    private var window: NSWindow!
    private var panel: PanelView!
    // Everything the panel shows; it is sent whole whenever it changes.
    private var state = Panel.State() {
        didSet { if state != oldValue { panel.send(state.message) } }
    }
    private var status = "Remote access off" {
        didSet { if status != oldValue { panel.send(Panel.status(status)) } }
    }
    private var log: [String] = []
    private var timer: Timer?

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 760, height: 620),
                          styleMask: [.titled, .closable, .resizable, .miniaturizable], backing: .buffered, defer: false)
        window.title = "NoComputer"
        window.center()
        window.isReleasedWhenClosed = false
        let resources = Bundle.main.bundleURL.appendingPathComponent("Contents/Resources/panel")
        panel = PanelView(resources: resources)
        panel.onAction = { [weak self] in self?.handle($0) }
        panel.onDropped = { [weak self] type in self?.record("Ignored panel message \(type ?? "without a type")") }
        window.contentView = panel.webView
        let menu = NSMenu()
        let appItem = NSMenuItem()
        menu.addItem(appItem)
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "Quit NoComputer", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        // Copy and Select All reach the web view through the responder chain.
        let editItem = NSMenuItem()
        menu.addItem(editItem)
        let editMenu = NSMenu(title: "Edit")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = editMenu
        NSApplication.shared.mainMenu = menu
        window.makeKeyAndOrderFront(nil)
        NSApplication.shared.activate(ignoringOtherApps: true)
        refreshPermissions()
        timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.status = self.remoteHost.flatMap { $0.running ? $0.statusLine() : nil } ?? "Remote access off"
            }
        }
    }

    // A grant made in System Settings shows up when the user comes back.
    func applicationDidBecomeActive(_ notification: Notification) {
        if panel != nil { refreshPermissions() }
    }

    private func handle(_ action: Panel.Action) {
        switch action {
        case .ready:
            // A fresh or reloaded page: replay everything it shows.
            panel.send(state.message)
            panel.send(Panel.status(status))
            for line in log { panel.send(Panel.log(line)) }
        case .setRemoteAccess(let on):
            setRemoteAccess(on)
        case .setClipboard(let on):
            state.clipboardOn = on
            remoteHost?.clipboardEnabled = on
        case .requestPermission(.screen):
            _ = CGRequestScreenCaptureAccess()
            refreshPermissions()
            record("Screen Recording requested; relaunch may be required after granting")
        case .requestPermission(.accessibility):
            let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
            _ = AXIsProcessTrustedWithOptions(options)
            refreshPermissions()
            record("Accessibility requested")
        case .refreshPermissions:
            refreshPermissions()
        case .removeDevice(let fingerprint):
            guard let device = state.devices.first(where: { $0.deviceFingerprint == fingerprint && !$0.revoked }) else {
                record("Ignored panel message removeDevice for a device that isn't listed as active")
                return
            }
            remoteHost?.confirmRemove(device)
        case .allowPairingAgain(let fingerprint):
            guard let device = state.devices.first(where: { $0.deviceFingerprint == fingerprint && $0.revoked }) else {
                record("Ignored panel message allowPairingAgain for a device that isn't listed as removed")
                return
            }
            remoteHost?.confirmAllowPairingAgain(device)
        }
    }

    // In memory only, for the user to read; never written to disk.
    private func record(_ message: String) {
        let line = String(format: "%.3f  ", ProcessInfo.processInfo.systemUptime) + message
        log.append(line)
        if log.count > 200 { log.removeFirst(log.count - 200) }
        panel.send(Panel.log(line))
    }

    private func refreshPermissions() {
        state.permissions = [
            .init(id: .screen, label: "Screen Recording", granted: CGPreflightScreenCaptureAccess()),
            .init(id: .accessibility, label: "Accessibility", granted: AXIsProcessTrusted()),
        ]
    }

    private func setRemoteAccess(_ on: Bool) {
        guard !state.remoteBusy else { return }
        if on {
            guard remoteHost?.running != true else { return }
            let host = remoteHost ?? RemoteHost(window: window)
            host.onLog = { [weak self] in self?.record($0) }
            host.onIdentity = { [weak self] in self?.state.host = $0 }
            host.onPairingNotice = { [weak self] in self?.state.pairingNotice = $0 }
            host.onSession = { [weak self] in self?.state.session = $0 }
            host.onPairedDevices = { [weak self] in self?.state.devices = $0 }
            host.onExit = { [weak self] in
                self?.state.remoteOn = false
                self?.state.remoteBusy = false
            }
            host.clipboardEnabled = state.clipboardOn
            remoteHost = host
            do {
                try host.enable()
                state.remoteOn = true
                state.remoteError = nil
            } catch {
                let message = "Remote access failed: \(error.localizedDescription)"
                state.remoteError = message
                record(message)
            }
        } else if remoteHost?.running == true {
            // Off once the host has exited; onExit clears busy.
            state.remoteBusy = true
            remoteHost?.disable()
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationWillTerminate(_ notification: Notification) {
        remoteHost?.disable()
        timer?.invalidate()
    }
}
