import CoreGraphics
import Foundation

enum InputProbe {
    struct Events {
        let insertion: [CGEvent]
        let erasure: [CGEvent]
    }

    // Prebuild a fixed nonsecret 'a' and Backspace. No clicks, Return, or
    // password handling; every key-down has a key-up, with no inherited flags.
    static func events(displayBounds bounds: CGRect) throws -> Events {
        guard bounds.origin.x.isFinite, bounds.origin.y.isFinite,
              bounds.width.isFinite, bounds.height.isFinite,
              bounds.width > 0, bounds.height > 0 else {
            throw NSError(domain: "NoComputer.InputProbe", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "Invalid display bounds"])
        }
        let point = CGPoint(x: bounds.minX + bounds.width * 0.75,
                            y: bounds.minY + bounds.height * 0.45)
        guard let source = CGEventSource(stateID: .privateState),
              let move = CGEvent(mouseEventSource: source, mouseType: .mouseMoved,
                                 mouseCursorPosition: point, mouseButton: .left),
              let down = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true),
              let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false),
              let eraseDown = CGEvent(keyboardEventSource: source, virtualKey: 51, keyDown: true),
              let eraseUp = CGEvent(keyboardEventSource: source, virtualKey: 51, keyDown: false) else {
            throw NSError(domain: "NoComputer.InputProbe", code: 2,
                          userInfo: [NSLocalizedDescriptionKey: "Could not create diagnostic input events"])
        }
        for event in [move, down, up, eraseDown, eraseUp] { event.flags = [] }
        return Events(insertion: [move, down, up], erasure: [eraseDown, eraseUp])
    }
}
