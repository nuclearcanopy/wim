//! 10-band graphic eq. Raw per-band gains are the mechanism, but presets
//! are the actual feature most people use; every well-regarded player
//! ships them so users don't have to hand-tune ten sliders. See
//! docs/features.md.
//!
//! Applied to mpv as a chain of lavfi `equalizer` filters on the `af`
//! property, one per ISO-standard center frequency, rather than mpv's
//! built-in `superequalizer` (which exposes a fixed 18-band layout that
//! doesn't line up with the 10 bands asked for here).

pub const BAND_HZ: [f64; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

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

/// Label on the trailing `astats` analysis stage (see
/// `build_af_filter`); mpv exposes a labeled lavfi filter's metadata
/// at `af-metadata/<label>/...`, polled by the now-playing level
/// meter. Doesn't touch the audio itself, purely an analysis tap.
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
    stages.push(format!("astats@{LEVEL_METADATA_LABEL}=metadata=1:reset=1"));
    format!("lavfi=[{}]", stages.join(","))
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
        assert!(filter.starts_with("lavfi=["));
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
            format!("lavfi=[astats@{LEVEL_METADATA_LABEL}=metadata=1:reset=1]")
        );
    }

    #[test]
    fn every_filter_keeps_the_level_meter_tap() {
        for preset in PRESETS {
            let filter = build_af_filter(&preset.gains);
            assert!(filter.contains(&format!("astats@{LEVEL_METADATA_LABEL}=")));
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
