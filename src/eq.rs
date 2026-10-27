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

/// Label on the trailing `astats` analysis stage (see
/// `build_af_filter`); mpv exposes a labeled lavfi filter's metadata
/// at `af-metadata/<label>/...`, polled by the now-playing level
/// meter. Doesn't touch the audio itself, purely an analysis tap.
pub const LEVEL_METADATA_LABEL: &str = "wimlevel";

/// Builds an mpv `af` filter-graph string applying `gains` (dB per
/// band, `BAND_HZ` order) as a chain of lavfi `equalizer` filters,
/// plus a labeled `astats` tap at the end for the level meter.
pub fn build_af_filter(gains: &[f64; 10]) -> String {
    let mut stages: Vec<String> = BAND_HZ
        .iter()
        .zip(gains.iter())
        .map(|(hz, g)| format!("equalizer=f={hz}:width_type=o:width=1:g={g}"))
        .collect();
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
    fn filter_string_has_one_stage_per_band_in_order() {
        let filter = build_af_filter(&PRESETS[1].gains); // bass boost
        assert!(filter.starts_with("lavfi=["));
        assert!(filter.ends_with(']'));
        assert_eq!(filter.matches("equalizer=f=").count(), 10);
        // first band (31 Hz) should come before the last (16000 Hz)
        assert!(filter.find("f=31:").unwrap() < filter.find("f=16000:").unwrap());
    }

    #[test]
    fn every_preset_has_ten_bands_matching_band_hz_order() {
        for preset in PRESETS {
            assert_eq!(preset.gains.len(), BAND_HZ.len());
        }
    }
}
