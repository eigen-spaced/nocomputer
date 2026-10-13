import Foundation
import IOKit.pwr_mgt

final class SleepAssertion {
    enum Kind {
        case system, display

        var type: String {
            switch self {
            case .system: return kIOPMAssertionTypePreventUserIdleSystemSleep
            case .display: return kIOPMAssertionTypePreventUserIdleDisplaySleep
            }
        }
    }

    private let kind: Kind
    private var id: IOPMAssertionID?
    var active: Bool { id != nil }

    init(kind: Kind = .system) { self.kind = kind }

    func enable() throws {
        guard id == nil else { return }
        var candidate: IOPMAssertionID = 0
        let result = IOPMAssertionCreateWithName(
            kind.type as CFString,
            IOPMAssertionLevel(kIOPMAssertionLevelOn),
            "NoComputer diagnostic: user enabled \(kind) keep-awake" as CFString,
            &candidate
        )
        guard result == kIOReturnSuccess else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(result),
                          userInfo: [NSLocalizedDescriptionKey: "Sleep assertion failed: \(result)"])
        }
        id = candidate
    }

    @discardableResult
    func disable() -> IOReturn {
        guard let id else { return kIOReturnSuccess }
        let result = IOPMAssertionRelease(id)
        if result == kIOReturnSuccess { self.id = nil }
        return result
    }

    deinit { disable() }
}

// A display-wake request on an already running system. This does not unlock
// the session, synthesize input, or wake an unreachable sleeping computer.
final class DisplayWakeRequest {
    private var id: IOPMAssertionID = 0

    func request() throws {
        let result = IOPMAssertionDeclareUserActivity(
            "NoComputer: explicitly scheduled display-wake diagnostic" as CFString,
            kIOPMUserActiveLocal, &id
        )
        guard result == kIOReturnSuccess else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(result),
                          userInfo: [NSLocalizedDescriptionKey: "Display wake failed: \(result)"])
        }
    }

    func release() {
        guard id != 0 else { return }
        _ = IOPMAssertionRelease(id)
        id = 0
    }

    deinit { release() }
}
