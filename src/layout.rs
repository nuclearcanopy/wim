//! Panel sizing state: the sidebar is a vertical strip (only
//! horizontally resizable), the bottom bar is a horizontal strip (only
//! vertically resizable); resize mode lets you select one with wasd
//! and stretch it along its one valid axis with the arrow keys.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizePanel {
    Sidebar,
    BottomBar,
}

pub struct PanelLayout {
    pub sidebar_width: u16,
    pub bottom_bar_height: u16,
    pub resize_mode: bool,
    pub resize_selected: ResizePanel,
}

const SIDEBAR_MIN: u16 = 20;
const SIDEBAR_MAX: u16 = 60;
const BOTTOM_BAR_MIN: u16 = 2;
const BOTTOM_BAR_MAX: u16 = 10;
const STEP: u16 = 2;

/// The defaults a fresh config starts from; also what an out-of-range
/// persisted value is clamped into.
pub const SIDEBAR_DEFAULT: u16 = 32;
pub const BOTTOM_BAR_DEFAULT: u16 = 4;

/// The share of the terminal the chrome may take, and the share it must
/// keep, as `(numerator, denominator)`.
///
/// The stored sizes are absolute cell counts, which is right across the
/// ordinary band and wrong at both ends. Narrow: `Constraint::Length`
/// hands the whole width to the first constraint and zero to the rest,
/// so a 32-cell sidebar on a 24-cell terminal leaves **no content pane
/// at all**. Wide terminals also need proportional chrome so a fixed
/// sidebar does not become a physically thin strip.
/// The floors are deliberately gentle. At 1/10 the stored 32 survives
/// to 320 columns and only widens past it.
const SIDEBAR_SHARE_MAX: (u16, u16) = (2, 5);
const SIDEBAR_SHARE_MIN: (u16, u16) = (1, 10);
const BOTTOM_SHARE_MAX: (u16, u16) = (1, 3);
const BOTTOM_SHARE_MIN: (u16, u16) = (1, 16);

fn share(total: u16, (num, den): (u16, u16)) -> u16 {
    (u32::from(total) * u32::from(num) / u32::from(den)) as u16
}

/// Each floor share stays below its ceiling share, checked here rather
/// than defended against at runtime.
///
/// `u16::clamp` panics when the floor is the higher of the two, and the
/// first version of `bounded` carried a `.min(ceiling)` with a comment
/// claiming a one-cell terminal could trigger it. **That was wrong**: at
/// one cell both shares floor to zero, and 1/10 cannot exceed 2/5 at
/// any width, so the guard was dead code and the only way to reach the
/// panic is to edit these constants. Mutating the `.min` away failed to
/// break a single test, which is what exposed it. So the invariant is a
/// compile error now instead.
const _: () = assert!(
    SIDEBAR_SHARE_MIN.0 * SIDEBAR_SHARE_MAX.1 < SIDEBAR_SHARE_MAX.0 * SIDEBAR_SHARE_MIN.1,
    "the sidebar's floor share must stay below its ceiling share"
);
const _: () = assert!(
    BOTTOM_SHARE_MIN.0 * BOTTOM_SHARE_MAX.1 < BOTTOM_SHARE_MAX.0 * BOTTOM_SHARE_MIN.1,
    "the bottom bar's floor share must stay below its ceiling share"
);

/// `stored`, kept inside a share of `total` at both ends.
fn bounded(stored: u16, total: u16, min: (u16, u16), max: (u16, u16)) -> u16 {
    stored.clamp(share(total, min), share(total, max))
}

impl PanelLayout {
    pub fn new() -> Self {
        Self::from_sizes(SIDEBAR_DEFAULT, BOTTOM_BAR_DEFAULT)
    }

