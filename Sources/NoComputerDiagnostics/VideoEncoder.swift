import Foundation
import CoreImage
import VideoToolbox

struct EncoderSnapshot {
    var submitted = 0
    var encoded = 0
    var bytes = 0
    var keyframes = 0
    var rawDrops = 0
    var encoderDrops = 0
    var inFlight = 0
    var peakInFlight = 0
    var totalMilliseconds = 0.0
    var maxMilliseconds = 0.0
    var hardware = false
    var frameDelayLimitSupported = false
    var parameterSetCount = 0
    var nalLengthBytes = 0
    var error: String?
    var sourceColor: [String: String] = [:]
    var encodedColor: [String: String] = [:]
    var colorPolicy = VideoColorPolicy.systemDefault.rawValue

    var metadata: [String: Any] {
        ["submitted": submitted, "encoded": encoded, "bytes": bytes,
         "keyframes": keyframes, "rawDrops": rawDrops, "encoderDrops": encoderDrops,
         "inFlight": inFlight, "peakInFlight": peakInFlight,
         "meanEncodeMilliseconds": encoded > 0 ? totalMilliseconds / Double(encoded) : 0.0,
         "maxEncodeMilliseconds": maxMilliseconds, "hardware": hardware,
         "error": error ?? "none", "requestedBitrate": VideoEncoder.bitrate,
         "maximumInFlight": VideoEncoder.maximumInFlight,
         "frameDelayLimitSupported": frameDelayLimitSupported,
         "parameterSetCount": parameterSetCount, "nalLengthBytes": nalLengthBytes,
         "sourceColor": sourceColor, "encodedColor": encodedColor, "colorPolicy": colorPolicy,
         "sampleFraming": "AVCC length-prefixed"]
    }

    var summary: String {
        let mean = encoded > 0 ? totalMilliseconds / Double(encoded) : 0
        return String(format: "H.264 hardware=%@; encoded=%d; bytes=%d; keyframes=%d; rawDrops=%d; encoderDrops=%d; pending=%d (peak %d); encode mean/max=%.2f/%.2f ms",
                      hardware ? "yes" : "no", encoded, bytes, keyframes, rawDrops,
                      encoderDrops, inFlight, peakInFlight, mean, maxMilliseconds)
            + (error.map { "; error=\($0)" } ?? "")
    }
}

// Samples are measured then optionally decoded locally. No video file or network.
// Submission and invalidation serialize separately from callback statistics:
// VideoToolbox may deliver a callback before encodeFrame returns.
final class VideoEncoder {
    static let maximumInFlight = 3
    static let bitrate = 8_000_000
    private let submissionLock = NSLock()
    private let stateLock = NSLock()
    private var session: VTCompressionSession?
    private var state = EncoderSnapshot()
    private var pending: [Int: TimeInterval] = [:]
    private var completing: Set<Int> = []
    private var sourcePreviews: [Int: CGImage] = [:]
    private var lastPreview: TimeInterval = 0
    private let context = CIContext(options: [.cacheIntermediates: false])
    private var nextID = 0
    private var lastPTS: CMTime?
    // Set by requestKeyframe(); consumed by the next frame actually submitted,
    // so repeated requests before then coalesce into one IDR.
    private var keyframeRequested = false
    private let width: Int
    private let height: Int
    let decoder: VideoDecoder?
    private let sampleConsumer: ((CMSampleBuffer) -> Void)?

    // The diagnostics keep the Milestone 1 two-second interval. Remote access asks
    // for keyframes on demand, so it uses a long interval only as a safety net.
    static let diagnosticKeyframeSeconds = 2
    static let remoteKeyframeSeconds = 20

