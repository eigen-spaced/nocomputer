import Foundation
import Testing
@testable import NoComputerDiagnostics

private func frame(_ type: UInt8, _ payload: [UInt8]) -> Data {
    let length = UInt32(1 + payload.count)
    return Data(withUnsafeBytes(of: length.bigEndian, Array.init) + [type] + payload)
}

struct LocalLinkTests {
    @Test func encodesOutgoingFramesPerSpec() throws {
        #expect(try LocalLink.encode(.hello(version: 1)) == frame(0x01, [0, 1]))
        #expect(try LocalLink.encode(.accessUnit(keyframe: true, captureMicroseconds: 0x0102_0304_0506_0708,
            annexB: Data([0, 0, 0, 1, 0x65]))) == frame(0x02, [1, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 1, 0x65]))
        #expect(try LocalLink.encode(.accessUnit(keyframe: false, captureMicroseconds: 0,
            annexB: Data([0x41]))) == frame(0x02, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0x41]))
        #expect(try LocalLink.encode(.captureStopped(reason: "lock")) == frame(0x03, Array("lock".utf8)))
        #expect(try LocalLink.encode(.pairingDecision(requestID: 7, approve: true))
            == frame(0x04, [0, 0, 0, 0, 0, 0, 0, 7, 1]))
        #expect(try LocalLink.encode(.pairingDecision(requestID: 7, approve: false))
            == frame(0x04, [0, 0, 0, 0, 0, 0, 0, 7, 0]))
    }

    @Test func refusesEmptyAndOversizedAccessUnits() {
        #expect(throws: LocalLink.ProtocolError.self) {
            try LocalLink.encode(.accessUnit(keyframe: false, captureMicroseconds: 0, annexB: Data()))
        }
        // 1 type + 1 flags + 8 time + payload must stay within 4 MiB.
        let tooBig = Data(count: LocalLink.maximumLength - 9)
        #expect(throws: LocalLink.ProtocolError.self) {
            try LocalLink.encode(.accessUnit(keyframe: false, captureMicroseconds: 0, annexB: tooBig))
        }
        let largest = Data(count: LocalLink.maximumLength - 10)
        #expect((try? LocalLink.encode(.accessUnit(keyframe: false, captureMicroseconds: 0, annexB: largest)))?.count
            == 4 + LocalLink.maximumLength)
    }

    @Test func truncatesStopReasonOnCharacterBoundary() throws {
        let encoded = try LocalLink.encode(.captureStopped(reason: String(repeating: "é", count: 600)))
        let payload = encoded.dropFirst(5)
        #expect(payload.count == 1024)
        #expect(String(data: payload, encoding: .utf8) != nil)
    }

    @Test func decodesIncomingAcrossArbitrarySplits() throws {
        let json = Array(#"{"type":"session","name":"latitude"}"#.utf8)
        let stream = frame(0x81, [0, 1]) + frame(0x82, []) + frame(0x84, []) + frame(0x85, json) + frame(0x83, [])
        let expected: [LocalLink.Incoming] = [.helloAck(version: 1), .startStream, .requestKeyframe,
                                              .event(Data(json)), .stopStream]
        for chunk in [1, 2, 3, 5, 7, stream.count] {
            var reader = LocalLink.Reader()
            var messages: [LocalLink.Incoming] = []
            var offset = 0
            while offset < stream.count {
                let end = min(offset + chunk, stream.count)
                messages += try reader.feed(stream.subdata(in: offset..<end))
                offset = end
            }
            #expect(messages == expected, "chunk size \(chunk)")
        }
    }

    @Test func violationsAreFatal() {
        let cases: [(String, Data)] = [
            ("zero length", Data([0, 0, 0, 0])),
            ("over 4 MiB", Data([0x00, 0x40, 0x00, 0x01])),
            ("unknown type", frame(0x86, [])),
            ("Swift-bound type sent back", frame(0x01, [0, 1])),
            ("payload on empty type", frame(0x82, [0])),
            ("short HelloAck", frame(0x81, [1])),
            ("Event not UTF-8", frame(0x85, [0xFF, 0xFE])),
        ]
        for (name, bytes) in cases {
            var reader = LocalLink.Reader()
            #expect(throws: LocalLink.ProtocolError.self, "\(name)") { try reader.feed(bytes) }
        }
    }

    @Test func oversizedLengthIsRejectedBeforeThePayloadArrives() {
        var reader = LocalLink.Reader()
        // Only the 4-byte header: nothing buffered beyond it, still refused.
        #expect(throws: LocalLink.ProtocolError.self) { try reader.feed(Data([0xFF, 0xFF, 0xFF, 0xFF])) }
    }

    @Test func parsesPairingPrompt() throws {
        let json = Data(#"{"type":"pairing_prompt","request_id":9007199254740991,"device_name":"latitude","device_fingerprint":"1111 2222","host_fingerprint":"0102 0304","extra":1}"#.utf8)
        #expect(LocalLink.eventType(json) == "pairing_prompt")
        let prompt = try JSONDecoder().decode(LocalLink.PairingPrompt.self, from: json)
        #expect(prompt == LocalLink.PairingPrompt(requestID: 9_007_199_254_740_991, deviceName: "latitude",
            deviceFingerprint: "1111 2222", hostFingerprint: "0102 0304"))
        #expect(LocalLink.eventType(Data("[]".utf8)) == nil)
    }
}

