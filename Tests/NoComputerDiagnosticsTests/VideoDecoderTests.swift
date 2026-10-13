import CoreImage
import Foundation
import Testing
import VideoToolbox
@testable import NoComputerDiagnostics

struct VideoDecoderTests {
    @Test(.enabled(if: ProcessInfo.processInfo.environment["NOCOMPUTER_TEST_HARDWARE"] == "1"))
    func higherResolutionHardwareRoundTrip() async throws {
        let encoder = try VideoEncoder(width: 2560, height: 1656, decodePreview: true, colorPolicy: .sRGB)
        defer { encoder.close() }
        var pixel: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, 2560, 1656, kCVPixelFormatType_32BGRA,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &pixel) == kCVReturnSuccess)
        let source = try #require(pixel)
        let context = CIContext()
        context.render(CIImage(color: CIColor(red: 0.2, green: 0.3, blue: 0.4))
            .cropped(to: CGRect(x: 0, y: 0, width: 2560, height: 1656)), to: source)
        for index in 0..<3 {
            encoder.encode(source, presentationTime: CMTime(value: Int64(index), timescale: 30))
            try await Task.sleep(nanoseconds: 33_333_333)
        }
        for _ in 0..<100 {
            if encoder.snapshot().inFlight == 0 { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        let encoded = encoder.snapshot()
        let decoded = try #require(encoder.decoder).snapshot()
        #expect(encoded.error == nil && decoded.error == nil)
        #expect(encoded.inFlight == 0 && encoded.encoded > 0)
        #expect(decoded.hardware && decoded.decoded == encoded.encoded)
        #expect(decoded.width == 2560 && decoded.height == 1656)
    }

    @Test func emptyDecoderHasNoImageOrNonfiniteMetrics() {
        let decoder = VideoDecoder()
        #expect(decoder.snapshot().image == nil)
        #expect((decoder.snapshot().metadata["meanDecodeMilliseconds"] as? Double) == 0)
        decoder.close()
        decoder.close()
        #expect(decoder.snapshot().decoded == 0)
        #expect(decoder.snapshot().error == nil)
    }

    @Test func rejectsUncompressedSamplesWithoutStartingDecoder() throws {
        var pixel: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, 8, 8, kCVPixelFormatType_32BGRA,
            nil, &pixel) == kCVReturnSuccess)
        let buffer = try #require(pixel)
        var format: CMVideoFormatDescription?
        #expect(CMVideoFormatDescriptionCreateForImageBuffer(allocator: nil, imageBuffer: buffer,
            formatDescriptionOut: &format) == noErr)
        var timing = CMSampleTimingInfo(duration: .invalid, presentationTimeStamp: .zero, decodeTimeStamp: .invalid)
        var sample: CMSampleBuffer?
        #expect(CMSampleBufferCreateReadyWithImageBuffer(allocator: nil, imageBuffer: buffer,
            formatDescription: try #require(format), sampleTiming: &timing, sampleBufferOut: &sample) == noErr)
        let decoder = VideoDecoder()
        decoder.decode(try #require(sample))
        #expect(decoder.snapshot().error != nil)
        #expect(!decoder.snapshot().hardware)
        #expect(decoder.snapshot().submitted == 0)
        #expect(decoder.snapshot().image == nil)
    }

    @Test(.enabled(if: ProcessInfo.processInfo.environment["NOCOMPUTER_TEST_HARDWARE"] == "1"))
    func hardwareRoundTripPreservesPatternAndRecreates() async throws {
        // Two independent sessions exercise recreation without desktop capture.
        for _ in 0..<2 {
            let encoder = try VideoEncoder(width: 320, height: 240, decodePreview: true)
            defer { encoder.close() }
            let decoder = try #require(encoder.decoder)
            var buffer: CVPixelBuffer?
            #expect(CVPixelBufferCreate(kCFAllocatorDefault, 320, 240, kCVPixelFormatType_32BGRA,
                [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
            let source = try #require(buffer)
            #expect(CVPixelBufferLockBaseAddress(source, []) == kCVReturnSuccess)
            let base = try #require(CVPixelBufferGetBaseAddress(source)).assumingMemoryBound(to: UInt8.self)
            let stride = CVPixelBufferGetBytesPerRow(source)
            // Upper half red, lower half blue. Encode pixels are never mutated
            // after submission. Includes orientation and channel-order checks.
            for y in 0..<240 {
                for x in 0..<320 {
                    let offset = y * stride + x * 4
                    base[offset] = y < 120 ? 0 : 255
                    base[offset + 1] = 0
                    base[offset + 2] = y < 120 ? 255 : 0
                    base[offset + 3] = 255
                }
            }
            CVPixelBufferUnlockBaseAddress(source, [])
            for index in 0..<12 {
                encoder.encode(source, presentationTime: CMTime(value: Int64(index), timescale: 30))
                try await Task.sleep(nanoseconds: 33_333_333)
            }
            for _ in 0..<100 {
                if encoder.snapshot().inFlight == 0 { break }
                try await Task.sleep(nanoseconds: 20_000_000)
            }
            let encoded = encoder.snapshot()
            let decoded = decoder.snapshot()
            #expect(encoded.error == nil)
            #expect(decoded.error == nil)
            #expect(encoded.inFlight == 0)
            #expect(encoded.peakInFlight <= VideoEncoder.maximumInFlight)
            #expect(encoded.encoded > 0)
            #expect(decoded.hardware)
            #expect(decoded.decoded == encoded.encoded)
            #expect(decoded.submitted == decoded.decoded)
            #expect(decoded.width == 320 && decoded.height == 240)
            let image = try #require(decoded.image)
            let original = try #require(decoded.sourceImage)
            #expect(original.width == image.width && original.height == image.height)
            #expect(decoded.comparisonPTS != nil)
            #expect(JSONSerialization.isValidJSONObject(decoded.comparisonMetadata))
            let context = CIContext()
            func pixel(y: Int) -> [UInt8] {
                var bytes = [UInt8](repeating: 0, count: 4)
                bytes.withUnsafeMutableBytes { storage in
                    context.render(CIImage(cgImage: image), toBitmap: storage.baseAddress!, rowBytes: 4,
                        bounds: CGRect(x: 160, y: y, width: 1, height: 1), format: .RGBA8,
                        colorSpace: CGColorSpaceCreateDeviceRGB())
                }
                return bytes
            }
            // Core Image uses bottom-up coordinates; source rows are top-down.
            let top = pixel(y: 180)
            let bottom = pixel(y: 60)
            #expect(top[0] > 200 && top[1] < 40 && top[2] < 40)
            #expect(bottom[2] > 200 && bottom[0] < 40 && bottom[1] < 40)
            encoder.close()
            #expect(decoder.snapshot().image == nil)
            #expect(decoder.snapshot().sourceImage == nil)
            #expect(decoder.snapshot().decodedImageColorSpace != "none")
            #expect(!decoder.snapshot().comparisonMetrics.isEmpty)
            #expect(decoder.snapshot().decoded == decoded.decoded)
            encoder.close()
        }
    }

    @Test(.enabled(if: ProcessInfo.processInfo.environment["NOCOMPUTER_TEST_HARDWARE"] == "1"), arguments: [false, true])
    func explicitSRGBPreservesGrayRamp(explicitSourceTransfer: Bool) async throws {
        let encoder = try VideoEncoder(width: 320, height: 240, decodePreview: true, colorPolicy: .sRGB)
        defer { encoder.close() }
        let decoder = try #require(encoder.decoder)
        var buffer: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, 320, 240, kCVPixelFormatType_32BGRA,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &buffer) == kCVReturnSuccess)
        let source = try #require(buffer)
        let space = try #require(CGColorSpace(name: CGColorSpace.sRGB))
        CVBufferSetAttachment(source, kCVImageBufferCGColorSpaceKey, space, .shouldPropagate)
        if explicitSourceTransfer {
            CVBufferSetAttachment(source, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
            CVBufferSetAttachment(source, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_sRGB, .shouldPropagate)
        }
        let gray: [UInt8] = [0, 16, 32, 64, 96, 128, 160, 192, 224, 255]
        #expect(CVPixelBufferLockBaseAddress(source, []) == kCVReturnSuccess)
        let base = try #require(CVPixelBufferGetBaseAddress(source)).assumingMemoryBound(to: UInt8.self)
        let stride = CVPixelBufferGetBytesPerRow(source)
        for y in 0..<240 {
            for x in 0..<320 {
                let offset = y * stride + x * 4
                for channel in 0..<3 { base[offset + channel] = gray[x / 32] }
                base[offset + 3] = 255
            }
        }
        CVPixelBufferUnlockBaseAddress(source, [])
        for index in 0..<12 {
            encoder.encode(source, presentationTime: CMTime(value: Int64(index), timescale: 30))
            try await Task.sleep(nanoseconds: 33_333_333)
        }
        for _ in 0..<100 {
            if encoder.snapshot().inFlight == 0 { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(encoder.snapshot().error == nil)
        #expect(decoder.snapshot().error == nil)
        let decoded = try #require(decoder.snapshot().image)
        let original = try #require(decoder.snapshot().sourceImage)
        let context = CIContext()
        func values(_ image: CGImage) -> [Int] {
            gray.indices.map { index in
                var bytes = [UInt8](repeating: 0, count: 4)
                bytes.withUnsafeMutableBytes { memory in
                    context.render(CIImage(cgImage: image), toBitmap: memory.baseAddress!, rowBytes: 4,
                        bounds: CGRect(x: index * 32 + 16, y: 120, width: 1, height: 1), format: .RGBA8, colorSpace: space)
                }
                #expect(abs(Int(bytes[0]) - Int(bytes[1])) <= 2)
                #expect(abs(Int(bytes[0]) - Int(bytes[2])) <= 2)
                return Int(bytes[0])
            }
        }
        let raw = values(original)
        let output = values(decoded)
        print("Synthetic sRGB gray ramp (explicit source transfer=\(explicitSourceTransfer)): source=\(raw), decoded=\(output)")
        for index in gray.indices {
            #expect(abs(raw[index] - Int(gray[index])) <= 2)
            #expect(abs(output[index] - raw[index]) <= 6)
        }
        #expect(encoder.snapshot().encodedColor["primaries"] == "ITU_R_709_2")
        #expect(encoder.snapshot().encodedColor["transfer"] == String(describing: kCVImageBufferTransferFunction_sRGB))
    }
}
