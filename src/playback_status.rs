//! The one answer to "what is playback actually doing right now".
//!
//! The complaint this exists for: "I don't understand when it's
//! buffering, when it's not loading — I just see the bottom bar isn't
//! moving but I don't understand why." A frozen bar is the correct
//! rendering of at least five different situations (opening, waiting
//! for data, seeking, paused-while-preloading, properly stalled), and
//! guessing between them is the user's job only when the player
//! doesn't say. Everything here is a pure function of facts mpv
//! already pushes, so every branch is testable with no process
//! anywhere near it.
//!
//! The bar shows the result on its mode line (the `-- INTERACT --`
//! row), replacing it the way `resize_hint` already does: a status
//! sentence is more important than a focus hint for as long as it is
//! true, and it goes away the moment it stops being true.

/// The facts the status is derived from, exactly as `mpv::Playback`
/// holds them plus the one config knob that decides how much
/// preloading "full" means. Assembled by the host; nothing here talks
/// to mpv.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Facts {
    /// A track is the now-playing item (wim's queue, not mpv's).
    pub loaded: bool,
    /// mpv has reported no position for it yet: resolving, or the
    /// demuxer is still opening the stream.
    pub opening: bool,
    /// The user's pause.
    pub paused: bool,
    /// mpv paused *itself* because the cache ran dry. The single most
    /// important distinction on screen: a paused-for-cache player is
    /// not paused, it is waiting for the network.
    pub paused_for_cache: bool,
    /// mpv's buffering progress, 0-100 while it waits.
    pub buffering_pct: Option<f64>,
    /// A seek is in flight.
    pub seeking: bool,
    /// mpv is holding at the end (`keep-open`).
    pub eof_reached: bool,
    /// Media seconds buffered ahead of the position right now.
    pub cache_ahead_secs: Option<f64>,
    /// How far ahead the cache is configured to fill (`buffer_ahead_secs`
    /// in config.toml). "Full" is a setting, not a constant.
    pub buffer_target_secs: f64,
}

/// What playback is doing. Ordered by precedence in [`status`]: the
/// things that explain a motionless bar rank above the things that
/// don't.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Status {
    /// Nothing is playing at all.
    #[default]
    Idle,
    /// A track is selected but mpv has not reported a position:
    /// resolving the link or opening the stream.
    Opening,
    /// Waiting for data. `pct` is mpv's own estimate, 0-100.
    Buffering { pct: f64 },
    /// A seek is in flight; the position will jump shortly.
    Seeking,
    /// Playing, with this much media buffered ahead (when mpv can
    /// tell — a local file may not report a cache at all).
    Playing { ahead: Option<f64> },
    /// The user paused. `ahead` is how much is buffered and
    /// `still_filling` whether the cache is still growing toward the
    /// configured target — pausing a stream is also how you preload
    /// it, and the bar says which of the two states that is.
    Paused { ahead: Option<f64>, still_filling: bool },
    /// Holding at the end of the track (the between-track gap or a
    /// finished queue).
    Ended,
}

/// The decision. Precedence, each line explaining the one below it:
/// nothing loaded, then the held end, then the network stall (the
/// thing a motionless bar most needs to name), then a seek, then the
/// user's pause, then still-opening, then playing.
pub fn status(f: &Facts) -> Status {
    if !f.loaded {
        return Status::Idle;
    }
    if f.eof_reached {
        return Status::Ended;
    }
    if f.paused_for_cache {
        return Status::Buffering {
            pct: f.buffering_pct.unwrap_or(0.0).clamp(0.0, 100.0),
        };
    }
    if f.seeking {
        return Status::Seeking;
    }
    if f.paused {
        let ahead = f.cache_ahead_secs;
        // "full" is the configured target; a cache that has stopped
        // short of it is still filling (mpv keeps reading ahead while
        // paused), which is exactly the preload-the-user-asked-for
        // state and reads as one only if the bar says so.
        let still_filling = ahead.is_some_and(|a| a < f.buffer_target_secs);
        return Status::Paused { ahead, still_filling };
    }
    if f.opening {
        return Status::Opening;
    }
    Status::Playing {
        ahead: f.cache_ahead_secs,
    }
}

