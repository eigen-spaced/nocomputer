import CoreImage
import Foundation
import VideoToolbox

struct DecoderSnapshot {
    var submitted = 0
    var decoded = 0
    var hardware = false
    var width = 0
    var height = 0
    var totalMilliseconds = 0.0
    var maxMilliseconds = 0.0
    var image: CGImage?
    var sourceImage: CGImage?
    var comparisonPTS: Double?
    var decodedColor: [String: String] = [:]
    var sourceImageColorSpace = "none"
    var decodedImageColorSpace = "none"
    var comparisonMetrics: [String: Any] = [:]
    var error: String?

    // Safe to retain after closing the review: contains no images or samples.
    var comparisonMetadata: [String: Any] {
        ["PTS": comparisonPTS.map { String($0) } ?? "none",
         "sourceImageColorSpace": sourceImageColorSpace,
         "decodedImageColorSpace": decodedImageColorSpace,
         "metrics": comparisonMetrics]
    }

    var metadata: [String: Any] {
        ["submitted": submitted, "decoded": decoded, "hardware": hardware,
         "width": width, "height": height, "error": error ?? "none",
         "meanDecodeMilliseconds": decoded > 0 ? totalMilliseconds / Double(decoded) : 0.0,
         "maxDecodeMilliseconds": maxMilliseconds,
         "decodedColor": decodedColor,
         "comparisonPTS": comparisonPTS.map { String($0) } ?? "none",
         "sourceImageColorSpace": sourceImageColorSpace,
         "decodedImageColorSpace": decodedImageColorSpace,
         "comparisonMetrics": comparisonMetrics,
         "mode": "synchronous local diagnostic; preview copies at 2 Hz"]
    }

    var summary: String {
        String(format: "Decode hardware=%@; submitted=%d; decoded=%d; size=%d×%d; decode mean/max=%.2f/%.2f ms",
               hardware ? "yes" : "no", submitted, decoded, width, height,
               decoded > 0 ? totalMilliseconds / Double(decoded) : 0, maxMilliseconds)
            + (error.map { "; error=\($0)" } ?? "")
    }
}

// Decode every encoded frame, without an encoded-frame queue or dependency-
// breaking drops. Synchronous decoding intentionally backpressures the local
// encoder callback. This is a correctness/readability diagnostic, not the
// architecture or renderer for the eventual Windows client.
final class VideoDecoder {
    private let submissionLock = NSLock()
    private let stateLock = NSLock()
    private let context = CIContext(options: [.cacheIntermediates: false])
    private var session: VTDecompressionSession?
    private var format: CMFormatDescription?
    private var state = DecoderSnapshot()
    private var closed = false
    private var lastPTS: CMTime?
    private var lastPreview: TimeInterval = 0

    func snapshot() -> DecoderSnapshot {
        stateLock.lock()
        defer { stateLock.unlock() }
        return state
    }

    private func fail(_ message: String) {
        stateLock.lock()
        state.error = message
        state.image = nil
        state.sourceImage = nil
        stateLock.unlock()
    }

