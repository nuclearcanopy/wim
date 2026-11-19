import XCTest
@testable import ShellswimKit

final class TranscriptTests: XCTestCase {
    private var url: URL!

    override func setUp() {
        url = FileManager.default.temporaryDirectory
            .appendingPathComponent("transcript-\(UUID().uuidString)/t.json")
    }

    override func tearDown() {
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }

    private func items(_ n: Int) -> [ChatItem] {
        (0..<n).map { ChatItem(role: $0 % 2 == 0 ? .user : .reply, text: "m\($0)") }
    }

    func testCapKeepsTheNewest500() {
        let capped = TranscriptStore.capped(items(750))
        XCTAssertEqual(capped.count, 500)
        XCTAssertEqual(capped.first?.text, "m250")
        XCTAssertEqual(capped.last?.text, "m749")
    }

    func testShortTranscriptIsUntouched() {
        XCTAssertEqual(TranscriptStore.capped(items(3)).count, 3)
    }

    func testRoundTrip() throws {
        let store = TranscriptStore(url: url)
        let written = items(5)
        try store.save(written)
        XCTAssertEqual(store.load(), written)
    }

    func testSaveCapsOnDisk() throws {
        let store = TranscriptStore(url: url)
        try store.save(items(600))
        let loaded = store.load()
        XCTAssertEqual(loaded.count, 500)
        XCTAssertEqual(loaded.first?.text, "m100")
    }

    func testMissingOrCorruptFileIsEmpty() throws {
        let store = TranscriptStore(url: url)
        XCTAssertEqual(store.load(), [])
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data("not json".utf8).write(to: url)
        XCTAssertEqual(store.load(), [])
    }

    func testHttpsOnly() {
        XCTAssertTrue(ServerSettings.isValidServerURL("https://music.example.com"))
        XCTAssertFalse(ServerSettings.isValidServerURL("http://music.example.com"))
        XCTAssertFalse(ServerSettings.isValidServerURL("https://"))
        XCTAssertFalse(ServerSettings.isValidServerURL("music.example.com"))
    }
}
