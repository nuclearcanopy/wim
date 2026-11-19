import AVFoundation
import Foundation
import MediaPlayer
import ShellswimKit
import WimCore

/// PlatformPlayer over AVPlayer. The Rust core calls in from any thread;
/// everything touching AVPlayer hops to the main queue.
/// Also owns the audio session (background playback, interruptions, route
/// changes) and the lock-screen Now Playing info and remote commands.
/// Next/previous stay disabled: the core exposes no such calls.
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
    private var currentURL: String?
    private var wasPlayingBeforeInterruption = false

    init() {
        DispatchQueue.main.async { [weak self] in
            self?.installTimeObserver()
            self?.installAudioSessionObservers()
            self?.installRemoteCommands()
        }
    }

    // MARK: audio session

    private func activateSession() {
        let s = AVAudioSession.sharedInstance()
        try? s.setCategory(.playback, mode: .spokenAudio)
        try? s.setActive(true)
    }

    private func installAudioSessionObservers() {
        let nc = NotificationCenter.default
        nc.addObserver(
            forName: AVAudioSession.interruptionNotification, object: nil, queue: .main
        ) { [weak self] n in self?.handleInterruption(n) }
        nc.addObserver(
            forName: AVAudioSession.routeChangeNotification, object: nil, queue: .main
        ) { [weak self] n in self?.handleRouteChange(n) }
    }

    private func handleInterruption(_ n: Notification) {
        guard let raw = n.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
              let type = AVAudioSession.InterruptionType(rawValue: raw)
        else { return }
        switch type {
        case .began:
            wasPlayingBeforeInterruption = player.rate > 0
            player.pause()
            updateNowPlaying()
        case .ended:
            let optRaw = n.userInfo?[AVAudioSessionInterruptionOptionKey] as? UInt ?? 0
            let shouldResume = AVAudioSession.InterruptionOptions(rawValue: optRaw)
                .contains(.shouldResume)
            if wasPlayingBeforeInterruption && shouldResume { play() }
            wasPlayingBeforeInterruption = false
        @unknown default: break
        }
    }

    /// Pause when the previous output (headphones) went away.
    private func handleRouteChange(_ n: Notification) {
        guard let raw = n.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt,
              AVAudioSession.RouteChangeReason(rawValue: raw) == .oldDeviceUnavailable
        else { return }
        player.pause()
        updateNowPlaying()
    }

    // MARK: now playing and remote commands

    private func updateNowPlaying() {
        guard player.currentItem != nil else {
            MPNowPlayingInfoCenter.default().nowPlayingInfo = nil
            return
        }
        let dur = player.currentItem?.duration.seconds
        MPNowPlayingInfoCenter.default().nowPlayingInfo = NowPlaying.info(
            title: NowPlaying.title(fromURL: currentURL),
            elapsed: player.currentTime().seconds,
            duration: dur,
            playing: player.rate > 0)
    }

    private func installRemoteCommands() {
        let c = MPRemoteCommandCenter.shared()
        c.playCommand.addTarget { [weak self] _ in self?.play(); return .success }
        c.pauseCommand.addTarget { [weak self] _ in self?.pause(); return .success }
        c.togglePlayPauseCommand.addTarget { [weak self] _ in
            guard let self else { return .commandFailed }
            if self.player.rate > 0 { self.pause() } else { self.play() }
            return .success
        }
        c.changePlaybackPositionCommand.addTarget { [weak self] e in
            guard let e = e as? MPChangePlaybackPositionCommandEvent else {
                return .commandFailed
            }
            self?.seek(secs: e.positionTime)
            return .success
        }
        c.nextTrackCommand.isEnabled = false
        c.previousTrackCommand.isEnabled = false
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
            self.updateNowPlaying()
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
            currentURL = url
            let item = AVPlayerItem(url: u)
            endObserver = NotificationCenter.default.addObserver(
                forName: .AVPlayerItemDidPlayToEndTime, object: item, queue: .main
            ) { [weak self] _ in self?.session?.onEnded() }
            player.replaceCurrentItem(with: item)
            player.volume = volume
        }
    }

    func play() {
        main { [self] in
            activateSession()
            player.play()
            updateNowPlaying()
        }
    }

    func pause() {
        main { [self] in
            player.pause()
            updateNowPlaying()
        }
    }

    func seek(secs: Double) {
        main { [self] in
            player.seek(to: CMTime(seconds: secs, preferredTimescale: 600)) { [weak self] _ in
                self?.updateNowPlaying()
            }
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
            currentURL = nil
            updateNowPlaying()
        }
    }
}
