import AppKit
import ApplicationServices

@main
enum NoComputerApplication {
    @MainActor
    static func main() {
        let app = NSApplication.shared
        let delegate = AppController()
        app.setActivationPolicy(.regular)
        app.delegate = delegate
        withExtendedLifetime(delegate) { app.run() }
    }
}