struct PipeAdmissionTests {
    @Test func startsAtAKeyframe() {
        var admission = PipeAdmission(limit: 3)
        #expect(admission.admit(keyframe: false) == .dropDependent)
        #expect(admission.admit(keyframe: true) == .send)
        #expect(admission.admit(keyframe: false) == .send)
    }

    @Test func pressureDropsEverythingUntilTheNextKeyframe() {
        var admission = PipeAdmission(limit: 2)
        #expect(admission.admit(keyframe: true) == .send)
        #expect(admission.admit(keyframe: false) == .send)
        #expect(admission.admit(keyframe: false) == .dropForPressure)
        admission.written()
        admission.written()
        // The pipe drained, but these P-frames reference the dropped one.
        #expect(admission.admit(keyframe: false) == .dropDependent)
        #expect(admission.admit(keyframe: false) == .dropDependent)
        #expect(admission.admit(keyframe: true) == .send)
        #expect(admission.admit(keyframe: false) == .send)
    }

    @Test func keyframeIsAlsoDroppedUnderPressure() {
        var admission = PipeAdmission(limit: 1)
        #expect(admission.admit(keyframe: true) == .send)
        #expect(admission.admit(keyframe: true) == .dropForPressure)
        admission.written()
        #expect(admission.admit(keyframe: false) == .dropDependent)
        #expect(admission.admit(keyframe: true) == .send)
    }

    @Test func failedConversionRequiresAKeyframe() {
        var admission = PipeAdmission(limit: 3)
        #expect(admission.admit(keyframe: true) == .send)
        #expect(admission.admit(keyframe: false) == .send)
        admission.failed()
        #expect(admission.queued == 1)
        #expect(admission.admit(keyframe: false) == .dropDependent)
    }

    @Test func expectKeyframeHoldsBackPFrames() {
        var admission = PipeAdmission(limit: 3)
        #expect(admission.admit(keyframe: true) == .send)
        admission.expectKeyframe()
        #expect(admission.admit(keyframe: false) == .dropDependent)
        #expect(admission.admit(keyframe: true) == .send)
    }
}

// crates/host owns local-v5; its golden vectors are the shared reference bytes.
struct LocalLinkGoldenTests {
    private static let directory = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("crates/host/testdata/local-v5")

    private static func golden(_ name: String) throws -> Data {
        try Data(contentsOf: directory.appendingPathComponent(name))
    }

    private static func annexB(_ name: String) throws -> Data {
        // Skip length (4), type (1), flags (1) and capture_us (8).
        try golden(name).dropFirst(14)
    }

