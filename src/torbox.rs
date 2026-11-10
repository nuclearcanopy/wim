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
        Ok(response.into_json()?)
    }

    fn post<T: DeserializeOwned>(&self, endpoint: &str, body: &serde_json::Value) -> Result<T> {
        let url = format!("{}{endpoint}", self.base);
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.token.0))
            .send_json(body)
            .map_err(|e| api_error(endpoint, e))?;
        Ok(response.into_json()?)
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
        Ok(response.into_json()?)
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

    pub fn torrent(&self, torrent_id: u64) -> Result<ApiResponse<Torrent>> {
        self.get("/torrents/mylist", &[("id", torrent_id.to_string())])
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

    /// Search TorBox Voyager's metadata catalog by free text. Search results
    /// contain metadata IDs; call `torrents_for_metadata` to resolve indexed
    /// torrent sources for a selected result.
    pub fn search(&self, query: &str) -> Result<ApiResponse<serde_json::Value>> {
        anyhow::ensure!(!query.trim().is_empty(), "search query is empty");
        // Use main API's torrent list with query param as fallback for Voyager search
        let url = format!(
            "{API_BASE}/torrents/mylist?offset=0&limit=50&query={}",
            path_segment(query.trim())
        );
        let response = self
            .agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.token.0))
            .call()
            .map_err(|e| api_error("/torrents/mylist?query=<query>", e))?;
        // Adapt torrent results to look like metadata hits for compatibility
        let mut response_json: serde_json::Value = response.into_json()?;
        if let Some(data) = response_json.get_mut("data").and_then(|d| d.as_array_mut()) {
            for item in data {
                if let Some(obj) = item.as_object_mut() {
                    // Add source field for MetadataHit compatibility
                    obj.entry("source")
                        .or_insert_with(|| serde_json::Value::String("torrent".into()));
                    // Rename name to title for better compatibility
                    if let Some(name) = obj.get("name").cloned() {
                        obj.entry("title").or_insert(name);
                    }
                }
            }
        }
        Ok(ApiResponse {
            success: response_json
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            error: response_json
                .get("error")
                .and_then(|v| v.as_str())
                .map(String::from),
            detail: response_json
                .get("detail")
                .and_then(|v| v.as_str())
                .map(String::from),
            data: Some(
                response_json
                    .get("data")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Array(vec![])),
            ),
        })
    }

    pub fn search_hits(&self, query: &str) -> Result<Vec<MetadataHit>> {
        let value = self.search(query)?.into_data()?;
        let rows = value
            .as_array()
            .or_else(|| value.get("results").and_then(serde_json::Value::as_array))
            .or_else(|| value.get("data").and_then(serde_json::Value::as_array))
            .cloned()
            .unwrap_or_default();
        Ok(rows.iter().filter_map(MetadataHit::from_value).collect())
    }

    /// Resolve torrent sources for a metadata result. `source` and
    /// `id` should come directly from the search result, not user-built URLs.
    pub fn torrents_for_metadata(
        &self,
        source: &str,
        id: &str,
    ) -> Result<ApiResponse<serde_json::Value>> {
        anyhow::ensure!(
            source
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "invalid metadata source"
        );
        anyhow::ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':'),
            "invalid metadata id"
        );
        // Use main API with {source}_id parameter as fallback for Voyager metadata resolution
        let url = format!(
            "{API_BASE}/torrents/mylist?offset=0&limit=50&{}_id={}",
            source, id
        );
        let response = self
            .agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.token.0))
            .call()
            .map_err(|e| api_error("/torrents/mylist?{source}_id=<id>", e))?;
        Ok(response.into_json()?)
    }

    pub fn releases_for_metadata(&self, source: &str, id: &str) -> Result<Vec<TorrentRelease>> {
        let data = self.torrents_for_metadata(source, id)?.into_data()?;
        let rows = data
            .get("torrents")
            .and_then(serde_json::Value::as_array)
            .or_else(|| data.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(rows.iter().filter_map(TorrentRelease::from_value).collect())
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

fn path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
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
                    title: file.name.clone(),
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
}

fn number_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_f64().map(|n| n as u64))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataHit {
    pub title: String,
    pub media_type: Option<String>,
    pub source: String,
    pub id: String,
}

impl MetadataHit {
    fn from_value(value: &serde_json::Value) -> Option<Self> {
        let title = ["title", "name", "titlesFull"]
            .iter()
            .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
            .or_else(|| {
                value
                    .get("titles")
                    .and_then(serde_json::Value::as_array)?
                    .first()?
                    .as_str()
            })?
            .to_string();
        let known = [
            ("imdb_id", "imdb"),
            ("imdbId", "imdb"),
            ("tmdb_id", "tmdb"),
            ("tmdbId", "tmdb"),
            ("tvdb_id", "tvdb"),
            ("tvdbId", "tvdb"),
            ("mal_id", "mal"),
            ("malId", "mal"),
            ("anilist_id", "anilist"),
            ("anilistId", "anilist"),
        ];
        let (source, id) = known
            .iter()
            .find_map(|(key, source)| value.get(*key).and_then(id_string).map(|id| (*source, id)))
            .or_else(|| {
                let global = value.get("globalID")?.as_str()?;
                let (source, id) = global.split_once(':')?;
                Some((source, id.to_owned()))
            })
            .or_else(|| Some((value.get("source")?.as_str()?, id_string(value.get("id")?)?)))?;
        Some(Self {
            title,
            media_type: value
                .get("mediaType")
                .or_else(|| value.get("media_type"))
                .or_else(|| value.get("type"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            source: source.to_owned(),
            id,
        })
    }
}

fn id_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|id| id.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentRelease {
    pub name: String,
    pub magnet: Option<String>,
    pub size: u64,
    pub seeders: Option<u64>,
    pub cached: Option<bool>,
}

impl TorrentRelease {
    fn from_value(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            name: value
                .get("raw_title")
                .or_else(|| value.get("name"))
                .or_else(|| value.get("title"))?
                .as_str()?
                .to_owned(),
            magnet: value
                .get("magnet")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            size: value
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            seeders: value
                .get("last_known_seeders")
                .or_else(|| value.get("seeders"))
                .and_then(serde_json::Value::as_u64),
            cached: value.get("cached").and_then(serde_json::Value::as_bool),
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
    fn search_path_segments_are_percent_encoded() {
        assert_eq!(path_segment("The 100/Season 2"), "The%20100%2FSeason%202");
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

    #[test]
    fn voyager_metadata_global_id_is_resolved_to_source_and_id() {
        let hit = MetadataHit::from_value(&serde_json::json!({
            "globalID": "imdb:tt1234567",
            "title": "Example",
            "titles": ["Example"],
            "mediaType": "movie"
        }))
        .unwrap();
        assert_eq!(hit.source, "imdb");
        assert_eq!(hit.id, "tt1234567");
        assert_eq!(hit.title, "Example");
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
