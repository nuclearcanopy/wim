//! The prompt's TorBox workspace: the same pages and keys as wim's,
//! driven through the shared `torbox_workspace` state machine.

use crossterm::event::KeyCode;
use wim::config;
use wim::torbox::Service;
use wim::torbox_workspace::{Page, Workspace};

use crate::host::Host;

pub fn open(host: &Host, tb: &mut Workspace) {
    if !tb.is_configured() {
        tb.begin_setup(host.cfg.torbox_token_file.as_deref());
    } else if tb.page == Page::Search {
        tb.begin_search_edit();
    }
}

/// A bracketed paste lands in whichever text box is active.
pub fn paste(tb: &mut Workspace, text: &str) -> bool {
    if let Some(input) = &mut tb.link_input {
        input.push_str(text);
    } else if tb.page == Page::Setup {
        tb.token_path_input.push_str(text);
    } else if tb.page == Page::Search && tb.search_editing {
        tb.query.push_str(text);
    } else {
        return false;
    }
    true
}

/// Per-frame upkeep: replies, and persisting a token path that setup
/// just validated.
pub fn poll(host: &mut Host, tb: &mut Workspace) {
    tb.poll();
    if let Some(path) = tb.take_configured_path() {
        host.cfg.torbox_token_file = Some(path);
        config::save_or_warn(&host.cfg, &mut host.notifications);
    }
    let _ = tb.take_setup_error();
}

pub fn key(host: &mut Host, tb: &mut Workspace, open: &mut bool, code: KeyCode) {
    if tb.link_input.is_some() {
        match code {
            KeyCode::Esc => tb.link_input = None,
            KeyCode::Backspace => {
                if let Some(input) = &mut tb.link_input {
                    input.pop();
                }
            }
            KeyCode::Enter => tb.commit_add(),
            KeyCode::Char(c) => {
                if let Some(input) = &mut tb.link_input {
                    input.push(c);
                }
            }
            _ => {}
        }
        return;
    }
    let editing = tb.page == Page::Search && tb.search_editing;
    match code {
        KeyCode::Char('v') if tb.page != Page::Setup && !editing => {
            tb.spoilers_revealed = !tb.spoilers_revealed
        }
        KeyCode::Char('V') if tb.page != Page::Setup && !editing => {
            host.cfg.watch_reveal_spoilers = !host.cfg.watch_reveal_spoilers;
            tb.spoilers_revealed = host.cfg.watch_reveal_spoilers;
            config::save_or_warn(&host.cfg, &mut host.notifications);
        }
        KeyCode::Esc if tb.page == Page::Setup => tb.page = Page::Search,
        KeyCode::Esc if editing => tb.search_editing = false,
        KeyCode::Esc => *open = false,
        KeyCode::Backspace if tb.page == Page::Setup => {
            tb.token_path_input.pop();
        }
        KeyCode::Char(c) if tb.page == Page::Setup => tb.token_path_input.push(c),
        KeyCode::Enter if tb.page == Page::Setup && !tb.setup_busy => tb.commit_setup(),
        KeyCode::Char(c) if editing => tb.query.push(c),
        KeyCode::Backspace if editing => {
            tb.query.pop();
        }
        KeyCode::Enter if editing => tb.search(),
        KeyCode::Char('/') if tb.page == Page::Search => tb.begin_search_edit(),
        KeyCode::Char('c') => tb.begin_setup(host.cfg.torbox_token_file.as_deref()),
        KeyCode::Char('s') if tb.page != Page::Search => tb.begin_search_edit(),
        KeyCode::Up | KeyCode::Char('k') => tb.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') => tb.move_selection(1),
        KeyCode::Char('b') if tb.page == Page::Search => tb.load_account(tb.service),
        KeyCode::Char('1') => tb.load_account(Service::Torrent),
        KeyCode::Char('2') => tb.load_account(Service::Usenet),
        KeyCode::Char('3') => tb.load_account(Service::WebDownload),
        KeyCode::Char('r') if tb.page == Page::Account => tb.load_account(tb.service),
        KeyCode::Char('n') if tb.page == Page::Account => tb.load_more_account(),
        KeyCode::Char('+') if tb.page == Page::Account => tb.begin_add(),
        KeyCode::Char('d') if tb.page == Page::Account => tb.control_selected("delete"),
        KeyCode::Char('p') if tb.page == Page::Account => {
            let op = tb.account.get(tb.selected).and_then(|item| {
                let paused = item.status.to_ascii_lowercase().contains("paused");
                match item.service {
                    Service::Torrent => paused.then_some("resume"),
                    Service::Usenet => Some(if paused { "resume" } else { "pause" }),
                    Service::WebDownload => None,
                }
            });
            match op {
                Some(op) => tb.control_selected(op),
                None => {
                    tb.message = Some("this service only supports resume or delete here".into())
                }
            }
        }
        KeyCode::Char('R') if tb.page == Page::Account && tb.service == Service::Torrent => {
            tb.control_selected("reannounce")
        }
        KeyCode::Backspace if tb.page == Page::Files => {
            tb.page = if tb.episode_show.is_some() {
                Page::Sources
            } else {
                Page::Account
            };
            tb.selected = 0;
        }
        KeyCode::Backspace if tb.page == Page::Sources => {
            tb.page = Page::Episodes;
            tb.selected = 0;
        }
        KeyCode::Backspace if tb.page == Page::Episodes => {
            tb.page = Page::Search;
            tb.selected = 0;
        }
        KeyCode::Enter if tb.page == Page::Files => {
            if let Some(item) = tb.selected_playable() {
                host.act_on_item(item, false);
                *open = false;
            }
        }
        KeyCode::Char('q') if tb.page == Page::Files => {
            if let Some(item) = tb.selected_playable() {
                host.act_on_item(item, true);
            }
        }
        KeyCode::Enter if tb.page == Page::Account => tb.open_account_item(),
        KeyCode::Enter if tb.page == Page::Search => tb.open_hit(),
        KeyCode::Enter if tb.page == Page::Episodes => tb.open_episode(),
        KeyCode::Enter if tb.page == Page::Sources => tb.open_source(),
        _ => {}
    }
}