impl Status {
    /// One fixed word for the diagnostics file, whose value type
    /// carries no free text: state changes land as one line each.
    pub fn diag_label(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Opening => "opening",
            Status::Buffering { .. } => "buffering",
            Status::Seeking => "seeking",
            Status::Playing { .. } => "playing",
            Status::Paused { .. } => "paused",
            Status::Ended => "ended",
        }
    }

    /// The sentence the bar's mode line shows while this status
    /// holds, or `None` for the statuses that need no words (playing
    /// and idle are what the bar already says).
    pub fn mode_line(&self) -> Option<String> {
        match self {
            Status::Idle | Status::Playing { .. } => None,
            Status::Opening => Some("OPENING".into()),
            Status::Buffering { pct } => Some(format!("BUFFERING {:.0}%", pct)),
            Status::Seeking => Some("SEEKING".into()),
            Status::Paused { ahead, still_filling } => Some(match (ahead, still_filling) {
                // the one the whole preload feature turns on: paused,
                // and the cache is still growing toward the target.
                (Some(secs), true) => {
                    format!("PAUSED · BUFFERING {:.0}s AHEAD", secs.max(0.0))
                }
                (None, true) => "PAUSED · BUFFERING".into(),
                (Some(secs), false) => format!("PAUSED · {:.0}s BUFFERED", secs.max(0.0)),
                (None, false) => "PAUSED".into(),
            }),
            Status::Ended => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape every caller fills in; one helper so the tests read
    /// as the transitions they describe.
    fn facts() -> Facts {
        Facts {
            loaded: true,
            opening: false,
            paused: false,
            paused_for_cache: false,
            buffering_pct: None,
            seeking: false,
            eof_reached: false,
            cache_ahead_secs: None,
            buffer_target_secs: 30.0,
        }
    }

    #[test]
    fn nothing_loaded_is_idle() {
        let mut f = facts();
        f.loaded = false;
        assert_eq!(status(&f), Status::Idle);
    }

    /// eof outranks everything: a held end is a held end even if mpv
    /// reports a last-gasp cache value.
    #[test]
    fn the_held_end_outranks_everything() {
        let mut f = facts();
        f.eof_reached = true;
        f.paused = true;
        f.paused_for_cache = true;
        assert_eq!(status(&f), Status::Ended);
    }

    /// The distinction the module exists for: mpv pausing itself for
    /// the cache is *not* the user's pause, and the bar has to say
    /// which one it is or a network stall reads as "the player is
    /// broken".
    #[test]
    fn a_cache_stall_is_buffering_not_paused() {
        let mut f = facts();
        f.paused_for_cache = true;
        f.paused = true;
        f.buffering_pct = Some(42.0);
        assert_eq!(status(&f), Status::Buffering { pct: 42.0 });
        assert_eq!(
            status(&f).mode_line().as_deref(),
            Some("BUFFERING 42%"),
            "the mode line names the stall"
        );
    }

    /// A buffering report with no percentage still says buffering:
    /// "waiting" is the fact, the number is decoration.
    #[test]
    fn buffering_without_a_percentage_still_names_itself() {
        let mut f = facts();
        f.paused_for_cache = true;
        assert_eq!(status(&f), Status::Buffering { pct: 0.0 });
        assert_eq!(status(&f).mode_line().as_deref(), Some("BUFFERING 0%"));
    }

    #[test]
    fn seeking_outranks_playing() {
        let mut f = facts();
        f.seeking = true;
        assert_eq!(status(&f), Status::Seeking);
    }

    /// Pausing a stream is also how you preload it: while the cache
    /// is still below the configured target the bar says it is
    /// filling, and once it reaches the target it says the buffer is
    /// full — the difference between "keep waiting" and "go make
    /// tea".
    #[test]
    fn a_pause_names_how_full_the_buffer_is() {
        let mut f = facts();
        f.paused = true;
        f.cache_ahead_secs = Some(12.0);
        assert_eq!(
            status(&f),
            Status::Paused {
                ahead: Some(12.0),
                still_filling: true
            }
        );
        assert_eq!(
            status(&f).mode_line().as_deref(),
            Some("PAUSED · BUFFERING 12s AHEAD")
        );
        f.cache_ahead_secs = Some(30.0);
        assert_eq!(
            status(&f).mode_line().as_deref(),
            Some("PAUSED · 30s BUFFERED"),
            "at the target the filling sentence stops"
        );
        // and the target is the config knob, not a constant
        f.buffer_target_secs = 60.0;
        assert_eq!(
            status(&f).mode_line().as_deref(),
            Some("PAUSED · BUFFERING 30s AHEAD"),
            "raising the target makes 30s 'still filling' again"
        );
    }

    /// A local file reports no cache (nothing to buffer), and a pause
    /// on it must not claim to be preloading anything.
    #[test]
    fn a_pause_with_no_cache_report_is_just_paused() {
        let mut f = facts();
        f.paused = true;
        assert_eq!(
            status(&f),
            Status::Paused {
                ahead: None,
                still_filling: false
            }
        );
        assert_eq!(status(&f).mode_line().as_deref(), Some("PAUSED"));
    }

    /// Between "play this" and the first reported position there is a
    /// resolving/opening gap, and the bar names it rather than showing
    /// a motionless `--:--`.
    #[test]
    fn a_track_with_no_position_yet_is_opening() {
        let mut f = facts();
        f.opening = true;
        assert_eq!(status(&f), Status::Opening);
        assert_eq!(status(&f).mode_line().as_deref(), Some("OPENING"));
    }

    /// Playing needs no mode line: the bar already says it (the play
    /// glyph, the moving position), and a permanent sentence there
    /// would just train the eye to ignore the row.
    #[test]
    fn playing_and_idle_stay_silent() {
        let mut f = facts();
        f.cache_ahead_secs = Some(5.0);
        assert_eq!(
            status(&f),
            Status::Playing { ahead: Some(5.0) }
        );
        assert_eq!(status(&f).mode_line(), None);
        f.loaded = false;
        assert_eq!(status(&f).mode_line(), None);
    }
}
