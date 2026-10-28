//! Elapsed/total formatting, shared by every surface that prints a
//! position. The scrubber itself lives in `controlsbar::track`: the
//! bottom bar and the full player draw the same one at different
//! widths, and it has a drag state this module had no notion of.

pub fn fmt_time(secs: Option<f64>) -> String {
    match secs {
        Some(s) => {
            let s = s.max(0.0) as u64;
            format!("{}:{:02}", s / 60, s % 60)
        }
        None => "--:--".to_string(),
    }
}
