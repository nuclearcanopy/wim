//! Navidrome profile management ('n' key): list configured servers,
//! add a new one, remove one. Adding always appends — it never
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
}

impl NavManager {
    pub fn new() -> Self {
        NavManager { step: NavManagerStep::List, selected: 0, message: String::new() }
    }
}

impl Default for NavManager {
    fn default() -> Self {
        Self::new()
    }
}
