import AppKit
import ApplicationServices

@main
enum DiagnosticsApplication {
    @MainActor
    static func main() {
        let app = NSApplication.shared
        let delegate = DiagnosticsController()
        app.setActivationPolicy(.regular)
        app.delegate = delegate
        withExtendedLifetime(delegate) { app.run() }
    }
}
