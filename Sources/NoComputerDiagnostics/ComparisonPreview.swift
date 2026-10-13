import AppKit

enum PreviewGeometry {
    static func documentSize(pixels: CGSize, viewport: CGSize, backingScale: CGFloat, native: Bool) -> CGSize {
        guard backingScale.isFinite, backingScale > 0 else { return viewport }
        if !native { return viewport }
        return CGSize(width: max(viewport.width, pixels.width / backingScale),
                      height: max(viewport.height, pixels.height / backingScale))
    }

    static func imageRect(pixels: CGSize, bounds: CGRect, backingScale: CGFloat, native: Bool) -> CGRect {
        guard pixels.width > 0, pixels.height > 0, backingScale > 0,
              pixels.width.isFinite, pixels.height.isFinite, backingScale.isFinite else { return .zero }
        let scale = native ? 1 / backingScale : min(bounds.width / pixels.width, bounds.height / pixels.height)
        let size = CGSize(width: pixels.width * scale, height: pixels.height * scale)
        let x = ((bounds.midX - size.width / 2) * backingScale).rounded() / backingScale
        let y = ((bounds.midY - size.height / 2) * backingScale).rounded() / backingScale
        return CGRect(origin: CGPoint(x: x, y: y), size: size)
    }
}

final class ComparisonImageView: NSView {
    var image: CGImage? { didSet { updateSize(); needsDisplay = true } }
    var nativePixels = false { didSet { updateSize(); needsDisplay = true } }

    func updateSize() {
        guard let scroll = enclosingScrollView else { return }
        let pixels = image.map { CGSize(width: $0.width, height: $0.height) } ?? .zero
        let size = PreviewGeometry.documentSize(pixels: pixels, viewport: scroll.contentSize,
            backingScale: window?.backingScaleFactor ?? 1, native: nativePixels)
        if frame.size != size { setFrameSize(size) }
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        updateSize()
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.black.setFill()
        bounds.fill()
        guard let image, let context = NSGraphicsContext.current?.cgContext else { return }
        let rectangle = PreviewGeometry.imageRect(pixels: CGSize(width: image.width, height: image.height),
            bounds: bounds, backingScale: window?.backingScaleFactor ?? 1, native: nativePixels)
        context.interpolationQuality = nativePixels ? .none : .high
        context.draw(image, in: rectangle)
    }
}

final class ComparisonScrollView: NSScrollView {
    override func layout() {
        super.layout()
        (documentView as? ComparisonImageView)?.updateSize()
        documentView?.needsDisplay = true
    }
}
