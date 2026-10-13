import CoreGraphics
import Testing
@testable import NoComputerDiagnostics

struct PreviewGeometryTests {
    @Test func fitPreservesAspectAndLetterboxes() {
        let result = PreviewGeometry.imageRect(pixels: CGSize(width: 1920, height: 1080),
            bounds: CGRect(x: 0, y: 0, width: 1920, height: 1200), backingScale: 2, native: false)
        #expect(result == CGRect(x: 0, y: 60, width: 1920, height: 1080))
    }

    @Test func nativeMapsOneVideoPixelToOneBackingPixel() {
        let pixels = CGSize(width: 1920, height: 1080)
        let viewport = CGSize(width: 800, height: 500)
        let size = PreviewGeometry.documentSize(pixels: pixels, viewport: viewport, backingScale: 2, native: true)
        #expect(size == CGSize(width: 960, height: 540))
        let result = PreviewGeometry.imageRect(pixels: pixels, bounds: CGRect(origin: .zero, size: size),
            backingScale: 2, native: true)
        #expect(result.size.width * 2 == pixels.width)
        #expect(result.size.height * 2 == pixels.height)
    }

    @Test func nativeCentersSmallImageWithoutUpscaling() {
        #expect(PreviewGeometry.imageRect(pixels: CGSize(width: 400, height: 200),
            bounds: CGRect(x: 0, y: 0, width: 800, height: 500), backingScale: 1, native: true)
            == CGRect(x: 200, y: 150, width: 400, height: 200))
    }

    @Test func invalidImageOrScaleHasNoDrawableRect() {
        #expect(PreviewGeometry.imageRect(pixels: .zero, bounds: .zero, backingScale: 2, native: true) == .zero)
        #expect(PreviewGeometry.imageRect(pixels: CGSize(width: 100, height: 100), bounds: .zero,
            backingScale: 0, native: true) == .zero)
    }
}
