//! The lyrics pane's state, as one thing rather than four locals.
//!
//! First of the `struct Ui` slices (see ROADMAP), done as small
//! cohesive groups rather than one bag of fifty-three fields: a single
//! struct holding every loose local relocates the problem without
//! naming anything. These four always move together -- open the pane
//! and all four are set, close it and all four stop mattering -- so
//! they are a unit, and `is_open` is a question about the group rather
//! than a convention about one field being `Some`.

use std::time::{Duration, Instant};

use crate::lyrics::{self, Lyrics, TimedLine};

/// Open lyrics, and everything needed to draw, sync and scroll them.
#[derive(Debug, Default)]
pub struct LyricsPane {
    /// `None` when the pane is closed, which is the one field the
    /// others are only meaningful alongside.
    lines: Option<Vec<String>>,
    timed: Vec<TimedLine>,
    scroll: usize,
    /// Which track the open lyrics belong to; shown in their header so
    /// a pane left open across a track change is not ambiguous.
    track: String,
    source: String,
    attribution: Option<String>,
    /// The workspace width they were last drawn at. Wrapping, and
    /// therefore how far they can scroll, depends on it, and only the
    /// draw pass knows it.
    width: u16,
    height: u16,
    manual_until: Option<Instant>,
}

impl LyricsPane {
    pub fn open(&mut self, lines: Vec<String>, track: String) {
        self.lines = Some(lines);
        self.timed.clear();
        self.track = track;
        self.source.clear();
        self.attribution = None;
        // a fresh open starts at the top; keeping the old offset
        // would land mid-song in a different song.
        self.scroll = 0;
        self.manual_until = None;
    }

    pub fn open_lyrics(&mut self, lyrics: &Lyrics, track: String) {
        let timed = lyrics
            .synced
            .as_deref()
            .map(lyrics::parse_synced)
            .unwrap_or_default();
        let lines = if lyrics.instrumental {
            vec!["(instrumental, no lyrics)".to_string()]
        } else if !timed.is_empty() {
            timed.iter().map(|line| line.text.clone()).collect()
        } else {
            lyrics
                .plain
                .as_deref()
                .or(lyrics.synced.as_deref())
                .unwrap_or("(no lyrics text available)")
                .lines()
                .map(str::to_owned)
                .collect()
        };
        self.open(lines, track);
        self.timed = timed;
        self.source.clone_from(&lyrics.source);
        self.attribution.clone_from(&lyrics.attribution);
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

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn attribution(&self) -> Option<&str> {
        self.attribution.as_deref()
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The draw pass reporting the workspace size used for wrapping
    /// and keeping the active lyric line visible.
    pub fn saw_size(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Cells available for wrapping, never zero so a caller can divide
    /// by it before the first draw has happened.
    pub fn width(&self) -> usize {
        self.width.max(1) as usize
    }

    pub fn set_scroll(&mut self, scroll: usize) {
        self.scroll = scroll;
        self.manual_until = Some(Instant::now() + Duration::from_secs(3));
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let Some(lines) = self.lines.as_deref() else {
            return;
        };
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        let total = crate::ui::lyrics_view::wrapped_len(&borrowed, self.width());
        let body_height = self.height.saturating_sub(crate::ui::header::ROWS) as usize;
        let footer =
            crate::ui::lyrics_view::attribution_rows(self.attribution.as_deref(), self.width());
        let visible = body_height.saturating_sub(footer).max(1);
        let max = total.saturating_sub(visible);
        self.set_scroll(self.scroll.saturating_add_signed(delta).min(max));
    }

    pub fn active_line(&self, position: Option<f64>) -> Option<usize> {
        let position = position?;
        let index = self.timed.partition_point(|line| line.at <= position);
        index.checked_sub(1)
    }

    pub fn is_synced(&self) -> bool {
        !self.timed.is_empty()
    }

    pub fn follow_playback(&mut self, position: Option<f64>, now: Instant) -> Option<usize> {
        let active = self.active_line(position);
        if self.manual_until.is_some_and(|until| now >= until) {
            self.manual_until = None;
        }
        if self.manual_until.is_none() {
            if let (Some(lines), Some(active)) = (self.lines.as_deref(), active) {
                let width = self.width.max(1) as usize;
                let row = crate::ui::lyrics_view::wrapped_row_for_line(lines, width, active);
                let footer = crate::ui::lyrics_view::attribution_rows(
                    self.attribution.as_deref(),
                    self.width.max(1) as usize,
                );
                let body_height =
                    self.height.saturating_sub(crate::ui::header::ROWS).max(1) as usize;
                let visible = body_height.saturating_sub(footer).max(1);
                self.scroll = row.saturating_sub(visible / 2);
            } else if position.is_some() {
                self.scroll = 0;
            }
        }
        active
    }

    pub fn following(&self) -> bool {
        self.manual_until.is_none()
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
        p.saw_size(40, 12);
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

    #[test]
    fn synced_lyrics_follow_playback_and_manual_scroll_temporarily_pauses() {
        let lyrics = Lyrics {
            plain: None,
            synced: Some("[00:01.00]one\n[00:04.00]two\n[00:08.00]three".into()),
            source: "test".into(),
            ..Lyrics::default()
        };
        let mut pane = LyricsPane::default();
        pane.open_lyrics(&lyrics, "song".into());
        pane.saw_size(60, 12);
        let start = Instant::now();
        assert_eq!(pane.follow_playback(Some(5.0), start), Some(1));
        pane.set_scroll(0);
        assert!(!pane.following());
        assert_eq!(
            pane.follow_playback(Some(9.0), start + Duration::from_secs(2)),
            Some(2)
        );
        assert_eq!(pane.scroll(), 0, "manual scroll has not expired");
        pane.follow_playback(Some(9.0), start + Duration::from_secs(4));
        assert!(pane.following());
    }

    #[test]
    fn scroll_by_is_clamped_and_starts_the_manual_follow_pause() {
        let lyrics = Lyrics {
            plain: Some("one\ntwo\nthree".into()),
            ..Lyrics::default()
        };
        let mut pane = LyricsPane::default();
        pane.open_lyrics(&lyrics, "song".into());
        pane.saw_size(40, 4);
        pane.scroll_by(100);
        assert_eq!(pane.scroll(), 2);
        assert!(!pane.following());
        pane.scroll_by(-100);
        assert_eq!(pane.scroll(), 0);
    }
}
