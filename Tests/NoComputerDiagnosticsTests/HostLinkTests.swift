import CoreMedia
import CoreVideo
import Foundation
import Testing
@testable import NoComputerDiagnostics

// Runs HostLink against a stand-in child: a shell script that acknowledges Hello
// and records everything it receives, so the pipe bytes can be checked.
struct HostLinkTests {
    private static func fakeChild(in directory: URL, received: URL) throws -> URL {
        let script = directory.appendingPathComponent("fake-host")
        // HelloAck(4) = [0 0 0 3][0x81][0 4]
        try """
            #!/bin/sh
            printf '\\000\\000\\000\\003\\201\\000\\004'
            exec cat > '\(received.path)'
            """.write(to: script, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: script.path)
        return script
    }

    private static func frames(_ data: Data) throws -> [(UInt8, Data)] {
        var result: [(UInt8, Data)] = []
        var offset = data.startIndex
        while offset < data.endIndex {
            let header = [UInt8](data[offset..<(offset + 4)])
            let length = Int(header[0]) << 24 | Int(header[1]) << 16 | Int(header[2]) << 8 | Int(header[3])
            try #require(length >= 1 && offset + 4 + length <= data.endIndex)
            result.append((data[offset + 4], Data(data[(offset + 5)..<(offset + 4 + length)])))
            offset += 4 + length
        }
        return result
    }

    @Test func sendsHelloThenDecodableAccessUnitsStartingAtAKeyframe() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("nocomputer-hostlink-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let received = directory.appendingPathComponent("received.bin")
        let link = HostLink(executable: try Self.fakeChild(in: directory, received: received))
        try link.start()
        // Frames before HelloAck are not sent; wait for the acknowledgement.
        // Generous: the shell child can start slowly while other tests load the machine.
        for _ in 0..<1000 where !link.acknowledgedForTesting { try await Task.sleep(nanoseconds: 10_000_000) }
        try #require(link.acknowledgedForTesting)

        let (width, height) = (320, 240)
        let encoder = try VideoEncoder(width: width, height: height,
            sampleConsumer: { link.submit($0) })
        var buffer: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
        let source = try #require(buffer)
        for index in 0..<20 {
            // A forced IDR mid-stream must arrive marked as a keyframe with SPS/PPS.
            if index == 10 { encoder.requestKeyframe() }
            encoder.encode(source, presentationTime: CMTime(value: Int64(index), timescale: 30))
            try await Task.sleep(nanoseconds: 33_333_333)
        }
        for _ in 0..<100 where encoder.snapshot().inFlight > 0 { try await Task.sleep(nanoseconds: 20_000_000) }
        encoder.close()
        link.captureStopped(reason: "test")
        link.decide(requestID: 42, approve: false)
        link.loginVerdict(requestID: 9, ok: true)
        link.stop()
        for _ in 0..<200 where link.runningForTesting { try await Task.sleep(nanoseconds: 10_000_000) }

        let messages = try Self.frames(try Data(contentsOf: received))
        try #require(messages.count >= 3)
        #expect(messages.first?.0 == 0x01)
        #expect(messages.first?.1 == Data([0, 4]))
        let units = messages.filter { $0.0 == 0x02 }
        #expect(units.count == 20)
        #expect(link.snapshot().accessUnits == 20)
        var keyframes: [Int] = []
        for (index, unit) in units.enumerated() {
            let flags = unit.1.first
            #expect(flags == 0 || flags == 1)
            let annexB = unit.1.dropFirst(9)
            #expect(annexB.starts(with: [0, 0, 0, 1]))
            // Annex B NAL type of the first unit: SPS (7) on keyframes, a slice otherwise.
            let firstNAL = annexB[annexB.startIndex + 4] & 0x1F
            if flags == 1 { keyframes.append(index); #expect(firstNAL == 7) } else { #expect(firstNAL == 1) }
        }
        #expect(keyframes.first == 0)
        #expect(keyframes.contains(10))
        #expect(messages[messages.count - 3].0 == 0x03)
        #expect(messages[messages.count - 3].1 == Data("test".utf8))
        #expect(messages[messages.count - 2].0 == 0x04)
        #expect(messages[messages.count - 2].1 == Data([0, 0, 0, 0, 0, 0, 0, 42, 0]))
        #expect(messages.last?.0 == 0x07)
        #expect(messages.last?.1 == Data([0, 0, 0, 0, 0, 0, 0, 9, 1]))
    }

    @Test func wrongHelloAckEndsTheLink() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("nocomputer-hostlink-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let script = directory.appendingPathComponent("fake-host")
        // HelloAck(1): a version this app no longer speaks.
        try "#!/bin/sh\nprintf '\\000\\000\\000\\003\\201\\000\\001'\nexec cat > /dev/null\n"
            .write(to: script, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: script.path)
        let link = HostLink(executable: script)
        try link.start()
        for _ in 0..<200 where link.runningForTesting { try await Task.sleep(nanoseconds: 10_000_000) }
        #expect(!link.runningForTesting)
        #expect(!link.acknowledgedForTesting)
    }
}
