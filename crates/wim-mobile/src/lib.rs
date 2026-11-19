//! Mobile front-end core: a thin layer over `wim::swimsession::SwimSession`
//! and the `wim::player::Player` trait.

uniffi::setup_scaffolding!();

mod types;
pub use types::{Bubble, BubbleKind, PlatformPlayer, WimConfig, WimError};
mod emulator;
pub use emulator::PlaylistEmulator;
mod driver;
mod session;
pub use driver::Driver;
pub use session::WimSession;
