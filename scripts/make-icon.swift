// Renders the Local Dictation app icon: a rounded-rect indigo→violet gradient
// with a white SF Symbol "waveform" glyph centered on it. Writes a 1024×1024
// PNG to the path given as the first argument.
//
//   swift scripts/make-icon.swift /tmp/icon-1024.png
//
// build-app.sh downsizes this into a full .iconset and runs iconutil.

import AppKit

let outPath = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "/tmp/icon-1024.png"
let side: CGFloat = 1024

let image = NSImage(size: NSSize(width: side, height: side))
image.lockFocus()
guard let ctx = NSGraphicsContext.current?.cgContext else {
    fatalError("no graphics context")
}

// Rounded-rect background with a vertical gradient. Inset slightly so the
// macOS icon grid's rounded corners look right.
let inset: CGFloat = 80
let rect = CGRect(x: inset, y: inset, width: side - inset * 2, height: side - inset * 2)
let corner: CGFloat = (side - inset * 2) * 0.225 // ~ macOS squircle radius
let path = NSBezierPath(roundedRect: rect, xRadius: corner, yRadius: corner)
path.addClip()

let top = NSColor(calibratedRed: 0.43, green: 0.36, blue: 0.93, alpha: 1).cgColor    // indigo
let bottom = NSColor(calibratedRed: 0.70, green: 0.33, blue: 0.92, alpha: 1).cgColor // violet
let gradient = CGGradient(
    colorsSpace: CGColorSpaceCreateDeviceRGB(),
    colors: [top, bottom] as CFArray,
    locations: [0, 1]
)!
ctx.drawLinearGradient(
    gradient,
    start: CGPoint(x: 0, y: side),
    end: CGPoint(x: 0, y: 0),
    options: []
)

// Centered white "waveform" SF Symbol.
let glyphSide = side * 0.52
let config = NSImage.SymbolConfiguration(pointSize: glyphSide, weight: .semibold)
if let symbol = NSImage(systemSymbolName: "waveform", accessibilityDescription: nil)?
    .withSymbolConfiguration(config) {
    let tinted = NSImage(size: symbol.size)
    tinted.lockFocus()
    NSColor.white.set()
    let r = CGRect(origin: .zero, size: symbol.size)
    symbol.draw(in: r)
    r.fill(using: .sourceAtop)
    tinted.unlockFocus()

    let gx = (side - tinted.size.width) / 2
    let gy = (side - tinted.size.height) / 2
    tinted.draw(in: CGRect(x: gx, y: gy, width: tinted.size.width, height: tinted.size.height))
}

image.unlockFocus()

guard let tiff = image.tiffRepresentation,
      let bitmap = NSBitmapImageRep(data: tiff),
      let png = bitmap.representation(using: .png, properties: [:]) else {
    fatalError("failed to encode PNG")
}
try! png.write(to: URL(fileURLWithPath: outPath))
print("wrote \(outPath)")
