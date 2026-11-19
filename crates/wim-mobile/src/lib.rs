//! Mobile front-end core: a thin layer over `wim::swimsession::SwimSession`
//! and the `wim::player::Player` trait.

uniffi::setup_scaffolding!();

mod types;
pub use types::{Bubble, BubbleKind, PlatformPlayer, WimConfig, WimError};
