//! The prompt's Navidrome profile manager: list, reconnect, remove
//! (armed by a second `d`) and add, using the same catalog helpers as
//! wim's manager so credentials and sync behave identically.

use crossterm::event::KeyCode;
use wim::catalog::{
    edit_servers, index_navidrome_server, is_authentication_message, test_navidrome_connectivity,
};
use wim::config;
use wim::navmanager::{ConnectionStatus, NavManager, NavManagerStep};
use wim::notify::Severity;
use wim::ui::login::LoginForm;

use crate::host::Host;

pub fn open(host: &mut Host) -> NavManager {
    let mut nm = NavManager::new();
    nm.connection_status = test_navidrome_connectivity(
        &host.cfg.navidrome_servers,
        &host.app.corpus.remote_clients(),
    );
    for (nav, status) in host.cfg.navidrome_servers.iter().zip(&nm.connection_status) {
        match status {
            Some(ConnectionStatus::Unreachable) => host
                .notifications
                .push(Severity::Alert, format!("{} is unreachable", nav.url)),
            Some(ConnectionStatus::ConfigurationIssue) => host.notifications.push(
                Severity::Alert,
                format!("{} has a configuration/credential problem", nav.url),
            ),
            Some(ConnectionStatus::Online) | None => {}
        }
    }
    nm
}

pub fn paste(nm: &mut NavManager, text: &str) -> bool {
    if let NavManagerStep::Add(form) = &mut nm.step {
        form.current_mut().push_str(text);
        true
    } else {
        false
    }
}

/// Returns `false` once the manager should close.
pub fn key(host: &mut Host, nm: &mut NavManager, code: KeyCode) -> bool {
    match &mut nm.step {
        NavManagerStep::List => {
            let count = host.cfg.navidrome_servers.len();
            if code != KeyCode::Char('d') {
                nm.pending_delete = None;
            }
            match code {
                KeyCode::Esc => return false,
                KeyCode::Char('j') | KeyCode::Down => nm.selected = (nm.selected + 1).min(count),
                KeyCode::Char('k') | KeyCode::Up => nm.selected = nm.selected.saturating_sub(1),
                KeyCode::Char('d') if nm.selected < count => {
                    if nm.pending_delete == Some(nm.selected) {
                        let at = nm.selected;
                        edit_servers(&mut host.cfg, &host.servers_tx, |servers| {
                            servers.remove(at);
                        });
                        nm.forget_status_at(at);
                        host.app.syncing_servers = !host.cfg.navidrome_servers.is_empty();
                        config::save_or_warn(&host.cfg, &mut host.notifications);
                        nm.selected = nm.selected.min(host.cfg.navidrome_servers.len());
                        nm.pending_delete = None;
                        nm.message =
                            "profile removed (already-indexed items stay until restart)".into();
                    } else {
                        nm.pending_delete = Some(nm.selected);
                        nm.message = format!(
                            "press d again to remove {}",
                            host.cfg.navidrome_servers[nm.selected].url
                        );
                    }
                }
                KeyCode::Enter if nm.selected == count => {
                    nm.step = NavManagerStep::Add(LoginForm::new());
                }
                KeyCode::Enter => {
                    let nav = host.cfg.navidrome_servers[nm.selected].clone();
                    let result = index_navidrome_server(&mut host.app, &nav);
                    if nm.selected >= nm.connection_status.len() {
                        nm.connection_status.resize(nm.selected + 1, None);
                    }
                    nm.connection_status[nm.selected] = Some(match &result {
                        Ok(_) => ConnectionStatus::Online,
                        Err(message) if is_authentication_message(message) => {
                            ConnectionStatus::ConfigurationIssue
                        }
                        Err(_) => ConnectionStatus::Unreachable,
                    });
                    nm.message = match result {
                        Ok(message) => format!("reconnected: {message}"),
                        Err(message) => {
                            host.notifications
                                .push(Severity::Error, format!("{}: {message}", nav.url));
                            message
                        }
                    };
                }
                _ => {}
            }
        }
        NavManagerStep::Add(form) => match code {
            KeyCode::Esc => {
                nm.step = NavManagerStep::List;
                nm.message.clear();
            }
            KeyCode::Char(c) => form.current_mut().push(c),
            KeyCode::Backspace => {
                if form.current_mut().is_empty() {
                    form.prev_field();
                } else {
                    form.current_mut().pop();
                }
            }
            KeyCode::Up => form.prev_field(),
            KeyCode::Down => form.next_field_clamped(),
            KeyCode::Tab => form.next_field(),
            KeyCode::Enter if !form.is_last_field() => form.next_field(),
            KeyCode::Enter => {
                let nav = config::NavidromeConfig {
                    url: form.url.clone(),
                    username: form.username.clone(),
                    password: Some(form.password.clone()),
                    password_file: None,
                    max_bitrate_kbps: None,
                    transcode_format: None,
                };
                match index_navidrome_server(&mut host.app, &nav) {
                    Ok(message) => {
                        host.notifications
                            .push(Severity::Info, format!("connected: {}", nav.url));
                        edit_servers(&mut host.cfg, &host.servers_tx, |servers| servers.push(nav));
                        config::save_or_warn(&host.cfg, &mut host.notifications);
                        let path = config::config_path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "config.toml".to_string());
                        host.notifications.push(
                            Severity::Alert,
                            format!(
                                "password saved in cleartext (0600) at {path}; use password_file for a secret manager"
                            ),
                        );
                        nm.connection_status.push(Some(ConnectionStatus::Online));
                        nm.selected = host.cfg.navidrome_servers.len() - 1;
                        nm.step = NavManagerStep::List;
                        nm.message = format!("connected: {message}");
                    }
                    Err(message) => {
                        host.notifications
                            .push(Severity::Error, format!("{}: {message}", nav.url));
                        nm.message = format!("{message} (esc to go back)");
                    }
                }
            }
            _ => {}
        },
    }
    true
}
