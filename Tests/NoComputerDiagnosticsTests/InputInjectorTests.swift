import CoreGraphics
import Foundation
import Testing
@testable import NoComputerDiagnostics

// Events are captured, never posted: these tests do not move the real pointer.
private final class Recorder {
    var events: [CGEvent] = []
    var time: TimeInterval = 100

    func injector(bounds: CGRect = CGRect(x: 0, y: 0, width: 1710, height: 1107)) -> InputInjector {
        InputInjector(displayBounds: bounds, post: { self.events.append($0) }, now: { self.time },
                      doubleClickInterval: 0.5)
    }

    var types: [CGEventType] { events.map(\.type) }
}

struct InputInjectorTests {
    @Test func mapsNormalizedCoordinatesInsideTheDisplay() {
        let recorder = Recorder()
        // A secondary display left of and below the primary one.
        let injector = recorder.injector(bounds: CGRect(x: -1920, y: 200, width: 1920, height: 1080))
        #expect(injector.point(x: 0, y: 0) == CGPoint(x: -1920, y: 200))
        #expect(injector.point(x: 65535, y: 65535) == CGPoint(x: -1, y: 1279))
        let middle = injector.point(x: 32768, y: 32768)
        #expect(abs(middle.x - -960.5) < 0.1)
        #expect(abs(middle.y - 739.5) < 0.1)
        injector.handle(.pointer(x: 65535, y: 0))
        #expect(recorder.types == [.mouseMoved])
        #expect(recorder.events[0].location == CGPoint(x: -1, y: 200))
    }

