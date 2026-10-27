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
}

impl NavManager {
    pub fn new() -> Self {
        NavManager {
            step: NavManagerStep::List,
            selected: 0,
            message: String::new(),
            connection_status: Vec::new(),
        }
    }
}

impl Default for NavManager {
    fn default() -> Self {
        Self::new()
    }
}
