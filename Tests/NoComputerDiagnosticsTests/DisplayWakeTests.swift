import Foundation
import IOKit.pwr_mgt
import Testing
@testable import NoComputerDiagnostics

// Declarations are recorded, never made: these tests do not wake the real display.
private final class Recorder {
    var time: TimeInterval = 100
    var declared: [IOPMAssertionID] = []
    var released: [IOPMAssertionID] = []
    var result = kIOReturnSuccess

    func request() -> DisplayWakeRequest {
        DisplayWakeRequest(declare: { id in
            if id == 0 { id = 7 }
            self.declared.append(id)
            return self.result
        }, release: { self.released.append($0) }, now: { self.time })
    }
}

struct DisplayWakeTests {
    @Test func declaresAtMostOncePerInterval() {
        let recorder = Recorder()
        let wake = recorder.request()
        #expect(wake.requestIfDue())
        recorder.time += 0.5
        #expect(!wake.requestIfDue())
        recorder.time += 0.5
        #expect(wake.requestIfDue())
        // The same assertion is updated, not a new one created.
        #expect(recorder.declared == [7, 7])
    }

    @Test func releaseEndsTheAssertionAndAllowsAnImmediateRequest() {
        let recorder = Recorder()
        let wake = recorder.request()
        wake.requestIfDue()
        wake.release()
        #expect(recorder.released == [7])
        // A new session may wake the display at once.
        #expect(wake.requestIfDue())
        wake.release()
        wake.release()
        #expect(recorder.released == [7, 7])
    }

    @Test func releaseWithoutARequestDoesNothing() {
        let recorder = Recorder()
        recorder.request().release()
        #expect(recorder.released.isEmpty)
    }

    @Test func countsFailuresAndStillThrottles() {
        let recorder = Recorder()
        recorder.result = kIOReturnError
        let wake = recorder.request()
        #expect(wake.requestIfDue())
        #expect(!wake.requestIfDue())
        #expect(wake.failures == 1)
    }
}
