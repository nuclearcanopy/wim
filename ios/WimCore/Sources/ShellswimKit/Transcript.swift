import Foundation

/// One line of the chat: something the user sent or something wim said.
public struct ChatItem: Codable, Equatable, Identifiable, Sendable {
    public enum Role: String, Codable, Sendable { case user, reply, system, error }

    public var id: UUID
    public var role: Role
    public var text: String

    public init(id: UUID = UUID(), role: Role, text: String) {
        self.id = id
        self.role = role
        self.text = text
    }
}

/// Keeps the last `limit` chat items as JSON in a file.
public struct TranscriptStore: Sendable {
    public static let limit = 500

    public let url: URL

    public init(url: URL) { self.url = url }

    /// `Application Support/transcript.json`, created on demand.
    public static func standard() throws -> TranscriptStore {
        let dir = try FileManager.default.url(
            for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true)
        return TranscriptStore(url: dir.appendingPathComponent("transcript.json"))
    }

    public static func capped(_ items: [ChatItem]) -> [ChatItem] {
        items.count > limit ? Array(items.suffix(limit)) : items
    }

    /// Missing or unreadable files give an empty transcript.
    public func load() -> [ChatItem] {
        guard let data = try? Data(contentsOf: url),
              let items = try? JSONDecoder().decode([ChatItem].self, from: data)
        else { return [] }
        return Self.capped(items)
    }

    public func save(_ items: [ChatItem]) throws {
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let data = try JSONEncoder().encode(Self.capped(items))
        try data.write(to: url, options: .atomic)
    }
}
