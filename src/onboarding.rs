//! First-run onboarding: welcome, controls, navidrome setup, library
//! paths; a proper guided sequence rather than a disconnected login
//! popup appearing with no context. Pure state only; main.rs owns the
//! network/filesystem orchestration (pinging navidrome, saving config),
//! same split as the rest of the app.

use crate::ui::login::LoginForm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Controls,
    Navidrome,
    LibraryPaths,
    Done,
}

pub struct Onboarding {
    pub step: Step,
    pub login: LoginForm,
    pub login_message: String,
    pub path_input: String,
    pub library_paths: Vec<String>,
}

impl Onboarding {
    pub fn new(default_library_paths: Vec<String>) -> Self {
        Onboarding {
            step: Step::Welcome,
            login: LoginForm::new(),
            login_message: String::new(),
            path_input: String::new(),
            library_paths: default_library_paths,
        }
    }

    pub fn advance(&mut self) {
        self.step = match self.step {
            Step::Welcome => Step::Controls,
            Step::Controls => Step::Navidrome,
            Step::Navidrome => Step::LibraryPaths,
            Step::LibraryPaths => Step::Done,
            Step::Done => Step::Done,
        };
    }
}
