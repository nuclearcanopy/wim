//! Every animation in the ui, expressed as a pure function of elapsed
//! wall-clock time rather than of frame count, so the motion looks
//! identical on a 60hz and a 144hz display; the refresh rate only
//! decides how often these get sampled (see `FrameBudget` and the
//! event-poll timeout in `main`). One module for all of it, so no
//! widget invents its own phase math.

use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crate::theme::lerp;

/// 0..1 breathe: 0 at t=0, 0.5 a quarter period in, 1 at the half
/// period, back to 0 at the full period. Cosine rather than a
/// triangle so the turnarounds ease instead of snapping.
pub fn pulse(elapsed: Duration, period: Duration) -> f32 {
    let period = period.as_secs_f32();
    if period <= 0.0 {
        return 0.0;
    }
    let t = elapsed.as_secs_f32() / period;
    (0.5 - 0.5 * (t * std::f32::consts::TAU).cos()).clamp(0.0, 1.0)
}

/// 0..1 sawtooth that wraps every `period`, for anything that travels
/// one way and restarts (the banner's brightness sweep).
pub fn phase(elapsed: Duration, period: Duration) -> f32 {
    let period = period.as_secs_f32();
    if period <= 0.0 {
        return 0.0;
    }
    (elapsed.as_secs_f32() / period).fract()
}

/// Alternates every `half_period`, starting visible; the classic
/// terminal cursor blink.
pub fn blink(elapsed: Duration, half_period: Duration) -> bool {
    let half = half_period.as_nanos();
    if half == 0 {
        return true;
    }
    (elapsed.as_nanos() / half).is_multiple_of(2)
}

/// 1.0 the moment something happened, falling linearly to 0.0 after
/// `duration` and staying there; transient acknowledgments (the
/// bottom bar's field flash, focus tails).
pub fn decay(since: Duration, duration: Duration) -> f32 {
    let duration = duration.as_secs_f32();
    if duration <= 0.0 {
        return 0.0;
    }
    (1.0 - since.as_secs_f32() / duration).clamp(0.0, 1.0)
}

/// Whole seconds left of `lifetime`, rounded up, so a countdown reads
/// "3s, 2s, 1s" instead of spending its last second showing 0.
pub fn countdown_secs(elapsed: Duration, lifetime: Duration) -> u64 {
    let left = lifetime.saturating_sub(elapsed);
    left.as_secs() + u64::from(left.subsec_nanos() > 0)
}

/// Background levels for a list's selected row. Both ends sit near
/// black, well under `theme::DIM` (the faintest text drawn over them),
/// so the row reads as lit without ever fighting its own label; the
/// focused pane breathes between them while an unfocused pane holds a
/// flat, dimmer level: still "this is where you left off", not a
/// second thing demanding attention.
pub const SELECTION_BG_LOW: f32 = 0.10;
pub const SELECTION_BG_HIGH: f32 = 0.24;
pub const SELECTION_BG_UNFOCUSED: f32 = 0.06;
pub const SELECTION_PULSE_PERIOD: Duration = Duration::from_millis(2400);

pub fn selection_bg(elapsed: Duration, focused: bool) -> f32 {
    if !focused {
        return SELECTION_BG_UNFOCUSED;
    }
    lerp(
        SELECTION_BG_LOW,
        SELECTION_BG_HIGH,
        pulse(elapsed, SELECTION_PULSE_PERIOD),
    )
}

/// The banner's two layered motions: a sweep across the glyphs plus a
/// slower overall breathe.
pub const BANNER_SWEEP_PERIOD: Duration = Duration::from_secs(4);
pub const BANNER_PULSE_PERIOD: Duration = Duration::from_millis(4800);
/// Half-phase of the search boxes' cursor, matching a real terminal's.
pub const CURSOR_BLINK: Duration = Duration::from_millis(530);
/// Synthetic level-meter breathe, used only when mpv reports no real
/// output level (see `main::audio_level`).
pub const LEVEL_FALLBACK_PERIOD: Duration = Duration::from_millis(3140);

