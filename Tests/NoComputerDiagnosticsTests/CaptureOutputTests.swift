import Testing
import ScreenCaptureKit
@testable import NoComputerDiagnostics

struct CaptureOutputTests {
    @Test @MainActor func streamResolutionUsesBackingPixelsWithoutChangingDisplay() throws {
        let points = CGSize(width: 1710, height: 1107)
        #expect(try CaptureSession.streamSize(points: points, pixelScale: 2, maximumWidth: 1600, even: true)
            == CGSize(width: 1600, height: 1034))
        #expect(try CaptureSession.streamSize(points: points, pixelScale: 2, maximumWidth: 2560, even: true)
            == CGSize(width: 2560, height: 1656))
        #expect(try CaptureSession.streamSize(points: CGSize(width: 1280, height: 720), pixelScale: 1,
            maximumWidth: 2560, even: true) == CGSize(width: 1280, height: 720))
        #expect(throws: (any Error).self) {
            try CaptureSession.streamSize(points: .zero, pixelScale: 2, maximumWidth: 1600, even: true)
        }
    }

    private func frame(_ status: SCFrameStatus) throws -> CMSampleBuffer {
        var pixel: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, 8, 8, kCVPixelFormatType_32BGRA,
                                   nil, &pixel) == kCVReturnSuccess)
        let buffer = try #require(pixel)
        var format: CMVideoFormatDescription?
        #expect(CMVideoFormatDescriptionCreateForImageBuffer(allocator: kCFAllocatorDefault,
                                                                   imageBuffer: buffer,
                                                                   formatDescriptionOut: &format) == noErr)
        var timing = CMSampleTimingInfo(duration: CMTime(value: 1, timescale: 30),
                                       presentationTimeStamp: CMTime(value: 42, timescale: 30),
                                       decodeTimeStamp: .invalid)
        var sample: CMSampleBuffer?
        let description = try #require(format)
        #expect(CMSampleBufferCreateReadyWithImageBuffer(allocator: kCFAllocatorDefault,
                                                              imageBuffer: buffer,
                                                              formatDescription: description,
                                                              sampleTiming: &timing,
                                                              sampleBufferOut: &sample) == noErr)
        let result = try #require(sample)
        let attachments = try #require(CMSampleBufferGetSampleAttachmentsArray(result, createIfNecessary: true))
        let dictionary = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
        let bridged = dictionary as NSMutableDictionary
        bridged[SCStreamFrameInfo.status.rawValue] = status.rawValue
        return result
    }

    private func send(_ status: SCFrameStatus, to output: CaptureOutput) throws {
        // The callback does not use the stream instance; use a minimal local
        // stream without starting capture or requesting screen permissions.
        let stream = SCStream(filter: SCContentFilter(), configuration: SCStreamConfiguration(), delegate: nil)
        output.stream(stream, didOutputSampleBuffer: try frame(status), of: .screen)
    }

    @Test func onlyCompleteFramesAreCounted() throws {
        let output = CaptureOutput()
        try send(.complete, to: output)
        let completeTime = output.snapshot().lastComplete
        for status: SCFrameStatus in [.idle, .blank, .suspended] { try send(status, to: output) }
        let state = output.snapshot()
        #expect(state.callbacks == 4)
        #expect(state.completeFrames == 1)
        #expect(state.lastComplete == completeTime)
    }

    @Test func terminalErrorWinsOverLateFrames() throws {
        let output = CaptureOutput()
        try send(.complete, to: output)
        let stream = SCStream(filter: SCContentFilter(), configuration: SCStreamConfiguration(), delegate: nil)
        let error = NSError(domain: SCStreamErrorDomain, code: -3815)
        output.stream(stream, didStopWithError: error)
        try send(.complete, to: output)
        let state = output.snapshot()
        #expect(state.callbacks == 1)
        #expect(state.stoppedAt != nil)
        #expect(state.errorDomain == SCStreamErrorDomain)
        #expect(state.errorCode == -3815)
    }

    @Test @MainActor func stoppedStreamDoesNotBlockRestartOrStop() async throws {
        let output = CaptureOutput()
        let stream = SCStream(filter: SCContentFilter(), configuration: SCStreamConfiguration(), delegate: nil)
        let session = CaptureSession(stream: stream, output: output)
        #expect(!session.retireStoppedStream())
        output.stream(stream, didStopWithError: NSError(domain: SCStreamErrorDomain, code: -3815))
        #expect(session.retireStoppedStream())
        #expect(session.stream == nil)
        #expect(session.output === output)
        #expect(!session.retireStoppedStream())
        try await session.stop() // Must not call stopCapture on the dead stream.
        #expect(session.output == nil)
        try await session.stop() // Stop is idempotent.
    }

    @Test @MainActor func onlyAlreadyStoppedErrorIsTreatedAsSuccessfulStop() {
        #expect(CaptureSession.isAlreadyStopped(NSError(domain: SCStreamErrorDomain, code: -3808)))
        #expect(!CaptureSession.isAlreadyStopped(NSError(domain: "other", code: -3808)))
        #expect(!CaptureSession.isAlreadyStopped(NSError(domain: SCStreamErrorDomain, code: -3811)))
    }
}
