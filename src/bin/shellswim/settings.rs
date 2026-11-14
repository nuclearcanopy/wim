//! The prompt's settings workspace: the same rows and limits as wim's
//! settings menu, minus the two that have nothing to act on here (album
//! art, TorBox setup), which are shown as unavailable instead of hidden
//! so the shared widget keeps its row order.

use crossterm::event::KeyCode;
use wim::config;
use wim::eq;
use wim::notify::Severity;
use wim::ui::settings_menu::{field_at, Field, FIELDS, FIELD_COUNT};

use crate::host::Host;

#[derive(Default)]
pub struct SettingsState {
    pub open: bool,
    pub selected: usize,
    token_editing: bool,
    token_input: String,
    rebuild_pending: bool,
}

pub fn values(host: &Host, state: &SettingsState) -> Vec<String> {
    let cfg = &host.cfg;
    FIELDS
        .iter()
        .map(|field| match field {
            Field::AlbumArt => "not available in shellswim".to_string(),
            Field::EqEnabled => on_off(cfg.eq_enabled),
            Field::VisualizerBands => format!("{} bands", cfg.visualizer_bands),
            Field::Crossfade => on_off(host.crossfade_enabled),
            Field::CrossfadeSeconds => format!("{:.1}s", cfg.crossfade_seconds),
            Field::CacheDaysThreshold => format!("{} days played", cfg.cache_days_threshold),
            Field::CacheEvictionDays => format!("{} days", cfg.cache_eviction_days),
            Field::OfflineCacheBudget => {
                format!("{} GB", cfg.offline_cache_max_bytes / 1_000_000_000)
            }
            Field::OfflineDownloadBitrate => {
                let effective = cfg.effective_offline_download_bitrate_kbps();
                if effective == cfg.offline_download_bitrate_kbps {
                    format!("{effective} kbps")
                } else {
                    format!("{effective} kbps (budget cap)")
                }
            }
            Field::OfflineDownloadFormat => format!(
                "{} ({})",
                cfg.offline_download_format,
                if cfg.offline_download_format == "flac" {
                    "lossless"
                } else {
                    "lossy"
                }
            ),
            Field::RebuildDownloads => {
                let count = downloaded_count(host);
                if state.rebuild_pending {
                    format!("confirm · {count} songs · enter again")
                } else {
                    format!("{count} songs · enter to rebuild")
                }
            }
            Field::ScrobblePercent => format!("{:.0}%", cfg.scrobble.threshold_percent),
            Field::ScrobbleSeconds => format!("{:.0}s", cfg.scrobble.threshold_seconds),
            Field::ListenbrainzToken => {
                if state.token_editing {
                    format!(
                        "{} (enter to save, esc to cancel)",
                        "*".repeat(state.token_input.chars().count())
                    )
                } else if cfg.listenbrainz_user_token.is_some()
                    || cfg.listenbrainz_user_token_file.is_some()
                {
                    "set (enter to change)".to_string()
                } else {
                    "not set (enter to add)".to_string()
                }
            }
            Field::TorboxTokenFile => "set up in wim".to_string(),
        })
        .collect()
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

fn downloaded_count(host: &Host) -> usize {
    host.cache_index
        .lock()
        .ok()
        .map(|index| index.download_manifest().count())
        .unwrap_or(0)
}

fn save(host: &mut Host) {
    config::save_or_warn(&host.cfg, &mut host.notifications);
}

fn warn_low_quality(host: &mut Host) {
    if host.cfg.offline_download_format != "flac"
        && host.cfg.effective_offline_download_bitrate_kbps() <= 96
    {
        let kbps = host.cfg.effective_offline_download_bitrate_kbps();
        host.notifications.push(
            Severity::Alert,
            format!("low audio quality: offline downloads at {kbps} kbps"),
        );
    }
}

fn set_filter(host: &mut Host, enabled: bool, bands: u8) -> bool {
    match host.player.set_af(&eq::build_player_af_filter(
        &host.app.eq_gains,
        enabled,
        bands as usize,
    )) {
        Ok(()) => true,
        Err(error) => {
            host.notifications
                .push(Severity::Error, format!("error setting eq: {error}"));
            false
        }
    }
}

/// One h/l step (`forward` = l/right) on `field`.
fn adjust(host: &mut Host, field: Field, forward: bool) {
    let sign = if forward { 1.0 } else { -1.0 };
    match field {
        Field::EqEnabled => {
            if forward != host.cfg.eq_enabled
                && set_filter(host, forward, host.cfg.visualizer_bands)
            {
                host.cfg.eq_enabled = forward;
                save(host);
            }
        }
        Field::VisualizerBands => {
            let bands = if forward {
                (host.cfg.visualizer_bands + 1).min(32)
            } else {
                host.cfg.visualizer_bands.saturating_sub(1).max(4)
            };
            if set_filter(host, host.cfg.eq_enabled, bands) {
                host.cfg.visualizer_bands = bands;
                save(host);
            }
        }
        Field::Crossfade => {
            host.crossfade_enabled = forward;
            if !forward {
                host.fade_in_started_at = None;
                let _ = host.player.set_volume(host.base_volume);
            }
        }
        Field::CrossfadeSeconds => {
            host.cfg.crossfade_seconds =
                config::crossfade_seconds(host.cfg.crossfade_seconds + sign * 0.5);
        }
        Field::CacheDaysThreshold => {
            host.cfg.cache_days_threshold = if forward {
                (host.cfg.cache_days_threshold + 1).min(20)
            } else {
                host.cfg.cache_days_threshold.saturating_sub(1).max(1)
            };
        }
        Field::CacheEvictionDays => {
            host.cfg.cache_eviction_days = if forward {
                (host.cfg.cache_eviction_days + 1).min(90)
            } else {
                host.cfg.cache_eviction_days.saturating_sub(1).max(1)
            };
        }
        Field::OfflineCacheBudget => {
            host.cfg.offline_cache_max_bytes = if forward {
                (host.cfg.offline_cache_max_bytes + 1_000_000_000).min(100_000_000_000)
            } else {
                host.cfg
                    .offline_cache_max_bytes
                    .saturating_sub(1_000_000_000)
                    .max(1_000_000_000)
            };
            wim::cache::initialize_budget(host.cfg.offline_cache_max_bytes);
            warn_low_quality(host);
        }
        Field::OfflineDownloadBitrate => {
            host.cfg.offline_download_bitrate_kbps =
                next_bitrate(host.cfg.offline_download_bitrate_kbps, forward);
            warn_low_quality(host);
        }
        Field::OfflineDownloadFormat => {
            host.cfg.offline_download_format =
                next_format(&host.cfg.offline_download_format, forward).into();
            warn_low_quality(host);
        }
        Field::ScrobblePercent => {
            host.cfg.scrobble.threshold_percent =
                (host.cfg.scrobble.threshold_percent + sign * 5.0).clamp(5.0, 100.0);
        }
        Field::ScrobbleSeconds => {
            host.cfg.scrobble.threshold_seconds =
                (host.cfg.scrobble.threshold_seconds + sign * 15.0).clamp(15.0, 600.0);
        }
        Field::AlbumArt
        | Field::RebuildDownloads
        | Field::ListenbrainzToken
        | Field::TorboxTokenFile => {}
    }
}

const BITRATES: [u32; 6] = [64, 96, 128, 192, 256, 320];

fn next_bitrate(current: u32, forward: bool) -> u32 {
    let at = BITRATES
        .iter()
        .position(|b| *b >= current)
        .unwrap_or(BITRATES.len() - 1);
    let at = if forward {
        if BITRATES[at] > current {
            at
        } else {
            at + 1
        }
    } else {
        at.saturating_sub(1)
    };
    BITRATES[at.min(BITRATES.len() - 1)]
}

const FORMATS: [&str; 4] = ["opus", "aac", "mp3", "flac"];

fn next_format(current: &str, forward: bool) -> &'static str {
    let at = FORMATS.iter().position(|f| *f == current).unwrap_or(0);
    let next = if forward {
        (at + 1) % FORMATS.len()
    } else {
        (at + FORMATS.len() - 1) % FORMATS.len()
    };
    FORMATS[next]
}

