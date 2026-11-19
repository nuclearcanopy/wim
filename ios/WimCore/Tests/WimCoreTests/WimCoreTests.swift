import XCTest
@testable import WimCore

/// Records what the Rust core asks of the audio engine.
final class FakePlayer: PlatformPlayer, @unchecked Sendable {
    private let lock = NSLock()
    private var log: [String] = []

    var calls: [String] {
        lock.lock(); defer { lock.unlock() }
        return log
    }

    private func record(_ s: String) {
        lock.lock(); defer { lock.unlock() }
        log.append(s)
    }

    func load(url: String) { record("load \(url)") }
    func play() { record("play") }
    func pause() { record("pause") }
    func seek(secs: Double) { record("seek \(secs)") }
    func setVolume(level: Double) { record("volume \(level)") }
    func stop() { record("stop") }
}

final class WimCoreTests: XCTestCase {
    private var dir: URL!

    override func setUpWithError() throws {
        dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("wimcore-\(UUID().uuidString)")
        let music = dir.appendingPathComponent("music")
        try FileManager.default.createDirectory(at: music, withIntermediateDirectories: true)
        try Data().write(to: music.appendingPathComponent("crest.mp3"))
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: dir)
    }

    private func session(_ player: FakePlayer) throws -> WimSession {
        try WimSession.newLocal(
            dataDir: dir.appendingPathComponent("home").path,
            libraryPath: dir.appendingPathComponent("music").path,
            player: player)
    }

    /// Ticks until `done` holds for the bubbles so far, or 5 seconds pass.
    private func collect(_ s: WimSession, until done: ([Bubble]) -> Bool) -> [Bubble] {
        var all: [Bubble] = []
        let end = Date().addingTimeInterval(5)
        while Date() < end {
            all += s.tick()
            if done(all) { break }
            Thread.sleep(forTimeInterval: 0.02)
        }
        return all
    }

    func testDryPlayRepliesToItsOwnMessage() throws {
        let s = try session(FakePlayer())
        let id = s.submit(text: "dry(play(crest))")
        let bubbles = collect(s) { $0.contains { $0.replyTo == id && $0.kind == .reply } }
        XCTAssertTrue(
            bubbles.contains { $0.replyTo == id && $0.kind == .reply },
            "no reply for \(id): \(bubbles)")
    }

    func testPlayReachesThePlayerLoad() throws {
        let player = FakePlayer()
        let s = try session(player)
        let id = s.submit(text: "play(crest)")
        _ = collect(s) { _ in player.calls.contains { $0.hasPrefix("load ") } }
        XCTAssertTrue(
            player.calls.contains { $0.hasPrefix("load ") && $0.contains("crest") },
            "message \(id): \(player.calls)")
    }
}
