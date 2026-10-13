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