    @Test func swiftEncodesTheGoldenOutgoingFrames() throws {
        #expect(try LocalLink.encode(.hello(version: 5)) == Self.golden("01-hello.bin"))
        #expect(try LocalLink.encode(.accessUnit(keyframe: true, captureMicroseconds: 0x0102_0304_0506_0708,
            annexB: Self.annexB("02-access-unit-key.bin"))) == Self.golden("02-access-unit-key.bin"))
        let delta = try Self.golden("02-access-unit-delta.bin")
        let microseconds = delta[6..<14].reduce(UInt64(0)) { $0 << 8 | UInt64($1) }
        #expect(try LocalLink.encode(.accessUnit(keyframe: false, captureMicroseconds: microseconds,
            annexB: Self.annexB("02-access-unit-delta.bin"))) == delta)
        let stopped = try Self.golden("03-capture-stopped.bin")
        let reason = try #require(String(data: stopped.dropFirst(5), encoding: .utf8))
        #expect(try LocalLink.encode(.captureStopped(reason: reason)) == stopped)
        #expect(try LocalLink.encode(.pairingDecision(requestID: 0x0A0B_0C0D_0E0F_1011, approve: true))
            == Self.golden("04-pairing-decision-approve.bin"))
        #expect(try LocalLink.encode(.pairingDecision(requestID: 7, approve: false))
            == Self.golden("04-pairing-decision-deny.bin"))
        let unavailable = try Self.golden("05-input-unavailable.bin")
        let unavailableReason = try #require(String(data: unavailable.dropFirst(5), encoding: .utf8))
        #expect(try LocalLink.encode(.inputUnavailable(reason: unavailableReason)) == unavailable)
        let clipboard = try Self.golden("06-clipboard.bin")
        let string = try #require(String(data: clipboard.dropFirst(5), encoding: .utf8))
        let text = try #require(LocalLink.ClipboardText(string))
        #expect(try LocalLink.encode(.clipboard(text)) == clipboard)
        #expect(try LocalLink.encode(.loginVerdict(requestID: 0x0A0B_0C0D_0E0F_1011, ok: true))
            == Self.golden("07-login-verdict-ok.bin"))
        #expect(try LocalLink.encode(.loginVerdict(requestID: 7, ok: false)) == Self.golden("07-login-verdict-refused.bin"))
    }

    @Test func swiftDecodesTheGoldenLoginFrames() throws {
        var reader = LocalLink.Reader()
        let messages = try reader.feed(Self.golden("8c-verify-login.bin") + Self.golden("85-event-pairing-withdrawn.bin"))
        try #require(messages.count == 2)
        guard case let .verifyLogin(request) = messages[0], case let .event(json) = messages[1] else {
            Issue.record("expected VerifyLogin and an event")
            return
        }
        #expect(request.requestID == 9)
        #expect(request.username == "Jürgen")
        #expect(request.password.withString { $0 } == "correct horse – ü 🙂")
        #expect(LocalLink.eventType(json) == "pairing_withdrawn")
        #expect(try JSONDecoder().decode(LocalLink.PairingWithdrawn.self, from: json).requestID == 7)
    }

    @Test func swiftDecodesTheGoldenIdentityEvent() throws {
        var reader = LocalLink.Reader()
        guard case let .event(json) = try reader.feed(Self.golden("85-event-identity.bin")).first else {
            Issue.record("expected an event")
            return
        }
        #expect(LocalLink.eventType(json) == "identity")
        let identity = try JSONDecoder().decode(LocalLink.Identity.self, from: json)
        #expect(identity.name == "MacBook Air")
        #expect(identity.hostFingerprint.hasPrefix("0102 0304"))
    }

    @Test func swiftDecodesTheGoldenClipboardFrame() throws {
        var reader = LocalLink.Reader()
        let messages = try reader.feed(Self.golden("8b-clipboard.bin"))
        #expect(messages == [.clipboard(try #require(LocalLink.ClipboardText("from the viewer\n– ü 🙂")))])
    }

    @Test func swiftDecodesTheGoldenIncomingFrames() throws {
        var reader = LocalLink.Reader()
        let names = ["81-hello-ack.bin", "82-start-stream.bin", "83-stop-stream.bin", "84-request-keyframe.bin",
                     "85-event-pairing-prompt.bin", "85-event-session.bin"]
        let messages = try reader.feed(names.map { try Self.golden($0) }.reduce(Data(), +))
        try #require(messages.count == 6)
        #expect(Array(messages[0..<4]) == [.helloAck(version: 5), .startStream, .stopStream, .requestKeyframe])
        guard case let .event(prompt) = messages[4], case let .event(session) = messages[5] else {
            Issue.record("expected two events")
            return
        }
        let request = try JSONDecoder().decode(LocalLink.PairingPrompt.self, from: prompt)
        #expect(request.requestID == 7)
        #expect(request.deviceName == "latitude")
        #expect(request.deviceFingerprint.hasPrefix("1111 2222"))
        #expect(request.hostFingerprint.hasPrefix("0102 0304"))
        #expect(LocalLink.eventType(session) == "session")
    }

    @Test func swiftDecodesTheGoldenInputFrames() throws {
        let expected: [(String, RemoteInput)] = [
            ("86-pointer.bin", .pointer(x: 0x1234, y: 0xFFFF)),
            ("87-button-left-down.bin", .button(.left, down: true)),
            ("87-button-forward-up.bin", .button(.forward, down: false)),
            ("88-scroll-pixel.bin", .scroll(dx: -3, dy: 120, unit: .pixel)),
            ("88-scroll-line.bin", .scroll(dx: 0, dy: -100, unit: .line)),
            ("89-key-down.bin", .key(usage: 0x04, state: .down)),
            ("89-key-up.bin", .key(usage: 0xE3, state: .up)),
            ("89-key-repeat.bin", .key(usage: 0x28, state: .repeated)),
            ("8a-release-all.bin", .releaseAll),
        ]
        var reader = LocalLink.Reader()
        let messages = try reader.feed(expected.map { try Self.golden($0.0) }.reduce(Data(), +))
        #expect(messages == expected.map { .input($0.1) })
    }

    @Test func swiftDecodesTheGoldenViewSizes() throws {
        var reader = LocalLink.Reader()
        let messages = try reader.feed(Self.golden("8d-view-size.bin") + Self.golden("8d-view-size-native.bin"))
        #expect(messages == [.viewSize(width: 1366, height: 0x02C1), .viewSize(width: 0xFFFF, height: 0xFFFF)])
        for bad in [frame(0x8D, [0, 0, 0, 1]), frame(0x8D, [0, 1, 0, 0]), frame(0x8D, [0, 1, 0]), frame(0x8D, [0, 1, 0, 1, 0])] {
            var reader = LocalLink.Reader()
            #expect(throws: LocalLink.ProtocolError.self) { try reader.feed(bad) }
        }
    }

    // Swift→Rust frames must never be accepted coming back the other way.
    @Test func outgoingGoldenFramesAreRejectedAsIncoming() throws {
        for name in ["01-hello.bin", "02-access-unit-key.bin", "03-capture-stopped.bin", "04-pairing-decision-deny.bin",
                     "05-input-unavailable.bin", "06-clipboard.bin", "07-login-verdict-ok.bin"] {
            var reader = LocalLink.Reader()
            #expect(throws: LocalLink.ProtocolError.self, "\(name)") { try reader.feed(Self.golden(name)) }
        }
    }
}

