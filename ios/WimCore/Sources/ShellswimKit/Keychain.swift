import Foundation
import Security

/// The three values the app needs to reach Navidrome.
public struct ServerSettings: Equatable, Sendable {
    public var url: String
    public var user: String
    public var password: String

    public init(url: String = "", user: String = "", password: String = "") {
        self.url = url
        self.user = user
        self.password = password
    }

    public var isComplete: Bool { !url.isEmpty && !user.isEmpty && !password.isEmpty }

    /// The phone talks to Navidrome over HTTPS only.
    public static func isValidServerURL(_ s: String) -> Bool {
        guard let u = URLComponents(string: s) else { return false }
        return u.scheme?.lowercased() == "https" && !(u.host ?? "").isEmpty
    }
}

/// Generic-password items under one service. Values are never logged.
public struct KeychainStore: Sendable {
    public static let service = "dev.loki.shellswim"
    private enum Account { static let url = "server-url", user = "user", password = "password" }

    public let service: String

    public init(service: String = KeychainStore.service) { self.service = service }

    private func query(_ account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account]
    }

    private func get(_ account: String) -> String? {
        var q = query(account)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess,
              let data = out as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    private func set(_ account: String, _ value: String) throws {
        SecItemDelete(query(account) as CFDictionary)
        var q = query(account)
        q[kSecValueData as String] = Data(value.utf8)
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        let status = SecItemAdd(q as CFDictionary, nil)
        guard status == errSecSuccess else { throw KeychainError.status(status) }
    }

    public func load() -> ServerSettings {
        ServerSettings(
            url: get(Account.url) ?? "",
            user: get(Account.user) ?? "",
            password: get(Account.password) ?? "")
    }

    public func save(_ s: ServerSettings) throws {
        try set(Account.url, s.url)
        try set(Account.user, s.user)
        try set(Account.password, s.password)
    }

    public func clear() {
        for a in [Account.url, Account.user, Account.password] {
            SecItemDelete(query(a) as CFDictionary)
        }
    }
}

public enum KeychainError: Error, CustomStringConvertible {
    case status(OSStatus)
    /// Carries the status code only, never a value.
    public var description: String {
        if case .status(let s) = self { return "keychain error \(s)" }
        return "keychain error"
    }
}