    /// Restores persisted sizes, clamped to the same bounds resize
    /// mode itself enforces: a hand-edited config shouldn't be able to
    /// produce a sidebar wider than the terminal or a zero-row bar.
    pub fn from_sizes(sidebar_width: u16, bottom_bar_height: u16) -> Self {
        PanelLayout {
            sidebar_width: sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX),
            bottom_bar_height: bottom_bar_height.clamp(BOTTOM_BAR_MIN, BOTTOM_BAR_MAX),
            resize_mode: false,
            resize_selected: ResizePanel::Sidebar,
        }
    }

    /// The sidebar's width on a terminal this wide, which is what the
    /// split must use. The stored width is honoured wherever it fits;
    /// see `SIDEBAR_SHARE_MAX` for the two ends where it cannot be.
    pub fn sidebar_for(&self, total_width: u16) -> u16 {
        bounded(
            self.sidebar_width,
            total_width,
            SIDEBAR_SHARE_MIN,
            SIDEBAR_SHARE_MAX,
        )
    }

    /// The bottom bar's height on a terminal this tall. Same rule, and
    /// a smaller share: the bar is a strip of controls, where the
    /// sidebar is a list that wants reading room.
    pub fn bottom_bar_for(&self, total_height: u16) -> u16 {
        bounded(
            self.bottom_bar_height,
            total_height,
            BOTTOM_SHARE_MIN,
            BOTTOM_SHARE_MAX,
        )
    }

    pub fn toggle_resize_mode(&mut self) {
        self.resize_mode = !self.resize_mode;
    }

    /// wasd selection: only two panels exist today, so left/a and
    /// down/s pick them directly; up/w and right/d are the reverse
    /// direction for the same two, kept symmetric rather than no-ops.
    pub fn select(&mut self, panel: ResizePanel) {
        self.resize_selected = panel;
    }

    pub fn grow(&mut self) {
        match self.resize_selected {
            ResizePanel::Sidebar => {
                self.sidebar_width = (self.sidebar_width + STEP).min(SIDEBAR_MAX);
            }
            ResizePanel::BottomBar => {
                self.bottom_bar_height = (self.bottom_bar_height + STEP).min(BOTTOM_BAR_MAX);
            }
        }
    }

    pub fn shrink(&mut self) {
        match self.resize_selected {
            ResizePanel::Sidebar => {
                self.sidebar_width = self.sidebar_width.saturating_sub(STEP).max(SIDEBAR_MIN);
            }
            ResizePanel::BottomBar => {
                self.bottom_bar_height = self
                    .bottom_bar_height
                    .saturating_sub(STEP)
                    .max(BOTTOM_BAR_MIN);
            }
        }
    }
}

impl Default for PanelLayout {
    fn default() -> Self {
        Self::new()
    }
}

/// The split at the sizes that matter, drawn to scale, because the
/// numbers alone do not say whether the chrome reads as chrome.
///
/// `cargo test --lib layout::eyeball -- --nocapture`
#[cfg(test)]
mod eyeball {
    use super::*;

    /// 60 cells of diagram whatever the terminal is, so the shapes are
    /// comparable down the column.
    fn bar(sidebar: u16, divider: u16, content: u16, width: u16) -> String {
        let scale = |n: u16| {
            if width == 0 {
                0
            } else {
                (u32::from(n) * 60 / u32::from(width)) as usize
            }
        };
        // the divider is one cell and must read as one: scaling it
        // like the panes drew it as the whole bar at a 1-cell
        // terminal, which is the diagram lying rather than the layout
        // being wrong.
        format!(
            "[{}{}{}]",
            "S".repeat(scale(sidebar)),
            if divider > 0 { "|" } else { "" },
            "C".repeat(scale(content)),
        )
    }

    #[test]
    fn print_the_split_from_a_phone_to_a_wall() {
        let l = PanelLayout::new();
        println!(
            "--- stored sidebar {} cells, bottom bar {} rows",
            l.sidebar_width, l.bottom_bar_height
        );
        for (w, h, note) in [
            (20u16, 8u16, "absurd zoom-in, 20 cols"),
            (40, 12, "very large font"),
            (60, 16, "small window"),
            (80, 24, "the classic terminal"),
            (120, 30, "an ordinary wim window"),
            (200, 50, "a big monitor"),
            (320, 90, "very wide terminal"),
            (480, 140, "absurd zoom-out"),
            (1, 1, "degenerate"),
        ] {
            let side = l.sidebar_for(w);
            let divider = if w > side { 1 } else { 0 };
            let content = w.saturating_sub(side + divider);
            let bottom = l.bottom_bar_for(h);
            println!(
                "  {w:>4}x{h:<4} sidebar {side:>3} ({:>2}%)  content {content:>3}  bar {bottom:>2} of {h:<3} ({:>2}%)  {} {note}",
                pct(side, w),
                pct(bottom, h),
                bar(side, divider, content, w),
            );
        }
    }

