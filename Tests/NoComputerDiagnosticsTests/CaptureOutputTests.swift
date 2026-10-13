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

    private func frame(_ status: SCFrameStatus, width: Int = 8, height: Int = 8,
                       time: Int64 = 42) throws -> CMSampleBuffer {
        var pixel: CVPixelBuffer?
        #expect(CVPixelBufferCreate(kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA,
                                   [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary,
                                   &pixel) == kCVReturnSuccess)
        let buffer = try #require(pixel)
        var format: CMVideoFormatDescription?
        #expect(CMVideoFormatDescriptionCreateForImageBuffer(allocator: kCFAllocatorDefault,
                                                                   imageBuffer: buffer,
                                                                   formatDescriptionOut: &format) == noErr)
        var timing = CMSampleTimingInfo(duration: CMTime(value: 1, timescale: 30),
                                       presentationTimeStamp: CMTime(value: time, timescale: 30),
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

    private func send(_ status: SCFrameStatus, to output: CaptureOutput, width: Int = 8, height: Int = 8,
                      time: Int64 = 42) throws {
        // The callback does not use the stream instance; use a minimal local
        // stream without starting capture or requesting screen permissions.
        let stream = SCStream(filter: SCContentFilter(), configuration: SCStreamConfiguration(), delegate: nil)
        output.stream(stream, didOutputSampleBuffer: try frame(status, width: width, height: height, time: time),
                      of: .screen)
    }

    private func waitFor(_ condition: () -> Bool) async throws {
        for _ in 0..<200 where !condition() { try await Task.sleep(nanoseconds: 10_000_000) }
    }

    @Test func keyframeRequestOnAStillScreenRepeatsTheLastFrameAsAnIDR() async throws {
        let encoder = try VideoEncoder(width: 320, height: 240)
        defer { encoder.close() }
        let output = CaptureOutput(encoder: encoder)
        try send(.complete, to: output, width: 320, height: 240)
        try await waitFor { encoder.snapshot().encoded == 1 }
        output.requestKeyframe(queue: DispatchQueue(label: "test"), delay: .milliseconds(10))
        try await waitFor { encoder.snapshot().encoded == 2 }
        let state = encoder.snapshot()
        #expect(state.encoded == 2)
        #expect(state.keyframes == 2)
        #expect(state.error == nil)
    }

    @Test func keyframeRequestUsesANewFrameInsteadOfRepeating() async throws {
        let encoder = try VideoEncoder(width: 320, height: 240)
        defer { encoder.close() }
        let output = CaptureOutput(encoder: encoder)
        try send(.complete, to: output, width: 320, height: 240, time: 1)
        try await waitFor { encoder.snapshot().encoded == 1 }
        output.requestKeyframe(queue: DispatchQueue(label: "test"), delay: .milliseconds(200))
        try send(.complete, to: output, width: 320, height: 240, time: 2)
        try await Task.sleep(nanoseconds: 500_000_000)
        let state = encoder.snapshot()
        #expect(state.encoded == 2)
        #expect(state.keyframes == 2)
        #expect(state.error == nil)
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

    @Test func keyDownIsTimedToTheNextCompleteFrameOnly() throws {
        let output = CaptureOutput()
        output.markKeyDown()
        output.markKeyDown() // a second key before any frame keeps the first time
        try send(.idle, to: output)
        #expect(output.snapshot().keyToFrameCount == 0)
        try send(.complete, to: output)
        try send(.complete, to: output, time: 43)
        let state = output.snapshot()
        #expect(state.keyToFrameCount == 1)
        #expect(state.keyToFrameMaxMilliseconds >= 0)
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
