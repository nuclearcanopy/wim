//! Elapsed/total formatting, shared by every surface that prints a
//! position. The scrubber itself lives in `controlsbar::track`: the
//! bottom bar and the full player draw the same one at different
//! widths, and it has a drag state this module had no notion of.

use crate::ui::textfit::width;

pub fn fmt_time(secs: Option<f64>) -> String {
    match secs {
        Some(s) => {
            let s = s.max(0.0) as u64;
            format!("{}:{:02}", s / 60, s % 60)
        }
        None => "--:--".to_string(),
    }
}

/// The two readouts a scrubber sits between, with the elapsed one
/// padded in front to the total's width. Reserving that width up front
/// costs a cell of track for the first ten minutes of a long song, and
/// in exchange the track never moves: the readout used to grow from
/// four cells to five at 9:59 -> 10:00 and shove the track one column
/// right, which a drag resolving against the frame before the crossing
/// then read one cell off. Chosen over recomputing the rect mid-drag
/// because it removes the class rather than handling one case of it.
///
/// Elapsed never outgrows the total, so this is stable for a whole
/// song. The exception is a stream with no duration at all, where the
/// total is `--:--` and elapsed can pass 99:59; nothing is lost there,
/// since `Scrub::at_fraction` has no duration to resolve a drag
/// against and leaves the target where it was.
pub fn readouts(elapsed_secs: Option<f64>, duration_secs: Option<f64>) -> (String, String) {
    let total = fmt_time(duration_secs);
    let elapsed = fmt_time(elapsed_secs);
    let pad = width(&total).saturating_sub(width(&elapsed));
    (format!("{}{elapsed}", " ".repeat(pad)), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_reads_as_minutes_and_padded_seconds() {
        assert_eq!(fmt_time(Some(0.0)), "0:00");
        assert_eq!(fmt_time(Some(9.0)), "0:09");
        assert_eq!(fmt_time(Some(599.0)), "9:59");
        assert_eq!(fmt_time(Some(600.0)), "10:00");
        // nothing playing is not "0:00", which would read as a track
        // sitting at its start.
        assert_eq!(fmt_time(None), "--:--");
    }

    /// The crossing the drag bug was about: the pair is the same width
    /// either side of it, so the track between them cannot shift.
    #[test]
    fn the_elapsed_readout_keeps_one_width_for_a_whole_song() {
        for duration in [Some(1800.0), Some(600.0), Some(6000.0), None] {
            let widths: Vec<usize> = [0.0, 59.0, 599.0, 599.9, 600.0, 601.0]
                .iter()
                .map(|e| {
                    let (elapsed, total) = readouts(Some(*e), duration);
                    assert_eq!(
                        width(&elapsed),
                        width(&total),
                        "{e} of {duration:?} padded to {elapsed:?} beside {total:?}"
                    );
                    width(&elapsed)
                })
                .collect();
            assert!(
                widths.iter().all(|w| *w == widths[0]),
                "{duration:?} changed readout width over a song: {widths:?}"
            );
        }
    }

    #[test]
    fn padding_goes_in_front_so_the_digits_line_up() {
        assert_eq!(readouts(Some(30.0), Some(1800.0)).0, " 0:30");
        assert_eq!(readouts(Some(600.0), Some(1800.0)).0, "10:00");
        // a song short enough to need none of it spends nothing.
        assert_eq!(
            readouts(Some(30.0), Some(120.0)),
            ("0:30".to_string(), "2:00".to_string())
        );
    }
}
