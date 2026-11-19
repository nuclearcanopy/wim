import MediaPlayer
import XCTest
@testable import ShellswimKit

final class NowPlayingTests: XCTestCase {
    func testPlayingSetsElapsedDurationRate() {
        let d = NowPlaying.info(title: "a.mp3", elapsed: 12.5, duration: 200, playing: true)
        XCTAssertEqual(d[MPMediaItemPropertyTitle] as? String, "a.mp3")
        XCTAssertEqual(d[MPNowPlayingInfoPropertyElapsedPlaybackTime] as? Double, 12.5)
        XCTAssertEqual(d[MPMediaItemPropertyPlaybackDuration] as? Double, 200)
        XCTAssertEqual(d[MPNowPlayingInfoPropertyPlaybackRate] as? Double, 1)
    }

    func testPausedRateIsZero() {
        let d = NowPlaying.info(title: "a", elapsed: 1, duration: 10, playing: false)
        XCTAssertEqual(d[MPNowPlayingInfoPropertyPlaybackRate] as? Double, 0)
    }

    func testUnknownDurationIsOmitted() {
        for dur in [nil, Double.nan, .infinity, 0, -1] as [Double?] {
            let d = NowPlaying.info(title: "a", elapsed: 1, duration: dur, playing: true)
            XCTAssertNil(d[MPMediaItemPropertyPlaybackDuration])
        }
    }

    func testBadElapsedBecomesZero() {
        let d = NowPlaying.info(title: "a", elapsed: .nan, duration: 5, playing: true)
        XCTAssertEqual(d[MPNowPlayingInfoPropertyElapsedPlaybackTime] as? Double, 0)
    }

    func testTitleFromURL() {
        XCTAssertEqual(NowPlaying.title(fromURL: "https://h/rest/stream/song%201.mp3?id=3"), "song 1.mp3")
        XCTAssertEqual(NowPlaying.title(fromURL: nil), "shellswim")
    }
}
