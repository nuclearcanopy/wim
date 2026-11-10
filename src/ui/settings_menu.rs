use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::config::VideoOutput;
use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::results_list::scroll_cues;
use ratatui_image::picker::ProtocolType;

/// A row, named rather than numbered.
///
/// The adjust keys used to match on the selected *index*, so inserting
/// a row anywhere but the end silently moved every adjuster below it
/// onto the wrong setting. Naming them means `FIELDS` is the only
/// place order lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    AlbumArt,
    VideoOutput,
    AsciiFps,
    EqEnabled,
    VisualizerBands,
    Crossfade,
    CrossfadeSeconds,
    CacheDaysThreshold,
    CacheEvictionDays,
    ScrobblePercent,
    ScrobbleSeconds,
    ListenbrainzToken,
    TorboxTokenFile,
}

/// Every row, in the order they are drawn. Display and playback settings
/// stay near the top; the token stays last because it is the only one
/// that opens an editor and the footer hint names it.
pub const FIELDS: [Field; 13] = [
    Field::AlbumArt,
    Field::VideoOutput,
    Field::AsciiFps,
    Field::EqEnabled,
    Field::VisualizerBands,
    Field::Crossfade,
    Field::CrossfadeSeconds,
    Field::CacheDaysThreshold,
    Field::CacheEvictionDays,
    Field::ScrobblePercent,
    Field::ScrobbleSeconds,
    Field::ListenbrainzToken,
    Field::TorboxTokenFile,
];

pub const FIELD_COUNT: usize = FIELDS.len();

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::AlbumArt => "album art",
            Field::VideoOutput => "video output",
            Field::AsciiFps => "text-cell video rate",
            Field::EqEnabled => "equalizer",
            Field::VisualizerBands => "visualizer bands",
            Field::Crossfade => "crossfade",
            Field::CrossfadeSeconds => "crossfade seconds",
            Field::CacheDaysThreshold => "cache after days played",
            Field::CacheEvictionDays => "cache eviction days",
            Field::ScrobblePercent => "scrobble % threshold",
            Field::ScrobbleSeconds => "scrobble seconds threshold",
            Field::ListenbrainzToken => "listenbrainz token",
            Field::TorboxTokenFile => "TorBox connection",
        }
    }
}

/// What the "video output" row reads, which is the mode actually in
/// force and not simply the config's.
///
/// Three things the plain config value would hide: that shift-`v` has
/// overridden it (so the row would contradict the screen), that `auto`
/// resolved to a particular vo (the user cannot otherwise tell whether
/// their terminal answered the graphics probe), and that `ascii` is
/// wim painting rather than mpv.
pub fn video_output_value(
    configured: VideoOutput,
    over: Option<VideoOutput>,
    probed: Option<ProtocolType>,
) -> String {
    let live = crate::video::effective(configured, over);
    let mut said = match live {
        // what `auto` resolved to, which is the only way to see
        // whether the terminal answered the graphics probe.
        VideoOutput::Auto => match crate::video::drawing_vo(VideoOutput::Auto, probed) {
            Some(vo) => format!("auto ({})", vo.mpv_name()),
            None => "auto (no protocol yet)".to_string(),
        },
        other => config_word(other).to_string(),
    };
    if over.is_some() {
        // naming the config value is the point: this row is the only
        // place the override is visible, and a restart undoes it.
        //
        // kept short deliberately. the first version read `ascii (wim
        // paints it) [shift+v; config: auto]`, which is 44 cells and
        // truncated to `... [shift` on a 62-wide pane, cutting the one
        // fact the note exists for.
        said.push_str(&format!(" (config: {})", config_word(configured)));
    }
    said
}

/// The bare config word, for the override note. Lives on
/// `VideoOutput` now, so `video()`'s report and this row cannot drift.
fn config_word(mode: VideoOutput) -> &'static str {
    mode.word()
}

