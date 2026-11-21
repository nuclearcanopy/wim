import Foundation
import MediaPlayer

/// Pure builder for the lock-screen Now Playing dictionary.
public enum NowPlaying {
    /// Last path component of a stream URL, used when no track name is known.
    public static func title(fromURL url: String?) -> String {
        guard let url, let u = URL(string: url), !u.lastPathComponent.isEmpty,
              u.lastPathComponent != "/"
        else { return "shellswim" }
        return u.lastPathComponent
    }

    /// Unknown duration (nil, NaN, infinite or <= 0) is omitted. Rate is 0
    /// when paused. Negative or non-finite elapsed becomes 0.
    public static func info(
        title: String, elapsed: Double, duration: Double?, playing: Bool
    ) -> [String: Any] {
        var d: [String: Any] = [
            MPMediaItemPropertyTitle: title,
            MPNowPlayingInfoPropertyElapsedPlaybackTime:
                elapsed.isFinite ? max(elapsed, 0.0) : 0.0,
            MPNowPlayingInfoPropertyPlaybackRate: playing ? 1.0 : 0.0,
        ]
        if let duration, duration.isFinite, duration > 0 {
            d[MPMediaItemPropertyPlaybackDuration] = duration
        }
        return d
    }
}
