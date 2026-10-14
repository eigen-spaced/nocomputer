import Foundation

// local-v4: the private pipe contract between this app and its bundled
// `nocomputer-host` child. crates/host/src/local.rs is the source of truth;
// crates/host/testdata/local-v4/ holds golden vectors both sides test against.
// v2 adds remote input (0x86–0x8A) and InputUnavailable (0x05); v3 adds
// clipboard text (0x06, 0x8B); v4 adds the account check (0x8C, 0x07).
// Frame: [u32 BE len = 1 + payload][u8 type][payload], 1 <= len <= 4 MiB.
enum LocalLink {
    static let version: UInt16 = 4
    static let maximumLength = 4 * 1024 * 1024
    static let maximumReason = 1024
    static let maximumClipboard = 1 << 20
    static let maximumUsername = 255
    static let maximumPassword = 1024

    // Clipboard text as the contract allows it: 1 byte to 1 MiB of UTF-8, no NUL.
    // Descriptions show only the length, so the text never reaches a log.
    struct ClipboardText: Equatable, CustomStringConvertible, CustomDebugStringConvertible {
        let text: String

        init?(_ text: String) {
            guard !text.isEmpty, text.utf8.count <= LocalLink.maximumClipboard, !text.contains("\0") else { return nil }
            self.text = text
        }

        var description: String { "ClipboardText(\(text.utf8.count) bytes)" }
        var debugDescription: String { description }
    }

    // A login password from VerifyLogin. Descriptions never show it, and its bytes
    // are overwritten when it is released. The String handed to OpenDirectory is a
    // copy this code can't clear, so clearing is best effort.
    // Sendable: the bytes never change after init, except when cleared in deinit.
    final class Password: @unchecked Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
        private var bytes: [UInt8]

        init(_ bytes: [UInt8]) { self.bytes = bytes }

        deinit {
            bytes.withUnsafeMutableBytes { buffer in
                guard let base = buffer.baseAddress else { return }
                _ = memset_s(base, buffer.count, 0, buffer.count)
            }
        }

        func withString<R>(_ body: (String) throws -> R) rethrows -> R {
            try body(String(decoding: bytes, as: UTF8.self))
        }