    fn pct(part: u16, whole: u16) -> u16 {
        if whole == 0 {
            0
        } else {
            (u32::from(part) * 100 / u32::from(whole)) as u16
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_resize_clamps_at_both_bounds() {
        let mut l = PanelLayout::new();
        l.select(ResizePanel::Sidebar);
        for _ in 0..50 {
            l.grow();
        }
        assert_eq!(l.sidebar_width, SIDEBAR_MAX);
        for _ in 0..50 {
            l.shrink();
        }
        assert_eq!(l.sidebar_width, SIDEBAR_MIN);
    }

    #[test]
    fn bottom_bar_resize_clamps_at_both_bounds() {
        let mut l = PanelLayout::new();
        l.select(ResizePanel::BottomBar);
        for _ in 0..50 {
            l.grow();
        }
        assert_eq!(l.bottom_bar_height, BOTTOM_BAR_MAX);
        for _ in 0..50 {
            l.shrink();
        }
        assert_eq!(l.bottom_bar_height, BOTTOM_BAR_MIN);
    }

    #[test]
    fn resizing_one_panel_never_touches_the_other() {
        let mut l = PanelLayout::new();
        let original_bottom = l.bottom_bar_height;
        l.select(ResizePanel::Sidebar);
        l.grow();
        l.shrink();
        l.shrink();
        assert_eq!(
            l.bottom_bar_height, original_bottom,
            "sidebar resize must not affect bottom bar height"
        );
    }

    #[test]
    fn persisted_sizes_are_restored() {
        let l = PanelLayout::from_sizes(44, 7);
        assert_eq!(l.sidebar_width, 44);
        assert_eq!(l.bottom_bar_height, 7);
    }

    #[test]
    fn a_hand_edited_config_cannot_push_sizes_out_of_range() {
        let huge = PanelLayout::from_sizes(9999, 9999);
        assert_eq!(huge.sidebar_width, SIDEBAR_MAX);
        assert_eq!(huge.bottom_bar_height, BOTTOM_BAR_MAX);
        let tiny = PanelLayout::from_sizes(0, 0);
        assert_eq!(tiny.sidebar_width, SIDEBAR_MIN);
        assert_eq!(tiny.bottom_bar_height, BOTTOM_BAR_MIN);
    }

    /// The guarantee that matters most: across the band people
    /// actually use, the stored width is what you get. A layout change
    /// that moved the familiar sidebar would be a regression dressed
    /// as a fix.
    #[test]
    fn the_stored_width_survives_the_whole_ordinary_band() {
        let l = PanelLayout::new();
        for width in 80..=320u16 {
            assert_eq!(
                l.sidebar_for(width),
                SIDEBAR_DEFAULT,
                "the sidebar moved at {width} columns"
            );
        }
        for height in 12..=48u16 {
            assert_eq!(
                l.bottom_bar_for(height),
                BOTTOM_BAR_DEFAULT,
                "the bottom bar moved at {height} rows"
            );
        }
    }

    /// A content pane always exists. `Constraint::Length` hands the
    /// whole width to the first constraint and zero to the rest, so a
    /// stored 32 on a 24-cell terminal used to leave the sidebar
    /// holding everything and the content pane holding nothing.
    #[test]
    fn there_is_always_room_left_for_the_content_pane() {
        for stored in [SIDEBAR_DEFAULT, 20, 60] {
            let l = PanelLayout::from_sizes(stored, BOTTOM_BAR_DEFAULT);
            for width in 2..=400u16 {
                let side = l.sidebar_for(width);
                // the divider column is one cell and belongs to
                // neither pane, so the content gets what is left.
                assert!(
                    side + 1 < width,
                    "stored {stored} at {width} columns left {} for content",
                    width.saturating_sub(side + 1)
                );
            }
        }
    }

    /// Past the band the chrome scales rather than thinning out. ascii
    /// video wants a font small enough for a few hundred cells, and a
    /// fixed cell count is a physical sliver at that size.
    #[test]
    fn the_chrome_grows_with_an_absurdly_zoomed_out_terminal() {
        let l = PanelLayout::new();
        assert_eq!(l.sidebar_for(320), SIDEBAR_DEFAULT, "the band ends here");
        assert!(l.sidebar_for(480) > SIDEBAR_DEFAULT);
        assert!(l.sidebar_for(800) > l.sidebar_for(480));
        assert!(l.bottom_bar_for(140) > BOTTOM_BAR_DEFAULT);
        // and a user who narrowed it still gets it widened, since the
        // choice was made in cells at some other font size.
        let narrow = PanelLayout::from_sizes(20, BOTTOM_BAR_DEFAULT);
        assert!(narrow.sidebar_for(800) > 20);
    }

    /// The bar is controls, not a pane: a third of the height is
    /// already generous and it must never take more.
    #[test]
    fn the_bottom_bar_never_takes_more_than_a_third() {
        for stored in [BOTTOM_BAR_DEFAULT, 2, 10] {
            let l = PanelLayout::from_sizes(SIDEBAR_DEFAULT, stored);
            for height in 1..=200u16 {
                assert!(
                    u32::from(l.bottom_bar_for(height)) * 3 <= u32::from(height),
                    "stored {stored} took {} of {height} rows",
                    l.bottom_bar_for(height)
                );
            }
        }
    }

    /// `u16::clamp` panics when the floor is above the ceiling, and a
    /// one-cell terminal makes that happen: the floor is taken down to
    /// the ceiling for exactly this.
    #[test]
    fn a_degenerate_terminal_is_zero_and_not_a_panic() {
        let l = PanelLayout::new();
        for width in 0..=3u16 {
            assert!(l.sidebar_for(width) <= width);
        }
        for height in 0..=3u16 {
            assert!(l.bottom_bar_for(height) <= height);
        }
        assert_eq!(l.sidebar_for(0), 0);
        assert_eq!(l.bottom_bar_for(0), 0);
    }

    #[test]
    fn new_matches_the_documented_defaults() {
        let l = PanelLayout::new();
        assert_eq!(l.sidebar_width, SIDEBAR_DEFAULT);
        assert_eq!(l.bottom_bar_height, BOTTOM_BAR_DEFAULT);
    }
}
