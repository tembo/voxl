import AppKit
import Foundation

/// What a tailnet computer offers.
enum VoxlStatus: Hashable {
    case checking
    /// voxl answers over HTTPS at `url`.
    case ready(url: String, selfSigned: Bool)
    /// voxl answers, but only over plain HTTP, where browsers won't decode video.
    case httpOnly
    case notRunning

    var isReady: Bool { if case .ready = self { return true } else { return false } }
}

/// A computer on the user's tailnet.
struct TailnetDevice: Identifiable, Hashable {
    var id: String { dns }
    let name: String
    let dns: String
    let ip: String?
    let os: String
    let isSelf: Bool
    var voxl: VoxlStatus

    var symbol: String {
        switch os.lowercased() {
        case "macos": return name.lowercased().contains("book") ? "laptopcomputer" : "desktopcomputer"
        case "windows": return "pc"
        default: return "server.rack"
        }
    }
}

/// Lists the computers on the user's tailnet and checks which ones run voxl.
@MainActor
final class TailnetDiscovery: ObservableObject {
    enum State: Equatable {
        case idle, searching, done
        /// Tailscale is missing, off or unreadable; `action` opens or installs it.
        case unavailable(String, action: Action?)
    }

    enum Action: Equatable { case openTailscale, installTailscale }

    @Published private(set) var state: State = .idle
    @Published private(set) var devices: [TailnetDevice] = []
    @Published private(set) var offlineCount = 0

    /// 6443: `tailscale serve --https=6443`; 443: `tailscale serve` on its default port;
    /// 6901: voxl's own port with --tls.
    nonisolated static let ports = [6443, 443, 6901]
    /// Phones, tablets and TVs don't run voxl.
    nonisolated private static let desktopOS: Set<String> = ["macos", "linux", "windows", "freebsd", "openbsd"]

    private var refreshing = false

    func refresh() async {
        guard !refreshing else { return }
        refreshing = true
        defer { refreshing = false }
        if devices.isEmpty { state = .searching }

        let result = await Task.detached { Self.readStatus() }.value
        let status: Status
        switch result {
        case .failure(let problem):
            state = .unavailable(problem.message, action: problem.action)
            devices = []
            return
        case .success(let s):
            status = s
        }

        var nodes: [(Node, Bool)] = [(status.Self_, true)]
        let peers = (status.Peer ?? [:]).values.filter { Self.desktopOS.contains(($0.OS ?? "").lowercased()) }
        nodes += peers.filter { $0.Online == true }.map { ($0, false) }
        offlineCount = peers.filter { $0.Online != true }.count

        // Keep what's known while re-checking, so rows don't flicker.
        let known = Dictionary(devices.map { ($0.dns, $0.voxl) }, uniquingKeysWith: { a, _ in a })
        devices = nodes.compactMap { node, isSelf in
            let dns = node.DNSName.hasSuffix(".") ? String(node.DNSName.dropLast()) : node.DNSName
            guard !dns.isEmpty else { return nil }
            return TailnetDevice(name: node.HostName, dns: dns, ip: node.TailscaleIPs?.first, os: node.OS ?? "",
                                 isSelf: isSelf, voxl: known[dns] ?? .checking)
        }
        sort()
        state = .done

        await withTaskGroup(of: (String, VoxlStatus).self) { group in
            for d in devices {
                group.addTask { (d.dns, await Self.probe(dns: d.dns, ip: d.ip)) }
            }
            for await (dns, voxl) in group {
                if let i = devices.firstIndex(where: { $0.dns == dns }) { devices[i].voxl = voxl }
            }
        }
        sort()
    }

    private func sort() {
        devices.sort {
            ($0.voxl.isReady ? 0 : 1, $0.isSelf ? 1 : 0, $0.name.lowercased())
                < ($1.voxl.isReady ? 0 : 1, $1.isSelf ? 1 : 0, $1.name.lowercased())
        }
    }

    // MARK: Probing

    nonisolated static func probe(dns: String, ip: String?) async -> VoxlStatus {
        // Try the usual HTTPS ports together; prefer them in the order listed.
        let found = await withTaskGroup(of: (Int, Bool?).self) { group in
            for port in ports {
                group.addTask {
                    let base = port == 443 ? "https://\(dns)" : "https://\(dns):\(port)"
                    return (port, await Probe.voxl(at: base))
                }
            }
            var out: [Int: Bool] = [:]
            for await (port, selfSigned) in group { if let selfSigned { out[port] = selfSigned } }
            return out
        }
        for port in ports {
            if let selfSigned = found[port] {
                return .ready(url: port == 443 ? "https://\(dns)" : "https://\(dns):\(port)", selfSigned: selfSigned)
            }
        }
        if let ip, await Probe.voxl(at: "http://\(ip):6901") != nil { return .httpOnly }
        return .notRunning
    }

