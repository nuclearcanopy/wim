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

/// Below this the printed `g=` would be inaudible anyway, so the whole
/// biquad is dropped rather than cascading an identity stage (float
/// rounding, see `build_af_filter`).
const MIN_AUDIBLE_DB: f64 = 0.01;

/// Builds an mpv `af` filter-graph string realizing `gains` (dB per
/// band, `BAND_HZ` order), plus a labeled `astats` tap at the end for
/// the level meter. A flat eq emits only the tap: ten identity biquads
/// are not free (they measured ~-77 dB of float rounding residue), and
/// an `astats`-only graph null-tests bit-transparent.
pub fn build_af_filter(gains: &[f64; 10]) -> String {
    let mut stages: Vec<String> = Vec::with_capacity(BAND_HZ.len() + 1);
    if !is_flat(gains) {
        for (hz, g) in BAND_HZ.iter().zip(gains.iter()) {
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
        assert_eq!(filter.matches("equalizer=f=").count(), 4);
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

    #[test]
    fn untouched_bands_are_dropped_rather_than_cascaded() {
        // an identity biquad is not free (float rounding), so a band
        // left at 0 dB must not reach the graph at all.
        let mut gains = [0.0; 10];
        gains[0] = 6.0;
        let filter = build_af_filter(&gains);
        assert_eq!(filter.matches("equalizer=f=").count(), 1);
        assert!(filter.contains("f=31:"));
    }
}
