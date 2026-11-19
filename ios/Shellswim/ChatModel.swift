import Foundation
import ShellswimKit
import SwiftUI
import WimCore

/// Owns the wim session and the transcript. All state changes on the main actor.
@MainActor
final class ChatModel: ObservableObject {
    @Published private(set) var items: [ChatItem] = []
    @Published var input = ""
    @Published private(set) var connected = false

    private let keychain = KeychainStore()
    private let store: TranscriptStore?
    private let backend = AVPlayerBackend()
    private var session: WimSession?

    init() {
        store = try? TranscriptStore.standard()
        items = store?.load() ?? []
    }

    /// Builds the session from the Keychain. The password is passed to the
    /// core and not kept anywhere else.
    func connect() {
        session = nil
        connected = false
        let s = keychain.load()
        guard s.isComplete, ServerSettings.isValidServerURL(s.url) else {
            append(.init(role: .system, text: "Open Settings and enter an https Navidrome server, user and password."))
            return
        }
        guard let dir = try? FileManager.default.url(
            for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true).appendingPathComponent("wim-data")
        else { return }
        let backend = self.backend
        Task.detached {
            do {
                try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                let cfg = WimConfig(
                    serverUrl: s.url, username: s.user, password: s.password,
                    dataDir: dir.path)
                let session = try WimSession(config: cfg, player: backend)
                backend.session = session
                await self.connected(session)
            } catch {
                await self.append(.init(role: .error, text: "connect failed: \(error)"))
            }
        }
    }

    private func connected(_ s: WimSession) {
        session = s
        connected = true
        append(.init(role: .system, text: "Connected."))
    }

    func send() {
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        input = ""
        append(.init(role: .user, text: text))
        guard let session else {
            append(.init(role: .error, text: "Not connected."))
            return
        }
        _ = session.submit(text: text)
        poll()
    }

    /// Drains whatever the core has to say. Cheap; called on a timer.
    func poll() {
        guard let session else { return }
        let bubbles = session.tick()
        guard !bubbles.isEmpty else { return }
        append(contentsOf: bubbles.map { b in
            let role: ChatItem.Role
            switch b.kind {
            case .reply: role = .reply
            case .system: role = .system
            case .error: role = .error
            }
            return ChatItem(role: role, text: b.text)
        })
    }

    private func append(_ item: ChatItem) { append(contentsOf: [item]) }

    private func append(contentsOf new: [ChatItem]) {
        items = TranscriptStore.capped(items + new)
        try? store?.save(items)
    }
}
