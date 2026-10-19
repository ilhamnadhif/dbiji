// Render an SVG to a square PNG with a transparent background (the icon of
// the macOS bundle). QuickLook thumbnails would add a white background.
// Usage: swift scripts/render-icon.swift <in.svg> <out.png> <size>
import AppKit

let args = CommandLine.arguments
guard args.count == 4, let size = Int(args[3]), let image = NSImage(contentsOfFile: args[1]) else {
    FileHandle.standardError.write("usage: render-icon.swift <in.svg> <out.png> <size>\n".data(using: .utf8)!)
    exit(2)
}
guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8, samplesPerPixel: 4,
                                 hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0) else { exit(1) }
rep.size = NSSize(width: size, height: size)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
NSColor.clear.set()
NSRect(x: 0, y: 0, width: size, height: size).fill(using: .copy)
image.draw(in: NSRect(x: 0, y: 0, width: size, height: size), from: .zero, operation: .sourceOver, fraction: 1.0)
NSGraphicsContext.restoreGraphicsState()
guard let png = rep.representation(using: .png, properties: [:]) else { exit(1) }
try png.write(to: URL(fileURLWithPath: args[2]))
