import CoreImage

enum ImageComparison {
    // Small in-memory grid; export aggregates only, never samples or images.
    static func measure(source: CGImage, decoded: CGImage, context: CIContext) -> [String: Any] {
        let width = 32
        let height = 32
        guard source.width == decoded.width, source.height == decoded.height,
              let space = CGColorSpace(name: CGColorSpace.sRGB) else { return ["error": "Mismatched image dimensions"] }
        func pixels(_ image: CGImage) -> [UInt8] {
            let scaleX = CGFloat(width) / CGFloat(image.width)
            let scaleY = CGFloat(height) / CGFloat(image.height)
            let transform = CGAffineTransform(scaleX: scaleX, y: scaleY)
            let ci = CIImage(cgImage: image).transformed(by: transform)
            var data = [UInt8](repeating: 0, count: width * height * 4)
            data.withUnsafeMutableBytes {
                context.render(ci, toBitmap: $0.baseAddress!, rowBytes: width * 4,
                    bounds: CGRect(x: 0, y: 0, width: width, height: height), format: .RGBA8, colorSpace: space)
            }
            return data
        }
        let raw = pixels(source)
        let output = pixels(decoded)
        var absolute = 0.0
        var signed = 0.0
        var darkShift = 0.0
        var darkCount = 0
        var transparent = false
        for offset in stride(from: 0, to: raw.count, by: 4) {
            transparent = transparent || raw[offset + 3] < 254
            let rawMean = (Double(raw[offset]) + Double(raw[offset + 1]) + Double(raw[offset + 2])) / 3
            for channel in 0..<3 {
                let difference = Double(output[offset + channel]) - Double(raw[offset + channel])
                absolute += abs(difference)
                signed += difference
                if rawMean < 64 { darkShift += difference; darkCount += 1 }
            }
        }
        let channels = Double(width * height * 3)
        return ["sampleGridWidth": width, "sampleGridHeight": height,
                "meanAbsoluteRGBError": absolute / channels, "meanSignedRGBShift": signed / channels,
                "meanDarkRGBShift": darkCount > 0 ? darkShift / Double(darkCount) : 0.0,
                "sourceHasSampledTransparency": transparent,
                "interpretation": "sRGB 8-bit aggregate grid; not a perceptual quality score"]
    }
}
