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

// Declares local user activity for a remote viewer, as a person at the Mac
// would: it turns a sleeping display on and restarts its idle timer. Proven to
// wake the display at the lock screen for fresh capture
// (docs/macos-feasibility.md). It does not unlock the session, synthesize
// input, or wake an unreachable sleeping computer.
// Not thread-safe: use from one queue.
final class DisplayWakeRequest {
    // Remote input can arrive at 60+ events a second; one declaration a second
    // keeps the display on.
    static let minimumInterval: TimeInterval = 1

    private let declare: (inout IOPMAssertionID) -> IOReturn
    private let releaseAssertion: (IOPMAssertionID) -> Void
    private let now: () -> TimeInterval
    private var id: IOPMAssertionID = 0
    private var lastRequest = -TimeInterval.infinity
    private(set) var failures = 0

    init(declare: @escaping (inout IOPMAssertionID) -> IOReturn = { id in
            IOPMAssertionDeclareUserActivity("NoComputer: remote viewer activity" as CFString,
                                             kIOPMUserActiveLocal, &id)
         },
         release: @escaping (IOPMAssertionID) -> Void = { _ = IOPMAssertionRelease($0) },
         now: @escaping () -> TimeInterval = { ProcessInfo.processInfo.systemUptime }) {
        self.declare = declare
        releaseAssertion = release
        self.now = now
    }

    // Declares activity unless one was declared under a second ago. Reusing
    // the id updates the same assertion rather than creating another.
    @discardableResult
    func requestIfDue() -> Bool {
        let time = now()
        guard time - lastRequest >= Self.minimumInterval else { return false }
        lastRequest = time
        if declare(&id) != kIOReturnSuccess { failures += 1 }
        return true
    }

    // The display may sleep again on its normal idle timer.
    func release() {
        lastRequest = -.infinity
        guard id != 0 else { return }
        releaseAssertion(id)
        id = 0
    }

    deinit { release() }
}
