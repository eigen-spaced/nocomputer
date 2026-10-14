import AppKit
import WebKit

// The host window's content: the bundled page in panel/, contract in
// docs/host-panel.md. The page renders what it is sent and reports clicks; every
// decision stays here.
enum Panel {
    static let version = 1
    static let scheme = "nocomputer-panel"
    static let indexURL = URL(string: "\(scheme)://panel/index.html")!
    static let maximumActionBytes = 4096
    static let files: [String: String] = [
        "/index.html": "text/html", "/panel.js": "text/javascript", "/panel.css": "text/css",
    ]

    enum Permission: String { case screen, accessibility }

    enum Action: Equatable {
        case ready
        case setRemoteAccess(Bool)
        case setClipboard(Bool)
        case requestPermission(Permission)
        case refreshPermissions
    }

    enum Parsed: Equatable {
        case action(Action)
        // Logged by type only, clamped; nil when there was no usable type.
        case dropped(type: String?)
    }

    // A message body as WebKit delivered it (already converted from JS).
    static func parse(_ body: Any) -> Parsed {
        guard let dict = body as? [String: Any], JSONSerialization.isValidJSONObject(dict),
              let json = try? JSONSerialization.data(withJSONObject: dict), json.count <= maximumActionBytes
        else { return .dropped(type: nil) }
        let type = (dict["type"] as? String).map { String($0.prefix(32)) }
        guard isNumber(dict["v"], equalTo: version), let type else { return .dropped(type: type) }
        switch type {
        case "ready": return .action(.ready)
        case "refreshPermissions": return .action(.refreshPermissions)
        case "setRemoteAccess", "setClipboard":
            guard let on = bool(dict["on"]) else { break }
            return .action(type == "setRemoteAccess" ? .setRemoteAccess(on) : .setClipboard(on))
        case "requestPermission":
            guard let id = dict["id"] as? String, let permission = Permission(rawValue: id) else { break }
            return .action(.requestPermission(permission))
        default: break
        }
        return .dropped(type: type)
    }

    // JS booleans arrive as CFBoolean; a number 1 must not count as true.
    private static func bool(_ value: Any?) -> Bool? {
        guard let number = value as? NSNumber, CFGetTypeID(number) == CFBooleanGetTypeID() else { return nil }
        return number.boolValue
    }

    private static func isNumber(_ value: Any?, equalTo expected: Int) -> Bool {
        guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else { return false }
        return number.doubleValue == Double(expected)
    }

    struct State: Equatable {
        struct PermissionState: Equatable {
            let id: Permission
            let label: String
            let granted: Bool
        }
        var host: LocalLink.Identity?
        var remoteOn = false
        var remoteBusy = false
        var remoteError: String?
        var permissions: [PermissionState] = []
        var session: LocalLink.Session?
        var pairingNotice: String?
        var clipboardOn = true

        // Contract field names, with explicit nulls.
        var message: [String: Any] {
            let null = NSNull()
            let state: [String: Any] = [
                "platform": "macos",
                "host": host.map { ["name": $0.name, "fingerprint": $0.hostFingerprint] } ?? null,
                "remoteAccess": ["on": remoteOn, "busy": remoteBusy, "error": remoteError ?? null] as [String: Any],
                "permissions": permissions.map {
                    ["id": $0.id.rawValue, "label": $0.label, "granted": $0.granted, "canRequest": !$0.granted] as [String: Any]
                },
                "session": session.map { ["device": $0.name, "deviceFingerprint": $0.deviceFingerprint] } ?? null,
                "pairingNotice": pairingNotice ?? null,
                "clipboard": ["on": clipboardOn],
            ]
            return ["v": version, "type": "state", "state": state]
        }
    }

    static func status(_ text: String) -> [String: Any] { ["v": version, "type": "status", "text": text] }
    static func log(_ line: String) -> [String: Any] { ["v": version, "type": "log", "line": line] }

