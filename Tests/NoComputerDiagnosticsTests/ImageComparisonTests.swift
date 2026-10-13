import CoreImage
import Testing
@testable import NoComputerDiagnostics

struct ImageComparisonTests {
    @Test func identicalImagesHaveNoAggregateDifference() throws {
        let context = CIContext()
        let source = try #require(context.createCGImage(CIImage(color: CIColor(red: 0.2, green: 0.3, blue: 0.4))
            .cropped(to: CGRect(x: 0, y: 0, width: 64, height: 64)), from: CGRect(x: 0, y: 0, width: 64, height: 64)))
        let metrics = ImageComparison.measure(source: source, decoded: source, context: context)
        #expect((metrics["meanAbsoluteRGBError"] as? Double) == 0)
        #expect((metrics["meanSignedRGBShift"] as? Double) == 0)
        #expect((metrics["sourceHasSampledTransparency"] as? Bool) == false)
    }

    @Test func brighterDecodedImageHasPositiveShift() throws {
        let context = CIContext()
        let bounds = CGRect(x: 0, y: 0, width: 64, height: 64)
        func image(_ gray: CGFloat) throws -> CGImage {
            try #require(context.createCGImage(CIImage(color: CIColor(red: gray, green: gray, blue: gray))
                .cropped(to: bounds), from: bounds))
        }
        let metrics = ImageComparison.measure(source: try image(0.1), decoded: try image(0.5), context: context)
        #expect((metrics["meanSignedRGBShift"] as? Double ?? 0) > 0)
        #expect((metrics["meanAbsoluteRGBError"] as? Double ?? 0) > 0)
    }
}
