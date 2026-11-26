import CryptoKit
import Foundation

/// Whether a voxl server is up, without signing in: `/healthz` needs no password.
enum Health: Equatable {
    case up
    /// The address answers but voxl doesn't (e.g. `tailscale serve` with voxl stopped).
    case notRunning
    /// Nothing answers: the computer is asleep, offline or off the tailnet.
    case unreachable(String)
    /// The certificate isn't trusted and isn't the one pinned for this connection.
    case untrusted

    var isUp: Bool { self == .up }

    /// Checks `/healthz` at `base`, trusting a self-signed certificate only when it
    /// matches `pinned`.
    static func check(_ base: URL, pinned: String?, timeout: TimeInterval = 4) async -> Health {
        let url = base.appendingPathComponent("healthz")
        let d = TrustDelegate(pinned: pinned)
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = timeout
        cfg.waitsForConnectivity = false
        let session = URLSession(configuration: cfg, delegate: d, delegateQueue: nil)
        defer { session.finishTasksAndInvalidate() }
        do {
            let (body, r) = try await session.data(from: url)
            let code = (r as? HTTPURLResponse)?.statusCode ?? 0
            let text = String(decoding: body, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
            return code == 200 && text == "ok" ? .up : .notRunning
        } catch {
            if d.rejected { return .untrusted }
            let e = error as NSError
            switch e.code {
            case NSURLErrorCannotFindHost, NSURLErrorDNSLookupFailed:
                return .unreachable("Can't find \(base.host ?? "the computer") on the network.")
            case NSURLErrorNotConnectedToInternet:
                return .unreachable("This Mac is offline.")
            case NSURLErrorTimedOut:
                return .unreachable("\(base.host ?? "The computer") isn't answering. It may be asleep or off your tailnet.")
            case NSURLErrorCannotConnectToHost:
                return .notRunning
            default:
                return .unreachable(e.localizedDescription)
            }
        }
    }

    static func fingerprint(_ trust: SecTrust) -> String? {
        guard let chain = SecTrustCopyCertificateChain(trust) as? [SecCertificate],
              let leaf = chain.first else { return nil }
        let data = SecCertificateCopyData(leaf) as Data
        return SHA256.hash(data: data).map { String(format: "%02X", $0) }.joined()
    }

    private final class TrustDelegate: NSObject, URLSessionDelegate {
        let pinned: String?
        var rejected = false
        init(pinned: String?) { self.pinned = pinned }

        func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
            guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
                  let trust = challenge.protectionSpace.serverTrust else {
                return completionHandler(.performDefaultHandling, nil)
            }
            if SecTrustEvaluateWithError(trust, nil) { return completionHandler(.performDefaultHandling, nil) }
            if let pinned, Health.fingerprint(trust) == pinned {
                return completionHandler(.useCredential, URLCredential(trust: trust))
            }
            rejected = true
            completionHandler(.cancelAuthenticationChallenge, nil)
        }
    }
}