fn rebuild(host: &mut Host) {
    match wim::core::rebuild_cached_downloads(
        &host.cache_index,
        &host.cfg,
        &mut host.downloading,
        &host.download_tx,
        &mut host.notifications,
    ) {
        Ok(result) => {
            warn_low_quality(host);
            let mut status = format!("rebuild started for {} song(s)", result.started);
            if result.already_running > 0 {
                status.push_str(&format!(", {} already running", result.already_running));
            }
            if result.unavailable_servers > 0 {
                status.push_str(&format!(
                    ", {} server(s) unavailable",
                    result.unavailable_servers
                ));
            }
            host.notifications.push(
                if result.unavailable_servers > 0 {
                    Severity::Error
                } else {
                    Severity::Alert
                },
                status,
            );
        }
        Err(error) => host.notifications.push(Severity::Error, error),
    }
}

/// One key while the workspace is open; `close_key` is the configured
/// key that also dismisses it.
pub fn key(host: &mut Host, state: &mut SettingsState, code: KeyCode, close_key: KeyCode) {
    if state.token_editing {
        match code {
            KeyCode::Char(c) => state.token_input.push(c),
            KeyCode::Backspace => {
                state.token_input.pop();
            }
            KeyCode::Esc => {
                state.token_editing = false;
                state.token_input.clear();
            }
            KeyCode::Enter => {
                let token = state.token_input.trim().to_string();
                host.cfg.listenbrainz_user_token =
                    if token.is_empty() { None } else { Some(token) };
                save(host);
                state.token_editing = false;
                state.token_input.clear();
            }
            _ => {}
        }
        return;
    }
    let field = field_at(state.selected);
    if state.rebuild_pending && !(code == KeyCode::Enter && field == Field::RebuildDownloads) {
        state.rebuild_pending = false;
    }
    if code == KeyCode::Esc || code == close_key {
        state.open = false;
        host.cfg.crossfade_enabled = host.crossfade_enabled;
        save(host);
        return;
    }
    match code {
        KeyCode::Char('j') | KeyCode::Down => state.selected = (state.selected + 1) % FIELD_COUNT,
        KeyCode::Char('k') | KeyCode::Up => {
            state.selected = (state.selected + FIELD_COUNT - 1) % FIELD_COUNT
        }
        KeyCode::Char('h') | KeyCode::Left => adjust(host, field, false),
        KeyCode::Char('l') | KeyCode::Right => adjust(host, field, true),
        KeyCode::Enter => match field {
            Field::RebuildDownloads if state.rebuild_pending => {
                state.rebuild_pending = false;
                rebuild(host);
            }
            Field::RebuildDownloads => {
                state.rebuild_pending = true;
                let count = downloaded_count(host);
                host.notifications.push(
                    Severity::Alert,
                    format!(
                        "this will re-download up to {count} saved song(s); existing copies remain until replacements succeed. enter again to confirm"
                    ),
                );
            }
            Field::ListenbrainzToken => {
                state.token_editing = true;
                state.token_input.clear();
            }
            _ => {}
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_steps_through_the_same_ladder_both_ways() {
        assert_eq!(next_bitrate(128, true), 192);
        assert_eq!(next_bitrate(128, false), 96);
        assert_eq!(next_bitrate(64, false), 64);
        assert_eq!(next_bitrate(320, true), 320);
    }

    #[test]
    fn format_cycles_in_both_directions() {
        assert_eq!(next_format("opus", true), "aac");
        assert_eq!(next_format("opus", false), "flac");
        assert_eq!(next_format("flac", true), "opus");
    }
}