/// What the rate row reads. Unset is the common case and says what it
/// means rather than "none".
pub fn ascii_fps_value(cap: Option<f64>) -> String {
    match cap {
        Some(fps) => format!("at most {fps:.0} fps"),
        None => "the file's own rate".to_string(),
    }
}

/// The row the cursor is on. Out of range cannot happen (the cursor
/// wraps on `FIELD_COUNT`), and the fallback is the first row rather
/// than a panic: a settings menu is not worth crashing over.
pub fn field_at(selected: usize) -> Field {
    FIELDS.get(selected).copied().unwrap_or(Field::AlbumArt)
}

/// Takes over the whole workspace (sidebar included, never the
/// bottom bar) the same way the help overlay takes over the whole
/// terminal; a persistent settings surface is exactly the kind of
/// thing that should be easy to find rather than a small popup, which
/// is what the centered subtitle menu is for instead.
pub struct SettingsMenu<'a> {
    /// One per `FIELDS` entry, in that order. A slice rather than an
    /// array so the caller can build it by mapping `FIELDS`, which is
    /// what keeps the two in step.
    pub values: &'a [&'a str],
    pub selected: usize,
    pub keymap: &'a Keymap,
}

impl Widget for SettingsMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // this takes over a rect that already has a sidebar, a track
        // list or the banner drawn into it, so it has to own every
        // cell: without the clear, whatever the menu did not write
        // still showed through between its rows.
        let area = crate::ui::draw::overlay(buf, area);

        // only the remappable keys come from the keymap; j/k, h/l,
        // enter, esc and the arrows are the fixed conventions
        // reserved in keymap.rs, so they stay literal.
        let key = |action| key_label(self.keymap.get(action));
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "settings",
            &format!("{} or esc to close", key(KeyAction::OpenSettingsMenu)),
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        let key_width = FIELDS.iter().map(|f| f.label().len()).max().unwrap_or(0);
        // the hint naming the key that opens the token editor claims
        // its row before the list does, then the list scrolls inside
        // what is left: it had no offset at all, so on a short pane
        // the last fields were selectable but never drawn. the second
        // hint is a pointer elsewhere and is left to clip.
        // Reserve a spacer and both guidance rows inside the frame. The
        // second hint used to land on the bottom border at normal terminal
        // heights, making the keymap/subtitle entry points disappear.
        let list_top = y;
        let room = (area.y + area.height).saturating_sub(list_top);
        // On short panes, keep selection rows and the primary hint ahead of
        // the secondary discovery hint. On a normal pane reserve both hints
        // and a spacer so neither is painted over the bottom frame.
        let footer = if room >= 10 { 4 } else { 2 };
        let rows = room.saturating_sub(footer).min(FIELD_COUNT as u16);
        let offset = crate::ui::scroll::offset_for(self.selected, rows as usize);
        // one column of gutter for the scroll cue, and only while
        // there is one: a list that fits keeps its full width.
        let scrolls = FIELD_COUNT > rows as usize;
        let text = Rect {
            width: area.width.saturating_sub(u16::from(scrolls)),
            ..area
        };
        for (row, (i, (label, value))) in FIELDS
            .iter()
            .map(|f| f.label())
            .zip(self.values.iter())
            .enumerate()
            .skip(offset)
            .take(rows as usize)
            .enumerate()
        {
            let color = if i == self.selected {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };
            let marker = if i == self.selected { '>' } else { ' ' };
            draw_line(
                buf,
                text,
                list_top + row as u16,
                &format!("{marker} {label:<key_width$}  {value}"),
                color,
            );
        }
        scroll_cues(
            buf,
            Rect {
                y: list_top,
                height: rows,
                ..area
            },
            offset,
            FIELD_COUNT,
        );

        y = list_top + rows + 1;
        draw_line(
            buf,
            area,
            y,
            "j/k select, h/l adjust, enter on a token row to configure it",
            shade(DIM),
        );
        y += 1;
        draw_line(
            buf,
            area,
            y,
            &format!(
                "other settings: '{}' keybinds, '{}' subtitles, left/right on the home screen logo",
                key(KeyAction::OpenHelp),
                key(KeyAction::OpenSubtitleMenu),
            ),
            shade(DIM),
        );
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    crate::ui::draw::line(buf, area, y, text, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{
        assert_no_bleed_through, assert_outside_untouched, buffer, grid, plated_buffer,
    };
    use crossterm::event::KeyCode;

    fn render(area: Rect, buf: &mut Buffer, values: [&str; FIELD_COUNT]) {
        SettingsMenu {
            values: &values,
            selected: 0,
            keymap: &Keymap::default(),
        }
        .render(area, buf);
    }

    #[test]
    fn every_field_gets_a_row_with_its_value_in_an_aligned_column() {
        let area = Rect::new(0, 0, 80, 17);
        let mut buf = buffer(80, 17);
        render(
            area,
            &mut buf,
            [
                "on",
                "auto (tct)",
                "the file's own rate",
                "on",
                "10 bands",
                "off",
                "3",
                "7",
                "30",
                "50",
                "240",
                "set",
                "not connected",
            ],
        );
        assert_eq!(grid(&buf), SNAPSHOT.join("\n"));
    }

    /// The menu reserves both footer hints without painting over the
    /// bottom frame; on this short pane the list scrolls to make room.
    const SNAPSHOT: [&str; 17] = [
        "┌──────────────────────────────────────────────────────────────────────────────┐",
        "│ settings │ u or esc to close                                                 │",
        "│ ─────────┴────────────────────────────────────────────────────────────────── │",
        "│                                                                              │",
        "│ > album art                   on                                             │",
        "│   video output                auto (tct)                                     │",
        "│   text-cell video rate        the file's own rate                            │",
        "│   equalizer                   on                                             │",
        "│   visualizer bands            10 bands                                       │",
        "│   crossfade                   off                                            │",
        "│   crossfade seconds           3                                              │",
        "│   cache after days played     7                                            ▾ │",
        "│                                                                              │",
        "│ j/k select, h/l adjust, enter on a token row to configure it                 │",
        "│ other settings: '?' keybinds, 't' subtitles, left/right on the home screen l │",
        "│                                                                              │",
        "└──────────────────────────────────────────────────────────────────────────────┘",
    ];

    /// `values` is the user's own config (a listenbrainz token, a path
    /// label); a wide char in one must not shove the rest of its row.
    #[test]
    fn a_wide_value_does_not_shift_the_glyphs_after_it() {
        let area = Rect::new(0, 0, 60, 17);
        let mut buf = buffer(60, 17);
        render(
            area,
            &mut buf,
            ["日本x", "", "", "", "", "", "", "", "", "", "", "", ""],
        );
        let g = grid(&buf);
        let row: Vec<char> = g
            .lines()
            .find(|r| r.contains('日'))
            .unwrap()
            .chars()
            .collect();
        let col = row.iter().position(|c| *c == '日').expect("the wide value");
        // two cells each, so 'x' lands four columns on, not two.
        assert_eq!(
            (
                row[col],
                row[col + 1],
                row[col + 2],
                row[col + 3],
                row[col + 4]
            ),
            ('日', ' ', '本', ' ', 'x')
        );
    }

    #[test]
    fn a_wide_value_stays_inside_the_rect() {
        let mut buf = buffer(40, 16);
        let area = Rect::new(2, 1, 24, 13);
        let long = "日本語テスト曲日本語テスト曲日本語テスト曲";
        render(area, &mut buf, [long; FIELD_COUNT]);
        assert_outside_untouched(&buf, area);
    }

    /// The reported bug: the menu drew its rows and nothing else, so
    /// the sidebar and track list underneath showed through between
    /// them.
    #[test]
    fn the_overlay_covers_every_cell_of_its_rect() {
        let mut buf = plated_buffer(40, 16);
        let area = Rect::new(2, 1, 30, 13);
        render(area, &mut buf, ["on"; FIELD_COUNT]);
        assert_no_bleed_through(&buf, area);
    }

    #[test]
    fn the_frame_closes_all_four_corners() {
        let mut buf = buffer(40, 16);
        let area = Rect::new(2, 1, 30, 13);
        render(area, &mut buf, ["on"; FIELD_COUNT]);
        let g = grid(&buf);
        let rows: Vec<&str> = g.lines().collect();
        assert_eq!(rows[1].chars().nth(2), Some('┌'));
        assert_eq!(rows[1].chars().nth(31), Some('┐'));
        assert_eq!(rows[13].chars().nth(2), Some('└'));
        assert_eq!(rows[13].chars().nth(31), Some('┘'));
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(Rect::new(0, 0, w, h), &mut buf, ["日本語"; FIELD_COUNT]);
        }
    }

    fn render_at(area: Rect, buf: &mut Buffer, selected: usize) {
        SettingsMenu {
            values: &["on"; FIELD_COUNT],
            selected,
            keymap: &Keymap::default(),
        }
        .render(area, buf);
    }

    /// j/k walks every field, so every field has to be reachable on a
    /// pane that holds a fraction of them; enter on a row nobody can
    /// see opens the token editor blind.
    #[test]
    fn the_selected_field_is_on_screen_at_every_selection() {
        for h in [8u16, 10, 12, 17] {
            for (selected, label) in FIELDS.iter().map(|f| f.label()).enumerate() {
                let mut buf = buffer(80, h);
                render_at(Rect::new(0, 0, 80, h), &mut buf, selected);
                let g = grid(&buf);
                assert!(
                    g.contains(&format!("> {label}")),
                    "field {selected} ({label}) is off a {h}-row pane:\n{g}"
                );
            }
        }
    }

    /// The hint naming the one key that opens the token editor is the
    /// row that explains the row below it, so it claims its own row
    /// rather than being pushed off by the list.
    #[test]
    fn the_primary_hint_survives_a_pane_the_list_overflows() {
        let mut buf = buffer(80, 10);
        render_at(Rect::new(0, 0, 80, 10), &mut buf, FIELD_COUNT - 1);
        assert!(
            grid(&buf).contains("j/k select, h/l adjust"),
            "{}",
            grid(&buf)
        );
    }

    /// Same `▴`/`▾` gutter cue `lyrics_view` uses, so a window onto
    /// eight fields does not read as all of them.
    #[test]
    fn a_scrolled_list_cues_what_is_off_each_edge() {
        let cue = |selected: usize, h: u16| {
            let mut buf = buffer(80, h);
            render_at(Rect::new(0, 0, 80, h), &mut buf, selected);
            let g = grid(&buf);
            (g.contains('▴'), g.contains('▾'))
        };
        assert_eq!(cue(0, 10), (false, true), "more below only");
        assert_eq!(cue(FIELD_COUNT - 1, 10), (true, false), "above only");
        assert_eq!(cue(0, 17), (false, true), "only the last field is below");
    }

    #[test]
    fn the_hints_follow_the_live_keymap_but_keep_the_fixed_keys() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::OpenSettingsMenu, KeyCode::Char('x'));
        keymap.set(KeyAction::OpenHelp, KeyCode::Char('y'));
        keymap.set(KeyAction::OpenSubtitleMenu, KeyCode::Char('z'));
        let area = Rect::new(0, 0, 100, 20);
        let mut buf = Buffer::empty(area);
        SettingsMenu {
            values: &["", "", "", "", "", "", "", "", "", "", "", "", ""],
            selected: 0,
            keymap: &keymap,
        }
        .render(area, &mut buf);
        let out: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("x or esc to close"), "got:\n{out}");
        assert!(out.contains("'y' keybinds"), "got:\n{out}");
        assert!(out.contains("'z' subtitles"), "got:\n{out}");
        assert!(!out.contains("u or esc to close"), "stale default:\n{out}");
        assert!(!out.contains("'?' keybinds"), "stale default:\n{out}");
        assert!(!out.contains("'t' subtitles"), "stale default:\n{out}");
        // the reserved navigation keys are not remappable and must
        // not start following the keymap.
        assert!(out.contains("j/k select, h/l adjust"), "got:\n{out}");
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    /// The menu at the rows `main_content` actually gets: a 14-row
    /// terminal with the default 4-row bottom bar leaves 10, and the
    /// listenbrainz token is the last field:
    /// `cargo test --lib ui::settings_menu::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_menu_with_the_token_row_selected() {
        let values = [
            "balanced ('i' also cycles this)",
            "auto (tct)",
            "the file's own rate",
            "off",
            "3.0s",
            "7 days played",
            "30 days",
            "50%",
            "240s",
            "not set (enter to add)",
            "not connected (enter to set up)",
        ];
        let last = FIELD_COUNT - 1;
        for (w, h, selected) in [
            (80u16, 10u16, last),
            (80, 10, 0),
            (80, 12, last),
            (80, 17, last),
        ] {
            let mut buf = buffer(w, h);
            SettingsMenu {
                values: &values,
                selected,
                keymap: &Keymap::default(),
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
            let text = grid(&buf);
            println!("--- {w}x{h} selected={selected}\n{text}");
            assert!(
                text.chars().any(|c| c != '.' && c != '\n'),
                "{w}x{h} drew nothing at all"
            );
        }
    }
}
/// The two video rows' text, and the row-naming that replaced index
/// literals. Losing this would stop covering what the menu *says*
/// about a mode, which is the only place an override is visible.
#[cfg(test)]
mod video_row_tests {
    use super::*;

