//! The player, as a library. `wim` (the full pane ui) and `shellswim`
//! (the shell, nothing else) are both thin front-ends over this; the
//! seam between them is the player core; read docs/library-seam.md
//! before moving anything across it.

pub mod anim;
pub mod app;
pub mod browse;
pub mod cache;
pub mod catalog;
pub mod config;
pub mod core;
pub mod coverart;
pub mod download;
pub mod enrich;
pub mod eq;
pub mod eq_menu_keys;
pub mod favorites;
pub mod focus;
pub mod graphics;
pub mod hit;
pub mod keymap;
pub mod layout;
pub mod library;
pub mod listenbrainz;
pub mod lyrics;
pub mod mpv;
pub mod namehint;
pub mod navmanager;
pub mod notify;
pub mod onboarding;
pub mod opensubtitles;
pub mod persistence;
pub mod playqueue;
pub mod queues;
pub mod search;
pub mod shellsong;
pub mod shellsong_host;
pub mod subsonic;
pub mod suggest;
pub mod swimconsole;
pub mod term;
pub mod theme;
pub mod trace;
pub mod transport;
pub mod ui;
pub mod undo;
pub mod video;

/// Which pane plain navigation (j/k/arrows/enter) currently targets.
/// Lives at the crate root rather than in a front-end because
/// `hit`'s mouse routing is written against it; shellswim has no
/// panes and never reads it. `shellsong::Focus` is the interpreter's
/// own copy, bridged by `core::shellsong_focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Workspace,
    BottomBar,
}
