//! 10-band graphic eq. Raw per-band gains are the mechanism, but presets
//! are the actual feature most people use; every well-regarded player
//! ships them so users don't have to hand-tune ten sliders. See
//! docs/features.md.
//!
//! Applied to mpv as a chain of lavfi `equalizer` filters on the `af`
//! property, one per ISO-standard center frequency, rather than mpv's
//! built-in `superequalizer` (which exposes a fixed 18-band layout that
//! doesn't line up with the 10 bands asked for here).

use serde::{Deserialize, Serialize};

pub const BAND_HZ: [f64; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

/// Furthest a single band may be pushed, either direction. One place,
/// since the menu's h/l nudge, a saved curve and a declaratively-set
/// one all have to agree on it.
pub const GAIN_LIMIT: f64 = 12.0;

/// Brings a curve from anywhere outside this module (config.toml, the
/// home-manager module) into range; a non-finite gain would poison
/// `compensated_gains`' elimination, so it reads as untouched.
pub fn clamp_gains(gains: [f64; 10]) -> [f64; 10] {
    gains.map(|g| {
        if g.is_finite() {
            g.clamp(-GAIN_LIMIT, GAIN_LIMIT)
        } else {
            0.0
        }
    })
}

pub struct Preset {
    pub name: &'static str,
    /// dB gain per band, same order as `BAND_HZ`. Starting-point values,
    /// not acoustically measured; refinable by ear later.
    pub gains: [f64; 10],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "flat",
        gains: [0.0; 10],
    },
    Preset {
        name: "bass boost",
        gains: [6.0, 5.0, 3.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        name: "vocal boost",
        gains: [-1.0, -1.0, 0.0, 1.0, 2.0, 3.0, 2.0, 1.0, 0.0, -1.0],
    },
    Preset {
        name: "treble boost",
        gains: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0],
    },
    Preset {
        name: "rock",
        gains: [4.0, 3.0, 1.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 4.0],
    },
    Preset {
        name: "classical",
        gains: [3.0, 2.0, 1.0, 0.0, 0.0, -1.0, 0.0, 1.0, 2.0, 3.0],
    },
];

/// Index of the preset named `name`, if any; used to restore a
/// persisted eq selection by name (see `config::Config::eq_preset`)
/// rather than a raw index, which could point at a different preset
/// entirely if `PRESETS` is ever reordered.
pub fn index_of(name: &str) -> Option<usize> {
    PRESETS.iter().position(|p| p.name == name)
}

/// A curve the user saved under a name, persisted in config.toml (see
/// `config::Config::eq_custom_presets`).
///
/// `gains` are the *requested* ones, exactly as the eq menu shows
/// them; `compensated_gains` runs at apply time, so storing its
/// output here would compensate an already-compensated curve on every
/// reload and the sound would drift further each restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomPreset {
    pub name: String,
    pub gains: [f64; 10],
}

/// Longest saved name accepted. Not arbitrary: the eq menu lists
/// saved presets next to the built-ins and tags the applied one on
/// the same row, so a name has to leave room for both.
pub const MAX_NAME_LEN: usize = 24;

/// Why a name was refused, or what saving under it would do. Returned
/// instead of a bool so the one collision that is recoverable (an
/// existing saved preset, which can be overwritten after a confirm)
/// is distinguishable from the ones that are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameCheck {
    Free,
    Empty,
    TooLong,
    /// Collides with a `PRESETS` entry. Refused outright rather than
    /// confirmed: built-ins cannot be replaced, so there would be
    /// nothing to overwrite and two rows would answer to one name.
    Builtin,
    /// Collides with the saved preset at this index: an overwrite,
    /// which is destructive and so goes behind a confirm (see
    /// `ui::eq_menu::Confirm`).
    Taken(usize),
}