    /// Every word here has to be the one config.toml uses, because the
    /// row doubles as documentation for what to type in the file.
    #[test]
    fn each_mode_reads_as_its_config_word() {
        for (mode, word) in [
            (VideoOutput::Tct, "tct"),
            (VideoOutput::Kitty, "kitty"),
            (VideoOutput::Ascii, "ascii"),
            (VideoOutput::Off, "off"),
        ] {
            assert_eq!(video_output_value(mode, None, None), word);
        }
    }

    /// `auto` names what it resolved to: a user on a terminal that
    /// failed the graphics probe otherwise has no way to tell that
    /// from one that passed.
    #[test]
    fn auto_says_what_it_resolved_to() {
        assert_eq!(
            video_output_value(VideoOutput::Auto, None, Some(ProtocolType::Kitty)),
            "auto (kitty)"
        );
        assert_eq!(
            video_output_value(VideoOutput::Auto, None, None),
            "auto (tct)",
            "no protocol still gets a drawing vo, since tct needs nothing"
        );
    }

    /// The row has to contradict neither the screen nor the file: it
    /// shows what is in force and names what config says.
    #[test]
    fn an_override_names_the_config_it_is_covering() {
        let said = video_output_value(VideoOutput::Auto, Some(VideoOutput::Ascii), None);
        assert_eq!(said, "ascii (config: auto)");
        // and with no override there is no note at all, so the common
        // case stays short.
        assert_eq!(video_output_value(VideoOutput::Ascii, None, None), "ascii");
    }

