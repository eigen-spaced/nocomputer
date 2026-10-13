import AppKit
import CoreImage
import ScreenCaptureKit

struct CaptureSnapshot {
    var callbacks = 0
    var completeFrames = 0
    var status = "No frames"
    var lastCallback: TimeInterval?
    var lastComplete: TimeInterval?
    var presentationTime: Double?
    var image: CGImage?
    var error: String?
    var errorDomain: String?
    var errorCode: Int?
    var stoppedAt: TimeInterval?
    var sourceVisible = false

    func previewIsCurrent(at now: TimeInterval) -> Bool {
        // Idle callbacks mean the existing image remains valid. Complete-frame
        // age alone cannot distinguish a static desktop from a stalled stream.
        guard stoppedAt == nil, image != nil, let callback = lastCallback else { return false }
        return now >= callback && now - callback < 2
    }
}

// Only this object crosses the capture queue/UI boundary. Retain one preview,
// never a queue of sample buffers; the UI polls at 2 Hz.
final class CaptureOutput: NSObject, SCStreamOutput, SCStreamDelegate {
    private let lock = NSLock()
    private var state = CaptureSnapshot()
    private var lastPreview: TimeInterval = 0
    private let context = CIContext(options: [.cacheIntermediates: false])
    private let previewEnabled: Bool
    let encoder: VideoEncoder?
    // Called once, on the capture queue, when the system ends the stream.
    private let onStop: ((String) -> Void)?

    init(previewEnabled: Bool = true, encoder: VideoEncoder? = nil, onStop: ((String) -> Void)? = nil) {
        self.previewEnabled = previewEnabled
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
        state.status = "Stopped with error"
        state.image = nil
        state.sourceVisible = false
        lock.unlock()
        encoder?.close()
        onStop?(error.localizedDescription)
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
                of type: SCStreamOutputType) {
        guard type == .screen, sampleBuffer.isValid else { return }
        let now = ProcessInfo.processInfo.systemUptime
        // A terminal delegate callback wins over any already queued frames.
        guard snapshot().stoppedAt == nil else { return }
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
            as? [[SCStreamFrameInfo: Any]]
        let raw = attachments?.first?[.status] as? Int
        let status = raw.flatMap(SCFrameStatus.init(rawValue:))
        if status == .complete, let buffer = sampleBuffer.imageBuffer {
            // Pass the original CVPixelBuffer, not the diagnostic CGImage copy.
            // VideoToolbox may perform its own BGRA conversion internally.
            encoder?.encode(buffer, presentationTime: sampleBuffer.presentationTimeStamp)
        }
        var preview: CGImage?
        if previewEnabled, status == .complete, now - lastPreview >= 0.5,
           let buffer = sampleBuffer.imageBuffer {
            lastPreview = now
            let image = CIImage(cvPixelBuffer: buffer)
            // This diagnostic preview intentionally copies at 2 Hz. It is not
            // the future capture-to-encoder path and must not be benchmarked as it.
            preview = context.createCGImage(image, from: image.extent)
        }
        lock.lock()
        defer { lock.unlock() }
        guard state.stoppedAt == nil else { return }
        state.callbacks += 1
        state.lastCallback = now
        state.presentationTime = sampleBuffer.presentationTimeStamp.seconds
        state.status = status.map { String(describing: $0) } ?? "Unknown status"
        if status == .complete {
            state.sourceVisible = true
            state.completeFrames += 1
            state.lastComplete = now
            if let preview { state.image = preview }
        } else if status != .idle {
            state.sourceVisible = false
            state.image = nil
        }
    }
}

@MainActor
final class CaptureSession {
    enum FilterMode: String {
        case excludeApplication = "exclude diagnostic application"
        case wholeDisplay = "whole display, no application filter"
    }

    private(set) var filterMode: FilterMode = .excludeApplication
    private(set) var stream: SCStream?
    private(set) var output: CaptureOutput?
    private(set) var displayDescription = "No display selected"
    private let queue = DispatchQueue(label: "dev.nocomputer.capture", qos: .userInitiated)

    init(stream: SCStream? = nil, output: CaptureOutput? = nil) {
        self.stream = stream
        self.output = output
    }

    // Keep the terminal snapshot available for export, but release the dead
    // stream so Start is allowed to construct a fresh one after unlocking.
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

    func start(mode: FilterMode = .excludeApplication, encodeVideo: Bool = false, decodePreview: Bool = false,
               colorPolicy: VideoColorPolicy = .systemDefault, maximumWidth: Int = 1600,
               sampleConsumer: ((CMSampleBuffer) -> Void)? = nil, onStop: ((String) -> Void)? = nil) async throws {
        retireStoppedStream()
        guard stream == nil else { return }
        // Do not use an interactive content picker: this experiment must also
        // test enumeration and fresh capture after the user locks the session.
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        guard let display = content.displays.first(where: { $0.displayID == CGMainDisplayID() })
                ?? content.displays.first else {
            throw NSError(domain: "NoComputer", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "No capturable display found"])
        }
        let filter: SCContentFilter
        switch mode {
        case .excludeApplication:
            let ownApps = content.applications.filter { $0.processID == ProcessInfo.processInfo.processIdentifier }
            filter = SCContentFilter(display: display, excludingApplications: ownApps, exceptingWindows: [])
        case .wholeDisplay:
            filter = SCContentFilter(display: display, excludingWindows: [])
        }
        let configuration = SCStreamConfiguration()
        let size = try Self.streamSize(points: filter.contentRect.size, pixelScale: CGFloat(filter.pointPixelScale),
            maximumWidth: maximumWidth, even: encodeVideo)
        configuration.width = Int(size.width)
        configuration.height = Int(size.height)
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 30)
        configuration.queueDepth = 3
        configuration.showsCursor = true
        configuration.capturesAudio = false
        configuration.pixelFormat = kCVPixelFormatType_32BGRA
        if colorPolicy == .sRGB {
            configuration.colorSpaceName = CGColorSpace.sRGB
            configuration.colorMatrix = kCVImageBufferYCbCrMatrix_ITU_R_709_2
        }
        // A whole-display preview would recursively capture itself. This mode
        // collects frame metadata only, without converting pixels for the UI.
        let encoder = encodeVideo ? try VideoEncoder(width: configuration.width, height: configuration.height,
            decodePreview: decodePreview, colorPolicy: colorPolicy, sampleConsumer: sampleConsumer) : nil
        // Disable preview copies during the encoder experiment to isolate its timing.
        let receiver = CaptureOutput(previewEnabled: mode != .wholeDisplay && !encodeVideo, encoder: encoder,
            onStop: onStop)
        let candidate = SCStream(filter: filter, configuration: configuration, delegate: receiver)
        try candidate.addStreamOutput(receiver, type: .screen, sampleHandlerQueue: queue)
        // Hold references during asynchronous startup, including error callbacks.
        output = receiver
        stream = candidate
        filterMode = mode
        do {
            try await candidate.startCapture()
            displayDescription = "Display \(display.displayID): \(display.width)×\(display.height); content points=\(filter.contentRect.size), pixelScale=\(filter.pointPixelScale); stream \(configuration.width)×\(configuration.height), 30 fps requested; filter=\(mode.rawValue); color=\(colorPolicy.rawValue)"
        } catch {
            encoder?.close()
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