    // MARK: Tailscale CLI

    struct Node: Decodable {
        let HostName: String
        let DNSName: String
        let OS: String?
        let Online: Bool?
        let TailscaleIPs: [String]?
    }

    struct Status: Decodable {
        let BackendState: String
        let Self_: Node
        let Peer: [String: Node]?
        enum CodingKeys: String, CodingKey { case BackendState, Self_ = "Self", Peer }
    }

    struct Problem: Error {
        let message: String
        let action: Action?
    }

    /// The Tailscale app (standalone or App Store) carries its CLI inside the bundle;
    /// Homebrew and the open-source build put `tailscale` on the PATH.
    nonisolated static func tailscaleCLI() -> String? {
        var candidates = ["/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                          NSHomeDirectory() + "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                          "/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale"]
        if let app = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "io.tailscale.ipn.macos")
            ?? NSWorkspace.shared.urlForApplication(withBundleIdentifier: "io.tailscale.ipn.macsys") {
            candidates.insert(app.appendingPathComponent("Contents/MacOS/Tailscale").path, at: 0)
        }
        return candidates.first { FileManager.default.isExecutableFile(atPath: $0) }
    }

    nonisolated static func readStatus() -> Result<Status, Problem> {
        guard let cli = tailscaleCLI() else {
            return .failure(Problem(message: "Install Tailscale to see the computers on your tailnet.",
                                    action: .installTailscale))
        }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: cli)
        p.arguments = ["status", "--json"]
        // The Tailscale app's binary acts as the CLI when asked to.
        var env = ProcessInfo.processInfo.environment
        env["TAILSCALE_BE_CLI"] = "1"
        p.environment = env
        let out = Pipe(), err = Pipe()
        p.standardOutput = out
        p.standardError = err
        p.standardInput = FileHandle.nullDevice
        do { try p.run() } catch {
            return .failure(Problem(message: "Couldn't run Tailscale: \(error.localizedDescription)", action: nil))
        }
        // Never hang on it.
        let deadline = DispatchTime.now() + 5
        DispatchQueue.global().asyncAfter(deadline: deadline) { if p.isRunning { p.terminate() } }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        let errText = String(decoding: err.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        p.waitUntilExit()

        guard let status = try? JSONDecoder().decode(Status.self, from: data) else {
            let why = errText.split(separator: "\n").first.map(String.init) ?? ""
            if why.lowercased().contains("not running") || why.lowercased().contains("failed to connect") {
                return .failure(Problem(message: "Tailscale isn't running.", action: .openTailscale))
            }
            return .failure(Problem(message: "Couldn't read Tailscale's status" + (why.isEmpty ? "." : ": \(why)"),
                                    action: .openTailscale))
        }
        switch status.BackendState {
        case "Running":
            return .success(status)
        case "NeedsLogin", "NeedsMachineAuth":
            return .failure(Problem(message: "Sign in to Tailscale to see your computers.", action: .openTailscale))
        default:
            return .failure(Problem(message: "Tailscale is turned off. Turn it on to see your computers.",
                                    action: .openTailscale))
        }
    }
}

/// Checks whether a voxl server answers at an address.
enum Probe {
    private final class Delegate: NSObject, URLSessionDelegate {
        var selfSigned = false
        func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
            guard let trust = challenge.protectionSpace.serverTrust else {
                return completionHandler(.performDefaultHandling, nil)
            }
            // Probing only: a self-signed voxl (--tls) still counts, and the viewer
            // asks before trusting it.
            if !SecTrustEvaluateWithError(trust, nil) { selfSigned = true }
            completionHandler(.useCredential, URLCredential(trust: trust))
        }
    }

    /// nil when no voxl answers; otherwise whether its certificate is self-signed.
    static func voxl(at base: String) async -> Bool? {
        guard let health = URL(string: base + "/healthz"),
              let worker = URL(string: base + "/worker.js") else { return nil }
        let d = Delegate()
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = 3
        let session = URLSession(configuration: cfg, delegate: d, delegateQueue: nil)
        defer { session.finishTasksAndInvalidate() }
        do {
            // /healthz says "ok"; voxl's worker script (served without sign-in) tells it
            // apart from other servers that happen to have a health check.
            let (body, r1) = try await session.data(from: health)
            guard (r1 as? HTTPURLResponse)?.statusCode == 200,
                  String(decoding: body, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines) == "ok"
            else { return nil }
            let (_, r2) = try await session.data(from: worker)
            guard let h = r2 as? HTTPURLResponse, h.statusCode == 200,
                  (h.value(forHTTPHeaderField: "Content-Type") ?? "").contains("javascript")
            else { return nil }
            return d.selfSigned
        } catch {
            return nil
        }
    }
}