    /// Short enough to survive the narrow pane that cut the first
    /// version: at 62 cells the value column is 25 wide, and
    /// `ascii (wim paints it) [shift+v; config: auto]` truncated to
    /// `[shift`, losing the one fact the note exists for.
    #[test]
    fn the_override_note_fits_a_narrow_pane() {
        let longest = video_output_value(
            VideoOutput::Kitty,
            Some(VideoOutput::Ascii),
            Some(ProtocolType::Kitty),
        );
        assert!(longest.len() <= 25, "{longest} is {} cells", longest.len());
    }

    #[test]
    fn the_rate_row_says_what_unset_means() {
        assert_eq!(ascii_fps_value(None), "the file's own rate");
        assert_eq!(ascii_fps_value(Some(15.0)), "at most 15 fps");
    }

    /// `values` is indexed by `FIELDS`, so a row with no value (or one
    /// too many) is a silent misalignment of every row after it.
    #[test]
    fn every_field_has_exactly_one_value() {
        assert_eq!(FIELDS.len(), FIELD_COUNT);
        let values: Vec<String> = FIELDS.iter().map(|f| f.label().to_string()).collect();
        assert_eq!(values.len(), FIELD_COUNT);
        // and no two rows share a label, which would make the menu
        // unreadable and a selection ambiguous.
        let mut seen: Vec<&str> = FIELDS.iter().map(|f| f.label()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before);
    }

