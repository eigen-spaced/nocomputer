import AppKit
import ScreenCaptureKit

struct CaptureSnapshot {
    var callbacks = 0
    var completeFrames = 0
    var lastComplete: TimeInterval?
    var error: String?
    var errorDomain: String?
    var errorCode: Int?
    var stoppedAt: TimeInterval?
}

// Hands complete frames straight to the encoder on the capture queue; only the
// counters cross to the main thread.
final class CaptureOutput: NSObject, SCStreamOutput, SCStreamDelegate {
    private let lock = NSLock()
    private var state = CaptureSnapshot()
    let encoder: VideoEncoder?
    // Called once, on the capture queue, when the system ends the stream.
    private let onStop: ((String) -> Void)?

    init(encoder: VideoEncoder? = nil, onStop: ((String) -> Void)? = nil) {
        self.encoder = encoder
        self.onStop = onStop
        super.init()
    }

    func snapshot() -> CaptureSnapshot {
        lock.lock()
        defer { lock.unlock() }
        return state
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        lock.lock()
        state.error = error.localizedDescription
        let failure = error as NSError
        state.errorDomain = failure.domain
        state.errorCode = failure.code
        state.stoppedAt = ProcessInfo.processInfo.systemUptime
        lock.unlock()
        encoder?.close()
        onStop?(error.localizedDescription)
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
                of type: SCStreamOutputType) {
        guard type == .screen, sampleBuffer.isValid else { return }
        // A terminal delegate callback wins over any already queued frames.
        guard snapshot().stoppedAt == nil else { return }
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
            as? [[SCStreamFrameInfo: Any]]
        let raw = attachments?.first?[.status] as? Int
        let status = raw.flatMap(SCFrameStatus.init(rawValue:))
        if status == .complete, let buffer = sampleBuffer.imageBuffer {
            encoder?.encode(buffer, presentationTime: sampleBuffer.presentationTimeStamp)
        }
        lock.lock()
        defer { lock.unlock() }
        guard state.stoppedAt == nil else { return }
        state.callbacks += 1
        if status == .complete {
            state.completeFrames += 1
            state.lastComplete = ProcessInfo.processInfo.systemUptime
        }
    }
}

@MainActor
final class CaptureSession {
    private(set) var stream: SCStream?
    private(set) var output: CaptureOutput?
    private(set) var displayDescription = "No display selected"
    private(set) var displayID: CGDirectDisplayID?
    private let queue = DispatchQueue(label: "dev.nocomputer.capture", qos: .userInitiated)

    init(stream: SCStream? = nil, output: CaptureOutput? = nil) {
        self.stream = stream
        self.output = output
    }

    // Release the dead stream so a fresh one can start, e.g. after unlocking.
    @discardableResult
    func retireStoppedStream() -> Bool {
        guard stream != nil, output?.snapshot().stoppedAt != nil else { return false }
        output?.encoder?.close()
        stream = nil
        return true
    }

    static func isAlreadyStopped(_ error: Error) -> Bool {
        let failure = error as NSError
        return failure.domain == SCStreamErrorDomain
            && failure.code == SCStreamError.Code.attemptToStopStreamState.rawValue
    }

    static func streamSize(points: CGSize, pixelScale: CGFloat, maximumWidth: Int, even: Bool) throws -> CGSize {
        guard points.width > 0, points.height > 0, pixelScale > 0,
              points.width.isFinite, points.height.isFinite, pixelScale.isFinite,
              [1600, 2560].contains(maximumWidth) else {
            throw NSError(domain: "NoComputer.Capture", code: 2,
                userInfo: [NSLocalizedDescriptionKey: "Invalid source geometry or stream width"])
        }
        let width = min(points.width * pixelScale, CGFloat(maximumWidth))
        let height = points.height * width / points.width
        guard width >= 2, height >= 2, height <= 8192 else {
            throw NSError(domain: "NoComputer.Capture", code: 3,
                userInfo: [NSLocalizedDescriptionKey: "Unsupported stream dimensions"])
        }
        if even { return CGSize(width: Int(width) / 2 * 2, height: Int(height) / 2 * 2) }
        return CGSize(width: Int(width), height: Int(height))
    }

    // Captures the whole main display, including this app's own windows: the
    // remote user sees exactly what is on screen.
    func start(maximumWidth: Int = 1600, keyframeSeconds: Int = VideoEncoder.remoteKeyframeSeconds,
               sampleConsumer: ((CMSampleBuffer) -> Void)? = nil, onStop: ((String) -> Void)? = nil) async throws {
        retireStoppedStream()
        guard stream == nil else { return }
        // No interactive content picker: capture must also start fresh after
        // the user locks the session.
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        guard let display = content.displays.first(where: { $0.displayID == CGMainDisplayID() })
                ?? content.displays.first else {
            throw NSError(domain: "NoComputer", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "No capturable display found"])
        }
        let filter = SCContentFilter(display: display, excludingWindows: [])
        let configuration = SCStreamConfiguration()
        let size = try Self.streamSize(points: filter.contentRect.size, pixelScale: CGFloat(filter.pointPixelScale),
            maximumWidth: maximumWidth, even: true)
        configuration.width = Int(size.width)
        configuration.height = Int(size.height)
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 30)
        configuration.queueDepth = 3
        configuration.showsCursor = true
        configuration.capturesAudio = false
        configuration.pixelFormat = kCVPixelFormatType_32BGRA
        configuration.colorSpaceName = CGColorSpace.sRGB
        configuration.colorMatrix = kCVImageBufferYCbCrMatrix_ITU_R_709_2
        let encoder = try VideoEncoder(width: configuration.width, height: configuration.height,
            keyframeSeconds: keyframeSeconds, sampleConsumer: sampleConsumer)
        let receiver = CaptureOutput(encoder: encoder, onStop: onStop)
        let candidate = SCStream(filter: filter, configuration: configuration, delegate: receiver)
        try candidate.addStreamOutput(receiver, type: .screen, sampleHandlerQueue: queue)
        // Hold references during asynchronous startup, including error callbacks.
        output = receiver
        stream = candidate
        do {
            try await candidate.startCapture()
            displayID = display.displayID
            displayDescription = "Display \(display.displayID): \(display.width)×\(display.height); content points=\(filter.contentRect.size), pixelScale=\(filter.pointPixelScale); stream \(configuration.width)×\(configuration.height), 30 fps requested"
        } catch {
            encoder.close()
            stream = nil
            output = nil
            throw error
        }
    }

    func stop() async throws {
        retireStoppedStream()
        guard let stream else {
            output?.encoder?.close()
            output = nil
            return
        }
        do {
            try await stream.stopCapture()
        } catch {
            // This race occurs when the system ends capture between our state
            // check and stopCapture(). Unrelated failures retain the live handle.
            guard Self.isAlreadyStopped(error) || output?.snapshot().stoppedAt != nil else { throw error }
        }
        self.stream = nil
        output?.encoder?.close()
        output = nil
    }
}
