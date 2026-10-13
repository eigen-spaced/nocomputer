import CoreGraphics
import Testing
@testable import NoComputerDiagnostics

struct InputProbeTests {
    @Test func probeMovesWithinDisplayAndReleasesTestCharacter() throws {
        let events = try InputProbe.events(displayBounds: CGRect(x: -1920, y: 100, width: 1920, height: 1080))
        #expect(events.insertion.map(\.type) == [.mouseMoved, .keyDown, .keyUp])
        #expect(events.insertion[0].location == CGPoint(x: -480, y: 586))
        for event in events.insertion.suffix(2) {
            #expect(event.getIntegerValueField(.keyboardEventKeycode) == 0)
            #expect(event.flags.isEmpty)
        }
    }

    @Test func probePreparesErasureWithoutSubmitting() throws {
        let events = try InputProbe.events(displayBounds: CGRect(x: 0, y: 0, width: 1710, height: 1107))
        #expect(events.erasure.map(\.type) == [.keyDown, .keyUp])
        for event in events.erasure {
            #expect(event.getIntegerValueField(.keyboardEventKeycode) == 51)
            #expect(event.flags.isEmpty)
        }
        let keys = (events.insertion + events.erasure).filter { $0.type == .keyDown }
        #expect(keys.map { $0.getIntegerValueField(.keyboardEventKeycode) } == [0, 51])
    }

    @Test func probeRejectsInvalidDisplayBounds() {
        #expect(throws: (any Error).self) {
            try InputProbe.events(displayBounds: .zero)
        }
        #expect(throws: (any Error).self) {
            try InputProbe.events(displayBounds: CGRect(x: 0, y: 0, width: Double.infinity, height: 100))
        }
    }
}
