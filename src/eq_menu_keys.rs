//! The eq menu's keyboard and its persistence.
//!
//! Pulled out of main.rs (see ROADMAP). The seam is the menu: every
//! function here either answers a key it owns, names the curve on
//! screen, or writes the live curve back to config.toml. The curve
//! maths itself is `eq.rs`; this is the menu around it.

use crossterm::event::KeyCode;

use crate::core::apply_eq_gains;
use crate::notify::Notifications;
use crate::{app, config, eq, keymap, mpv, ui};

/// One h/l nudge (`dir`: -1.0 or 1.0) on band `band`, 1 dB per step,
/// clamped to `eq::GAIN_LIMIT`; touching any band diverges from
/// whatever named curve was active, which keeps its name but reads as
/// modified from here on (`app.eq_source` is deliberately not
/// cleared, so the menu can still say which curve this came from).
pub fn adjust_eq_band(
    app: &mut app::App,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    band: usize,
    dir: f64,
) {
    app.eq_gains[band] = (app.eq_gains[band] + dir).clamp(-eq::GAIN_LIMIT, eq::GAIN_LIMIT);
    app.eq_custom = true;
    apply_eq_gains(app, player, notifications);
}

/// A built-in or saved curve's name, that name plus "(modified)" once
/// a band's been nudged off it, or "custom" for a curve that never
/// had one; shared by the player view and the bottom bar so neither
/// keeps crediting a curve that no longer matches `app.eq_gains`.
pub fn eq_label(app: &app::App, cfg: &config::Config) -> String {
    eq::applied_label(app.eq_source, app.eq_custom, &cfg.eq_custom_presets)
}

/// Writes the live eq state back to config.toml. Called on every
/// structural change (a save, rename or delete) as well as on closing
/// the menu, so a curve you deliberately named is on disk before
/// anything else can go wrong rather than only at esc.
pub fn persist_eq(cfg: &mut config::Config, app: &app::App, notifications: &mut Notifications) {
    let (preset, gains) = eq::persisted(app.eq_source, app.eq_gains, &cfg.eq_custom_presets);
    cfg.eq_preset = preset;
    cfg.eq_gains = gains;
    config::save_or_warn(cfg, notifications);
}

/// Why rename/delete did nothing: with nothing saved at all, the
/// useful answer is how to get there, not that the cursor is on the
/// wrong kind of row.
pub fn eq_saved_row_hint(cfg: &config::Config, verb: &str) -> String {
    if cfg.eq_custom_presets.is_empty() {
        format!(
            "nothing saved yet; {} saves the live curve under a name",
            keymap::key_label(ui::eq_menu::SAVE_KEY)
        )
    } else {
        format!("{verb} works on a row under 'saved'")
    }
}

/// One keypress while the eq menu's name editor is up. The editor
/// owns every key while it is open, so a letter types rather than
/// firing a menu action; split out because the validation branches
/// all have to write the menu's own message line. Returns which edit
/// committed, if any: the store changed, so config.toml needs
/// rewriting and the caller has to put the cursor back on the row
/// that now holds the curve (a new preset shifts every band row down
/// one).
pub fn eq_menu_editor_key(
    key: KeyCode,
    menu: &mut ui::eq_menu::EqMenuState,
    saved: &mut eq::CustomPresets,
    app: &mut app::App,
) -> Option<ui::eq_menu::EditorMode> {
    // taken rather than borrowed: dropping it on the floor is how the
    // editor closes, and putting it back is how it stays open.
    let mut editor = menu.editor.take()?;
    match key {
        KeyCode::Esc => {
            menu.message.clear();
            return None;
        }
        KeyCode::Backspace => editor.pop(),
        KeyCode::Char(c) => editor.push(c),
        KeyCode::Enter => {
            let name = editor.text.trim().to_string();
            match saved.check_name(&name, editor.rename_target()) {
                eq::NameCheck::Empty => menu.message = "give it a name first".to_string(),
                eq::NameCheck::TooLong => {
                    menu.message = format!("names are capped at {} characters", eq::MAX_NAME_LEN);
                }
                eq::NameCheck::Builtin => {
                    menu.message = format!("'{name}' is a built-in preset already");
                }
                // overwriting a saved curve destroys it, so it takes
                // a second enter, same gate as a delete.
                eq::NameCheck::Taken(i) if !editor.overwrite.press(i) => {
                    menu.message = format!("'{name}' exists; enter again to overwrite it");
                }
                _ => {
                    menu.message = eq_commit_name(editor.mode, &name, saved, app);
                    return Some(editor.mode);
                }
            }
        }
        _ => {}
    }
    menu.editor = Some(editor);
    None
}

/// Commits an accepted (or confirmed) name: a rename leaves the curve
/// alone, a save stores the live one, which makes the live gains that
/// saved preset exactly rather than a nameless curve.
pub fn eq_commit_name(
    mode: ui::eq_menu::EditorMode,
    name: &str,
    saved: &mut eq::CustomPresets,
    app: &mut app::App,
) -> String {
    match mode {
        ui::eq_menu::EditorMode::Rename(i) => {
            saved.rename(i, name);
            format!("renamed to '{name}'")
        }
        ui::eq_menu::EditorMode::Save => {
            let index = saved.insert(name, app.eq_gains);
            app.eq_source = Some(eq::Source::Saved(index));
            app.eq_custom = false;
            format!("saved as '{name}'")
        }
    }
}