/// Names are compared trimmed and case-insensitively, so "Rock" and
/// "rock " are the same name: two rows differing only in case would
/// be indistinguishable in the menu and both would answer to
/// `config.toml`'s `eq_preset`. Only ascii case folds, which is all
/// the built-ins use.
fn same_name(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// The user's saved curves, in the order they were created. Not
/// sorted: the menu lists them in this order, and a rename reordering
/// the list would move a row out from under the cursor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CustomPresets(Vec<CustomPreset>);

impl CustomPresets {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, CustomPreset> {
        self.0.iter()
    }

    pub fn get(&self, index: usize) -> Option<&CustomPreset> {
        self.0.get(index)
    }

    pub fn name_at(&self, index: usize) -> Option<&str> {
        self.0.get(index).map(|p| p.name.as_str())
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.0.iter().position(|p| same_name(&p.name, name))
    }

    /// `except` is the preset a rename is allowed to collide with
    /// (itself), so re-capitalizing a name isn't reported as taken.
    pub fn check_name(&self, name: &str, except: Option<usize>) -> NameCheck {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return NameCheck::Empty;
        }
        if trimmed.chars().count() > MAX_NAME_LEN {
            return NameCheck::TooLong;
        }
        if PRESETS.iter().any(|p| same_name(p.name, trimmed)) {
            return NameCheck::Builtin;
        }
        match self.index_of(trimmed) {
            Some(i) if Some(i) != except => NameCheck::Taken(i),
            _ => NameCheck::Free,
        }
    }

    /// Saves `gains` under `name`, replacing the preset already
    /// holding that name rather than appending a second one; returns
    /// the index it ended up at. Callers gate the replacing case
    /// behind a confirm, since it destroys the previous curve.
    pub fn insert(&mut self, name: &str, gains: [f64; 10]) -> usize {
        let preset = CustomPreset {
            name: name.trim().to_string(),
            gains: clamp_gains(gains),
        };
        match self.index_of(&preset.name) {
            Some(i) => {
                self.0[i] = preset;
                i
            }
            None => {
                self.0.push(preset);
                self.0.len() - 1
            }
        }
    }

    /// Renames in place, keeping the index (and so the position in
    /// the menu, and any `Source::Saved` pointing at it) stable.
    pub fn rename(&mut self, index: usize, name: &str) -> bool {
        match self.0.get_mut(index) {
            Some(preset) => {
                preset.name = name.trim().to_string();
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, index: usize) -> Option<CustomPreset> {
        (index < self.0.len()).then(|| self.0.remove(index))
    }
}

/// Which named curve the live gains came from. Stored rather than
/// re-derived because it has to survive a band nudge: once the gains
/// no longer match, the name is the only thing that can tell
/// "rock, modified" from a curve that never had a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Builtin(usize),
    Saved(usize),
}

/// The name behind a `Source`, or `None` if it points at a saved
/// preset that has since been deleted.
pub fn source_name(source: Source, saved: &CustomPresets) -> Option<&str> {
    match source {
        Source::Builtin(i) => PRESETS.get(i).map(|p| p.name),
        Source::Saved(i) => saved.name_at(i),
    }
}

/// What the bottom bar, the player view and the eq menu all call the
/// live curve. Three states, not two: a named curve, that curve
/// nudged off its saved values, and one that never had a name. The
/// middle state used to render as a bare "custom", which lost the
/// name it came from.
pub fn applied_label(source: Option<Source>, modified: bool, saved: &CustomPresets) -> String {
    match source.and_then(|s| source_name(s, saved)) {
        Some(name) if modified => format!("{name} (modified)"),
        Some(name) => name.to_string(),
        None => "custom".to_string(),
    }
}

/// Where `source` points once the saved preset at `removed` is
/// deleted: nowhere if it *was* that one (the curve stays audible,
/// it just has no name any more), one index down if it sat after it.
pub fn source_after_removal(source: Option<Source>, removed: usize) -> Option<Source> {
    match source {
        Some(Source::Saved(i)) if i == removed => None,
        Some(Source::Saved(i)) if i > removed => Some(Source::Saved(i - 1)),
        other => other,
    }
}

/// Which named curve `name` refers to, built-ins before saved ones,
/// and its gains. By name rather than index, same reasoning as
/// `index_of`: an index would point at a different curve the moment
/// either list is reordered.
pub fn resolve(name: &str, saved: &CustomPresets) -> Option<(Source, [f64; 10])> {
    if let Some(i) = PRESETS.iter().position(|p| same_name(p.name, name)) {
        return Some((Source::Builtin(i), PRESETS[i].gains));
    }
    let i = saved.index_of(name)?;
    Some((Source::Saved(i), clamp_gains(saved.get(i)?.gains)))
}

/// Which named curve `gains` exactly is, if any; built-ins first.
/// Lets a persisted nameless curve that happens to match a preset be
/// credited to it instead of reading as anonymous.
pub fn source_of_gains(gains: &[f64; 10], saved: &CustomPresets) -> Option<Source> {
    if let Some(i) = PRESETS.iter().position(|p| p.gains == *gains) {
        return Some(Source::Builtin(i));
    }
    saved
        .iter()
        .position(|p| clamp_gains(p.gains) == *gains)
        .map(Source::Saved)
}

/// The live eq state: which named curve it came from (if any),
/// whether it still matches that curve, and the ten requested gains.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Restored {
    pub source: Option<Source>,
    pub modified: bool,
    pub gains: [f64; 10],
}

