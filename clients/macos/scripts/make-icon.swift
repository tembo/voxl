// Draws the app icon: swift scripts/make-icon.swift Resources/AppIcon.iconset
import AppKit

let out = CommandLine.arguments[1]
try? FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)

func draw(_ px: Int) -> Data {
    let s = CGFloat(px)
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    // macOS icon grid: an 824/1024 rounded square, centred.
    let inset = s * 100 / 1024
    let box = NSRect(x: inset, y: inset, width: s - inset * 2, height: s - inset * 2)
    let r = box.width * 0.225
    let shape = NSBezierPath(roundedRect: box, xRadius: r, yRadius: r)
    NSGraphicsContext.current?.cgContext.setShadow(offset: CGSize(width: 0, height: -s * 0.012),
                                                   blur: s * 0.03, color: NSColor.black.withAlphaComponent(0.35).cgColor)
    NSGradient(colors: [NSColor(red: 0.12, green: 0.10, blue: 0.30, alpha: 1),
                        NSColor(red: 0.36, green: 0.20, blue: 0.85, alpha: 1)])!.draw(in: shape, angle: 60)
    NSGraphicsContext.current?.cgContext.setShadow(offset: .zero, blur: 0, color: nil)
    // A soft highlight across the top.
    NSGraphicsContext.saveGraphicsState()
    shape.addClip()
    NSGradient(colors: [NSColor.white.withAlphaComponent(0.18), NSColor.white.withAlphaComponent(0)])!
        .draw(in: NSRect(x: box.minX, y: box.midY, width: box.width, height: box.height / 2), angle: 270)
    NSGraphicsContext.restoreGraphicsState()
    // The screen, with a stream of light running into it.
    let cfg = NSImage.SymbolConfiguration(pointSize: s * 0.40, weight: .medium)
        .applying(.init(paletteColors: [.white]))
    if let sym = NSImage(systemSymbolName: "display", accessibilityDescription: nil)?.withSymbolConfiguration(cfg) {
        let sz = sym.size
        sym.draw(in: NSRect(x: (s - sz.width) / 2, y: (s - sz.height) / 2 + s * 0.02, width: sz.width, height: sz.height))
    }
    let bolt = NSImage.SymbolConfiguration(pointSize: s * 0.15, weight: .bold)
        .applying(.init(paletteColors: [NSColor(red: 0.55, green: 0.95, blue: 0.85, alpha: 1)]))
    if let b = NSImage(systemSymbolName: "bolt.fill", accessibilityDescription: nil)?.withSymbolConfiguration(bolt) {
        let sz = b.size
        b.draw(in: NSRect(x: (s - sz.width) / 2, y: (s - sz.height) / 2 + s * 0.065, width: sz.width, height: sz.height))
    }
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

for base in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
        try! draw(base * scale).write(to: URL(fileURLWithPath: "\(out)/\(name)"))
    }
}
