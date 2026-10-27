//! First-run onboarding: welcome, controls, navidrome setup,
//! listenbrainz token (optional, skippable), library paths; a proper
//! guided sequence rather than a disconnected login popup appearing
//! with no context. Pure state only; main.rs owns the
//! network/filesystem orchestration (pinging navidrome, saving config),
//! same split as the rest of the app.
//!
//! TODO(listenbrainz): once a "save song" / download feature exists
//! (not on this branch as of writing; check other feature worktrees),
//! also offer this same token prompt the first time a song is saved
//! with no token configured yet, not just here at first run.

use crate::ui::login::LoginForm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Controls,
    Navidrome,
    ListenBrainz,
    LibraryPaths,
    Done,
}

pub struct Onboarding {
    pub step: Step,
    pub login: LoginForm,
    pub login_message: String,
    /// Raw token as typed so far; entirely optional, skippable with
    /// esc or an empty enter (see main.rs's `Step::ListenBrainz`
    /// handling). Also the integration point for "prompt again on
    /// first save" once a save/download feature exists on a branch
    /// this one has merged with (not present here yet; see
    /// `onboarding.rs` module doc).
    pub listenbrainz_token_input: String,
    pub path_input: String,
    pub library_paths: Vec<String>,
}

impl Onboarding {
    pub fn new(default_library_paths: Vec<String>) -> Self {
        Onboarding {
            step: Step::Welcome,
            login: LoginForm::new(),
            login_message: String::new(),
            listenbrainz_token_input: String::new(),
            path_input: String::new(),
            library_paths: default_library_paths,
        }
    }

    pub fn advance(&mut self) {
        self.step = match self.step {
            Step::Welcome => Step::Controls,
            Step::Controls => Step::Navidrome,
            Step::Navidrome => Step::ListenBrainz,
            Step::ListenBrainz => Step::LibraryPaths,
            Step::LibraryPaths => Step::Done,
            Step::Done => Step::Done,
        };
    }
}