/// Rebuilds the live eq state from what config.toml persisted, which
/// is a name plus the gains that were actually applied. Pure, so all
/// three states (named, named-and-nudged, nameless) can be tested
/// without a config file or an mpv.
///
/// `gains` absent with a name present is the declarative case: a
/// home-manager config setting only `eqPreset` means "that preset,
/// exactly", not "that preset, modified to flat".
pub fn restore(preset: Option<&str>, gains: Option<[f64; 10]>, saved: &CustomPresets) -> Restored {
    let live = gains.map(clamp_gains);
    match (preset.and_then(|n| resolve(n, saved)), live) {
        (Some((source, named)), Some(live)) if live != named => Restored {
            source: Some(source),
            modified: true,
            gains: live,
        },
        (Some((source, named)), _) => Restored {
            source: Some(source),
            modified: false,
            gains: named,
        },
        (None, Some(live)) => {
            let source = source_of_gains(&live, saved);
            Restored {
                source,
                modified: false,
                gains: live,
            }
        }
        // nothing persisted at all, i.e. a first run.
        (None, None) => Restored {
            source: Some(Source::Builtin(0)),
            modified: false,
            gains: PRESETS[0].gains,
        },
    }
}

/// What config.toml should hold for the live state: the source's name
/// (which survives a reorder of either list, unlike an index) and the
/// requested gains, always, so a nudged curve comes back nudged
/// instead of snapping to the preset it started from.
pub fn persisted(
    source: Option<Source>,
    gains: [f64; 10],
    saved: &CustomPresets,
) -> (Option<String>, Option<[f64; 10]>) {
    let name = source
        .and_then(|s| source_name(s, saved))
        .map(str::to_string);
    (name, Some(gains))
}

/// mpv's own label on the `lavfi` filter `build_af_filter` emits;
/// `af-metadata/<label>/...` (observed by the now-playing level meter,
/// see `mpv::PROP_LEVEL_RMS`) resolves against *this*, not against a
/// label written inside the lavfi graph. Labelling only the inner
/// `astats` stage made that property resolve nothing at all, which is
/// what left the meter dead. Purely an analysis tap; it doesn't touch
/// the audio.
pub const LEVEL_METADATA_LABEL: &str = "wimlevel";

/// True when no band asks for anything, i.e. the eq has nothing to do
/// and `build_af_filter` must not emit a single `equalizer` stage (see
/// its doc for why identity stages are not free).
pub fn is_flat(gains: &[f64; 10]) -> bool {
    gains.iter().all(|g| *g == 0.0)
}

/// Fraction of one band's gain that its peaking filter also lands on
/// the band center `d` octaves away; index is `d`, measured off the
/// realized chain (1-octave-wide biquads, centers an octave apart).
/// Past 4 octaves the spill is under 0.01 dB per dB, so it's dropped.
const BAND_SPILL: [f64; 5] = [1.0, 0.208, 0.0433, 0.0100, 0.0024];

/// Below this the printed `g=` would be inaudible anyway, so the whole
/// biquad is dropped rather than cascading an effectively-identity
/// stage (float rounding, see `build_af_filter`).
const MIN_AUDIBLE_DB: f64 = 0.01;

