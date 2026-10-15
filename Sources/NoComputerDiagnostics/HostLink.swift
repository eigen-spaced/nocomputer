import CoreMedia
import Foundation

struct HostLinkSnapshot {
    var accessUnits = 0
    var keyframes = 0
    var bytes = 0
    // Encoded frames dropped because the pipe was backed up, and the frames that
    // followed them until the next keyframe (they depend on the dropped one).
    var pressureDrops = 0
    var dependentDrops = 0
    var conversionErrors = 0
    var peakQueued = 0
}

// Decides whether an encoded frame may enter the pipe. Once any frame is dropped,
// later frames depend on it, so everything is refused until the next keyframe.
struct PipeAdmission {
    enum Decision: Equatable { case send, dropForPressure, dropDependent }

    let limit: Int
    private(set) var queued = 0
    private(set) var awaitingKeyframe = true

    init(limit: Int) { self.limit = limit }

    mutating func expectKeyframe() { awaitingKeyframe = true }

    mutating func admit(keyframe: Bool) -> Decision {
        if queued >= limit {
            awaitingKeyframe = true
            return .dropForPressure
        }
        if awaitingKeyframe && !keyframe { return .dropDependent }
        awaitingKeyframe = false
        queued += 1
        return .send
    }

    // A frame admitted earlier could not be converted or written after all.
    mutating func failed() {
        queued -= 1
        awaitingKeyframe = true
    }

    mutating func written() { queued -= 1 }
}

// Owns the bundled `nocomputer-host` child and the local-v5 pipe to it. The pipe is
// inherited, never a socket, so no other local process can reach this channel.
// Network, pairing state and keys live entirely in the child.
// Sendable: its mutable state is behind `lock`, and writes go through `writer`.
final class HostLink: @unchecked Sendable {
    static let maximumQueued = 3

    // All handlers run on the main queue.
    var onStartStream: (() -> Void)?
    var onStopStream: (() -> Void)?
    var onRequestKeyframe: (() -> Void)?
    var onEvent: ((Data) -> Void)?
    var onClipboard: ((LocalLink.ClipboardText) -> Void)?
    var onViewSize: ((UInt16, UInt16) -> Void)?
    var onExit: ((String) -> Void)?
    // Not on the main queue: called in order on the pipe reader's queue, so
    // input never waits behind UI work. The receiver must not block.
    var onInput: ((RemoteInput) -> Void)?
    // Also on the reader's queue, so the password never passes through the UI.
    // Answer with `loginVerdict`; the receiver must not block.
    var onVerifyLogin: ((LocalLink.LoginRequest) -> Void)?

    private let process = Process()
    private let input = Pipe()
    private let output = Pipe()
    private let writer = DispatchQueue(label: "dev.nocomputer.host-link.writer")
    private let lock = NSLock()
    private var reader = LocalLink.Reader()
    private var acknowledged = false
    private var admission = PipeAdmission(limit: HostLink.maximumQueued)
    private var failed: String?
    // Set by stop(): the child is closing, so a failed write is expected.
    private var stopping = false
    private var state = HostLinkSnapshot()

    init(executable: URL, arguments: [String] = []) {
        process.executableURL = executable
        process.arguments = arguments
        process.standardInput = input
        process.standardOutput = output
        // stderr is inherited: the child logs there and never logs keys or frames.
    }

    func snapshot() -> HostLinkSnapshot {
        lock.lock()
        defer { lock.unlock() }
        return state
    }

    var acknowledgedForTesting: Bool {
        lock.lock()
        defer { lock.unlock() }
        return acknowledged
    }

    var runningForTesting: Bool { process.isRunning }

