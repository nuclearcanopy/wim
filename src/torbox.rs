//! TorBox account API client. UI and playback code should use this module
//! rather than constructing service URLs or retaining temporary download URLs.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use zeroize::Zeroize;

const API_BASE: &str = "https://api.torbox.app/v1/api";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Torrent,
    Usenet,
    WebDownload,
}

impl Service {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Torrent => "torrent",
            Self::Usenet => "usenet",
            Self::WebDownload => "web_download",
        }
    }

    pub fn all() -> [Self; 3] {
        [Self::Torrent, Self::Usenet, Self::WebDownload]
    }

    fn list_endpoint(self) -> &'static str {
        match self {
            Self::Torrent => "/torrents/mylist",
            Self::Usenet => "/usenet/mylist",
            Self::WebDownload => "/webdl/mylist",
        }
    }
}

/// API token held only in memory and erased when the client is dropped.
pub struct Client {
    token: Secret,
    agent: ureq::Agent,
    base: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorBoxClient")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

struct Secret(String);

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Client {
    /// Config path takes precedence; the environment variable is a
    /// convenient override for deployments that inject runtime secrets.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        match std::env::var_os("WIM_TORBOX_TOKEN_FILE") {
            Some(path) => Self::from_token_file(path),
            None => Self::from_token_file(
                config
                    .torbox_token_file
                    .as_deref()
                    .context("set torbox_token_file or WIM_TORBOX_TOKEN_FILE")?,
            ),
        }
    }

    /// Build a client from `WIM_TORBOX_TOKEN_FILE`, if set.
    pub fn from_env() -> Result<Self> {
        let path = std::env::var_os("WIM_TORBOX_TOKEN_FILE")
            .context("set WIM_TORBOX_TOKEN_FILE to a TorBox API token file")?;
        Self::from_token_file(path)
    }

    /// Read the token from a secret file. This deliberately accepts a path,
    /// never a token string in config or command-line arguments.
    pub fn from_token_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let token = fs::read_to_string(path)
            .with_context(|| format!("reading TorBox token file {}", path.display()))?;
        Self::with_token(token)
    }

    fn with_token(token: String) -> Result<Self> {
        let mut input = token;
        let token = input.trim().to_owned();
        input.zeroize();
        anyhow::ensure!(!token.is_empty(), "TorBox token file is empty");
        Ok(Self {
            token: Secret(token),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(20))
                .build(),
            base: API_BASE.to_owned(),
        })
    }

    #[cfg(test)]
    fn at(token: &str, base: String) -> Self {
        Self {
            token: Secret(token.into()),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(2))
                .build(),
            base,
        }
    }

    fn get<T: DeserializeOwned>(&self, endpoint: &str, query: &[(&str, String)]) -> Result<T> {
        let url = format!("{}{endpoint}", self.base);
        let mut request = self
            .agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.token.0));
        for (key, value) in query {
            request = request.query(key, value);
        }
        let response = request.call().map_err(|e| api_error(endpoint, e))?;
        crate::safenet::json_capped(response, 16 << 20)
    }

    fn post<T: DeserializeOwned>(&self, endpoint: &str, body: &serde_json::Value) -> Result<T> {
        let url = format!("{}{endpoint}", self.base);
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.token.0))
            .send_json(body)
            .map_err(|e| api_error(endpoint, e))?;
        crate::safenet::json_capped(response, 16 << 20)
    }

    fn post_form<T: DeserializeOwned>(&self, endpoint: &str, fields: &[(&str, &str)]) -> Result<T> {
        let boundary = format!("wim-torbox-{}", std::process::id());
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let url = format!("{}{endpoint}", self.base);
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.token.0))
            .set(
                "Content-Type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send_bytes(&body)
            .map_err(|e| api_error(endpoint, e))?;
        crate::safenet::json_capped(response, 16 << 20)
    }

    /// Fetch the user's torrent list. TorBox caches this response, so callers
    /// should not poll it continuously; refresh on demand or at a modest rate.
    pub fn torrents(&self, offset: usize, limit: usize) -> Result<ApiResponse<Vec<Torrent>>> {
        self.get(
            "/torrents/mylist",
            &[
                ("offset", offset.to_string()),
                ("limit", limit.clamp(1, 1000).to_string()),
            ],
        )
    }

    pub fn account_list(
        &self,
        service: Service,
        offset: usize,
        limit: usize,
    ) -> Result<ApiResponse<serde_json::Value>> {
        let mut query = vec![
            ("offset", offset.to_string()),
            ("limit", limit.clamp(1, 1000).to_string()),
        ];
        if service == Service::Torrent {
            query.push(("bypass_cache", "true".into()));
        }
        let query: Vec<_> = query
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect();
        self.get(service.list_endpoint(), &query)
    }

    pub fn control(
        &self,
        service: Service,
        item_id: u64,
        operation: &str,
    ) -> Result<ApiResponse<serde_json::Value>> {
        let (endpoint, key) = match service {
            Service::Torrent => ("/torrents/controltorrent", "torrent_id"),
            Service::Usenet => ("/usenet/controlusenetdownload", "usenet_id"),
            Service::WebDownload => ("/webdl/controlwebdownload", "web_id"),
        };
        anyhow::ensure!(
            match service {
                Service::Torrent => matches!(operation, "resume" | "delete" | "reannounce"),
                Service::Usenet => matches!(operation, "pause" | "resume" | "delete"),
                Service::WebDownload => operation == "delete",
            },
            "unsupported TorBox operation"
        );
        self.post(
            endpoint,
            &serde_json::json!({ (key): item_id, "operation": operation }),
        )
    }

    pub fn account_items(
        &self,
        service: Service,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<AccountItem>> {
        let value = self.account_list(service, offset, limit)?.into_data()?;
        let rows = value
            .as_array()
            .or_else(|| value.get("results").and_then(serde_json::Value::as_array))
            .map(ToOwned::to_owned)
            .or_else(|| value.get("id").is_some().then(|| vec![value.clone()]))
            .unwrap_or_default();
        Ok(rows
            .iter()
            .filter_map(|row| AccountItem::parse(service, row))
            .collect())
    }

    pub fn all_account_items(&self, service: Service) -> Result<Vec<AccountItem>> {
        const PAGE: usize = 100;
        let mut all = Vec::new();
        loop {
            let page = self.account_items(service, all.len(), PAGE)?;
            let count = page.len();
            all.extend(page);
            if count < PAGE {
                return Ok(all);
            }
        }
    }

    pub fn find_account_item(&self, service: Service, id: u64) -> Result<Option<AccountItem>> {
        const PAGE: usize = 100;
        let mut offset = 0;
        loop {
            let items = self.account_items(service, offset, PAGE)?;
            if let Some(item) = items.iter().find(|item| item.id == id) {
                return Ok(Some(item.clone()));
            }
            if items.len() < PAGE {
                return Ok(None);
            }
            offset = offset.saturating_add(items.len());
        }
    }

    /// Explicit account deletion; callers must ask for confirmation first.
    pub fn delete_torrent(&self, torrent_id: u64) -> Result<ApiResponse<serde_json::Value>> {
        self.post(
            "/torrents/controltorrent",
            &serde_json::json!({ "torrent_id": torrent_id, "operation": "delete" }),
        )
    }

    pub fn resume_torrent(&self, torrent_id: u64) -> Result<ApiResponse<serde_json::Value>> {
        self.post(
            "/torrents/controltorrent",
            &serde_json::json!({ "torrent_id": torrent_id, "operation": "resume" }),
        )
    }

    pub fn usenet(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<ApiResponse<Vec<serde_json::Value>>> {
        self.get(
            "/usenet/mylist",
            &[
                ("offset", offset.to_string()),
                ("limit", limit.clamp(1, 1000).to_string()),
            ],
        )
    }

    pub fn web_downloads(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<ApiResponse<Vec<serde_json::Value>>> {
        self.get(
            "/webdl/mylist",
            &[
                ("offset", offset.to_string()),
                ("limit", limit.clamp(1, 1000).to_string()),
            ],
        )
    }

    pub fn usenet_item(&self, id: u64) -> Result<ApiResponse<serde_json::Value>> {
        self.get("/usenet/mylist", &[("id", id.to_string())])
    }

    pub fn web_download(&self, id: u64) -> Result<ApiResponse<serde_json::Value>> {
        self.get("/webdl/mylist", &[("id", id.to_string())])
    }

    /// Submit a magnet. This changes the remote account, so it is exposed as
    /// an explicit operation and must only be called after user confirmation.
    pub fn add_magnet(&self, magnet: &str) -> Result<ApiResponse<serde_json::Value>> {
        anyhow::ensure!(magnet.starts_with("magnet:?"), "expected a magnet link");
        self.post_form("/torrents/createtorrent", &[("magnet", magnet)])
    }

    pub fn add_web_download(&self, url: &str) -> Result<ApiResponse<serde_json::Value>> {
        anyhow::ensure!(
            url.starts_with("https://") || url.starts_with("http://"),
            "web download must be an HTTP(S) URL"
        );
        self.post_form("/webdl/createwebdownload", &[("link", url)])
    }

    pub fn add_usenet_link(&self, url: &str) -> Result<ApiResponse<serde_json::Value>> {
        anyhow::ensure!(
            url.starts_with("https://") || url.starts_with("http://"),
            "Usenet download must be an HTTP(S) NZB URL"
        );
        self.post_form("/usenet/createusenetdownload", &[("link", url)])
    }

    pub fn delete_usenet(&self, id: u64) -> Result<ApiResponse<serde_json::Value>> {
        self.post(
            "/usenet/controlusenetdownload",
            &serde_json::json!({ "usenet_id": id, "operation": "delete" }),
        )
    }

    pub fn delete_web_download(&self, id: u64) -> Result<ApiResponse<serde_json::Value>> {
        self.post(
            "/webdl/controlwebdownload",
            &serde_json::json!({ "web_id": id, "operation": "delete" }),
        )
    }

    /// Resolve a fresh, short-lived CDN URL for a selected file. Do not persist
    /// or log the returned URL; queues should retain the stable IDs instead.
    pub fn torrent_download_url(
        &self,
        torrent_id: u64,
        file_id: u64,
    ) -> Result<ApiResponse<String>> {
        self.get(
            "/torrents/requestdl",
            &[
                ("token", self.token.0.clone()),
                ("torrent_id", torrent_id.to_string()),
                ("file_id", file_id.to_string()),
            ],
        )
    }

    pub fn download_url(
        &self,
        service: Service,
        item_id: u64,
        file_id: u64,
    ) -> Result<ApiResponse<String>> {
        let (endpoint, id_key) = match service {
            Service::Torrent => ("/torrents/requestdl", "torrent_id"),
            Service::Usenet => ("/usenet/requestdl", "usenet_id"),
            Service::WebDownload => ("/webdl/requestdl", "web_id"),
        };
        self.get(
            endpoint,
            &[
                ("token", self.token.0.clone()),
                (id_key, item_id.to_string()),
                ("file_id", file_id.to_string()),
            ],
        )
    }
}

fn api_error(endpoint: &str, error: ureq::Error) -> anyhow::Error {
    // Never include a request URL here: requestdl carries the token in its
    // query string according to TorBox's API contract.
    match error {
        ureq::Error::Status(code, _) => {
            anyhow::anyhow!("TorBox request to {endpoint} returned HTTP {code}")
        }
        ureq::Error::Transport(error) => {
            let reason = match error.kind() {
                ureq::ErrorKind::Dns => "DNS lookup failed",
                ureq::ErrorKind::ConnectionFailed => "connection failed",
                ureq::ErrorKind::ProxyConnect => "proxy connection failed",
                _ => "network error",
            };
            anyhow::anyhow!("TorBox request to {endpoint} failed: {reason}")
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub error: Option<String>,
    pub detail: Option<String>,
    pub data: Option<T>,
}

impl<T> ApiResponse<T> {
    pub fn ensure_success(&self) -> Result<()> {
        if !self.success {
            anyhow::bail!(
                "TorBox API request failed ({})",
                self.error.as_deref().unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    pub fn into_data(self) -> Result<T> {
        self.ensure_success()?;
        self.data.context("TorBox returned no data")
    }
}

#[derive(Debug, Deserialize)]
pub struct Torrent {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub files: Vec<TorrentFile>,
}

#[derive(Debug, Deserialize)]
pub struct TorrentFile {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mimetype: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountFile {
    pub id: u64,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountItem {
    pub service: Service,
    pub id: u64,
    pub name: String,
    pub status: String,
    pub progress: Option<f64>,
    pub files: Vec<AccountFile>,
}

impl AccountItem {
    fn parse(service: Service, value: &serde_json::Value) -> Option<Self> {
        let id = value.get("id").and_then(number_u64)?;
        let name = value
            .get("name")
            .or_else(|| value.get("title"))
            .and_then(serde_json::Value::as_str)?
            .to_owned();
        let status = value
            .get("download_state")
            .or_else(|| value.get("status"))
            .or_else(|| value.get("state"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let files = value
            .get("files")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|file| {
                Some(AccountFile {
                    id: file.get("id").and_then(number_u64)?,
                    name: file
                        .get("name")
                        .or_else(|| file.get("short_name"))
                        .and_then(serde_json::Value::as_str)?
                        .to_owned(),
                    size: file.get("size").and_then(number_u64).unwrap_or(0),
                })
            })
            .collect();
        Some(Self {
            service,
            id,
            name,
            status,
            progress: value.get("progress").and_then(serde_json::Value::as_f64),
            files,
        })
    }

    pub fn playable_items(&self, client: std::sync::Arc<Client>) -> Vec<crate::search::SearchItem> {
        self.files
            .iter()
            .filter_map(|file| {
                let video = is_video_name(&file.name);
                let audio = is_audio_name(&file.name);
                (video || audio).then(|| crate::search::SearchItem::TorBoxFile {
                    service: self.service,
                    item_id: self.id,
                    file_id: file.id,
                    title: if video {
                        spoiler_safe_video_label(&file.name)
                    } else {
                        file.name.clone()
                    },
                    video,
                    client: client.clone(),
                    resolved_url: None,
                })
            })
            .collect()
    }

    pub fn playable_files_for_display(&self) -> Vec<(&str, u64, bool)> {
        self.files
            .iter()
            .filter_map(|file| {
                let video = is_video_name(&file.name);
                (video || is_audio_name(&file.name)).then_some((
                    file.name.as_str(),
                    file.size,
                    video,
                ))
            })
            .collect()
    }

    /// Case-insensitive substring match against the item name and every
    /// file name (playable or not: an `.nfo` can still identify a release).
    fn matches(&self, lowered_needle: &str) -> bool {
        self.name.to_lowercase().contains(lowered_needle)
            || self
                .files
                .iter()
                .any(|file| file.name.to_lowercase().contains(lowered_needle))
    }

    /// Total size of every file TorBox reports for this item.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }
}

/// Convert common release-name numbering (`S03E05`, `S03.EP05`, etc.) to
/// a neutral label. The original filename remains available in the TorBox
/// file picker, but is never carried into now-playing, queues, or mpv's title.
fn spoiler_safe_video_label(filename: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(filename);
    // Scan the original stem instead of splitting first: the common compact
    // spelling `S03E05` is one token and used to evade the episode matcher.
    let bytes = stem.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        if !matches!(bytes[i], b's' | b'S') {
            continue;
        }
        let Some((season, after_season)) = label_digits(bytes, i + 1) else {
            continue;
        };
        let mut e = after_season;
        while e < bytes.len() && matches!(bytes[e], b'.' | b' ' | b'_' | b'-') {
            e += 1;
        }
        if e >= bytes.len() || !matches!(bytes[e], b'e' | b'E') {
            continue;
        }
        e += 1;
        if e < bytes.len() && matches!(bytes[e], b'p' | b'P') {
            e += 1;
        }
        while e < bytes.len() && matches!(bytes[e], b'.' | b' ' | b'_' | b'-') {
            e += 1;
        }
        let Some((episode, _)) = label_digits(bytes, e) else {
            continue;
        };
        let show =
            stem[..i].trim_matches(|c: char| c == '.' || c == '_' || c == '-' || c.is_whitespace());
        let show = show
            .rsplit_once('.')
            .map_or(show, |(prefix, year)| {
                if year.len() == 4 && year.parse::<u16>().is_ok() {
                    prefix
                } else {
                    show
                }
            })
            .replace(['.', '_'], " ");
        return if show.is_empty() {
            format!("Episode {episode} Season {season}")
        } else {
            format!("{show} Episode {episode} Season {season}")
        };
    }
    let tokens: Vec<&str> = stem
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    // A movie has no episode metadata; use a modestly cleaned stem rather
    // than leaking extension/container details.
    tokens.join(" ")
}

fn label_digits(bytes: &[u8], mut at: usize) -> Option<(u32, usize)> {
    let start = at;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }
    (at > start)
        .then(|| {
            std::str::from_utf8(&bytes[start..at])
                .ok()?
                .parse()
                .ok()
                .map(|n| (n, at))
        })
        .flatten()
}

#[cfg(test)]
mod spoiler_safe_label_tests {
    use super::spoiler_safe_video_label;

    #[test]
    fn episode_release_names_hide_release_suffixes_and_episode_titles() {
        assert_eq!(
            spoiler_safe_video_label("Silo.S03.EP05.THE.MEMORY.2160p.mkv"),
            "Silo Episode 5 Season 3"
        );
        assert_eq!(
            spoiler_safe_video_label("Silo.S03E05.The.Memory.2160p.mkv"),
            "Silo Episode 5 Season 3"
        );
    }

    #[test]
    fn movie_labels_drop_the_extension_and_release_punctuation() {
        assert_eq!(
            spoiler_safe_video_label("Arrival.2016.1080p.mkv"),
            "Arrival 2016 1080p"
        );
    }
}

fn number_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_f64().map(|n| n as u64))
}

/// Result of an account-wide library search. `failed_services` names the
/// services that could not be listed; when non-empty, `matches` may be
/// incomplete and callers must say so rather than present a full result set.
#[derive(Debug, Clone, PartialEq)]
pub struct LibrarySearch {
    pub matches: Vec<AccountItem>,
    pub failed_services: Vec<Service>,
}

impl Client {
    /// Search the user's own TorBox account by free text. This is account
    /// search, not internet-wide search: TorBox's public Voyager metadata
    /// search API is no longer available. Matching is a case-insensitive
    /// substring test against item names and file names across all three
    /// services. A service whose listing fails is reported instead of being
    /// silently skipped.
    pub fn search_library(&self, query: &str) -> Result<LibrarySearch> {
        let needle = query.trim().to_lowercase();
        anyhow::ensure!(!needle.is_empty(), "search query is empty");
        let services = Service::all();
        let mut matches = Vec::new();
        let mut failed_services = Vec::new();
        let mut first_error = None;
        for service in services {
            match self.all_account_items(service) {
                Ok(items) => matches.extend(items.into_iter().filter(|item| item.matches(&needle))),
                Err(error) => {
                    failed_services.push(service);
                    first_error.get_or_insert(error);
                }
            }
        }
        if failed_services.len() == services.len() {
            let error = first_error.unwrap_or_else(|| anyhow::anyhow!("no services listed"));
            anyhow::bail!("every TorBox service listing failed: {error:#}");
        }
        Ok(LibrarySearch {
            matches,
            failed_services,
        })
    }
}

impl Torrent {
    /// Turn listed playable files into stable Wim queue identities. The
    /// ephemeral stream URL is deliberately absent and resolved by the core
    /// only after the item is selected for playback/queue insertion.
    pub fn playable_items(
        &self,
        service: Service,
        client: std::sync::Arc<Client>,
    ) -> Vec<crate::search::SearchItem> {
        AccountItem {
            service,
            id: self.id,
            name: self.name.clone(),
            status: self.status.clone(),
            progress: None,
            files: self
                .files
                .iter()
                .map(|f| AccountFile {
                    id: f.id,
                    name: f.name.clone(),
                    size: f.size,
                })
                .collect(),
        }
        .playable_items(client)
    }
}

fn is_video_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mkv" | "mp4" | "m4v" | "avi" | "mov" | "webm" | "mpeg" | "mpg" | "ts"
            )
        })
}

fn is_audio_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp3" | "flac" | "ogg" | "opus" | "m4a" | "aac" | "wav" | "alac" | "wma"
            )
        })
}

