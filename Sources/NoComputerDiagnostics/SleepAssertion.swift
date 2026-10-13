import Foundation
import IOKit.pwr_mgt

// Prevents idle system sleep (the display may still sleep) while remote
// access is on.
final class SleepAssertion {
    private var id: IOPMAssertionID?
    var active: Bool { id != nil }

    func enable() throws {
        guard id == nil else { return }
        var candidate: IOPMAssertionID = 0
        let result = IOPMAssertionCreateWithName(
            kIOPMAssertionTypePreventUserIdleSystemSleep as CFString,
            IOPMAssertionLevel(kIOPMAssertionLevelOn),
            "NoComputer: remote access enabled" as CFString,
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
// Not wired up yet: kept for waking the display when a viewer connects
// (proven with fresh capture at the lock screen, docs/macos-feasibility.md).
final class DisplayWakeRequest {
    private var id: IOPMAssertionID = 0

    func request() throws {
        let result = IOPMAssertionDeclareUserActivity(
            "NoComputer: display wake for a remote viewer" as CFString,
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
