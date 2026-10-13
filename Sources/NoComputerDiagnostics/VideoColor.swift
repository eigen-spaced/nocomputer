import CoreGraphics
import CoreVideo
import VideoToolbox

enum VideoColorPolicy: String {
    case systemDefault = "system/display defaults"
    case sRGB = "explicit SDR sRGB / BT.709 matrix"
}

enum VideoColor {
    // Only known colour fields are exported, never arbitrary buffer attachments.
    static func bufferMetadata(_ buffer: CVPixelBuffer) -> [String: String] {
        var result: [String: String] = ["pixelFormat": String(CVPixelBufferGetPixelFormatType(buffer))]
        for (name, key) in [("primaries", kCVImageBufferColorPrimariesKey),
                            ("transfer", kCVImageBufferTransferFunctionKey),
                            ("matrix", kCVImageBufferYCbCrMatrixKey)] {
            result[name] = CVBufferCopyAttachment(buffer, key, nil).map { String(describing: $0) } ?? "unspecified"
        }
        if let value = CVBufferCopyAttachment(buffer, kCVImageBufferCGColorSpaceKey, nil),
           CFGetTypeID(value) == CGColorSpace.typeID {
            let space = unsafeBitCast(value, to: CGColorSpace.self)
            result["colorSpace"] = space.name.map { String($0) } ?? "unnamed"
        } else { result["colorSpace"] = "unspecified" }
        return result
    }

    static func formatMetadata(_ format: CMFormatDescription) -> [String: String] {
        var result: [String: String] = [:]
        for (name, key) in [("primaries", kCMFormatDescriptionExtension_ColorPrimaries),
                            ("transfer", kCMFormatDescriptionExtension_TransferFunction),
                            ("matrix", kCMFormatDescriptionExtension_YCbCrMatrix),
                            ("fullRange", kCMFormatDescriptionExtension_FullRangeVideo)] {
            result[name] = CMFormatDescriptionGetExtension(format, extensionKey: key).map { String(describing: $0) } ?? "unspecified"
        }
        return result
    }
}