/// What the poll loop sleeps for between frames while something is
/// actually moving, derived from the display. 60hz when detection
/// fails, and capped: a terminal cannot present faster than its own
/// redraw, so past `MAX_HZ` the extra frames are pure cpu burn.
pub const FALLBACK_HZ: f32 = 60.0;
pub const MAX_HZ: f32 = 144.0;
pub const MIN_HZ: f32 = 24.0;

/// Poll timeout when nothing is animating; a static screen must not
/// redraw at the display's rate just because it could.
pub const IDLE_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameBudget {
    hz: f32,
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self { hz: FALLBACK_HZ }
    }
}

impl FrameBudget {
    pub fn new(hz: f32) -> Self {
        Self { hz: clamp_hz(hz) }
    }

    pub fn interval(self) -> Duration {
        Duration::from_secs_f32(1.0 / self.hz)
    }
}

/// Garbage in (zero, negative, NaN, a 540hz panel), a rate worth
/// driving a terminal at out.
pub fn clamp_hz(hz: f32) -> f32 {
    if !hz.is_finite() || hz <= 0.0 {
        return FALLBACK_HZ;
    }
    hz.clamp(MIN_HZ, MAX_HZ)
}

/// Refresh rate of the output the user is actually on, asked of the
/// compositor: sway answers in mhz over its ipc, wlr-randr is the
/// generic wlroots fallback. `/sys/class/drm/*/modes` is no use here;
/// it lists resolutions only, with no refresh rate attached.
fn detect_hz() -> Option<f32> {
    if let Some(out) = run("swaymsg", &["-t", "get_outputs", "-r"]) {
        if let Some(hz) = parse_sway_outputs(&out) {
            return Some(hz);
        }
    }
    run("wlr-randr", &[]).as_deref().and_then(parse_wlr_randr)
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One subprocess per run, on its own thread: the first frame must
/// never wait on a compositor answering, and nothing may shell out per
/// frame. Until this lands the loop runs on `FrameBudget::default`.
pub fn spawn_probe() -> Receiver<FrameBudget> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let hz = detect_hz().unwrap_or(FALLBACK_HZ);
        let _ = tx.send(FrameBudget::new(hz));
        crate::trace::mark("refresh rate probed");
    });
    rx
}

/// `swaymsg -t get_outputs -r`: an array of outputs, each with a
/// `current_mode.refresh` in millihertz. The focused output is the one
/// this terminal is on; with none flagged, the fastest active one is
/// the safest guess.
pub fn parse_sway_outputs(json: &str) -> Option<f32> {
    let outputs: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    let refresh = |o: &serde_json::Value| o["current_mode"]["refresh"].as_f64();
    let mhz = outputs
        .iter()
        .find(|o| o["focused"].as_bool() == Some(true))
        .and_then(refresh)
        .or_else(|| {
            outputs
                .iter()
                .filter(|o| o["active"].as_bool() != Some(false))
                .filter_map(refresh)
                .max_by(|a, b| a.total_cmp(b))
        })?;
    let hz = mhz as f32 / 1000.0;
    (hz > 0.0).then_some(hz)
}

/// wlr-randr prints one indented mode line per mode, the live one
/// tagged `current`; both the spaced form ("1920x1080 px, 60.006 Hz")
/// and the older packed one ("1920x1080@60.006Hz") appear in the wild.
pub fn parse_wlr_randr(out: &str) -> Option<f32> {
    out.lines()
        .filter(|line| line.contains("current"))
        .find_map(parse_hz_token)
}