    init(width: Int, height: Int, decodePreview: Bool = false, colorPolicy: VideoColorPolicy = .systemDefault,
         keyframeSeconds: Int = VideoEncoder.diagnosticKeyframeSeconds,
         sampleConsumer: ((CMSampleBuffer) -> Void)? = nil) throws {
        guard width > 0, height > 0, width <= 8192, height <= 8192, (1...60).contains(keyframeSeconds) else {
            throw NSError(domain: "NoComputer.Encoder", code: -1,
                          userInfo: [NSLocalizedDescriptionKey: "Invalid encoder dimensions"])
        }
        self.width = width
        self.height = height
        self.decoder = decodePreview ? VideoDecoder() : nil
        self.sampleConsumer = sampleConsumer
        state.colorPolicy = colorPolicy.rawValue
        let spec = [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true] as CFDictionary
        try Self.check(VTCompressionSessionCreate(allocator: nil, width: Int32(width), height: Int32(height),
            codecType: kCMVideoCodecType_H264, encoderSpecification: spec,
            imageBufferAttributes: nil, compressedDataAllocator: nil,
            outputCallback: nil, refcon: nil, compressionSessionOut: &session), "create hardware encoder")
        guard let session else { throw NSError(domain: "NoComputer.Encoder", code: -2) }
        do {
            if colorPolicy == .sRGB {
                for (key, value) in [
                    (kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2),
                    (kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_sRGB),
                    (kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2)
                ] {
                    try Self.check(VTSessionSetProperty(session, key: key, value: value), "set \(key)")
                }
            }
            for (key, value) in [
                (kVTCompressionPropertyKey_RealTime, kCFBooleanTrue!),
                (kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse!),
                (kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Main_AutoLevel),
                (kVTCompressionPropertyKey_AverageBitRate, NSNumber(value: Self.bitrate)),
                (kVTCompressionPropertyKey_ExpectedFrameRate, NSNumber(value: 30)),
                (kVTCompressionPropertyKey_MaxKeyFrameInterval, NSNumber(value: 30 * keyframeSeconds)),
                (kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, NSNumber(value: keyframeSeconds))
            ] as [(CFString, CFTypeRef)] {
                try Self.check(VTSessionSetProperty(session, key: key, value: value), "set \(key)")
            }
            let delayResult = VTSessionSetProperty(session, key: kVTCompressionPropertyKey_MaxFrameDelayCount,
                value: NSNumber(value: Self.maximumInFlight))
            // Some hardware encoders do not support this optional property.
            // The application admission bound remains enforced independently.
            if delayResult == kVTPropertyNotSupportedErr {
                state.frameDelayLimitSupported = false
            } else {
                try Self.check(delayResult, "set maximum frame delay")
                state.frameDelayLimitSupported = true
            }
            try Self.check(VTCompressionSessionPrepareToEncodeFrames(session), "prepare encoder")
            var value: Unmanaged<CFTypeRef>?
            try Self.check(VTSessionCopyProperty(session,
                key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                allocator: nil, valueOut: &value), "query hardware encoder")
            state.hardware = (value?.takeRetainedValue() as? NSNumber)?.boolValue == true
            guard state.hardware else { throw NSError(domain: "NoComputer.Encoder", code: -3,
                userInfo: [NSLocalizedDescriptionKey: "Hardware encoder was not selected"] ) }
        } catch {
            VTCompressionSessionInvalidate(session)
            self.session = nil
            throw error
        }
    }

    private static func check(_ status: OSStatus, _ operation: String) throws {
        guard status == noErr else { throw NSError(domain: "NoComputer.Encoder", code: Int(status),
            userInfo: [NSLocalizedDescriptionKey: "\(operation) failed (OSStatus \(status))"]) }
    }

    func snapshot() -> EncoderSnapshot {
        stateLock.lock()
        defer { stateLock.unlock() }
        return state
    }

    func requestKeyframe() {
        submissionLock.lock()
        keyframeRequested = true
        submissionLock.unlock()
    }

