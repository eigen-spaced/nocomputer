import CoreMedia
import Foundation

// Converts encoder output (AVCC length-prefixed NAL units) to one Annex B access
// unit. Keyframes carry SPS/PPS in-band first, so a decoder can start at any IDR.
enum AnnexB {
    enum ConversionError: Error { case invalid(String) }

    static let maximumAccessUnit = 4 * 1024 * 1024

    static func isKeyframe(_ sample: CMSampleBuffer) -> Bool {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]]
        return !(attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool ?? false)
    }

    static func accessUnit(_ sample: CMSampleBuffer) throws -> Data {
        guard let format = sample.formatDescription, let block = sample.dataBuffer else {
            throw ConversionError.invalid("sample has no format or data")
        }
        var pointer: UnsafePointer<UInt8>?
        var size = 0
        var count = 0
        var header: Int32 = 0
        guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: 0,
            parameterSetPointerOut: &pointer, parameterSetSizeOut: &size,
            parameterSetCountOut: &count, nalUnitHeaderLengthOut: &header) == noErr,
            count >= 2, count <= 16, [1, 2, 4].contains(header) else {
            throw ConversionError.invalid("missing parameter sets or bad NAL length size")
        }
        var output = Data()
        if isKeyframe(sample) {
            for index in 0..<count {
                guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: index,
                    parameterSetPointerOut: &pointer, parameterSetSizeOut: &size,
                    parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil) == noErr,
                    let pointer, size > 0, size <= 65_536 else {
                    throw ConversionError.invalid("unreadable parameter set")
                }
                output.append(contentsOf: [0, 0, 0, 1])
                output.append(pointer, count: size)
            }
        }
        let length = CMBlockBufferGetDataLength(block)
        guard length > 0, length <= maximumAccessUnit else { throw ConversionError.invalid("sample size \(length)") }
        var input = [UInt8](repeating: 0, count: length)
        let status = input.withUnsafeMutableBytes {
            CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: length, destination: $0.baseAddress!)
        }
        guard status == noErr else { throw ConversionError.invalid("copy failed (OSStatus \(status))") }
        var offset = 0
        while offset < input.count {
            guard input.count - offset >= Int(header) else { throw ConversionError.invalid("truncated NAL length") }
            var nalSize = 0
            for _ in 0..<Int(header) {
                nalSize = (nalSize << 8) | Int(input[offset])
                offset += 1
            }
            guard nalSize > 0, nalSize <= input.count - offset else { throw ConversionError.invalid("bad NAL size") }
            output.append(contentsOf: [0, 0, 0, 1])
            output.append(contentsOf: input[offset..<(offset + nalSize)])
            offset += nalSize
        }
        guard output.count <= maximumAccessUnit else { throw ConversionError.invalid("access unit too large") }
        return output
    }
}