fn parse_hz_token(line: &str) -> Option<f32> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let idx = tokens
        .iter()
        .position(|t| t.to_ascii_lowercase().ends_with("hz"))?;
    let head = &tokens[idx][..tokens[idx].len() - 2];
    let text = match head.rsplit('@').next().unwrap_or("") {
        "" => tokens.get(idx.checked_sub(1)?)?.trim_end_matches(','),
        head => head,
    };
    text.parse::<f32>().ok().filter(|hz| *hz > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::DIM;

    fn secs(s: f32) -> Duration {
        Duration::from_secs_f32(s)
    }

    #[test]
    fn a_pulse_runs_trough_to_peak_over_a_half_period() {
        let period = secs(2.0);
        assert!(pulse(Duration::ZERO, period).abs() < 1e-5);
        assert!((pulse(secs(0.5), period) - 0.5).abs() < 1e-5);
        assert!((pulse(secs(1.0), period) - 1.0).abs() < 1e-5);
        assert!((pulse(secs(1.5), period) - 0.5).abs() < 1e-5);
        assert!(pulse(secs(2.0), period).abs() < 1e-5);
    }

    /// Walks whole frames at `budget`'s rate up to a second of wall
    /// clock, exactly as the main loop does, and reads the level off
    /// the clock at the last frame.
    fn level_after_a_second(budget: FrameBudget) -> f32 {
        let mut elapsed = Duration::ZERO;
        while elapsed + budget.interval() <= Duration::from_secs(1) {
            elapsed += budget.interval();
        }
        pulse(elapsed, SELECTION_PULSE_PERIOD)
    }

    /// The whole point of driving animations off wall clock: reaching
    /// one second takes 60 frames on one display and 144 on another,
    /// and the level there has to match. A frame-count-driven pulse
    /// would be 2.4x further along on the fast one.
    #[test]
    fn the_level_is_the_same_at_60hz_and_144hz() {
        let at_60 = level_after_a_second(FrameBudget::new(60.0));
        let at_144 = level_after_a_second(FrameBudget::new(144.0));
        let exact = pulse(Duration::from_secs(1), SELECTION_PULSE_PERIOD);
        // slack is one 60hz frame's worth of motion, the most the
        // slower sampler can be short by; nothing to do with speed.
        let slack = 0.03;
        assert!((at_60 - at_144).abs() < slack, "{at_60} vs {at_144}");
        assert!((at_60 - exact).abs() < slack, "{at_60} vs {exact}");
        assert!((at_144 - exact).abs() < slack, "{at_144} vs {exact}");
    }

    #[test]
    fn a_pulse_with_no_period_is_flat_instead_of_nan() {
        assert_eq!(pulse(secs(1.0), Duration::ZERO), 0.0);
        assert_eq!(phase(secs(1.0), Duration::ZERO), 0.0);
    }

    #[test]
    fn a_phase_wraps_once_per_period() {
        let period = secs(4.0);
        assert!(phase(Duration::ZERO, period).abs() < 1e-5);
        assert!((phase(secs(1.0), period) - 0.25).abs() < 1e-5);
        assert!(phase(secs(4.0), period).abs() < 1e-5);
        assert!((phase(secs(5.0), period) - 0.25).abs() < 1e-5);
    }

    #[test]
    fn a_blink_alternates_each_half_phase() {
        let half = Duration::from_millis(500);
        assert!(blink(Duration::ZERO, half));
        assert!(!blink(secs(0.6), half));
        assert!(blink(secs(1.1), half));
    }

    #[test]
    fn a_decay_falls_from_one_to_zero_and_stays_there() {
        let duration = Duration::from_millis(700);
        assert!((decay(Duration::ZERO, duration) - 1.0).abs() < 1e-5);
        assert!((decay(Duration::from_millis(350), duration) - 0.5).abs() < 1e-5);
        assert_eq!(decay(secs(5.0), duration), 0.0);
    }

    #[test]
    fn a_countdown_rounds_up_so_it_never_shows_a_stale_zero() {
        let lifetime = secs(3.0);
        assert_eq!(countdown_secs(Duration::ZERO, lifetime), 3);
        assert_eq!(countdown_secs(secs(0.1), lifetime), 3);
        assert_eq!(countdown_secs(secs(2.5), lifetime), 1);
        assert_eq!(countdown_secs(secs(3.0), lifetime), 0);
        assert_eq!(countdown_secs(secs(9.0), lifetime), 0);
    }

    /// Both ends of the breathe have to stay under the faintest text
    /// level drawn over them, or a selected row becomes unreadable.
    #[test]
    fn the_selection_background_breathes_between_two_near_black_levels() {
        assert!((selection_bg(Duration::ZERO, true) - SELECTION_BG_LOW).abs() < 1e-5);
        let peak = selection_bg(SELECTION_PULSE_PERIOD / 2, true);
        assert!((peak - SELECTION_BG_HIGH).abs() < 1e-5);
        assert!(peak < DIM, "{peak} would fight the text drawn over it");
    }

    #[test]
    fn an_unfocused_selection_holds_a_flat_dimmer_level() {
        for t in [0.0, 0.3, 0.6, 1.2, 2.0] {
            assert_eq!(selection_bg(secs(t), false), SELECTION_BG_UNFOCUSED);
        }
        // dimmer than even the trough of the focused pane's breathe.
        assert!(selection_bg(Duration::ZERO, false) < selection_bg(Duration::ZERO, true));
    }

    #[test]
    fn a_frame_budget_clamps_to_rates_worth_driving_a_terminal_at() {
        assert_eq!(clamp_hz(0.0), FALLBACK_HZ);
        assert_eq!(clamp_hz(-144.0), FALLBACK_HZ);
        assert_eq!(clamp_hz(f32::NAN), FALLBACK_HZ);
        assert_eq!(clamp_hz(540.0), MAX_HZ);
        assert_eq!(clamp_hz(1.0), MIN_HZ);
        assert_eq!(clamp_hz(59.95), 59.95);
        assert_eq!(FrameBudget::default(), FrameBudget::new(FALLBACK_HZ));
        assert_eq!(FrameBudget::new(240.0), FrameBudget::new(MAX_HZ));
        let interval = FrameBudget::new(60.0).interval();
        assert!((interval.as_secs_f32() - 1.0 / 60.0).abs() < 1e-6);
    }

    /// Verbatim shape of `swaymsg -t get_outputs -r`, trimmed to the
    /// fields read here.
    const SWAY_OUTPUTS: &str = r#"[
      {
        "name": "eDP-1",
        "active": true,
        "focused": true,
        "current_mode": { "width": 1920, "height": 1080, "refresh": 60006 }
      },
      {
        "name": "DP-2",
        "active": true,
        "focused": false,
        "current_mode": { "width": 2560, "height": 1440, "refresh": 143912 }
      }
    ]"#;

    #[test]
    fn sway_reports_the_focused_outputs_rate_in_mhz() {
        let hz = parse_sway_outputs(SWAY_OUTPUTS).unwrap();
        assert!((hz - 60.006).abs() < 1e-3, "got {hz}");
    }

    #[test]
    fn with_nothing_focused_sway_falls_back_to_the_fastest_active_output() {
        let json = SWAY_OUTPUTS.replace("\"focused\": true", "\"focused\": false");
        let hz = parse_sway_outputs(&json).unwrap();
        assert!((hz - 143.912).abs() < 1e-3, "got {hz}");
    }

    #[test]
    fn garbage_from_either_tool_parses_to_nothing_rather_than_a_wrong_rate() {
        assert_eq!(parse_sway_outputs(""), None);
        assert_eq!(parse_sway_outputs("not json at all"), None);
        assert_eq!(parse_sway_outputs("[]"), None);
        assert_eq!(parse_sway_outputs(r#"[{"name":"eDP-1"}]"#), None);
        assert_eq!(
            parse_sway_outputs(r#"[{"current_mode":{"refresh":0}}]"#),
            None
        );
        assert_eq!(parse_wlr_randr(""), None);
        assert_eq!(
            parse_wlr_randr("eDP-1 \"AU Optronics\"\n  Enabled: yes"),
            None
        );
        assert_eq!(parse_wlr_randr("  1920x1080 px, zz Hz (current)"), None);
    }

    #[test]
    fn wlr_randr_reports_the_current_mode_in_either_format() {
        let spaced = "eDP-1 \"AU Optronics 0x103D\"\n  Modes:\n    1920x1080 px, 60.006000 Hz (preferred)\n    1920x1080 px, 143.912000 Hz (current)\n";
        let hz = parse_wlr_randr(spaced).unwrap();
        assert!((hz - 143.912).abs() < 1e-3, "got {hz}");

        let packed = "DP-2 \"Dell\"\n  Modes:\n    2560x1440@59.951000Hz\n    2560x1440@143.912000Hz (preferred, current)\n";
        let hz = parse_wlr_randr(packed).unwrap();
        assert!((hz - 143.912).abs() < 1e-3, "got {hz}");
    }
}
