import AppKit
import CoreGraphics

// Remote input, already validated by nocomputer-host and decoded from local-v6.
enum RemoteInput: Equatable {
    enum Button: UInt8, CaseIterable { case left = 0, right, middle, back, forward }
    enum ScrollUnit: UInt8 { case pixel = 0, line }
    enum KeyState: UInt8 { case up = 0, down, repeated }

    // x and y span the captured display: 0 is the left/top edge, 65535 the right/bottom.
    case pointer(x: UInt16, y: UInt16)
    case button(Button, down: Bool)
    // DOM sign convention: positive dy scrolls toward the end of the document.
    case scroll(dx: Int32, dy: Int32, unit: ScrollUnit)
    // USB HID usage, keyboard page 7: a physical key, not a character.
    case key(usage: UInt16, state: KeyState)
    case releaseAll
}

struct InputSnapshot {
    var pointerEvents = 0
    var buttonEvents = 0
    var scrollEvents = 0
    // Counts only: key identities and typed text are never recorded.
    var keyEvents = 0
    var unmappedKeys = 0
    var ignoredReleases = 0
    var releaseAlls = 0
}

// Turns remote input into Quartz events for the captured display. Tracks held
// keys and buttons so that every press gets a release, including on disconnect.
final class InputInjector {
    static let maximumPixelScroll: Int32 = 10_000
    static let maximumLineScroll: Int32 = 100
    static let doubleClickDistance: CGFloat = 4

    private let post: (CGEvent) -> Void
    private let now: () -> TimeInterval
    private let doubleClickInterval: TimeInterval
    private let source = CGEventSource(stateID: .privateState)
    private var bounds: CGRect
    private var position: CGPoint
    private var heldButtons: [RemoteInput.Button] = []
    private var heldKeys: Set<CGKeyCode> = []
    private var lastClick: (button: RemoteInput.Button, time: TimeInterval, point: CGPoint, count: Int64)?
    private(set) var state = InputSnapshot()

    init(displayBounds: CGRect, post: @escaping (CGEvent) -> Void = { $0.post(tap: .cghidEventTap) },
         now: @escaping () -> TimeInterval = { ProcessInfo.processInfo.systemUptime },
         doubleClickInterval: TimeInterval = NSEvent.doubleClickInterval) {
        bounds = displayBounds
        position = CGPoint(x: displayBounds.midX, y: displayBounds.midY)
        self.post = post
        self.now = now
        self.doubleClickInterval = doubleClickInterval
    }

    // Quartz global coordinates (top-left origin), e.g. CGDisplayBounds.
    func update(displayBounds: CGRect) { bounds = displayBounds }

    func handle(_ input: RemoteInput) {
        switch input {
        case let .pointer(x, y): move(to: point(x: x, y: y))
        case let .button(button, down): press(button, down: down)
        case let .scroll(dx, dy, unit): scroll(dx: dx, dy: dy, unit: unit)
        case let .key(usage, keyState): key(usage: usage, state: keyState)
        case .releaseAll: releaseAll()
        }
    }

    // Keys first, so a held modifier cannot turn a button release into a
    // modified click; modifiers after other keys.
    func releaseAll() {
        state.releaseAlls += 1
        let modifiers = heldKeys.filter { Self.modifierFlag($0) != nil }
        for code in heldKeys.subtracting(modifiers).sorted() + modifiers.sorted() { keyEvent(code, down: false) }
        for button in heldButtons { buttonEvent(button, down: false) }
        heldButtons.removeAll()
    }

    func point(x: UInt16, y: UInt16) -> CGPoint {
        // Clamp inside the display: the far edge pixel, never one past it.
        let px = bounds.minX + CGFloat(x) / 65535 * max(0, bounds.width - 1)
        let py = bounds.minY + CGFloat(y) / 65535 * max(0, bounds.height - 1)
        return CGPoint(x: min(max(px, bounds.minX), bounds.maxX - 1), y: min(max(py, bounds.minY), bounds.maxY - 1))
    }

    private var flags: CGEventFlags {
        heldKeys.reduce(into: CGEventFlags()) { result, code in
            if let flag = Self.modifierFlag(code) { result.insert(flag) }
        }
    }