/// Per-band filter gains whose cascade actually lands on `target` at
/// each band center. Neighbouring 1-octave bands overlap heavily, so
/// handing the requested curve straight to the filters overshoots it
/// (a flat +6 request measured +8.8 dB); this solves the band
/// interaction instead. See docs/audio-quality.md.
fn compensated_gains(target: &[f64; 10]) -> [f64; 10] {
    const N: usize = BAND_HZ.len();
    // augmented [spill matrix | target]; spill is symmetric and
    // depends only on octave distance, so row i column j is
    // BAND_SPILL[|i - j|].
    let mut m = [[0.0f64; N + 1]; N];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().take(N).enumerate() {
            *cell = BAND_SPILL.get(i.abs_diff(j)).copied().unwrap_or(0.0);
        }
        row[N] = target[i];
    }

    // gaussian elimination with partial pivoting; the matrix is
    // diagonally dominant and fixed, so this never degenerates.
    for col in 0..N {
        let pivot = (col..N)
            .max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))
            .unwrap_or(col);
        m.swap(col, pivot);
        let (done, rest) = m.split_at_mut(col + 1);
        let pivot_row = &done[col];
        let diag = pivot_row[col];
        for row in rest.iter_mut() {
            let factor = row[col] / diag;
            for (cell, above) in row.iter_mut().zip(pivot_row.iter()).skip(col) {
                *cell -= factor * above;
            }
        }
    }

    let mut out = [0.0f64; N];
    for i in (0..N).rev() {
        let row = &m[i];
        let solved: f64 = row
            .iter()
            .zip(out.iter())
            .skip(i + 1)
            .map(|(coeff, g)| coeff * g)
            .sum();
        out[i] = (row[N] - solved) / row[i];
    }
    out
}

