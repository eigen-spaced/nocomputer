import Foundation
import Testing
import VideoToolbox
@testable import NoComputerDiagnostics

struct VideoEncoderTests {
    @Test func rejectsInvalidDimensions() {
        for dimensions in [(0, 100), (100, -1), (8193, 100)] {
            #expect(throws: (any Error).self) {
                try VideoEncoder(width: dimensions.0, height: dimensions.1)
            }
        }
    }

    // Opt in: exercises the real local encoder, not screen capture or TCC.
    @Test(.enabled(if: ProcessInfo.processInfo.environment["NOCOMPUTER_TEST_HARDWARE"] == "1"))
    func hardwareEncodesSyntheticFramesAndCloses() async throws {
        let encoder = try VideoEncoder(width: 320, height: 240)
        defer { encoder.close() }
        var buffer: CVPixelBuffer?
        let status = CVPixelBufferCreate(kCFAllocatorDefault, 320, 240,
            kCVPixelFormatType_32BGRA,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer)
        #expect(status == kCVReturnSuccess)
        let pixelBuffer = try #require(buffer)
        #expect(CVPixelBufferLockBaseAddress(pixelBuffer, []) == kCVReturnSuccess)
        if let base = CVPixelBufferGetBaseAddress(pixelBuffer) {
            memset(base, 0, CVPixelBufferGetBytesPerRow(pixelBuffer) * 240)
        }
        CVPixelBufferUnlockBaseAddress(pixelBuffer, [])
        // Never modify pixels while VideoToolbox might be using this buffer.
        for index in 0..<30 {
            encoder.encode(pixelBuffer, presentationTime: CMTime(value: Int64(index), timescale: 30))
            try await Task.sleep(nanoseconds: 33_333_333)
        }
        for _ in 0..<100 {
            if encoder.snapshot().inFlight == 0 { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        let state = encoder.snapshot()
        #expect(state.hardware)
        #expect(state.error == nil)
        #expect(state.encoded > 0)
        #expect(state.bytes > 0)
        #expect(state.keyframes > 0)
        #expect(state.parameterSetCount >= 2)
        #expect([1, 2, 4].contains(state.nalLengthBytes))
        #expect(state.peakInFlight <= VideoEncoder.maximumInFlight)
        #expect(state.inFlight == 0)
        encoder.close()
        encoder.close()
        encoder.encode(pixelBuffer, presentationTime: CMTime(value: 100, timescale: 30))
        #expect(encoder.snapshot().submitted == state.submitted)
        #expect(encoder.snapshot().inFlight == 0)
    }

    @Test(.enabled(if: ProcessInfo.processInfo.environment["NOCOMPUTER_TEST_HARDWARE"] == "1"))
    func hardwareRejectsWrongSizedSource() throws {
        let encoder = try VideoEncoder(width: 320, height: 240)
        defer { encoder.close() }
        var buffer: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, 160, 120, kCVPixelFormatType_32BGRA,
            nil, &buffer) == kCVReturnSuccess)
        encoder.encode(try #require(buffer), presentationTime: .zero)
        #expect(encoder.snapshot().submitted == 0)
        #expect(encoder.snapshot().error != nil)
        #expect(encoder.snapshot().inFlight == 0)
    }
}