    // The bundled file for a request, or nil: only the listed files, from the panel host.
    static func resource(for url: URL?, in root: URL) -> (file: URL, mimeType: String)? {
        guard let url, url.scheme == scheme, url.host == "panel", url.query == nil,
              let mimeType = files[url.path] else { return nil }
        return (root.appendingPathComponent(String(url.path.dropFirst())), mimeType)
    }

    static func isIndex(_ url: URL?) -> Bool { url?.absoluteString == indexURL.absoluteString }
}

// Serves the page from Contents/Resources/panel under its own scheme, so the page's
// CSP 'self' has a real origin and nothing outside those files is reachable.
final class PanelSchemeHandler: NSObject, WKURLSchemeHandler {
    private let root: URL

    init(root: URL) { self.root = root }

    func webView(_ webView: WKWebView, start task: WKURLSchemeTask) {
        guard let (file, mimeType) = Panel.resource(for: task.request.url, in: root),
              let data = try? Data(contentsOf: file), let url = task.request.url else {
            task.didFailWithError(URLError(.fileDoesNotExist))
            return
        }
        let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: "HTTP/1.1",
                                       headerFields: ["Content-Type": "\(mimeType); charset=utf-8", "Cache-Control": "no-store"])!
        task.didReceive(response)
        task.didReceive(data)
        task.didFinish()
    }

    func webView(_ webView: WKWebView, stop task: WKURLSchemeTask) {}
}

// Copy is the only context menu item: no Reload, Back or Inspect.
final class PanelWebView: WKWebView {
    override func willOpenMenu(_ menu: NSMenu, with event: NSEvent) {
        for item in menu.items where item.identifier?.rawValue != "WKMenuItemIdentifierCopy" {
            menu.removeItem(item)
        }
        super.willOpenMenu(menu, with: event)
    }
}

@MainActor
final class PanelView: NSObject, WKNavigationDelegate, WKScriptMessageHandler {
    let webView: PanelWebView
    var onAction: ((Panel.Action) -> Void)?
    var onDropped: ((String?) -> Void)?
    // Messages are sent only after the page said ready; ready gets a full replay.
    private var pageReady = false

    init(resources: URL) {
        let configuration = WKWebViewConfiguration()
        configuration.setURLSchemeHandler(PanelSchemeHandler(root: resources), forURLScheme: Panel.scheme)
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        configuration.websiteDataStore = .nonPersistent()
        webView = PanelWebView(frame: .zero, configuration: configuration)
        super.init()
        // The content controller holds its handler strongly; a weak hop avoids a cycle.
        configuration.userContentController.add(WeakHandler(self), name: "panel")
        webView.navigationDelegate = self
        webView.isInspectable = false
        webView.allowsBackForwardNavigationGestures = false
        webView.allowsMagnification = false
        webView.load(URLRequest(url: Panel.indexURL))
    }

    func send(_ message: [String: Any]) {
        guard pageReady else { return }
        webView.callAsyncJavaScript("window.panel.receive(msg)", arguments: ["msg": message], in: nil, in: .page) { _ in }
    }

    nonisolated func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        MainActor.assumeIsolated {
            let origin = message.frameInfo.securityOrigin
            guard message.webView === webView, message.frameInfo.isMainFrame,
                  origin.protocol == Panel.scheme, origin.host == "panel" else { return }
            switch Panel.parse(message.body) {
            case .action(let action):
                if action == .ready { pageReady = true }
                onAction?(action)
            case .dropped(let type):
                onDropped?(type)
            }
        }
    }

    // Only the page itself, in the main frame; links, redirects and frames are refused.
    nonisolated func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                             decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
        MainActor.assumeIsolated {
            let allowed = action.targetFrame?.isMainFrame == true && Panel.isIndex(action.request.url)
            decisionHandler(allowed ? .allow : .cancel)
        }
    }

    nonisolated func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        MainActor.assumeIsolated {
            pageReady = false
            webView.load(URLRequest(url: Panel.indexURL))
        }
    }

    private final class WeakHandler: NSObject, WKScriptMessageHandler {
        weak var target: PanelView?
        init(_ target: PanelView) { self.target = target }
        func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            target?.userContentController(controller, didReceive: message)
        }
    }
}
