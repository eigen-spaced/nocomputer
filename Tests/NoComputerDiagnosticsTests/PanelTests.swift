import Foundation
import Testing
@testable import NoComputerDiagnostics

struct PanelTests {
    @Test func acceptsEachContractAction() {
        #expect(Panel.parse(["v": 2, "type": "ready"]) == .action(.ready))
        #expect(Panel.parse(["v": 2, "type": "setRemoteAccess", "on": true]) == .action(.setRemoteAccess(true)))
        #expect(Panel.parse(["v": 2, "type": "setClipboard", "on": false]) == .action(.setClipboard(false)))
        #expect(Panel.parse(["v": 2, "type": "requestPermission", "id": "screen"]) == .action(.requestPermission(.screen)))
        #expect(Panel.parse(["v": 2, "type": "requestPermission", "id": "accessibility"]) == .action(.requestPermission(.accessibility)))
        #expect(Panel.parse(["v": 2, "type": "refreshPermissions"]) == .action(.refreshPermissions))
    }

    @Test func dropsOtherVersionsTypesAndFields() {
        #expect(Panel.parse(["v": 1, "type": "ready"]) == .dropped(type: "ready"))
        #expect(Panel.parse(["type": "ready"]) == .dropped(type: "ready"))
        #expect(Panel.parse(["v": true, "type": "ready"]) == .dropped(type: "ready"))
        #expect(Panel.parse(["v": 2, "type": "decidePairing", "approve": true]) == .dropped(type: "decidePairing"))
        #expect(Panel.parse(["v": 2, "type": "requestPermission", "id": "camera"]) == .dropped(type: "requestPermission"))
        // A number is not a boolean.
        #expect(Panel.parse(["v": 2, "type": "setRemoteAccess", "on": 1]) == .dropped(type: "setRemoteAccess"))
        #expect(Panel.parse(["v": 2, "type": "setRemoteAccess"]) == .dropped(type: "setRemoteAccess"))
        #expect(Panel.parse(["v": 1]) == .dropped(type: nil))
        #expect(Panel.parse("ready") == .dropped(type: nil))
        #expect(Panel.parse(["v": 2, "type": String(repeating: "x", count: 100)]) == .dropped(type: String(repeating: "x", count: 32)))
    }

