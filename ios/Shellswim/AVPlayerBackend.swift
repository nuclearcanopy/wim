import AVFoundation
import Foundation
import WimCore

/// PlatformPlayer over AVPlayer. The Rust core calls in from any thread;
/// everything touching AVPlayer hops to the main queue.
/// M4 hooks (not implemented here): AVAudioSession category .playback,
/// background audio mode, MPRemoteCommandCenter and Now Playing info.
final class AVPlayerBackend: PlatformPlayer, @unchecked Sendable {
    /// Set once the session exists; time and end events go to it.
    private let lock = NSLock()
    private weak var _session: WimSession?
    var session: WimSession? {
        get { lock.lock(); defer { lock.unlock() }; return _session }
        set { lock.lock(); defer { lock.unlock() }; _session = newValue }
    }

    private let player = AVPlayer()
    private var timeObserver: Any?
    private var endObserver: NSObjectProtocol?
    private var volume: Float = 1

    init() {
        DispatchQueue.main.async { [weak self] in self?.installTimeObserver() }
    }

    private func installTimeObserver() {
        let interval = CMTime(seconds: 0.5, preferredTimescale: 600)
        timeObserver = player.addPeriodicTimeObserver(
            forInterval: interval, queue: .main
        ) { [weak self] time in
            guard let self, let item = self.player.currentItem else { return }
            let secs = time.seconds
            let dur = item.duration.seconds
            guard secs.isFinite else { return }
            self.session?.onTime(secs: secs, duration: dur.isFinite ? dur : 0)
        }
    }

    private func main(_ work: @escaping @Sendable () -> Void) {
        if Thread.isMainThread { work() } else { DispatchQueue.main.async(execute: work) }
    }

    func load(url: String) {
        main { [self] in
            guard let u = URL(string: url) else {
                session?.onError(message: "bad stream URL")
                return
            }
            if let o = endObserver { NotificationCenter.default.removeObserver(o) }
            let item = AVPlayerItem(url: u)
            endObserver = NotificationCenter.default.addObserver(
                forName: .AVPlayerItemDidPlayToEndTime, object: item, queue: .main
            ) { [weak self] _ in self?.session?.onEnded() }
            player.replaceCurrentItem(with: item)
            player.volume = volume
        }
    }

    func play() { main { [self] in player.play() } }
    func pause() { main { [self] in player.pause() } }

    func seek(secs: Double) {
        main { [self] in
            player.seek(to: CMTime(seconds: secs, preferredTimescale: 600))
        }
    }

    func setVolume(level: Double) {
        main { [self] in
            volume = Float(min(max(level, 0), 1))
            player.volume = volume
        }
    }

    func stop() {
        main { [self] in
            player.pause()
            if let o = endObserver {
                NotificationCenter.default.removeObserver(o)
                endObserver = nil
            }
            player.replaceCurrentItem(with: nil)
        }
    }
}