    func encode(_ buffer: CVPixelBuffer, presentationTime: CMTime) {
        submissionLock.lock()
        defer { submissionLock.unlock() }
        guard let session else { return }
        stateLock.lock()
        guard state.error == nil else { stateLock.unlock(); return }
        guard CVPixelBufferGetWidth(buffer) == width, CVPixelBufferGetHeight(buffer) == height,
              presentationTime.isNumeric,
              lastPTS.map({ CMTimeCompare(presentationTime, $0) > 0 }) ?? true else {
            state.error = "Unexpected buffer dimensions or non-increasing presentation time"
            stateLock.unlock()
            return
        }
        // Dropping an unencoded source frame cannot break decoder dependencies.
        guard pending.count < Self.maximumInFlight else {
            state.rawDrops += 1
            stateLock.unlock()
            return
        }
        nextID += 1
        let id = nextID
        state.sourceColor = VideoColor.bufferMetadata(buffer)
        // A matched raw/decoded pair, not two independently sampled previews.
        // Diagnostic copies happen before the encode timing starts.
        if decoder != nil, ProcessInfo.processInfo.systemUptime - lastPreview >= 0.5 {
            lastPreview = ProcessInfo.processInfo.systemUptime
            let image = CIImage(cvPixelBuffer: buffer)
            if let preview = context.createCGImage(image, from: image.extent) {
                sourcePreviews[id] = preview
            } else { state.error = "Could not create source comparison image"; stateLock.unlock(); return }
        }
        pending[id] = ProcessInfo.processInfo.systemUptime
        lastPTS = presentationTime
        state.submitted += 1
        state.inFlight = pending.count
        state.peakInFlight = max(state.peakInFlight, state.inFlight)
        stateLock.unlock()
        let properties = keyframeRequested
            ? [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue!] as CFDictionary : nil
        keyframeRequested = false
        var flags = VTEncodeInfoFlags()
        let result = VTCompressionSessionEncodeFrame(session, imageBuffer: buffer,
            presentationTimeStamp: presentationTime, duration: .invalid,
            frameProperties: properties, infoFlagsOut: &flags) { [weak self] status, flags, sample in
                self?.complete(id: id, status: status, flags: flags, sample: sample)
            }
        // Synchronous drops/failures may not produce a callback. Completion is
        // idempotent so synchronous callbacks cannot be counted twice.
        if result != noErr || flags.contains(.frameDropped) {
            complete(id: id, status: result, flags: flags, sample: nil)
        }
    }

    private func complete(id: Int, status: OSStatus, flags: VTEncodeInfoFlags, sample: CMSampleBuffer?) {
        var deliver = false
        stateLock.lock()
        guard let start = pending[id], completing.insert(id).inserted else {
            stateLock.unlock()
            return
        }
        let sourcePreview = sourcePreviews[id]
        defer {
            stateLock.unlock()
            // Never call downstream code under the statistics lock.
            if deliver, let sample {
                decoder?.decode(sample, sourceImage: sourcePreview)
                // Consumers must not reenter encode/close from this callback.
                // The synthetic interoperability test uses this output boundary.
                sampleConsumer?(sample)
            }
            stateLock.lock()
            pending.removeValue(forKey: id)
            completing.remove(id)
            sourcePreviews.removeValue(forKey: id)
            state.inFlight = pending.count
            stateLock.unlock()
        }
        guard status == noErr else {
            state.error = "Encode callback failed (OSStatus \(status))"
            return
        }
        guard !flags.contains(.frameDropped), let sample, CMSampleBufferDataIsReady(sample) else {
            state.encoderDrops += 1
            return
        }
        guard let format = sample.formatDescription,
              CMFormatDescriptionGetMediaSubType(format) == kCMVideoCodecType_H264,
              CMSampleBufferGetTotalSampleSize(sample) > 0 else {
            state.error = "Encoder returned invalid H.264 sample"
            return
        }
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
            as? [[CFString: Any]]
        let notSync = attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool ?? false
        if state.parameterSetCount == 0 {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            var count = 0
            var headerLength: Int32 = 0
            let result = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,
                parameterSetIndex: 0, parameterSetPointerOut: &pointer, parameterSetSizeOut: &size,
                parameterSetCountOut: &count, nalUnitHeaderLengthOut: &headerLength)
            guard result == noErr, count >= 2, size > 0, [1, 2, 4].contains(headerLength) else {
                state.error = "Missing H.264 parameter sets or invalid NAL length size"
                return
            }
            state.parameterSetCount = count
            state.nalLengthBytes = Int(headerLength)
        }
        if !notSync { state.keyframes += 1 }
        state.encodedColor = VideoColor.formatMetadata(format)
        state.encoded += 1
        state.bytes += CMSampleBufferGetTotalSampleSize(sample)
        let milliseconds = (ProcessInfo.processInfo.systemUptime - start) * 1000
        state.totalMilliseconds += milliseconds
        state.maxMilliseconds = max(state.maxMilliseconds, milliseconds)
        deliver = true
    }

    func close() {
        submissionLock.lock()
        defer { submissionLock.unlock() }
        if let session { VTCompressionSessionInvalidate(session) }
        session = nil
        stateLock.lock()
        pending.removeAll()
        completing.removeAll()
        sourcePreviews.removeAll()
        state.inFlight = 0
        stateLock.unlock()
        decoder?.close()
    }

    deinit { close() }
}
