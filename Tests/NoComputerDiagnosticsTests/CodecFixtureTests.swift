import Foundation
import Testing
import VideoToolbox
@testable import NoComputerDiagnostics

// Synthetic fixture only. Never connected to ScreenCaptureKit or the app UI.
private final class FixtureCollector {
    private let lock = NSLock()
    private var bytes = Data()
    private var failure: String?
    private var count = 0
    private(set) var syncIndices: [Int] = []

    func append(_ sample: CMSampleBuffer) {
        lock.lock()
        defer { lock.unlock() }
        guard failure == nil else { return }
        do {
            let packet = try AnnexB.accessUnit(sample)
            guard bytes.count + packet.count <= 8_000_000 else { throw FixtureError.invalid }
            bytes.append(packet)
            if AnnexB.isKeyframe(sample) { syncIndices.append(count) }
            count += 1
        } catch { failure = String(describing: error) }
    }

    func result() -> (Data, Int, String?) {
        lock.lock()
        defer { lock.unlock() }
        return (bytes, count, failure)
    }

    private enum FixtureError: Error { case invalid }
}

struct CodecFixtureTests {
    @Test(.enabled(if: ProcessInfo.processInfo.environment["NOCOMPUTER_FIXTURE_PATH"] != nil))
    func exportsSyntheticInteroperabilityFixture() async throws {
        let destination = try #require(ProcessInfo.processInfo.environment["NOCOMPUTER_FIXTURE_PATH"])
        let collector = FixtureCollector()
        let encoder = try VideoEncoder(width: 320, height: 240, keyframeSeconds: 2,
            sampleConsumer: { collector.append($0) })
        defer { encoder.close() }
        var buffer: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, 320, 240, kCVPixelFormatType_32BGRA,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
        let source = try #require(buffer)
        CVBufferSetAttachment(source, kCVImageBufferCGColorSpaceKey,
            try #require(CGColorSpace(name: CGColorSpace.sRGB)), .shouldPropagate)
        #expect(CVPixelBufferLockBaseAddress(source, []) == kCVReturnSuccess)
        let base = try #require(CVPixelBufferGetBaseAddress(source)).assumingMemoryBound(to: UInt8.self)
        let stride = CVPixelBufferGetBytesPerRow(source)
        for y in 0..<240 {
            for x in 0..<320 {
                let offset = y * stride + x * 4
                base[offset] = UInt8(y)
                base[offset + 1] = UInt8(x % 256)
                base[offset + 2] = 128
                base[offset + 3] = 255
            }
        }
        CVPixelBufferUnlockBaseAddress(source, [])
        for index in 0..<30 {
            encoder.encode(source, presentationTime: CMTime(value: Int64(index), timescale: 30))
            try await Task.sleep(nanoseconds: 33_333_333)
        }
        for _ in 0..<100 {
            if encoder.snapshot().inFlight == 0 { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(encoder.snapshot().error == nil)
        #expect(encoder.snapshot().inFlight == 0)
        let (data, count, failure) = collector.result()
        #expect(failure == nil)
        #expect(count == 30)
        #expect(!data.isEmpty)
        // Refuse overwrite: generated artifacts are not edited source files.
        try data.write(to: URL(fileURLWithPath: destination), options: [.withoutOverwriting])
    }

    // Moving 1920x1080 fixture for viewers: a frame-number band and a moving bar make
    // frame updates/order visible, and 90 frames span more than one 60-frame GOP.
    @Test(.enabled(if: ProcessInfo.processInfo.environment["NOCOMPUTER_MOVING_FIXTURE_PATH"] != nil))
    func exportsMovingInteroperabilityFixture() async throws {
        let destination = try #require(ProcessInfo.processInfo.environment["NOCOMPUTER_MOVING_FIXTURE_PATH"])
        let (width, height, frames) = (1920, 1080, 90)
        let collector = FixtureCollector()
        let encoder = try VideoEncoder(width: width, height: height, keyframeSeconds: 2,
            sampleConsumer: { collector.append($0) })
        defer { encoder.close() }
        for index in 0..<frames {
            let source = try Self.movingFrame(index: index, width: width, height: height)
            encoder.encode(source, presentationTime: CMTime(value: Int64(index), timescale: 30))
            try await Task.sleep(nanoseconds: 33_333_333)
        }
        for _ in 0..<100 {
            if encoder.snapshot().inFlight == 0 { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(encoder.snapshot().error == nil)
        #expect(encoder.snapshot().inFlight == 0)
        let (data, count, failure) = collector.result()
        #expect(failure == nil)
        #expect(count == frames)
        #expect(collector.syncIndices.count > 1)
        print("moving fixture: \(count) frames, \(data.count) bytes, sync at \(collector.syncIndices)")
        try data.write(to: URL(fileURLWithPath: destination), options: [.withoutOverwriting])
    }

    // Seven-segment masks for digits 0-9; bits a..g = 0..6.
    private static let segments: [UInt8] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F]

    private static func movingFrame(index: Int, width: Int, height: Int) throws -> CVPixelBuffer {
        var buffer: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
        let source = try #require(buffer)
        CVBufferSetAttachment(source, kCVImageBufferCGColorSpaceKey,
            try #require(CGColorSpace(name: CGColorSpace.sRGB)), .shouldPropagate)
        #expect(CVPixelBufferLockBaseAddress(source, []) == kCVReturnSuccess)
        defer { CVPixelBufferUnlockBaseAddress(source, []) }
        let base = try #require(CVPixelBufferGetBaseAddress(source)).assumingMemoryBound(to: UInt8.self)
        let stride = CVPixelBufferGetBytesPerRow(source)
        func fill(_ x0: Int, _ y0: Int, _ w: Int, _ h: Int, _ bgr: (UInt8, UInt8, UInt8)) {
            for y in max(0, y0)..<min(height, y0 + h) {
                for x in max(0, x0)..<min(width, x0 + w) {
                    let offset = y * stride + x * 4
                    (base[offset], base[offset + 1], base[offset + 2], base[offset + 3]) = (bgr.0, bgr.1, bgr.2, 255)
                }
            }
        }
        // Static background: same channel layout as the 320x240 fixture, scaled to the frame.
        for y in 0..<height {
            for x in 0..<width {
                let offset = y * stride + x * 4
                base[offset] = UInt8(y * 255 / (height - 1))
                base[offset + 1] = UInt8(x * 255 / (width - 1))
                base[offset + 2] = 128
                base[offset + 3] = 255
            }
        }
        // Moving white bar below the band; one full traversal over the clip.
        let band = 280
        fill(index * (width - 96) / 89, band, 96, height - band, (255, 255, 255))
        // Black band with the zero-padded frame number as large white seven-segment digits.
        fill(0, 0, width, band, (0, 0, 0))
        let (digitW, digitH, t) = (140, 220, 28)
        for (position, character) in String(format: "%03d", index).enumerated() {
            let mask = segments[Int(String(character))!]
            let x = 40 + position * (digitW + 50), y = 30
            let rects = [(x, y, digitW, t), (x + digitW - t, y, t, digitH / 2),
                         (x + digitW - t, y + digitH / 2, t, digitH / 2), (x, y + digitH - t, digitW, t),
                         (x, y + digitH / 2, t, digitH / 2), (x, y, t, digitH / 2),
                         (x, y + (digitH - t) / 2, digitW, t)]
            for (bit, rect) in rects.enumerated() where mask & (1 << bit) != 0 {
                fill(rect.0, rect.1, rect.2, rect.3, (255, 255, 255))
            }
        }
        return source
    }
}