struct LocalLinkInputTests {
    @Test func decodesInputFrames() throws {
        var reader = LocalLink.Reader()
        let stream = frame(0x86, [0xFF, 0xFF, 0x00, 0x01]) + frame(0x87, [1, 1]) + frame(0x87, [4, 0])
            + frame(0x88, [0xFF, 0xFF, 0xFF, 0xFD, 0x00, 0x00, 0x00, 0x78, 1]) + frame(0x89, [0x00, 0xE3, 2])
            + frame(0x8A, [])
        #expect(try reader.feed(stream) == [
            .input(.pointer(x: 65535, y: 1)), .input(.button(.right, down: true)), .input(.button(.forward, down: false)),
            .input(.scroll(dx: -3, dy: 120, unit: .line)), .input(.key(usage: 0xE3, state: .repeated)),
            .input(.releaseAll),
        ])
    }

    @Test func malformedInputIsFatal() {
        let cases: [(String, Data)] = [
            ("short pointer", frame(0x86, [0, 0, 0])),
            ("long pointer", frame(0x86, [0, 0, 0, 0, 0])),
            ("unknown button", frame(0x87, [5, 1])),
            ("button flag 2", frame(0x87, [0, 2])),
            ("unknown scroll unit", frame(0x88, [0, 0, 0, 0, 0, 0, 0, 1, 2])),
            ("unknown key state", frame(0x89, [0, 4, 3])),
            ("payload on ReleaseAll", frame(0x8A, [0])),
            ("empty clipboard", frame(0x8B, [])),
            ("clipboard with NUL", frame(0x8B, [0x61, 0, 0x62])),
            ("clipboard not UTF-8", frame(0x8B, [0xC3, 0x28])),
            ("clipboard over 1 MiB", frame(0x8B, [UInt8](repeating: 0x61, count: LocalLink.maximumClipboard + 1))),
        ]
        for (name, bytes) in cases {
            var reader = LocalLink.Reader()
            #expect(throws: LocalLink.ProtocolError.self, "\(name)") { try reader.feed(bytes) }
        }
    }

    @Test func encodesInputUnavailable() throws {
        #expect(try LocalLink.encode(.inputUnavailable(reason: "no AX")) == frame(0x05, Array("no AX".utf8)))
        #expect(try LocalLink.encode(.hello(version: LocalLink.version)) == frame(0x01, [0, 5]))
    }

    @Test func clipboardTextIsBoundedAndNeverDescribed() throws {
        #expect(LocalLink.ClipboardText("") == nil)
        #expect(LocalLink.ClipboardText("a\0b") == nil)
        #expect(LocalLink.ClipboardText(String(repeating: "a", count: LocalLink.maximumClipboard + 1)) == nil)
        #expect(LocalLink.ClipboardText(String(repeating: "a", count: LocalLink.maximumClipboard)) != nil)
        let secret = try #require(LocalLink.ClipboardText("hunter2"))
        let message = LocalLink.Incoming.clipboard(secret)
        #expect(!"\(message)".contains("hunter2"))
        #expect(!String(reflecting: message).contains("hunter2"))
        var reader = LocalLink.Reader()
        #expect(try reader.feed(frame(0x8B, [UInt8](repeating: 0x61, count: LocalLink.maximumClipboard))).count == 1)
    }
}

