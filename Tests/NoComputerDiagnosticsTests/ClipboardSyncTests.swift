import AppKit
import Testing
@testable import NoComputerDiagnostics

// Uses a private named pasteboard, never the user's clipboard.
@MainActor
struct ClipboardSyncTests {
    private func make() -> (ClipboardSync, NSPasteboard, () -> [String]) {
        let pasteboard = NSPasteboard(name: .init("dev.nocomputer.test.\(UUID().uuidString)"))
        let sync = ClipboardSync(pasteboard: pasteboard)
        var sent: [String] = []
        sync.onLocalText = { sent.append($0.text) }
        return (sync, pasteboard, { sent })
    }

    private func copy(_ text: String, to pasteboard: NSPasteboard, types: [NSPasteboard.PasteboardType] = []) {
        pasteboard.clearContents()
        pasteboard.declareTypes([.string] + types, owner: nil)
        pasteboard.setString(text, forType: .string)
        for type in types { pasteboard.setData(Data(), forType: type) }
    }

    @Test func sendsLocalChangesOnlyAfterStartAndNotTheExistingText() {
        let (sync, pasteboard, sent) = make()
        defer { pasteboard.releaseGlobally() }
        copy("before", to: pasteboard)
        sync.poll()
        sync.start()
        defer { sync.stop() }
        sync.poll()
        #expect(sent().isEmpty, "the viewer sends first at session start")
        copy("hello\nworld", to: pasteboard)
        sync.poll()
        sync.poll()
        #expect(sent() == ["hello\nworld"])
        // Copying the same text again is not resent.
        copy("hello\nworld", to: pasteboard)
        sync.poll()
        #expect(sent() == ["hello\nworld"])
        #expect(sync.state.sent == 1)
    }

    @Test func appliedTextIsNotEchoedBack() throws {
        let (sync, pasteboard, sent) = make()
        defer { pasteboard.releaseGlobally() }
        sync.start()
        defer { sync.stop() }
        sync.apply(try #require(LocalLink.ClipboardText("from windows")))
        sync.poll()
        #expect(pasteboard.string(forType: .string) == "from windows")
        #expect(sent().isEmpty)
        #expect(sync.state.applied == 1)
        // Copying the same text locally afterwards is not news to the viewer either.
        copy("from windows", to: pasteboard)
        sync.poll()
        #expect(sent().isEmpty)
        copy("new", to: pasteboard)
        sync.poll()
        #expect(sent() == ["new"])
    }

    @Test func skipsSecretsLargeTextAndNonText() {
        let (sync, pasteboard, sent) = make()
        defer { pasteboard.releaseGlobally() }
        sync.start()
        defer { sync.stop() }
        copy("password", to: pasteboard, types: [.init("org.nspasteboard.ConcealedType")])
        sync.poll()
        copy(String(repeating: "a", count: LocalLink.maximumClipboard + 1), to: pasteboard)
        sync.poll()
        pasteboard.clearContents()
        pasteboard.setData(Data([1, 2, 3]), forType: .png)
        sync.poll()
        #expect(sent().isEmpty)
        #expect(sync.state.skippedSecret == 1)
        #expect(sync.state.skippedLarge == 1)
        #expect(sync.state.skippedNonText == 1)
    }

    @Test func textIsCutAtTheFirstNUL() {
        let (sync, pasteboard, sent) = make()
        defer { pasteboard.releaseGlobally() }
        sync.start()
        defer { sync.stop() }
        copy("visible\0hidden", to: pasteboard)
        sync.poll()
        copy("\0only hidden", to: pasteboard)
        sync.poll()
        #expect(sent() == ["visible"])
        #expect(sync.state.skippedNonText == 1)
    }

    @Test func nothingIsSentOrAppliedWhileStopped() throws {
        let (sync, pasteboard, sent) = make()
        defer { pasteboard.releaseGlobally() }
        sync.apply(try #require(LocalLink.ClipboardText("ignored")))
        #expect(pasteboard.string(forType: .string) == nil)
        copy("local", to: pasteboard)
        sync.poll() // before start: nothing is watched yet
        sync.start()
        sync.stop()
        copy("after stop", to: pasteboard)
        #expect(!sync.running)
        sync.poll()
        #expect(sent().isEmpty)
        #expect(sync.state.applied == 0)
    }
}