    func start() throws {
        // A write after the child exits must fail with EPIPE, not kill this app.
        _ = fcntl(input.fileHandleForWriting.fileDescriptor, F_SETNOSIGPIPE, 1)
        output.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            if data.isEmpty { handle.readabilityHandler = nil; return }
            self?.received(data)
        }
        process.terminationHandler = { [weak self] process in
            let reason = "nocomputer-host exited (status \(process.terminationStatus))"
            DispatchQueue.main.async { self?.onExit?(self?.failureReason() ?? reason) }
        }
        try process.run()
        send(.hello(version: LocalLink.version))
    }

    // Closing stdin asks the child to end its sessions and exit; terminate it if
    // it has not exited after a grace period.
    func stop() {
        lock.lock()
        stopping = true
        lock.unlock()
        writer.async { [input, process] in
            try? input.fileHandleForWriting.close()
            DispatchQueue.global().asyncAfter(deadline: .now() + 3) {
                if process.isRunning { process.terminate() }
            }
        }
    }

    func captureStopped(reason: String) { send(.captureStopped(reason: reason)) }

    func inputUnavailable(reason: String) { send(.inputUnavailable(reason: reason)) }

    func decide(requestID: UInt64, approve: Bool) { send(.pairingDecision(requestID: requestID, approve: approve)) }

    func loginVerdict(requestID: UInt64, ok: Bool) { send(.loginVerdict(requestID: requestID, ok: ok)) }

    func sendClipboard(_ text: LocalLink.ClipboardText) {
        lock.lock()
        let ready = acknowledged && failed == nil
        lock.unlock()
        if ready { send(.clipboard(text)) }
    }

    // The next frame must be an IDR, e.g. at stream start or after a request.
    func expectKeyframe() {
        lock.lock()
        admission.expectKeyframe()
        lock.unlock()
    }

    // Called on the encoder's callback thread. Never blocks: when the pipe is
    // backed up the frame is dropped, and so is everything after it until the
    // next keyframe, which is requested. A P-frame is never sent without its IDR.
    func submit(_ sample: CMSampleBuffer) {
        let keyframe = AnnexB.isKeyframe(sample)
        lock.lock()
        guard acknowledged, failed == nil else { lock.unlock(); return }
        let decision = admission.admit(keyframe: keyframe)
        switch decision {
        case .send: break
        case .dropForPressure: state.pressureDrops += 1
        case .dropDependent: state.dependentDrops += 1
        }
        state.peakQueued = max(state.peakQueued, admission.queued)
        lock.unlock()
        guard decision == .send else { requestKeyframe(); return }
        let frame: Data
        do {
            let annexB = try AnnexB.accessUnit(sample)
            let microseconds = UInt64(ProcessInfo.processInfo.systemUptime * 1_000_000)
            frame = try LocalLink.encode(.accessUnit(keyframe: keyframe, captureMicroseconds: microseconds, annexB: annexB))
        } catch {
            lock.lock()
            state.conversionErrors += 1
            admission.failed()
            lock.unlock()
            requestKeyframe()
            return
        }
        lock.lock()
        state.accessUnits += 1
        state.bytes += frame.count
        if keyframe { state.keyframes += 1 }
        lock.unlock()
        write(frame, accessUnit: true)
    }

    private func requestKeyframe() {
        DispatchQueue.main.async { [weak self] in self?.onRequestKeyframe?() }
    }

    private func send(_ message: LocalLink.Outgoing) {
        do { write(try LocalLink.encode(message), accessUnit: false) } catch { fail("encode: \(error)") }
    }

    private func write(_ frame: Data, accessUnit: Bool) {
        writer.async { [weak self, input] in
            do {
                try input.fileHandleForWriting.write(contentsOf: frame)
            } catch {
                self?.writeFailed(error)
            }
            if accessUnit, let self {
                self.lock.lock()
                self.admission.written()
                self.lock.unlock()
            }
        }
    }

    private func received(_ data: Data) {
        let messages: [LocalLink.Incoming]
        do {
            messages = try reader.feed(data)
        } catch {
            fail("protocol error from nocomputer-host: \(error)")
            return
        }
        for message in messages {
            lock.lock()
            let first = !acknowledged
            lock.unlock()
            if first {
                guard message == .helloAck(version: LocalLink.version) else {
                    fail("expected HelloAck(\(LocalLink.version)), got \(message)")
                    return
                }
                lock.lock()
                acknowledged = true
                lock.unlock()
                continue
            }
            if case let .input(input) = message {
                onInput?(input)
                continue
            }
            if case let .verifyLogin(request) = message {
                onVerifyLogin?(request)
                continue
            }
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                switch message {
                case .helloAck: self.fail("duplicate HelloAck")
                case .startStream: self.expectKeyframe(); self.onStartStream?()
                case .stopStream: self.onStopStream?()
                case .requestKeyframe: self.expectKeyframe(); self.onRequestKeyframe?()
                case let .event(json): self.onEvent?(json)
                case let .clipboard(text): self.onClipboard?(text)
                case let .viewSize(width, height): self.onViewSize?(width, height)
                case .input, .verifyLogin: break
                }
            }
        }
    }

    // While stopping, the child closes its end and a write in flight fails; that
    // is the expected end, not a contract violation.
    private func writeFailed(_ error: Error) {
        lock.lock()
        let expected = stopping
        lock.unlock()
        if !expected { fail("pipe write failed: \(error.localizedDescription)") }
    }

    private func failureReason() -> String? {
        lock.lock()
        defer { lock.unlock() }
        return failed
    }

    // Any contract violation ends the link: the child is terminated and the
    // owner learns why through onExit.
    private func fail(_ reason: String) {
        lock.lock()
        let first = failed == nil
        if first { failed = reason }
        lock.unlock()
        guard first else { return }
        if process.isRunning { process.terminate() }
    }
}