struct LocalLinkLoginTests {
    private static func verifyLogin(id: UInt64 = 1, username: [UInt8], password: [UInt8]) -> Data {
        var payload = withUnsafeBytes(of: id.bigEndian) { [UInt8]($0) }
        payload.append(UInt8(truncatingIfNeeded: username.count))
        return frame(0x8C, payload + username + password)
    }

    @Test func passwordsAreNeverDescribed() throws {
        var reader = LocalLink.Reader()
        let messages = try reader.feed(Self.verifyLogin(username: Array("me".utf8), password: Array("hunter2".utf8)))
        let message = try #require(messages.first)
        #expect(!"\(message)".contains("hunter2"))
        #expect(!String(reflecting: message).contains("hunter2"))
        guard case let .verifyLogin(request) = message else { Issue.record("expected VerifyLogin"); return }
        #expect("\(request.password)" == "Password(redacted)")
    }

    @Test func malformedVerifyLoginIsFatal() {
        let cases: [(String, Data)] = [
            ("too short", frame(0x8C, [0, 0, 0, 0, 0, 0, 0, 1, 1, 0x61])),
            ("empty username", Self.verifyLogin(username: [], password: [0x61])),
            ("no password", Self.verifyLogin(username: [0x61], password: [])),
            ("control character in username", Self.verifyLogin(username: [0x61, 0x0A], password: [0x61])),
            ("username not UTF-8", Self.verifyLogin(username: [0xC3, 0x28], password: [0x61])),
            ("NUL in password", Self.verifyLogin(username: [0x61], password: [0x61, 0, 0x62])),
            ("password not UTF-8", Self.verifyLogin(username: [0x61], password: [0xC3, 0x28])),
            ("password over 1024 bytes", Self.verifyLogin(username: [0x61], password: [UInt8](repeating: 0x61, count: 1025))),
        ]
        for (name, bytes) in cases {
            var reader = LocalLink.Reader()
            #expect(throws: LocalLink.ProtocolError.self, "\(name)") { try reader.feed(bytes) }
        }
    }

    @Test func boundsMatchTheRustSide() throws {
        // Rust's char::is_control is Cc only, so a format character must pass.
        var reader = LocalLink.Reader()
        let zwj = Array("a\u{200D}b".utf8)
        let longest = [UInt8](repeating: 0x61, count: LocalLink.maximumUsername)
        let messages = try reader.feed(Self.verifyLogin(username: zwj, password: [0x61])
            + Self.verifyLogin(username: longest, password: [UInt8](repeating: 0x62, count: LocalLink.maximumPassword)))
        #expect(messages.count == 2)
    }

    @Test func onlyThisAccountsNamesAreChecked() {
        #expect(AccountCheck.names("alex", short: "alex", full: "Alex Doe"))
        #expect(AccountCheck.names("ALEX", short: "alex", full: "Alex Doe"))
        #expect(AccountCheck.names("alex doe", short: "alex", full: "Alex Doe"))
        #expect(!AccountCheck.names("root", short: "alex", full: "Alex Doe"))
        #expect(!AccountCheck.names("", short: "alex", full: ""))
        #expect(!AccountCheck.names("alex2", short: "alex", full: "Alex Doe"))
    }

    // Never sends a password for the real account: a wrong one could count toward
    // its failed-login policy. Another account is refused before any lookup.
    @Test func anotherAccountIsRefusedWithoutLookup() {
        let password = LocalLink.Password(Array("certainly not the password \(UUID())".utf8))
        #expect(!AccountCheck.verify(username: "root", password: password))
    }
}