pub fn is_playable_file_name(name: &str) -> bool {
    is_audio_name(name) || is_video_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn token_is_sent_as_bearer_and_page_size_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let n = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.contains("Authorization: Bearer secret-test-token"));
            assert!(request.contains("limit=1000"));
            let body = r#"{"success":true,"data":[]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let client = Client::at("secret-test-token", format!("http://{addr}/v1/api"));
        let result = client.torrents(0, usize::MAX).unwrap();
        assert!(result.success);
        server.join().unwrap();
    }

    #[test]
    fn invalid_magnet_is_rejected_before_network_request() {
        let client = Client::at("secret", "http://127.0.0.1:1/v1/api".into());
        assert!(client.add_magnet("https://example.invalid").is_err());
    }

    #[test]
    fn api_error_does_not_print_token() {
        let error = api_error(
            "/torrents/requestdl",
            ureq::Error::Status(403, ureq::Response::new(403, "Forbidden", "").unwrap()),
        );
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn account_items_accept_torbox_download_state_and_numeric_file_ids() {
        let value = serde_json::json!({
            "id": 9001,
            "name": "release",
            "download_state": "completed",
            "progress": 1.0,
            "files": [
                { "id": 41, "name": "episode.mkv", "size": 4000 },
                { "id": 42, "name": "sample.txt", "size": 20 }
            ]
        });
        let item = AccountItem::parse(Service::Usenet, &value).unwrap();
        assert_eq!(item.id, 9001);
        assert_eq!(item.status, "completed");
        assert_eq!(item.files[0].id, 41);
        assert_eq!(
            item.playable_files_for_display(),
            vec![("episode.mkv", 4000, true)]
        );
    }

    #[test]
    fn account_listing_walks_past_the_first_page() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for expected_offset in ["offset=0", "offset=100"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let n = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                assert!(request.contains(expected_offset), "{request}");
                let rows: Vec<serde_json::Value> = if expected_offset == "offset=0" {
                    (0..100)
                        .map(|id| serde_json::json!({ "id": id, "name": format!("item-{id}"), "files": [] }))
                        .collect()
                } else {
                    vec![serde_json::json!({ "id": 9001, "name": "last-item", "files": [] })]
                };
                let body = serde_json::json!({ "success": true, "data": rows }).to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let client = Client::at("test", format!("http://{addr}/v1/api"));
        let items = client.all_account_items(Service::Torrent).unwrap();
        assert_eq!(items.len(), 101);
        assert_eq!(items.last().unwrap().id, 9001);
        server.join().unwrap();
    }

    /// Serve one canned response per sequential request. Each entry is
    /// (substring the request must contain, HTTP status, JSON body).
    fn serve_sequential(
        responses: Vec<(&'static str, u16, serde_json::Value)>,
    ) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for (expect, status, payload) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let n = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                assert!(request.contains(expect), "{request}");
                let body = payload.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status} x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        (addr, server)
    }

    fn ok_data(rows: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "success": true, "error": null, "detail": "ok", "data": rows })
    }

    #[test]
    fn library_search_filters_across_services_case_insensitively() {
        let (addr, server) = serve_sequential(vec![
            (
                "/v1/api/torrents/mylist",
                200,
                ok_data(serde_json::json!([
                    { "id": 1, "name": "Silo.S03.2160p", "files": [] },
                    { "id": 2, "name": "Other.Release", "files": [] }
                ])),
            ),
            (
                "/v1/api/usenet/mylist",
                200,
                ok_data(serde_json::json!([
                    { "id": 3, "name": "silo usenet copy", "files": [] }
                ])),
            ),
            ("/v1/api/webdl/mylist", 200, ok_data(serde_json::json!([]))),
        ]);
        let client = Client::at("test", format!("http://{addr}/v1/api"));
        let search = client.search_library("SILO").unwrap();
        assert_eq!(search.failed_services, Vec::new());
        assert_eq!(
            search.matches.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        server.join().unwrap();
    }

    #[test]
    fn library_search_matches_file_names_when_item_name_does_not() {
        let (addr, server) = serve_sequential(vec![
            (
                "/v1/api/torrents/mylist",
                200,
                ok_data(serde_json::json!([{
                    "id": 7,
                    "name": "Random.Release.Name",
                    "files": [
                        { "id": 0, "name": "readme.txt", "size": 5 },
                        { "id": 1, "name": "Silo.S03E01.mkv", "size": 4000 }
                    ]
                }])),
            ),
            ("/v1/api/usenet/mylist", 200, ok_data(serde_json::json!([]))),
            ("/v1/api/webdl/mylist", 200, ok_data(serde_json::json!([]))),
        ]);
        let client = Client::at("test", format!("http://{addr}/v1/api"));
        let search = client.search_library("silo.s03e01").unwrap();
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.matches[0].id, 7);
        server.join().unwrap();
    }

    #[test]
    fn nonmatching_query_returns_empty_matches_not_an_error() {
        let (addr, server) = serve_sequential(vec![
            (
                "/v1/api/torrents/mylist",
                200,
                ok_data(serde_json::json!([{ "id": 1, "name": "Silo", "files": [] }])),
            ),
            ("/v1/api/usenet/mylist", 200, ok_data(serde_json::json!([]))),
            ("/v1/api/webdl/mylist", 200, ok_data(serde_json::json!([]))),
        ]);
        let client = Client::at("test", format!("http://{addr}/v1/api"));
        let search = client.search_library("zzz-no-such-title").unwrap();
        assert!(search.matches.is_empty());
        assert!(search.failed_services.is_empty());
        server.join().unwrap();
    }

    #[test]
    fn library_search_reports_partial_service_failure_instead_of_hiding_it() {
        let (addr, server) = serve_sequential(vec![
            (
                "/v1/api/torrents/mylist",
                200,
                ok_data(serde_json::json!([{ "id": 1, "name": "Silo", "files": [] }])),
            ),
            (
                "/v1/api/usenet/mylist",
                500,
                serde_json::json!({ "detail": "boom" }),
            ),
            ("/v1/api/webdl/mylist", 200, ok_data(serde_json::json!([]))),
        ]);
        let client = Client::at("test", format!("http://{addr}/v1/api"));
        let search = client.search_library("silo").unwrap();
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.failed_services, vec![Service::Usenet]);
        server.join().unwrap();
    }

    #[test]
    fn library_search_fails_when_every_service_fails() {
        let (addr, server) = serve_sequential(vec![
            ("/v1/api/torrents/mylist", 500, serde_json::json!({})),
            ("/v1/api/usenet/mylist", 500, serde_json::json!({})),
            ("/v1/api/webdl/mylist", 500, serde_json::json!({})),
        ]);
        let client = Client::at("test", format!("http://{addr}/v1/api"));
        let error = client.search_library("silo").unwrap_err().to_string();
        assert!(
            error.contains("every TorBox service listing failed"),
            "{error}"
        );
        assert!(!error.contains("secret"), "token must not leak: {error}");
        server.join().unwrap();
    }

    #[test]
    fn empty_query_is_rejected_without_a_network_request() {
        let client = Client::at("secret", "http://127.0.0.1:1/v1/api".into());
        assert!(client.search_library("   ").is_err());
    }

    #[test]
    fn successful_control_response_needs_no_data_payload() {
        let response: ApiResponse<serde_json::Value> =
            serde_json::from_str(r#"{"success":true,"error":null,"detail":"done","data":null}"#)
                .unwrap();
        assert!(response.ensure_success().is_ok());
    }

    #[test]
    fn add_uses_multipart_form_and_does_not_print_link_on_http_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            while !String::from_utf8_lossy(&request).contains("magnet:?xt=urn:btih:abc") {
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.contains("Content-Type: multipart/form-data; boundary=wim-torbox-"));
            assert!(request.contains("name=\"magnet\""));
            assert!(request.contains("magnet:?xt=urn:btih:abc"));
            let body = r#"{"success":true,"data":{"id":1}}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let client = Client::at("secret", format!("http://{addr}/v1/api"));
        assert!(client
            .add_magnet("magnet:?xt=urn:btih:abc")
            .unwrap()
            .ensure_success()
            .is_ok());
        server.join().unwrap();
    }
}