    @Test func dragsWhileAButtonIsHeld() {
        let recorder = Recorder()
        let injector = recorder.injector()
        injector.handle(.pointer(x: 100, y: 100))
        injector.handle(.button(.left, down: true))
        injector.handle(.pointer(x: 200, y: 200))
        injector.handle(.button(.left, down: false))
        injector.handle(.pointer(x: 300, y: 300))
        injector.handle(.button(.middle, down: true))
        injector.handle(.pointer(x: 400, y: 400))
        injector.handle(.button(.middle, down: false))
        #expect(recorder.types == [.mouseMoved, .leftMouseDown, .leftMouseDragged, .leftMouseUp, .mouseMoved,
                                   .otherMouseDown, .otherMouseDragged, .otherMouseUp])
        #expect(recorder.events[5].getIntegerValueField(.mouseEventButtonNumber) == 2)
        // Button events land where the pointer is.
        #expect(recorder.events[3].location == recorder.events[2].location)
    }

    @Test func countsDoubleAndTripleClicks() {
        let recorder = Recorder()
        let injector = recorder.injector()
        injector.handle(.pointer(x: 1000, y: 1000))
        for _ in 0..<3 {
            injector.handle(.button(.left, down: true))
            injector.handle(.button(.left, down: false))
            recorder.time += 0.2
        }
        let states = recorder.events.dropFirst().map { $0.getIntegerValueField(.mouseEventClickState) }
        #expect(states == [1, 1, 2, 2, 3, 3])

        // Too slow, too far, or another button: a fresh single click.
        recorder.events.removeAll()
        recorder.time += 1
        injector.handle(.button(.left, down: true))
        injector.handle(.button(.left, down: false))
        recorder.time += 0.1
        injector.handle(.pointer(x: 3000, y: 3000))
        injector.handle(.button(.left, down: true))
        injector.handle(.button(.left, down: false))
        recorder.time += 0.1
        injector.handle(.button(.right, down: true))
        injector.handle(.button(.right, down: false))
        let fresh = recorder.events.filter { $0.type != .mouseMoved }.map { $0.getIntegerValueField(.mouseEventClickState) }
        #expect(fresh == [1, 1, 1, 1, 1, 1])
    }

    @Test func scrollUsesQuartzSignsAndClamps() {
        let recorder = Recorder()
        let injector = recorder.injector()
        injector.handle(.scroll(dx: 0, dy: 120, unit: .pixel))
        injector.handle(.scroll(dx: -3, dy: 0, unit: .line))
        injector.handle(.scroll(dx: 0, dy: 1_000_000, unit: .pixel))
        injector.handle(.scroll(dx: 0, dy: -1_000, unit: .line))
        injector.handle(.scroll(dx: 0, dy: 0, unit: .pixel))
        #expect(recorder.types == [.scrollWheel, .scrollWheel, .scrollWheel, .scrollWheel])
        let deltas = recorder.events.map {
            ($0.getIntegerValueField(.scrollWheelEventPointDeltaAxis1), $0.getIntegerValueField(.scrollWheelEventDeltaAxis2),
             $0.getIntegerValueField(.scrollWheelEventIsContinuous))
        }
        #expect(deltas[0].0 == -120 && deltas[0].2 == 1)
        #expect(deltas[1].1 == 3 && deltas[1].2 == 0)
        #expect(deltas[2].0 == -10_000)
        #expect(recorder.events[3].getIntegerValueField(.scrollWheelEventDeltaAxis1) == 100)
    }

    @Test func modifiersComeFromRemoteHeldKeysOnly() {
        let recorder = Recorder()
        let injector = recorder.injector()
        injector.handle(.key(usage: 0xE0, state: .down)) // left Control
        injector.handle(.key(usage: 0x06, state: .down)) // c
        injector.handle(.key(usage: 0x06, state: .up))
        injector.handle(.key(usage: 0xE0, state: .up))
        injector.handle(.key(usage: 0x06, state: .down))
        injector.handle(.key(usage: 0x06, state: .up))
        let codes = recorder.events.map { $0.getIntegerValueField(.keyboardEventKeycode) }
        #expect(codes == [0x3B, 0x08, 0x08, 0x3B, 0x08, 0x08])
        #expect(recorder.events[1].flags.contains(.maskControl))
        #expect(recorder.events[2].flags.contains(.maskControl))
        #expect(!recorder.events[3].flags.contains(.maskControl))
        #expect(!recorder.events[4].flags.contains(.maskControl))
        // Left GUI is Command; the viewer decides which Windows key sends it.
        recorder.events.removeAll()
        injector.handle(.key(usage: 0xE3, state: .down))
        injector.handle(.button(.left, down: true))
        #expect(recorder.events[1].flags.contains(.maskCommand))
    }

    @Test func repeatsAreMarkedAndUnheldReleasesIgnored() {
        let recorder = Recorder()
        let injector = recorder.injector()
        injector.handle(.key(usage: 0x04, state: .down))
        injector.handle(.key(usage: 0x04, state: .repeated))
        injector.handle(.key(usage: 0x04, state: .repeated))
        injector.handle(.key(usage: 0x04, state: .up))
        injector.handle(.key(usage: 0x04, state: .up))
        injector.handle(.key(usage: 0xE1, state: .down))
        injector.handle(.key(usage: 0xE1, state: .repeated))
        #expect(recorder.events.map { $0.getIntegerValueField(.keyboardEventAutorepeat) } == [0, 1, 1, 0, 0])
        #expect(injector.state.ignoredReleases == 1)
        #expect(injector.state.keyEvents == 5)
    }

    @Test func unmappedUsagesAndCapsLockAreDropped() {
        let recorder = Recorder()
        let injector = recorder.injector()
        for usage: UInt16 in [0x39, 0x00, 0x46, 0xFFFF] { injector.handle(.key(usage: usage, state: .down)) }
        #expect(recorder.events.isEmpty)
        #expect(injector.state.unmappedKeys == 4)
    }

    @Test func releaseAllLiftsEverythingHeld() {
        let recorder = Recorder()
        let injector = recorder.injector()
        injector.handle(.key(usage: 0xE1, state: .down)) // Shift
        injector.handle(.key(usage: 0x04, state: .down)) // a
        injector.handle(.button(.left, down: true))
        injector.handle(.button(.right, down: true))
        recorder.events.removeAll()
        injector.handle(.releaseAll)
        // Keys before buttons, ordinary keys before modifiers.
        // Quartz turns a modifier key event into flagsChanged.
        #expect(recorder.types == [.keyUp, .flagsChanged, .leftMouseUp, .rightMouseUp])
        #expect(recorder.events.map { $0.getIntegerValueField(.keyboardEventKeycode) }.prefix(2) == [0x00, 0x38])
        #expect(!recorder.events[2].flags.contains(.maskShift))
        recorder.events.removeAll()
        injector.handle(.releaseAll)
        #expect(recorder.events.isEmpty)
    }

    @Test func everyMappedUsageHasADistinctKeyCode() {
        var seen: [CGKeyCode: UInt16] = [:]
        for usage in UInt16(0)...0xFF {
            guard let code = InputInjector.keyCode(usage: usage) else { continue }
            #expect(seen[code] == nil, "usage \(usage) and \(seen[code] ?? 0) share key code \(code)")
            seen[code] = usage
        }
        // 26 letters, 10 digits, 20 F-keys, 10 keypad digits, and 43 named keys.
        #expect(seen.count == 109)
    }
}
