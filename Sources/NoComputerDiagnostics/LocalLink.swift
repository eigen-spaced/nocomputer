import Foundation

// local-v1: the private pipe contract between this app and its bundled
// `nocomputer-host` child. crates/host/src/local.rs is the source of truth;
// crates/host/testdata/local-v1/ holds golden vectors both sides test against.
// Frame: [u32 BE len = 1 + payload][u8 type][payload], 1 <= len <= 4 MiB.
enum LocalLink {
    static let version: UInt16 = 1
    static let maximumLength = 4 * 1024 * 1024
    static let maximumReason = 1024

    enum ProtocolError: Error, Equatable { case invalid(String) }

    // Swift → Rust
    enum Outgoing: Equatable {
        case hello(version: UInt16)
        case accessUnit(keyframe: Bool, captureMicroseconds: UInt64, annexB: Data)
        case captureStopped(reason: String)
        case pairingDecision(requestID: UInt64, approve: Bool)
    }

    // Rust → Swift
    enum Incoming: Equatable {
        case helloAck(version: UInt16)
        case startStream
        case stopStream
        case requestKeyframe
        case event(Data)
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
            // Truncate on a character boundary so the payload stays valid UTF-8.
            var text = Substring(reason)
            while text.utf8.count > maximumReason { text = text.dropLast() }
            payload.append(contentsOf: Array(text.utf8))
        case let .pairingDecision(requestID, approve):
            type = 0x04
            append(requestID, to: &payload)
            payload.append(approve ? 1 : 0)
        }
        let length = 1 + payload.count
        guard length <= maximumLength else { throw ProtocolError.invalid("frame of \(length) bytes exceeds 4 MiB") }
        var frame = Data(capacity: 4 + length)
        append(UInt32(length), to: &frame)
        frame.append(type)
        frame.append(payload)
        return frame
    }

    static func decode(type: UInt8, payload: Data) throws -> Incoming {
        func empty(_ message: Incoming) throws -> Incoming {
            guard payload.isEmpty else { throw ProtocolError.invalid("type \(type) must be empty") }
            return message
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
        default:
            throw ProtocolError.invalid(String(format: "unknown frame type 0x%02x", type))
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
                let payload = Data(buffer[(start + 5)..<(start + 4 + length)])
                buffer = Data(buffer[(start + 4 + length)...])
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

    static func eventType(_ json: Data) -> String? {
        struct Tagged: Decodable { let type: String }
        return try? JSONDecoder().decode(Tagged.self, from: json).type
    }
}
