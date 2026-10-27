//! Navidrome profile management ('n' key): list configured servers,
//! add a new one, remove one. Adding always appends; it never
//! replaces an existing profile, so there's no "did that just erase my
//! setup?" ambiguity the way a single always-blank login form had.

use crate::ui::login::LoginForm;

pub enum NavManagerStep {
    List,
    Add(LoginForm),
}

pub struct NavManager {
    pub step: NavManagerStep,
    pub selected: usize,
    pub message: String,
    /// Connectivity check per configured server, same index as
    /// `cfg.navidrome_servers`; `None` = not tested this session yet.
    /// Tested all at once right when the manager opens, not lazily per
    /// row, so you see where every profile stands immediately.
    pub connection_status: Vec<Option<bool>>,
    /// Row index a first 'd' press armed for removal. Removing a
    /// profile rewrites config.toml immediately and can't be undone,
    /// so it takes a second press on the same row; anything else
    /// disarms it. `d` sat next to purely benign keys before this.
    pub pending_delete: Option<usize>,
}

impl NavManager {
    pub fn new() -> Self {
        NavManager {
            step: NavManagerStep::List,
            selected: 0,
            message: String::new(),
            connection_status: Vec::new(),
            pending_delete: None,
        }
    }

    /// Keeps `connection_status` aligned with the server list after a
    /// removal at `index`. Truncating when the status vec is shorter
    /// matters: it's grown lazily, so silently skipping the removal
    /// shifted every later server's badge onto the wrong row.
    pub fn forget_status_at(&mut self, index: usize) {
        if index < self.connection_status.len() {
            self.connection_status.remove(index);
        } else {
            self.connection_status.truncate(index);
        }
    }
}

impl Default for NavManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_row_keeps_later_badges_on_their_own_rows() {
        let mut nm = NavManager::new();
        nm.connection_status = vec![Some(true), Some(false), None];
        nm.forget_status_at(0);
        assert_eq!(nm.connection_status, vec![Some(false), None]);
    }

    #[test]
    fn removing_past_a_lazily_grown_status_vec_truncates_instead_of_misaligning() {
        let mut nm = NavManager::new();
        // only the first two of four servers have been tested.
        nm.connection_status = vec![Some(true), Some(false)];
        nm.forget_status_at(3);
        assert_eq!(
            nm.connection_status,
            vec![Some(true), Some(false)],
            "entries before the removal are still correctly aligned"
        );
        nm.connection_status = vec![Some(true), Some(false)];
        nm.forget_status_at(1);
        assert_eq!(nm.connection_status, vec![Some(true)]);
    }

    #[test]
    fn a_fresh_manager_has_nothing_armed_for_deletion() {
        assert_eq!(NavManager::new().pending_delete, None);
    }
}