    private func move(to point: CGPoint) {
        position = point
        let (type, button): (CGEventType, CGMouseButton) = switch heldButtons.first {
        case .left?: (.leftMouseDragged, .left)
        case .right?: (.rightMouseDragged, .right)
        case let other?: (.otherMouseDragged, CGMouseButton(rawValue: UInt32(other.rawValue))!)
        case nil: (.mouseMoved, .left)
        }
        guard let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: point,
                                  mouseButton: button) else { return }
        event.flags = flags
        post(event)
        state.pointerEvents += 1
    }

    private func press(_ button: RemoteInput.Button, down: Bool) {
        if down {
            guard !heldButtons.contains(button) else { return }
            let time = now()
            if let last = lastClick, last.button == button, time - last.time <= doubleClickInterval,
               hypot(position.x - last.point.x, position.y - last.point.y) <= Self.doubleClickDistance {
                lastClick = (button, time, position, last.count + 1)
            } else {
                lastClick = (button, time, position, 1)
            }
            heldButtons.append(button)
        } else {
            guard let index = heldButtons.firstIndex(of: button) else { state.ignoredReleases += 1; return }
            heldButtons.remove(at: index)
        }
        buttonEvent(button, down: down)
    }

    private func buttonEvent(_ button: RemoteInput.Button, down: Bool) {
        let type: CGEventType = switch (button, down) {
        case (.left, true): .leftMouseDown
        case (.left, false): .leftMouseUp
        case (.right, true): .rightMouseDown
        case (.right, false): .rightMouseUp
        case (_, true): .otherMouseDown
        case (_, false): .otherMouseUp
        }
        guard let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: position,
                                  mouseButton: CGMouseButton(rawValue: UInt32(button.rawValue))!) else { return }
        let clicks = lastClick?.button == button ? lastClick?.count ?? 1 : 1
        event.setIntegerValueField(.mouseEventClickState, value: clicks)
        event.flags = flags
        post(event)
        state.buttonEvents += 1
    }

    private func scroll(dx: Int32, dy: Int32, unit: RemoteInput.ScrollUnit) {
        let limit = unit == .pixel ? Self.maximumPixelScroll : Self.maximumLineScroll
        let x = min(max(dx, -limit), limit)
        let y = min(max(dy, -limit), limit)
        guard x != 0 || y != 0 else { return }
        // Quartz wheel deltas are positive toward the top/left, the opposite of the DOM.
        guard let event = CGEvent(scrollWheelEvent2Source: source, units: unit == .pixel ? .pixel : .line,
                                  wheelCount: 2, wheel1: -y, wheel2: -x, wheel3: 0) else { return }
        event.location = position
        event.flags = flags
        post(event)
        state.scrollEvents += 1
    }

    private func key(usage: UInt16, state keyState: RemoteInput.KeyState) {
        guard let code = Self.keyCode(usage: usage) else { state.unmappedKeys += 1; return }
        switch keyState {
        case .down, .repeated:
            let repeated = heldKeys.contains(code)
            // A modifier is a state, not a character: never repeat it.
            if repeated && Self.modifierFlag(code) != nil { return }
            heldKeys.insert(code)
            keyEvent(code, down: true, autorepeat: repeated)
        case .up:
            guard heldKeys.contains(code) else { state.ignoredReleases += 1; return }
            keyEvent(code, down: false)
        }
    }

    private func keyEvent(_ code: CGKeyCode, down: Bool, autorepeat: Bool = false) {
        if !down { heldKeys.remove(code) }
        guard let event = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down) else { return }
        // Flags come from the remote held keys only, never from the local keyboard.
        event.flags = flags
        if autorepeat { event.setIntegerValueField(.keyboardEventAutorepeat, value: 1) }
        post(event)
        state.keyEvents += 1
    }

    static func modifierFlag(_ code: CGKeyCode) -> CGEventFlags? {
        switch code {
        case 0x38, 0x3C: .maskShift
        case 0x3B, 0x3E: .maskControl
        case 0x3A, 0x3D: .maskAlternate
        case 0x37, 0x36: .maskCommand
        default: nil
        }
    }

    // USB HID keyboard page usages to macOS virtual key codes (US ANSI layout,
    // Carbon kVK_*). Caps Lock is deliberately absent: its toggle state would
    // drift between the two machines.
    static func keyCode(usage: UInt16) -> CGKeyCode? { keyCodes[usage] }

    private static let keyCodes: [UInt16: CGKeyCode] = {
        var table: [UInt16: CGKeyCode] = [:]
        // a–z
        let letters: [CGKeyCode] = [0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E,
                                    0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06]
        for (index, code) in letters.enumerated() { table[0x04 + UInt16(index)] = code }
        // 1–9, 0
        let digits: [CGKeyCode] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D]
        for (index, code) in digits.enumerated() { table[0x1E + UInt16(index)] = code }
        // F1–F12
        let functions: [CGKeyCode] = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F]
        for (index, code) in functions.enumerated() { table[0x3A + UInt16(index)] = code }
        // F13–F20
        let upper: [CGKeyCode] = [0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A]
        for (index, code) in upper.enumerated() { table[0x68 + UInt16(index)] = code }
        // Keypad 1–9, 0
        let keypad: [CGKeyCode] = [0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5B, 0x5C, 0x52]
        for (index, code) in keypad.enumerated() { table[0x59 + UInt16(index)] = code }
        let rest: [UInt16: CGKeyCode] = [
            0x28: 0x24, // Return
            0x29: 0x35, // Escape
            0x2A: 0x33, // Backspace → Delete
            0x2B: 0x30, // Tab
            0x2C: 0x31, // Space
            0x2D: 0x1B, 0x2E: 0x18, 0x2F: 0x21, 0x30: 0x1E, 0x31: 0x2A, // - = [ ] \
            0x33: 0x29, 0x34: 0x27, 0x35: 0x32, 0x36: 0x2B, 0x37: 0x2F, 0x38: 0x2C, // ; ' ` , . /
            0x49: 0x72, // Insert → Help
            0x4A: 0x73, 0x4B: 0x74, 0x4C: 0x75, 0x4D: 0x77, 0x4E: 0x79, // Home PgUp Delete End PgDn
            0x4F: 0x7C, 0x50: 0x7B, 0x51: 0x7D, 0x52: 0x7E, // → ← ↓ ↑
            0x53: 0x47, // Num Lock → keypad Clear
            0x54: 0x4B, 0x55: 0x43, 0x56: 0x4E, 0x57: 0x45, 0x58: 0x4C, 0x63: 0x41, 0x67: 0x51, // keypad / * - + Enter . =
            0x64: 0x0A, // non-US backslash → ISO section
            0xE0: 0x3B, 0xE1: 0x38, 0xE2: 0x3A, 0xE3: 0x37, // left Control Shift Option Command
            0xE4: 0x3E, 0xE5: 0x3C, 0xE6: 0x3D, 0xE7: 0x36, // right Control Shift Option Command
        ]
        table.merge(rest) { _, new in new }
        return table
    }()
}
