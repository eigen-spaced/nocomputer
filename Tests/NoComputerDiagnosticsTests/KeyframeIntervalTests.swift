import AppKit
import CoreMedia
import CoreText
import Foundation
import Testing
import VideoToolbox
@testable import NoComputerDiagnostics

// Opt-in measurement, not a pass/fail check: encodes 30 s of a terminal-like
// screen (dense static text, one line being typed) with each keyframe interval
// and prints the compressed sizes. Synthetic content; a live desktop differs.
private final class SizeCollector {
    private let lock = NSLock()
    private(set) var keyframes: [Int] = []
    private(set) var deltas: [Int] = []

    func append(_ sample: CMSampleBuffer) {
        let size = CMSampleBufferGetTotalSampleSize(sample)
        lock.lock()
        if AnnexB.isKeyframe(sample) { keyframes.append(size) } else { deltas.append(size) }
        lock.unlock()
    }
}

struct KeyframeIntervalTests {
    static let (width, height, frames, fps) = (1600, 1034, 900, 30)

    private static func frame(_ index: Int) throws -> CVPixelBuffer {
        var buffer: CVPixelBuffer?
        guard CVPixelBufferCreate(kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess,
            let buffer else { throw CocoaError(.featureUnsupported) }
        CVBufferSetAttachment(buffer, kCVImageBufferCGColorSpaceKey, CGColorSpace(name: CGColorSpace.sRGB)!, .shouldPropagate)
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        guard let context = CGContext(data: CVPixelBufferGetBaseAddress(buffer), width: width, height: height,
            bitsPerComponent: 8, bytesPerRow: CVPixelBufferGetBytesPerRow(buffer),
            space: CGColorSpace(name: CGColorSpace.sRGB)!,
            bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue)
        else { throw CocoaError(.featureUnsupported) }
        context.setFillColor(CGColor(red: 0.12, green: 0.12, blue: 0.14, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let font = CTFontCreateWithName("Menlo" as CFString, 13, nil)
        let attributes: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor(white: 0.85, alpha: 1)]
        let lineHeight = 16
        let rows = height / lineHeight - 1
        for row in 0..<rows {
            // Static lines: deterministic pseudo-log text, the bulk of a terminal.
            var text = String(format: "%04d  ", row)
            var seed = UInt32(row &* 2_654_435_761 & 0xFFFF_FFFF)
            for _ in 0..<180 {
                seed = seed &* 1_103_515_245 &+ 12_345
                let value = Int(seed >> 16) % 40
                text.append(value < 6 ? " " : Character(UnicodeScalar(UInt8(97 + value % 26))))
            }
            if row == rows - 1 {
                // The prompt line: four characters typed per second, plus a blinking cursor.
                let typed = String(repeating: "x", count: min(150, index * 4 / fps))
                text = "$ " + typed + (index / 15 % 2 == 0 ? "█" : " ")
            }
            let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attributes))
            context.textPosition = CGPoint(x: 8, y: height - (row + 1) * lineHeight)
            CTLineDraw(line, context)
        }
        return buffer
    }

    private static func measure(keyframeSeconds: Int) async throws -> SizeCollector {
        let collector = SizeCollector()
        let encoder = try VideoEncoder(width: width, height: height, colorPolicy: .sRGB,
            keyframeSeconds: keyframeSeconds, sampleConsumer: { collector.append($0) })
        defer { encoder.close() }
        for index in 0..<frames {
            let buffer = try frame(index)
            while encoder.snapshot().inFlight >= VideoEncoder.maximumInFlight { try await Task.sleep(nanoseconds: 1_000_000) }
            encoder.encode(buffer, presentationTime: CMTime(value: Int64(index), timescale: Int32(fps)))
        }
        for _ in 0..<200 where encoder.snapshot().inFlight > 0 { try await Task.sleep(nanoseconds: 10_000_000) }
        #expect(encoder.snapshot().error == nil)
        #expect(encoder.snapshot().rawDrops == 0)
        return collector
    }

    @Test(.enabled(if: ProcessInfo.processInfo.environment["NOCOMPUTER_MEASURE_KEYFRAMES"] == "1"))
    func measureKeyframeIntervals() async throws {
        let seconds = Double(Self.frames) / Double(Self.fps)
        for interval in [VideoEncoder.diagnosticKeyframeSeconds, 10, VideoEncoder.remoteKeyframeSeconds, 30] {
            let result = try await Self.measure(keyframeSeconds: interval)
            let keyBytes = result.keyframes.reduce(0, +)
            let deltaBytes = result.deltas.reduce(0, +)
            let total = keyBytes + deltaBytes
            print(String(format: "keyframe interval %2d s: %d frames, %d keyframes (mean %.1f KB), deltas mean %.2f KB, total %.2f MB = %.2f Mbit/s, keyframes %.0f%% of bytes",
                interval, result.keyframes.count + result.deltas.count, result.keyframes.count,
                Double(keyBytes) / Double(max(1, result.keyframes.count)) / 1000,
                Double(deltaBytes) / Double(max(1, result.deltas.count)) / 1000,
                Double(total) / 1e6, Double(total) * 8 / seconds / 1e6, Double(keyBytes) * 100 / Double(max(1, total))))
            #expect(result.keyframes.count + result.deltas.count == Self.frames)
        }
    }
}