    private func prepare(format: CMFormatDescription) throws {
        let spec = [kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true] as CFDictionary
        let attributes = [kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
                          kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
        let result = VTDecompressionSessionCreate(allocator: nil, formatDescription: format,
            decoderSpecification: spec, imageBufferAttributes: attributes,
            outputCallback: nil, decompressionSessionOut: &session)
        guard result == noErr, let session else {
            throw NSError(domain: "NoComputer.Decoder", code: Int(result),
                userInfo: [NSLocalizedDescriptionKey: "Create hardware decoder failed (OSStatus \(result))"])
        }
        var hardware: Unmanaged<CFTypeRef>?
        let query = VTSessionCopyProperty(session, key: kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
            allocator: nil, valueOut: &hardware)
        guard query == noErr, (hardware?.takeRetainedValue() as? NSNumber)?.boolValue == true else {
            throw NSError(domain: "NoComputer.Decoder", code: Int(query),
                userInfo: [NSLocalizedDescriptionKey: "Hardware decoder selection could not be verified"])
        }
        self.format = format
        stateLock.lock()
        state.hardware = true
        stateLock.unlock()
    }

    func decode(_ sample: CMSampleBuffer, sourceImage: CGImage? = nil) {
        submissionLock.lock()
        defer { submissionLock.unlock() }
        guard !closed, snapshot().error == nil else { return }
        let pts = sample.presentationTimeStamp
        guard sample.isValid, CMSampleBufferDataIsReady(sample),
              CMSampleBufferGetNumSamples(sample) == 1,
              let format = sample.formatDescription,
              CMFormatDescriptionGetMediaSubType(format) == kCMVideoCodecType_H264,
              pts.isNumeric, lastPTS.map({ CMTimeCompare(pts, $0) > 0 }) ?? true else {
            fail("Invalid H.264 sample or non-increasing timestamp")
            return
        }
        if session == nil {
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
                as? [[CFString: Any]]
            guard !(attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool ?? false) else {
                fail("First decoder sample must be a keyframe")
                return
            }
            do { try prepare(format: format) }
            catch { fail(error.localizedDescription); return }
        } else if let previous = self.format, !CMFormatDescriptionEqual(previous, otherFormatDescription: format) {
            fail("Format changed; stop/start this diagnostic to recreate the decoder")
            return
        }
        guard let session else { return }
        lastPTS = pts
        stateLock.lock()
        state.submitted += 1
        stateLock.unlock()
        let start = ProcessInfo.processInfo.systemUptime
        // Both asynchronous and temporal processing flags are OFF. Apple
        // guarantees callback completion before this call returns in this mode.
        let result = VTDecompressionSessionDecodeFrame(session, sampleBuffer: sample,
            flags: [], infoFlagsOut: nil) { [weak self] status, flags, buffer, _, _ in
                guard let self else { return }
                guard status == noErr, !flags.contains(.frameDropped), let buffer else {
                    self.fail("Decode callback failed/dropped frame (OSStatus \(status))")
                    return
                }
                let now = ProcessInfo.processInfo.systemUptime
                let elapsed = (now - start) * 1000
                var preview: CGImage?
                var comparison: [String: Any]?
                if sourceImage != nil || (self.snapshot().sourceImage == nil && now - self.lastPreview >= 0.5) {
                    self.lastPreview = now
                    let image = CIImage(cvPixelBuffer: buffer)
                    preview = self.context.createCGImage(image, from: image.extent)
                    guard preview != nil else {
                        self.fail("Decoded frame could not be converted for visual review")
                        return
                    }
                    if let sourceImage, let preview {
                        comparison = ImageComparison.measure(source: sourceImage, decoded: preview, context: self.context)
                    }
                }
                self.stateLock.lock()
                self.state.decoded += 1
                self.state.width = CVPixelBufferGetWidth(buffer)
                self.state.height = CVPixelBufferGetHeight(buffer)
                self.state.totalMilliseconds += elapsed
                self.state.maxMilliseconds = max(self.state.maxMilliseconds, elapsed)
                self.state.decodedColor = VideoColor.bufferMetadata(buffer)
                if let preview {
                    self.state.image = preview
                    self.state.sourceImage = sourceImage
                    self.state.comparisonPTS = pts.seconds
                    self.state.sourceImageColorSpace = sourceImage?.colorSpace?.name.map { String($0) } ?? "unnamed"
                    self.state.decodedImageColorSpace = preview.colorSpace?.name.map { String($0) } ?? "unnamed"
                    if let comparison { self.state.comparisonMetrics = comparison }
                }
                self.stateLock.unlock()
            }
        if result != noErr { fail("Decode submission failed (OSStatus \(result))") }
    }

    func close() {
        submissionLock.lock()
        defer { submissionLock.unlock() }
        closed = true
        if let session { VTDecompressionSessionInvalidate(session) }
        session = nil
        format = nil
        stateLock.lock()
        state.image = nil
        state.sourceImage = nil
        stateLock.unlock()
    }

    deinit { close() }
}