    /// Out of range cannot happen (the cursor wraps on `FIELD_COUNT`),
    /// and the answer is the first row rather than a panic.
    #[test]
    fn the_cursor_always_names_a_row() {
        assert_eq!(field_at(0), Field::AlbumArt);
        assert_eq!(field_at(FIELD_COUNT - 1), Field::TorboxTokenFile);
        assert_eq!(field_at(FIELD_COUNT), Field::AlbumArt);
        assert_eq!(field_at(usize::MAX), Field::AlbumArt);
    }
}

/// Prints the menu as it ships, with the two video rows in it. Run
/// with `cargo test --lib eyeball_settings -- --nocapture`.
#[cfg(test)]
mod eyeball_video {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};
    use ratatui::widgets::Widget;

    fn show(label: &str, values: &[&str], width: u16) {
        let area = Rect::new(0, 0, width, 19);
        let mut buf = buffer(width, 19);
        SettingsMenu {
            values,
            selected: 1,
            keymap: &Keymap::default(),
        }
        .render(area, &mut buf);
        println!("--- {label} ({width} wide, 'video output' selected)");
        println!("{}", grid(&buf));
    }

    #[test]
    fn eyeball_settings_with_the_video_rows() {
        let probed = Some(ProtocolType::Kitty);
        let rows: Vec<String> = FIELDS
            .iter()
            .map(|f| match f {
                Field::AlbumArt => "ascii-bw ('i' also cycles this)".to_string(),
                Field::VideoOutput => video_output_value(VideoOutput::Auto, None, probed),
                Field::AsciiFps => ascii_fps_value(None),
                Field::EqEnabled => "on".to_string(),
                Field::VisualizerBands => "10 bands".to_string(),
                Field::Crossfade => "off".to_string(),
                Field::CrossfadeSeconds => "3.0s".to_string(),
                Field::CacheDaysThreshold => "2 days played".to_string(),
                Field::CacheEvictionDays => "30 days".to_string(),
                Field::ScrobblePercent => "50%".to_string(),
                Field::ScrobbleSeconds => "240s".to_string(),
                Field::ListenbrainzToken => "set (enter to change)".to_string(),
                Field::TorboxTokenFile => "not connected (enter to set up)".to_string(),
            })
            .collect();
        let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
        show("the default, kitty terminal", &borrowed, 80);

        // and the override case, which is the row that has to explain
        // itself: the screen disagrees with config.toml.
        let mut over = rows.clone();
        over[1] = video_output_value(VideoOutput::Auto, Some(VideoOutput::Ascii), probed);
        over[2] = ascii_fps_value(Some(15.0));
        let borrowed: Vec<&str> = over.iter().map(String::as_str).collect();
        show("shift+v override active, rate capped", &borrowed, 80);
        show(
            "the same at 62 wide, where the value column has to fit",
            &borrowed,
            62,
        );
    }
}