        static func == (a: Password, b: Password) -> Bool { a.bytes == b.bytes }
        var description: String { "Password(redacted)" }
        var debugDescription: String { description }
    }

    struct LoginRequest: Equatable, Sendable {
        let requestID: UInt64
        let username: String
        let password: Password
    }

    enum ProtocolError: Error, Equatable { case invalid(String) }

    // Swift → Rust
    enum Outgoing: Equatable {
        case hello(version: UInt16)
        case accessUnit(keyframe: Bool, captureMicroseconds: UInt64, annexB: Data)
        case captureStopped(reason: String)
        case pairingDecision(requestID: UInt64, approve: Bool)
        case inputUnavailable(reason: String)
        case clipboard(ClipboardText)
        case loginVerdict(requestID: UInt64, ok: Bool)
    }

    // Rust → Swift
    enum Incoming: Equatable {
        case helloAck(version: UInt16)
        case startStream
        case stopStream
        case requestKeyframe
        case event(Data)
        case input(RemoteInput)
        case clipboard(ClipboardText)
        case verifyLogin(LoginRequest)
    }

    static func encode(_ message: Outgoing) throws -> Data {
        var payload = Data()
        let type: UInt8
        switch message {
        case let .hello(version):
            type = 0x01
            append(version, to: &payload)
        case let .accessUnit(keyframe, microseconds, annexB):
            guard !annexB.isEmpty else { throw ProtocolError.invalid("empty access unit") }
            type = 0x02
            payload.append(keyframe ? 1 : 0)
            append(microseconds, to: &payload)
            payload.append(annexB)
        case let .captureStopped(reason):
            type = 0x03
            payload.append(truncated(reason))
        case let .pairingDecision(requestID, approve):
            type = 0x04
            append(requestID, to: &payload)
            payload.append(approve ? 1 : 0)
        case let .inputUnavailable(reason):
            type = 0x05
            payload.append(truncated(reason))
        case let .clipboard(text):
            type = 0x06
            payload.append(Data(text.text.utf8))
        case let .loginVerdict(requestID, ok):
            type = 0x07
            append(requestID, to: &payload)
            payload.append(ok ? 1 : 0)
        }
        let length = 1 + payload.count
        guard length <= maximumLength else { throw ProtocolError.invalid("frame of \(length) bytes exceeds 4 MiB") }
        var frame = Data(capacity: 4 + length)
        append(UInt32(length), to: &frame)
        frame.append(type)
        frame.append(payload)
        return frame
    }

    // Truncate on a character boundary so the payload stays valid UTF-8.
    private static func truncated(_ reason: String) -> Data {
        var text = Substring(reason)
        while text.utf8.count > maximumReason { text = text.dropLast() }
        return Data(text.utf8)
    }

    static func decode(type: UInt8, payload: Data) throws -> Incoming {
        // Before the general copy below, so the password is copied only once.
        if type == 0x8C { return .verifyLogin(try loginRequest(payload)) }
        func empty(_ message: Incoming) throws -> Incoming {
            guard payload.isEmpty else { throw ProtocolError.invalid("type \(type) must be empty") }
            return message
        }
        let bytes = [UInt8](payload)
        func exactly(_ count: Int) throws {
            guard bytes.count == count else { throw ProtocolError.invalid("type \(type) length \(bytes.count)") }
        }
        func u16(_ at: Int) -> UInt16 { UInt16(bytes[at]) << 8 | UInt16(bytes[at + 1]) }
        func i32(_ at: Int) -> Int32 {
            Int32(bitPattern: UInt32(bytes[at]) << 24 | UInt32(bytes[at + 1]) << 16 | UInt32(bytes[at + 2]) << 8 | UInt32(bytes[at + 3]))
        }
        func flag(_ value: UInt8) throws -> Bool {
            guard value <= 1 else { throw ProtocolError.invalid("type \(type) flag \(value)") }
            return value == 1
        }
        switch type {
        case 0x81:
            guard payload.count == 2 else { throw ProtocolError.invalid("HelloAck length \(payload.count)") }
            let bytes = [UInt8](payload)
            return .helloAck(version: UInt16(bytes[0]) << 8 | UInt16(bytes[1]))
        case 0x82: return try empty(.startStream)
        case 0x83: return try empty(.stopStream)
        case 0x84: return try empty(.requestKeyframe)
        case 0x85:
            guard String(data: payload, encoding: .utf8) != nil else { throw ProtocolError.invalid("Event is not UTF-8") }
            return .event(payload)
        case 0x86:
            try exactly(4)
            return .input(.pointer(x: u16(0), y: u16(2)))
        case 0x87:
            try exactly(2)
            guard let button = RemoteInput.Button(rawValue: bytes[0]) else { throw ProtocolError.invalid("button \(bytes[0])") }
            return .input(.button(button, down: try flag(bytes[1])))
        case 0x88:
            try exactly(9)
            guard let unit = RemoteInput.ScrollUnit(rawValue: bytes[8]) else { throw ProtocolError.invalid("scroll unit \(bytes[8])") }
            return .input(.scroll(dx: i32(0), dy: i32(4), unit: unit))
        case 0x89:
            try exactly(3)
            guard let state = RemoteInput.KeyState(rawValue: bytes[2]) else { throw ProtocolError.invalid("key state \(bytes[2])") }
            return .input(.key(usage: u16(0), state: state))
        case 0x8A:
            return try empty(.input(.releaseAll))
        case 0x8B:
            guard let string = String(bytes: payload, encoding: .utf8), let text = ClipboardText(string) else {
                throw ProtocolError.invalid("Clipboard is not 1 byte to 1 MiB of UTF-8 without NUL")
            }
            return .clipboard(text)
        default:
            throw ProtocolError.invalid(String(format: "unknown frame type 0x%02x", type))
        }
    }

    // u64 BE request_id, u8 username length (1...255), username, password (the rest,
    // 1...1024 bytes). The username has no control characters; the password no NUL.
    private static func loginRequest(_ payload: Data) throws -> LoginRequest {
        let start = payload.startIndex
        guard payload.count >= 11 else { throw ProtocolError.invalid("VerifyLogin length \(payload.count)") }
        let requestID = payload[start..<start + 8].reduce(UInt64(0)) { $0 << 8 | UInt64($1) }
        let length = Int(payload[start + 8])
        let usernameEnd = start + 9 + length
        guard length >= 1, usernameEnd < payload.endIndex else { throw ProtocolError.invalid("VerifyLogin username length \(length)") }
        // Control characters as Rust's char::is_control: general category Cc.
        guard let username = String(data: payload[(start + 9)..<usernameEnd], encoding: .utf8),
              !username.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) else {
            throw ProtocolError.invalid("VerifyLogin username is not valid")
        }
        let password = payload[usernameEnd...]
        guard password.count <= maximumPassword, !password.contains(0), isUTF8(password) else {
            throw ProtocolError.invalid("VerifyLogin password is not 1 to 1024 bytes of UTF-8 without NUL")
        }
        return LoginRequest(requestID: requestID, username: username, password: Password([UInt8](password)))
    }

    // Validates without building a String, which would be another copy.
    private static func isUTF8(_ bytes: Data) -> Bool {
        var iterator = bytes.makeIterator()
        var decoder = UTF8()
        while true {
            switch decoder.decode(&iterator) {
            case .scalarValue: continue
            case .emptyInput: return true
            case .error: return false
            }
        }
    }

    private static func append<T: FixedWidthInteger>(_ value: T, to data: inout Data) {
        withUnsafeBytes(of: value.bigEndian) { data.append(contentsOf: $0) }
    }

    // Incremental reader for the child's stdout. Lengths are checked before
    // buffering a payload; any violation is fatal to the link.
    struct Reader {
        private var buffer = Data()

        mutating func feed(_ data: Data) throws -> [Incoming] {
            buffer.append(data)
            var messages: [Incoming] = []
            while buffer.count >= 4 {
                let header = [UInt8](buffer.prefix(4))
                let length = Int(header[0]) << 24 | Int(header[1]) << 16 | Int(header[2]) << 8 | Int(header[3])
                guard length >= 1, length <= LocalLink.maximumLength else {
                    throw ProtocolError.invalid("frame length \(length)")
                }
                guard buffer.count >= 4 + length else { break }
                let start = buffer.startIndex
                let type = buffer[start + 4]
                var payload = Data(buffer[(start + 5)..<(start + 4 + length)])
                // Frames can carry a password; clear consumed bytes before releasing them.
                buffer.resetBytes(in: start..<(start + 4 + length))
                buffer = Data(buffer[(start + 4 + length)...])
                defer { payload.resetBytes(in: payload.startIndex..<payload.endIndex) }
                messages.append(try LocalLink.decode(type: type, payload: payload))
            }
            return messages
        }
    }

    // Display-only JSON from Event frames. Unknown "type"s are ignored by callers.
    struct PairingPrompt: Decodable, Equatable {
        let requestID: UInt64
        let deviceName: String
        let deviceFingerprint: String
        let hostFingerprint: String

        enum CodingKeys: String, CodingKey {
            case requestID = "request_id", deviceName = "device_name"
            case deviceFingerprint = "device_fingerprint", hostFingerprint = "host_fingerprint"
        }
    }

    struct PairingWithdrawn: Decodable, Equatable {
        let requestID: UInt64
        enum CodingKeys: String, CodingKey { case requestID = "request_id" }
    }

    static func eventType(_ json: Data) -> String? {
        struct Tagged: Decodable { let type: String }
        return try? JSONDecoder().decode(Tagged.self, from: json).type
    }
}
