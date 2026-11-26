import AppKit
import Foundation

/// Small pictures of each saved computer's screen, for the gallery and as the backdrop
/// while a viewer connects. Kept in the app's caches folder between launches.
@MainActor
final class Thumbnails: ObservableObject {
    static let shared = Thumbnails()

    @Published private(set) var images: [UUID: NSImage] = [:]
    private var fetched: [UUID: Date] = [:]
    private let dir: URL

    /// Wide enough for a large gallery card on a Retina screen.
    private static let width: CGFloat = 720

    private init() {
        dir = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("local.voxl.viewer/thumbnails", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        for url in (try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)) ?? [] {
            if let id = UUID(uuidString: url.deletingPathExtension().lastPathComponent),
               let img = NSImage(contentsOf: url) { images[id] = img }
        }
    }

    func set(_ image: NSImage, for id: UUID) {
        guard let small = Self.downscale(image) else { return }
        images[id] = small
        if let tiff = small.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
           let jpg = rep.representation(using: .jpeg, properties: [.compressionFactor: 0.8]) {
            try? jpg.write(to: dir.appendingPathComponent(id.uuidString + ".jpg"))
        }
    }

    func remove(_ id: UUID) {
        images[id] = nil
        try? FileManager.default.removeItem(at: dir.appendingPathComponent(id.uuidString + ".jpg"))
    }

    /// Asks the computer's voxl for a screenshot, at most once a minute.
    func refresh(_ c: Connection, force: Bool = false) async {
        if !force, let last = fetched[c.id], Date().timeIntervalSince(last) < 60 { return }
        fetched[c.id] = Date()
        // Never send the password over plain HTTP to another machine.
        guard c.isSecureContext, let base = c.baseURL, let password = c.quietPassword else { return }
        var req = URLRequest(url: base.appendingPathComponent("screenshot"))
        req.setValue("Bearer \(password)", forHTTPHeaderField: "Authorization")
        req.timeoutInterval = 8
        let cfg = URLSessionConfiguration.ephemeral
        let session = URLSession(configuration: cfg, delegate: PinnedTrust(pinned: c.certFingerprint), delegateQueue: nil)
        defer { session.finishTasksAndInvalidate() }
        guard let (data, r) = try? await session.data(for: req),
              (r as? HTTPURLResponse)?.statusCode == 200,
              let image = NSImage(data: data) else { return }
        // It may have been removed while the screenshot was on its way.
        guard ConnectionStore.shared.connection(c.id) != nil else { return }
        set(image, for: c.id)
    }

    private static func downscale(_ image: NSImage) -> NSImage? {
        guard let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else { return nil }
        let scale = min(1, width / CGFloat(cg.width))
        let w = Int(CGFloat(cg.width) * scale), h = Int(CGFloat(cg.height) * scale)
        guard w > 0, h > 0,
              let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpaceCreateDeviceRGB(),
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.interpolationQuality = .high
        ctx.draw(cg, in: CGRect(x: 0, y: 0, width: w, height: h))
        guard let out = ctx.makeImage() else { return nil }
        return NSImage(cgImage: out, size: NSSize(width: w, height: h))
    }
}

/// Trusts the system's authorities, plus one pinned self-signed certificate.
final class PinnedTrust: NSObject, URLSessionDelegate {
    let pinned: String?
    init(pinned: String?) { self.pinned = pinned }

    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        guard let trust = challenge.protectionSpace.serverTrust else {
            return completionHandler(.performDefaultHandling, nil)
        }
        if SecTrustEvaluateWithError(trust, nil) { return completionHandler(.performDefaultHandling, nil) }
        if let pinned, Health.fingerprint(trust) == pinned {
            return completionHandler(.useCredential, URLCredential(trust: trust))
        }
        completionHandler(.cancelAuthenticationChallenge, nil)
    }
}
