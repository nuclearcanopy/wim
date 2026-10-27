//! Shared by the mini player bar and the full player view: both need
//! the same elapsed/total formatting and the same text progress bar,
//! just at different widths.

pub fn fmt_time(secs: Option<f64>) -> String {
    match secs {
        Some(s) => {
            let s = s.max(0.0) as u64;
            format!("{}:{:02}", s / 60, s % 60)
        }
        None => "--:--".to_string(),
    }
}

/// A real scrubber: heavy line (━) up to the current position, a
/// filled circle (●) marking it, light line (─) for the rest.
/// Empty/unknown duration just reads as 0% progress rather than
/// guessing.
pub fn bar(elapsed: Option<f64>, duration: Option<f64>, width: usize) -> String {
    let width = width.max(4);
    let frac = match (elapsed, duration) {
        (Some(e), Some(d)) if d > 0.0 => (e / d).clamp(0.0, 1.0),
        _ => 0.0,
    };
    let pos = ((frac * (width - 1) as f64).round() as usize).min(width - 1);
    (0..width)
        .map(|i| match i.cmp(&pos) {
            std::cmp::Ordering::Less => '━',
            std::cmp::Ordering::Equal => '●',
            std::cmp::Ordering::Greater => '─',
        })
        .collect()
}