/// Builds an mpv `af` filter-graph string realizing `gains` (dB per
/// band, `BAND_HZ` order), plus a labeled `astats` tap at the end for
/// the level meter. A flat eq emits only the tap: ten identity biquads
/// are not free (they measured ~-77 dB of float rounding residue), and
/// an `astats`-only graph null-tests bit-transparent.
pub fn build_af_filter(gains: &[f64; 10]) -> String {
    let mut stages: Vec<String> = Vec::with_capacity(BAND_HZ.len() + 1);
    if !is_flat(gains) {
        let solved = compensated_gains(gains);
        for (hz, g) in BAND_HZ.iter().zip(solved.iter()) {
            if g.abs() < MIN_AUDIBLE_DB {
                continue;
            }
            stages.push(format!("equalizer=f={hz}:width_type=o:width=1:g={g:.3}"));
        }
    }
    stages.push("astats=metadata=1:reset=1".to_string());
    format!("@{LEVEL_METADATA_LABEL}:lavfi=[{}]", stages.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_preset_is_actually_flat() {
        assert_eq!(PRESETS[0].name, "flat");
        assert!(PRESETS[0].gains.iter().all(|&g| g == 0.0));
    }

    #[test]
    fn filter_string_emits_bands_low_to_high() {
        let filter = build_af_filter(&PRESETS[1].gains); // bass boost
        assert!(filter.starts_with(&format!("@{LEVEL_METADATA_LABEL}:lavfi=[")));
        assert!(filter.ends_with(']'));
        // bass boost only touches the bottom four bands, so the top
        // ones solve to nothing and are dropped entirely.
        assert!(filter.contains("f=31:"));
        assert!(filter.contains("f=250:"));
        assert!(!filter.contains("f=16000:"));
        assert!(filter.find("f=31:").unwrap() < filter.find("f=250:").unwrap());
    }

    #[test]
    fn every_preset_has_ten_bands_matching_band_hz_order() {
        for preset in PRESETS {
            assert_eq!(preset.gains.len(), BAND_HZ.len());
        }
    }

    #[test]
    fn is_flat_only_accepts_an_untouched_curve() {
        assert!(is_flat(&[0.0; 10]));
        assert!(!is_flat(&[
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5
        ]));
        assert!(!is_flat(&[
            -0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0
        ]));
    }

    /// A flat eq must not color the sound at all, so it may not emit a
    /// single biquad; only the level meter's analysis tap, which
    /// null-tests bit-transparent (see docs/audio-quality.md).
    #[test]
    fn flat_gains_emit_only_the_level_meter_tap() {
        let filter = build_af_filter(&[0.0; 10]);
        assert!(!filter.contains("equalizer"));
        assert_eq!(
            filter,
            format!("@{LEVEL_METADATA_LABEL}:lavfi=[astats=metadata=1:reset=1]")
        );
    }

    /// The tap has to be in the graph *and* the graph has to carry
    /// mpv's own label, or `af-metadata/<label>` resolves nothing.
    #[test]
    fn every_filter_keeps_the_labeled_level_meter_tap() {
        for preset in PRESETS {
            let filter = build_af_filter(&preset.gains);
            assert!(filter.starts_with(&format!("@{LEVEL_METADATA_LABEL}:lavfi=[")));
            assert!(filter.contains("astats=metadata=1:reset=1"));
        }
    }

    /// Forward-applies `BAND_SPILL`, i.e. what the cascade realizes at
    /// each band center for a given set of filter gains.
    fn realized(filter_gains: &[f64; 10]) -> [f64; 10] {
        let mut out = [0.0f64; 10];
        for (i, slot) in out.iter_mut().enumerate() {
            for (j, g) in filter_gains.iter().enumerate() {
                *slot += BAND_SPILL.get(i.abs_diff(j)).copied().unwrap_or(0.0) * g;
            }
        }
        out
    }

    #[test]
    fn compensated_gains_realize_the_requested_curve() {
        for preset in PRESETS {
            let got = realized(&compensated_gains(&preset.gains));
            for (band, (want, have)) in preset.gains.iter().zip(got.iter()).enumerate() {
                assert!(
                    (want - have).abs() < 1e-6,
                    "{} band {band}: wanted {want} dB, realized {have} dB",
                    preset.name
                );
            }
        }
    }

    #[test]
    fn compensated_gains_cancel_a_single_bands_spill_onto_its_neighbours() {
        let mut target = [0.0; 10];
        target[0] = 12.0;
        let solved = compensated_gains(&target);
        // the band itself overshoots slightly, and its neighbour goes
        // negative; that negative stage is what kills the smear.
        assert!(solved[0] > 12.0, "own band: {}", solved[0]);
        assert!(solved[1] < -1.0, "neighbour: {}", solved[1]);
        let got = realized(&solved);
        assert!((got[0] - 12.0).abs() < 1e-6);
        assert!(got[1].abs() < 1e-6, "62 Hz still smeared: {}", got[1]);
    }

    #[test]
    fn compensated_gains_leave_a_flat_curve_flat() {
        assert_eq!(compensated_gains(&[0.0; 10]), [0.0; 10]);
    }

    #[test]
    fn compensation_is_sign_symmetric() {
        let cut = [-4.0, -3.0, -1.0, 0.0, 1.0, 1.0, 0.0, -2.0, -3.0, -4.0];
        let boost = cut.map(|g: f64| -g);
        let solved_cut = compensated_gains(&cut);
        let solved_boost = compensated_gains(&boost);
        for (a, b) in solved_cut.iter().zip(solved_boost.iter()) {
            assert!((a + b).abs() < 1e-9);
        }
    }

    /// A curve with every band at a different value, so a reordering
    /// or a dropped band shows up rather than cancelling out.
    const TUNED: [f64; 10] = [3.0, -2.0, 1.0, 0.5, -4.0, 6.0, -1.5, 2.0, -3.0, 12.0];

    fn store(entries: &[(&str, [f64; 10])]) -> CustomPresets {
        let mut saved = CustomPresets::default();
        for (name, gains) in entries {
            saved.insert(name, *gains);
        }
        saved
    }

    #[test]
    fn saving_appends_in_order_and_loading_gives_back_the_same_curve() {
        let saved = store(&[("night", TUNED), ("car", PRESETS[1].gains)]);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved.name_at(0), Some("night"));
        assert_eq!(saved.name_at(1), Some("car"));
        assert_eq!(saved.get(0).unwrap().gains, TUNED);
        assert_eq!(resolve("night", &saved), Some((Source::Saved(0), TUNED)));
    }

    /// The whole point of the store: what comes back out is the
    /// *requested* curve, bit for bit. Storing `compensated_gains`'
    /// output instead would compensate it again on the next apply.
    #[test]
    fn a_saved_curve_round_trips_the_requested_gains_exactly() {
        let saved = store(&[("night", TUNED)]);
        let (source, gains) = resolve("night", &saved).unwrap();
        assert_eq!(source, Source::Saved(0));
        assert_eq!(gains, TUNED, "requested gains, never compensated ones");
        assert_ne!(
            compensated_gains(&TUNED),
            TUNED,
            "a curve the compensator leaves alone would not prove anything"
        );
    }

    #[test]
    fn renaming_keeps_the_index_and_the_curve() {
        let mut saved = store(&[("a", TUNED), ("b", PRESETS[2].gains)]);
        assert!(saved.rename(0, " night  "));
        assert_eq!(saved.name_at(0), Some("night"), "trimmed on the way in");
        assert_eq!(saved.get(0).unwrap().gains, TUNED);
        assert_eq!(saved.name_at(1), Some("b"), "the other row did not move");
        assert!(!saved.rename(9, "nope"));
    }

    #[test]
    fn deleting_removes_only_that_one() {
        let mut saved = store(&[("a", TUNED), ("b", PRESETS[2].gains)]);
        let gone = saved.remove(0).unwrap();
        assert_eq!(gone.name, "a");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved.name_at(0), Some("b"));
        assert!(saved.remove(5).is_none());
    }

    /// A name collision is handled deliberately, not silently: a
    /// second preset under an existing name is reported as an
    /// overwrite (which the menu confirms) and a built-in's name is
    /// refused outright, since there is nothing there to replace.
    #[test]
    fn a_colliding_name_is_an_overwrite_and_a_builtins_name_is_refused() {
        let saved = store(&[("night", TUNED)]);
        assert_eq!(saved.check_name("car", None), NameCheck::Free);
        assert_eq!(saved.check_name("night", None), NameCheck::Taken(0));
        // same name, different case/padding: still the same preset.
        assert_eq!(saved.check_name("  Night ", None), NameCheck::Taken(0));
        // renaming a preset to its own name is not a collision.
        assert_eq!(saved.check_name("Night", Some(0)), NameCheck::Free);
        assert_eq!(saved.check_name("flat", None), NameCheck::Builtin);
        assert_eq!(saved.check_name("Bass Boost", None), NameCheck::Builtin);
        assert_eq!(saved.check_name("   ", None), NameCheck::Empty);
        assert_eq!(
            saved.check_name(&"x".repeat(MAX_NAME_LEN), None),
            NameCheck::Free
        );
        assert_eq!(
            saved.check_name(&"x".repeat(MAX_NAME_LEN + 1), None),
            NameCheck::TooLong
        );
    }

    #[test]
    fn saving_over_an_existing_name_replaces_it_rather_than_adding_a_second() {
        let mut saved = store(&[("night", TUNED)]);
        let index = saved.insert("NIGHT", PRESETS[1].gains);
        assert_eq!(index, 0);
        assert_eq!(saved.len(), 1, "one row, not two answering to one name");
        assert_eq!(saved.get(0).unwrap().gains, PRESETS[1].gains);
    }

    #[test]
    fn gains_from_outside_are_clamped_into_range() {
        let mut saved = CustomPresets::default();
        saved.insert(
            "wild",
            [99.0, -99.0, f64::NAN, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        let gains = saved.get(0).unwrap().gains;
        assert_eq!(gains[0], GAIN_LIMIT);
        assert_eq!(gains[1], -GAIN_LIMIT);
        assert_eq!(gains[2], 0.0, "a non-finite gain reads as untouched");
    }

    #[test]
    fn deleting_the_applied_preset_leaves_the_curve_nameless() {
        assert_eq!(source_after_removal(Some(Source::Saved(1)), 1), None);
        // a preset after the deleted one shifts down with it.
        assert_eq!(
            source_after_removal(Some(Source::Saved(2)), 1),
            Some(Source::Saved(1))
        );
        // one before it, and a built-in, are untouched.
        assert_eq!(
            source_after_removal(Some(Source::Saved(0)), 1),
            Some(Source::Saved(0))
        );
        assert_eq!(
            source_after_removal(Some(Source::Builtin(3)), 1),
            Some(Source::Builtin(3))
        );
    }

    #[test]
    fn the_label_names_all_three_applied_states() {
        let saved = store(&[("night", TUNED)]);
        assert_eq!(
            applied_label(Some(Source::Saved(0)), false, &saved),
            "night"
        );
        assert_eq!(
            applied_label(Some(Source::Saved(0)), true, &saved),
            "night (modified)"
        );
        assert_eq!(applied_label(None, true, &saved), "custom");
        assert_eq!(
            applied_label(Some(Source::Builtin(4)), false, &saved),
            "rock"
        );
        // a source pointing at a deleted preset falls back rather
        // than crediting a name that is gone.
        assert_eq!(
            applied_label(Some(Source::Saved(7)), false, &saved),
            "custom"
        );
    }

    #[test]
    fn a_named_curve_round_trips_through_config_unchanged() {
        let saved = store(&[("night", TUNED)]);
        for source in [Source::Builtin(4), Source::Saved(0)] {
            let gains = match source {
                Source::Builtin(i) => PRESETS[i].gains,
                Source::Saved(i) => saved.get(i).unwrap().gains,
            };
            let (name, persisted_gains) = persisted(Some(source), gains, &saved);
            let back = restore(name.as_deref(), persisted_gains, &saved);
            assert_eq!(
                back,
                Restored {
                    source: Some(source),
                    modified: false,
                    gains
                }
            );
        }
    }

    /// The state the menu used to hide: a named curve nudged off its
    /// saved values. It has to come back as that name plus "modified",
    /// with the nudged gains, not as either one of the two clean
    /// states.
    #[test]
    fn a_nudged_named_curve_round_trips_as_modified() {
        let saved = store(&[("night", TUNED)]);
        let mut nudged = TUNED;
        nudged[3] += 1.0;
        let (name, gains) = persisted(Some(Source::Saved(0)), nudged, &saved);
        let back = restore(name.as_deref(), gains, &saved);
        assert_eq!(back.source, Some(Source::Saved(0)));
        assert!(back.modified);
        assert_eq!(back.gains, nudged);
        assert_eq!(
            applied_label(back.source, back.modified, &saved),
            "night (modified)"
        );
    }

    #[test]
    fn a_nameless_curve_round_trips_without_gaining_a_name() {
        let saved = store(&[("night", TUNED)]);
        let mut hand_tuned = TUNED;
        hand_tuned[0] = -7.0;
        let (name, gains) = persisted(None, hand_tuned, &saved);
        assert_eq!(name, None);
        let back = restore(name.as_deref(), gains, &saved);
        assert_eq!(back.source, None);
        assert_eq!(back.gains, hand_tuned);
    }

    #[test]
    fn restoring_degrades_gracefully_when_the_name_is_gone() {
        let saved = store(&[("night", TUNED)]);
        // a preset deleted (or renamed in a declarative config) since
        // it was persisted: keep the gains, drop the credit.
        let back = restore(Some("deleted"), Some(TUNED), &saved);
        assert_eq!(
            back.source,
            Some(Source::Saved(0)),
            "the curve itself still matches a saved one, so credit it"
        );
        let back = restore(Some("deleted"), Some([1.0; 10]), &saved);
        assert_eq!(back.source, None);
        assert_eq!(back.gains, [1.0; 10]);
    }

    /// A declarative config that names a preset and nothing else must
    /// get that preset exactly, not "that preset, modified to flat".
    #[test]
    fn a_name_with_no_gains_restores_the_named_curve_exactly() {
        let saved = store(&[("night", TUNED)]);
        let back = restore(Some("night"), None, &saved);
        assert_eq!(back.source, Some(Source::Saved(0)));
        assert!(!back.modified);
        assert_eq!(back.gains, TUNED);
    }

    #[test]
    fn an_empty_config_restores_flat_rather_than_an_anonymous_curve() {
        let back = restore(None, None, &CustomPresets::default());
        assert_eq!(back.source, Some(Source::Builtin(0)));
        assert!(!back.modified);
        assert_eq!(back.gains, [0.0; 10]);
    }

    #[test]
    fn near_zero_solved_bands_are_dropped_rather_than_cascaded() {
        // a lone 31 Hz boost solves to ~0 for everything from 125 Hz
        // up, so those bands must not reach the graph as identity
        // stages (float rounding, see docs/audio-quality.md).
        let mut target = [0.0; 10];
        target[0] = 6.0;
        let filter = build_af_filter(&target);
        assert_eq!(filter.matches("equalizer=f=").count(), 2);
        assert!(filter.contains("f=31:"));
        assert!(filter.contains("f=62:"));
    }
}
