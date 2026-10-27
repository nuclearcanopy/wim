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

impl PanelLayout {
    pub fn new() -> Self {
        PanelLayout {
            sidebar_width: 32,
            bottom_bar_height: 4,
            resize_mode: false,
            resize_selected: ResizePanel::Sidebar,
        }
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
                self.bottom_bar_height = self.bottom_bar_height.saturating_sub(STEP).max(BOTTOM_BAR_MIN);
            }
        }
    }
}

impl Default for PanelLayout {
    fn default() -> Self {
        Self::new()
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
        assert_eq!(l.bottom_bar_height, original_bottom, "sidebar resize must not affect bottom bar height");
    }
}
