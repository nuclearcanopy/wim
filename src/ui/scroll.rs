//! The one clamp-to-selection scroll rule every custom-painted list
//! in the ui uses (sidebar results, browse track list, artist-profile
//! albums column). It lived twice, copied, and a third list simply
//! never got it: past the pane height `j` moved an invisible cursor.

/// First row of `visible_rows` to actually draw, given which row is
/// selected: the window stays put until the selection would fall past
/// the bottom of the pane, then follows it one row at a time.
pub fn offset_for(selected_row: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 || selected_row < visible_rows {
        0
    } else {
        selected_row + 1 - visible_rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_already_on_screen_does_not_scroll() {
        assert_eq!(offset_for(0, 10), 0);
        assert_eq!(offset_for(9, 10), 0);
    }

    #[test]
    fn scrolling_keeps_the_selected_row_as_the_last_visible_one() {
        assert_eq!(offset_for(10, 10), 1);
        assert_eq!(offset_for(42, 10), 33);
    }

    #[test]
    fn a_zero_height_pane_has_no_offset() {
        assert_eq!(offset_for(7, 0), 0);
    }
}
