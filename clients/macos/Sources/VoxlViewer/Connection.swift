import Foundation
import Security

/// A saved voxl server. The password lives in the Keychain, never in here.
struct Connection: Codable, Identifiable, Hashable {
    var id = UUID()
    var name: String
    /// The server's address without the token, e.g. https://mini.tailnet.ts.net:6443
    var url: String
    /// SHA-256 of a self-signed certificate the user chose to trust (voxl --tls).
    var certFingerprint: String?

    var baseURL: URL? { Connection.normalize(url) }
    var host: String { baseURL?.host ?? url }

    var password: String? {
        get { Keychain.get(id.uuidString, interactive: true) }
        nonmutating set { storePassword(newValue) }
    }

    /// Saves (or with nil, removes) the password; false if the Keychain refused.
    @discardableResult
    func storePassword(_ value: String?) -> Bool {
        if let value, !value.isEmpty { return Keychain.set(value, for: id.uuidString) }
        Keychain.delete(id.uuidString)
        return true
    }

    /// The password if it can be read without macOS asking the user (for background
    /// work such as gallery previews).
    var quietPassword: String? { Keychain.get(id.uuidString, interactive: false) }

    /// The URL the viewer opens: voxl's page signs in with `?token=` and passes it on
    /// to its own WebSocket, worker and file requests.
    func loadURL(password: String?) -> URL? {
        guard let base = baseURL, var c = URLComponents(url: base, resolvingAgainstBaseURL: false)
        else { return nil }
        var items = (c.queryItems ?? []).filter { $0.name != "token" }
        if let password, !password.isEmpty { items.append(URLQueryItem(name: "token", value: password)) }
        c.queryItems = items.isEmpty ? nil : items
        return c.url
    }

    /// True when browsers will decode video there: HTTPS, or plain HTTP on this Mac.
    var isSecureContext: Bool {
        guard let u = baseURL else { return false }
        if u.scheme == "https" { return true }
        return ["localhost", "127.0.0.1", "::1"].contains(u.host ?? "")
    }

    /// Adds https:// when the scheme is missing; nil when it isn't a usable address.
    /// Credentials in the address (`user:secret@host`) are dropped: passwords belong in
    /// the Keychain, not in the saved address.
    static func normalize(_ s: String) -> URL? {
        var s = s.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !s.isEmpty else { return nil }
        if !s.contains("://") { s = "https://" + s }
        guard var c = URLComponents(string: s), let scheme = c.scheme?.lowercased(),
              ["http", "https"].contains(scheme), c.host?.isEmpty == false else { return nil }
        c.user = nil
        c.password = nil
        return c.url
    }

    /// Splits a pasted sign-in link (…/?token=…) into its address and password.
    static func split(link: String) -> (url: String, token: String?) {
        let trimmed = link.trimmingCharacters(in: .whitespacesAndNewlines)
        // A password in the address (https://:secret@host) counts as the token.
        let userInfoPassword = URLComponents(string: trimmed.contains("://") ? trimmed : "https://" + trimmed)?.password
        guard let u = normalize(trimmed), var c = URLComponents(url: u, resolvingAgainstBaseURL: false)
        else { return (trimmed, nil) }
        let token = c.queryItems?.first(where: { $0.name == "token" })?.value ?? userInfoPassword
        let rest = (c.queryItems ?? []).filter { $0.name != "token" }
        c.queryItems = rest.isEmpty ? nil : rest
        var out = c.string ?? trimmed
        if out.hasSuffix("/") && c.path == "/" && c.query == nil { out.removeLast() }
        return (out, token)
    }
}

/// Saved connections, kept in UserDefaults.
final class ConnectionStore: ObservableObject {
    static let shared = ConnectionStore()
    private let key = "connections"

    @Published private(set) var connections: [Connection] = []

    private init() {
        if let data = UserDefaults.standard.data(forKey: key),
           let list = try? JSONDecoder().decode([Connection].self, from: data) {
            connections = list
        }
    }

    private func save() {
        if let data = try? JSONEncoder().encode(connections) {
            UserDefaults.standard.set(data, forKey: key)
        }
    }

    func upsert(_ c: Connection) {
        if let i = connections.firstIndex(where: { $0.id == c.id }) { connections[i] = c }
        else { connections.append(c) }
        save()
    }

    func delete(_ c: Connection) {
        c.password = nil
        connections.removeAll { $0.id == c.id }
        save()
    }

    func connection(_ id: UUID) -> Connection? { connections.first { $0.id == id } }

    /// Whether a saved connection already points at this address.
    func contains(url: String) -> Bool {
        guard let u = Connection.normalize(url) else { return false }
        return connections.contains {
            guard let b = $0.baseURL else { return false }
            return b.host == u.host && (b.port ?? 443) == (u.port ?? 443)
        }
    }
}

/// Passwords in the login Keychain, remembered for the session once read.
enum Keychain {
    static let service = "Voxl Viewer"
    private static var cache: [String: String] = [:]
    private static let lock = NSLock()

    private static func base(_ account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account]
    }

    /// `interactive: false` never shows macOS's "wants to access the keychain" prompt
    /// (which can appear after the app is updated); it returns nil instead.
    static func get(_ account: String, interactive: Bool = true) -> String? {
        lock.lock(); defer { lock.unlock() }
        if let hit = cache[account] { return hit }
        var q = base(account)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        // The login keychain ignores kSecUseAuthenticationUI; this older switch is what
        // keeps its prompt away.
        if !interactive { SecKeychainSetUserInteractionAllowed(false) }
        let status = SecItemCopyMatching(q as CFDictionary, &out)
        if !interactive { SecKeychainSetUserInteractionAllowed(true) }
        guard status == errSecSuccess,
              let data = out as? Data, let s = String(data: data, encoding: .utf8) else { return nil }
        cache[account] = s
        return s
    }

    /// Updates the item in place (adding it if there's none); false if the Keychain
    /// refused, in which case nothing changes.
    @discardableResult
    static func set(_ value: String, for account: String) -> Bool {
        let data = Data(value.utf8)
        var status = SecItemUpdate(base(account) as CFDictionary,
                                   [kSecValueData as String: data] as CFDictionary)
        if status == errSecItemNotFound {
            var q = base(account)
            q[kSecValueData as String] = data
            q[kSecAttrLabel as String] = "Voxl Viewer password"
            status = SecItemAdd(q as CFDictionary, nil)
        }
        guard status == errSecSuccess else {
            NSLog("Voxl Viewer: couldn't save a password in the Keychain (OSStatus %d)", status)
            return false
        }
        lock.lock(); cache[account] = value; lock.unlock()
        return true
    }

    static func delete(_ account: String) {
        lock.lock(); cache[account] = nil; lock.unlock()
        SecItemDelete(base(account) as CFDictionary)
    }
}

/// User preferences.
enum Prefs {
    static let captureAppShortcutsKey = "captureAppShortcuts"
    static let captureSystemShortcutsKey = "captureSystemShortcuts"

    /// Send ⌘ shortcuts (⌘W, ⌘Q, ⌘T…) to the remote computer while a viewer is focused.
    static var captureAppShortcuts: Bool {
        UserDefaults.standard.object(forKey: captureAppShortcutsKey) as? Bool ?? true
    }
    /// Also take system shortcuts (⌘Tab, ⌘Space, Raycast's hotkey…) from macOS while a
    /// viewer is in front. Needs Accessibility.
    static var captureSystemShortcuts: Bool {
        UserDefaults.standard.object(forKey: captureSystemShortcutsKey) as? Bool ?? true
    }
}
