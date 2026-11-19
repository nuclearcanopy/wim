//! Plain data crossing the FFI boundary.

/// What one reply bubble is. `Reply` is the command's answer, `System`
/// is the player talking on its own (a track change, a queue that
/// finished), `Error` is a failure or a decline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum BubbleKind {
    Reply,
    System,
    Error,
}

/// One chat bubble. `reply_to` is the id `submit` returned for the
/// message that caused it, or 0 for a bubble nobody asked for.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Bubble {
    pub reply_to: u64,
    pub kind: BubbleKind,
    pub text: String,
}

/// A Navidrome server and where the session may write.
/// The password lives in memory for the session's lifetime and is
/// never written to disk by this crate.
#[derive(Clone, uniffi::Record)]
pub struct WimConfig {
    pub server_url: String,
    pub username: String,
    pub password: String,
    /// App-private writable directory (config, favorites, caches).
    pub data_dir: String,
}

#[derive(Debug, uniffi::Error)]
pub enum WimError {
    Setup { message: String },
}

impl std::fmt::Display for WimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WimError::Setup { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WimError {}

/// What the native audio engine (AVPlayer, ExoPlayer) must provide.
/// Volume is 0.0 to 1.0. The engine reports back through
/// `WimSession::on_time`, `on_ended` and `on_error`.
#[uniffi::export(with_foreign)]
pub trait PlatformPlayer: Send + Sync {
    fn load(&self, url: String);
    fn play(&self);
    fn pause(&self);
    fn seek(&self, secs: f64);
    fn set_volume(&self, level: f64);
    fn stop(&self);
}