    @Test func deviceActionsTakeExactlyAFingerprint() {
        let fp = "1111 2222"
        #expect(Panel.parse(["v": 2, "type": "removeDevice", "fingerprint": fp]) == .action(.removeDevice(fingerprint: fp)))
        #expect(Panel.parse(["v": 2, "type": "allowPairingAgain", "fingerprint": fp]) == .action(.allowPairingAgain(fingerprint: fp)))
        #expect(Panel.parse(["v": 2, "type": "removeDevice", "fingerprint": fp, "confirm": true]) == .dropped(type: "removeDevice"))
        #expect(Panel.parse(["v": 2, "type": "removeDevice"]) == .dropped(type: "removeDevice"))
        #expect(Panel.parse(["v": 2, "type": "removeDevice", "fingerprint": ""]) == .dropped(type: "removeDevice"))
        #expect(Panel.parse(["v": 2, "type": "removeDevice", "fingerprint": 7]) == .dropped(type: "removeDevice"))
        #expect(Panel.parse(["v": 2, "type": "allowPairingAgain", "fingerprint": String(repeating: "A", count: 129)])
            == .dropped(type: "allowPairingAgain"))
        #expect(Panel.parse(["v": 1, "type": "removeDevice", "fingerprint": fp]) == .dropped(type: "removeDevice"))
    }

    @Test func dialogNamesDropHiddenCharacters() {
        #expect(RemoteHost.displayName("lati\u{202E}tude\n") == "latitude")
        #expect(RemoteHost.displayName("pc\u{200B}\u{2028}one") == "pcone")
        #expect(RemoteHost.displayName("\u{0007} \u{2066}") == "unnamed device")
        #expect(RemoteHost.displayName("Jürgen’s PC") == "Jürgen’s PC")
        #expect(RemoteHost.displayName(String(repeating: "x", count: 70)) == String(repeating: "x", count: 64) + "…")
    }

    @Test func stateListsDevicesWithContractNames() throws {
        var state = Panel.State()
        #expect((try roundTrip(state.message)["state"] as? [String: Any])?["devices"] as? [Any] != nil)
        state.devices = [.init(name: "pc", deviceFingerprint: "1111", pairedAt: 1_760_000_000, revoked: true)]
        let s = try #require(try roundTrip(state.message)["state"] as? [String: Any])
        let device = try #require((s["devices"] as? [[String: Any]])?.first)
        #expect(device["name"] as? String == "pc" && device["fingerprint"] as? String == "1111")
        #expect(device["pairedAt"] as? Int == 1_760_000_000 && device["removed"] as? Bool == true)
        #expect(Set(device.keys) == ["name", "fingerprint", "pairedAt", "removed"])
    }

    @Test func dropsActionsOver4KiB() {
        let padding = String(repeating: "x", count: 4096)
        #expect(Panel.parse(["v": 2, "type": "ready", "pad": padding]) == .dropped(type: nil))
    }

    @Test func stateMessageUsesContractNamesAndNulls() throws {
        var state = Panel.State()
        state.permissions = [.init(id: .screen, label: "Screen Recording", granted: false)]
        let off = try roundTrip(state.message)
        #expect(off["v"] as? Int == 2 && off["type"] as? String == "state")
        let s = try #require(off["state"] as? [String: Any])
        #expect(s["platform"] as? String == "macos")
        #expect(s["host"] is NSNull && s["session"] is NSNull && s["pairingNotice"] is NSNull)
        let remote = try #require(s["remoteAccess"] as? [String: Any])
        #expect(remote["on"] as? Bool == false && remote["busy"] as? Bool == false && remote["error"] is NSNull)
        let permission = try #require((s["permissions"] as? [[String: Any]])?.first)
        #expect(permission["id"] as? String == "screen" && permission["canRequest"] as? Bool == true)
        #expect((s["clipboard"] as? [String: Any])?["on"] as? Bool == true)

        state.host = try JSONDecoder().decode(LocalLink.Identity.self, from: Data(#"{"type":"identity","name":"Mac","host_fingerprint":"7D40"}"#.utf8))
        state.session = try JSONDecoder().decode(LocalLink.Session.self, from: Data(#"{"type":"session","name":"<b>pc</b>","device_fingerprint":"1111"}"#.utf8))
        let on = try #require(try roundTrip(state.message)["state"] as? [String: Any])
        #expect((on["host"] as? [String: Any])?["fingerprint"] as? String == "7D40")
        let session = try #require(on["session"] as? [String: Any])
        #expect(session["device"] as? String == "<b>pc</b>" && session["deviceFingerprint"] as? String == "1111")
    }

    @Test func servesOnlyTheListedFilesFromThePanelHost() {
        let root = URL(fileURLWithPath: "/bundle/panel")
        #expect(Panel.resource(for: Panel.indexURL, in: root)?.file.path == "/bundle/panel/index.html")
        #expect(Panel.resource(for: URL(string: "nocomputer-panel://panel/panel.js"), in: root)?.mimeType == "text/javascript")
        for bad in ["nocomputer-panel://panel/../Info.plist", "nocomputer-panel://panel/%2e%2e/Info.plist",
                    "nocomputer-panel://other/index.html", "file:///bundle/panel/index.html",
                    "nocomputer-panel://panel/index.html?x", "nocomputer-panel://panel/secret.txt"] {
            #expect(Panel.resource(for: URL(string: bad), in: root) == nil, "\(bad)")
        }
        #expect(Panel.isIndex(Panel.indexURL) && !Panel.isIndex(URL(string: "nocomputer-panel://panel/panel.js")))
    }

    private func roundTrip(_ message: [String: Any]) throws -> [String: Any] {
        let data = try JSONSerialization.data(withJSONObject: message)
        return try #require(try JSONSerialization.jsonObject(with: data) as? [String: Any])
    }
}
