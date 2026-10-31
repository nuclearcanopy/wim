//! The lyrics pane's state, as one thing rather than four locals.
//!
//! First of the `struct Ui` slices (see ROADMAP), done as small
//! cohesive groups rather than one bag of fifty-three fields: a single
//! struct holding every loose local relocates the problem without
//! naming anything. These four always move together -- open the pane
//! and all four are set, close it and all four stop mattering -- so
//! they are a unit, and `is_open` is a question about the group rather
//! than a convention about one field being `Some`.

/// Open lyrics, and everything needed to draw and scroll them.
#[derive(Debug, Default)]
pub struct LyricsPane {
    /// `None` when the pane is closed, which is the one field the
    /// others are only meaningful alongside.
    lines: Option<Vec<String>>,
    scroll: usize,
    /// Which track the open lyrics belong to; shown in their header so
    /// a pane left open across a track change is not ambiguous.
    track: String,
    /// The workspace width they were last drawn at. Wrapping, and
    /// therefore how far they can scroll, depends on it, and only the
    /// draw pass knows it.
    width: u16,
}

impl LyricsPane {
    pub fn open(&mut self, lines: Vec<String>, track: String) {
        self.lines = Some(lines);
        self.track = track;
        // a fresh open starts at the top; keeping the old offset
        // would land mid-song in a different song.
        self.scroll = 0;
    }

    pub fn close(&mut self) {
        self.lines = None;
    }

    pub fn is_open(&self) -> bool {
        self.lines.is_some()
    }

    pub fn lines(&self) -> Option<&[String]> {
        self.lines.as_deref()
    }

    pub fn track(&self) -> &str {
        &self.track
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The draw pass reporting the width it used; see `width`.
    pub fn saw_width(&mut self, width: u16) {
        self.width = width;
    }

    /// Cells available for wrapping, never zero so a caller can divide
    /// by it before the first draw has happened.
    pub fn width(&self) -> usize {
        self.width.max(1) as usize
    }

    pub fn set_scroll(&mut self, scroll: usize) {
        self.scroll = scroll;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_pane_has_no_lines() {
        let p = LyricsPane::default();
        assert!(!p.is_open());
        assert!(p.lines().is_none());
    }

    /// Opening a different song must not keep the old scroll: the
    /// offset would land mid-song in a song it was never measured
    /// against.
    #[test]
    fn opening_resets_the_scroll() {
        let mut p = LyricsPane::default();
        p.open(vec!["a".into(), "b".into()], "first".into());
        p.set_scroll(7);
        p.open(vec!["c".into()], "second".into());
        assert_eq!(p.scroll(), 0);
        assert_eq!(p.track(), "second");
    }

    /// Closing drops the lines and nothing else: reopening the same
    /// track should not have to re-report the width the draw pass
    /// already measured.
    #[test]
    fn closing_only_drops_the_lines() {
        let mut p = LyricsPane::default();
        p.open(vec!["a".into()], "t".into());
        p.saw_width(40);
        p.close();
        assert!(!p.is_open());
        assert_eq!(p.width(), 40);
    }

    /// Never zero, so a caller can wrap against it before the first
    /// draw has reported anything.
    #[test]
    fn width_is_never_zero() {
        assert_eq!(LyricsPane::default().width(), 1);
    }
}
