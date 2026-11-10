//! Async state machine for online metadata browsing. It is intentionally
//! independent of TorBox credentials and the local/Navidrome music corpus.

use md5::{Digest, Md5};
use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;

use crate::media::{
    seasons_from_episodes, Episode, MediaItem, PlaybackIdentity, Season, Source, SpoilerPolicy,
};
use crate::stremio::{resolve_external_stream_url, AddonClient, Manifest, Stream};

const SEARCH_RESULT_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Search,
    Detail,
    Episodes,
    Sources,
}

enum Reply {
    Configured(u64, Result<(Arc<AddonClient>, Manifest), String>),
    Search(u64, Result<Vec<MediaItem>, String>),
    Detail(u64, Result<(MediaItem, Vec<Episode>), String>),
    Sources(u64, Result<Vec<Stream>, String>),
    SourceResolved(u64, Result<PreparedSource, String>),
}

/// Ephemeral source credentials live here only while the selection is active.
/// Debug output deliberately omits the signed URL.
pub struct PreparedSource {
    pub identity: PlaybackIdentity,
    pub source: Source,
    pub addresses: Vec<std::net::SocketAddr>,
    _ephemeral_url: String,
}

impl std::fmt::Debug for PreparedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSource")
            .field("transport", &self.source.transport)
            .field("addresses", &self.addresses)
            .field("url", &"[ephemeral URL redacted]")
            .finish()
    }
}

impl PreparedSource {
    pub fn into_search_item(self) -> crate::search::SearchItem {
        crate::search::SearchItem::WatchSource {
            identity: self.identity,
            title: self.source.label,
            url: crate::search::EphemeralUrl::new(self._ephemeral_url),
        }
    }
}

pub struct Workspace {
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    generation: u64,
    client: Option<Arc<AddonClient>>,
    manifest: Option<Manifest>,
    pub page: Page,
    pub query: String,
    pub results: Vec<MediaItem>,
    pub detail: Option<MediaItem>,
    pub seasons: Vec<Season>,
    pub season_selected: usize,
    pub sources: Vec<Stream>,
    pub prepared_source: Option<PreparedSource>,
    pub selected: usize,
    pub loading: bool,
    pub message: Option<String>,
    pub search_editing: bool,
    pub spoilers_revealed: bool,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            generation: 0,
            client: None,
            manifest: None,
            page: Page::Search,
            query: String::new(),
            results: Vec::new(),
            detail: None,
            seasons: Vec::new(),
            season_selected: 0,
            sources: Vec::new(),
            prepared_source: None,
            selected: 0,
            loading: false,
            message: None,
            search_editing: true,
            spoilers_revealed: false,
        }
    }

    pub fn configure(&mut self, url: String) {
        if self.loading {
            self.message = Some("provider request already in progress".into());
            return;
        }
        let (generation, tx) = self.next_request();
        self.loading = true;
        self.message = Some("connecting to the configured metadata provider…".into());
        thread::spawn(move || {
            let result = AddonClient::new(&url)
                .and_then(|client| {
                    client
                        .manifest()
                        .map(|manifest| (Arc::new(client), manifest))
                })
                .map_err(|_| {
                    "provider unavailable or manifest invalid; check its URL and network".to_owned()
                });
            let _ = tx.send(Reply::Configured(generation, result));
        });
    }

    pub fn is_configured(&self) -> bool {
        self.client.is_some() && self.manifest.is_some()
    }

    pub fn poll(&mut self) {
        loop {
            let reply = match self.rx.try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            let reply_generation = match &reply {
                Reply::Configured(generation, _)
                | Reply::Search(generation, _)
                | Reply::Detail(generation, _)
                | Reply::Sources(generation, _)
                | Reply::SourceResolved(generation, _) => *generation,
            };
            if reply_generation != self.generation {
                self.loading = false;
                continue;
            }
            match reply {
                Reply::Configured(generation, result) if generation == self.generation => {
                    self.loading = false;
                    match result {
                        Ok((client, manifest)) => {
                            self.client = Some(client);
                            self.manifest = Some(manifest);
                            self.message = Some(
                                "provider connected; it can see your searches and IP address"
                                    .into(),
                            );
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
                Reply::Search(generation, result) if generation == self.generation => {
                    self.loading = false;
                    match result {
                        Ok(results) => {
                            self.results = results;
                            self.selected = 0;
                            self.search_editing = false;
                            self.message = if self.results.is_empty() {
                                Some("no movie or series results".into())
                            } else {
                                None
                            };
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
                Reply::Detail(generation, result) if generation == self.generation => {
                    self.loading = false;
                    match result {
                        Ok((item, episodes)) => {
                            self.detail = Some(item.clone());
                            if item.kind == crate::media::MediaKind::Series {
                                self.seasons = seasons_from_episodes(episodes);
                                self.season_selected = 0;
                                self.page = Page::Detail;
                                self.message = if self.seasons.is_empty() {
                                    Some("series metadata has no episode list".into())
                                } else {
                                    None
                                };
                            } else {
                                self.page = Page::Detail;
                                self.message = None;
                            }
                            self.selected = 0;
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
                Reply::Sources(generation, result) if generation == self.generation => {
                    self.loading = false;
                    match result {
                        Ok(sources) => {
                            self.sources = sources;
                            self.selected = 0;
                            self.page = Page::Sources;
                            self.message = if self.sources.is_empty() {
                                Some("provider returned no source candidates".into())
                            } else {
                                None
                            };
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
                Reply::SourceResolved(generation, result) if generation == self.generation => {
                    self.loading = false;
                    match result {
                        Ok(source) => {
                            let host = source.addresses.first().map_or_else(
                                || "address unavailable".to_owned(),
                                |address| address.ip().to_string(),
                            );
                            self.message = Some(format!(
                                "source validated to public address {host}; press enter again to play through the pinned relay"
                            ));
                            self.prepared_source = Some(source);
                        }
                        Err(error) => {
                            self.prepared_source = None;
                            self.message = Some(error);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn search(&mut self) {
        if self.loading {
            self.message = Some("provider request already in progress".into());
            return;
        }
        let query = self.query.trim().to_owned();
        if query.is_empty() {
            self.message = Some("enter a movie or series title".into());
            return;
        }
        let (Some(client), Some(manifest)) = (self.client.clone(), self.manifest.clone()) else {
            self.message = Some("configure a metadata provider before searching".into());
            return;
        };
        let (generation, tx) = self.next_request();
        self.loading = true;
        self.message = Some("searching configured catalogs…".into());
        thread::spawn(move || {
            let mut results = Vec::new();
            let mut seen = HashSet::new();
            let mut had_catalog = false;
            let mut successful_catalogs = 0usize;
            for catalog in &manifest.catalogs {
                if !matches!(catalog.media_type.as_str(), "movie" | "series")
                    || !manifest.supports("catalog", &catalog.media_type, None)
                {
                    continue;
                }
                had_catalog = true;
                match client.search(&manifest, &catalog.media_type, &catalog.id, &query) {
                    Ok(response) => {
                        successful_catalogs += 1;
                        for preview in response.metas {
                            if let Some(item) = preview.media_item() {
                                if seen.insert(item.id.clone()) {
                                    results.push(item);
                                    if results.len() >= SEARCH_RESULT_LIMIT {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
                if results.len() >= SEARCH_RESULT_LIMIT {
                    break;
                }
            }
            let result = if !had_catalog {
                Err("provider declares no movie or series catalogs".into())
            } else if successful_catalogs == 0 {
                Err("all configured catalog searches failed".into())
            } else {
                Ok(results)
            };
            let _ = tx.send(Reply::Search(generation, result));
        });
    }

    pub fn open_selected(&mut self) {
        if self.loading {
            return;
        }
        let Some(item) = self.results.get(self.selected).cloned() else {
            return;
        };
        self.load_detail(item);
    }

    fn load_detail(&mut self, item: MediaItem) {
        if self.loading {
            return;
        }
        let (Some(client), Some(manifest)) = (self.client.clone(), self.manifest.clone()) else {
            return;
        };
        let media_type = match item.kind {
            crate::media::MediaKind::Movie => "movie",
            crate::media::MediaKind::Series => "series",
        };
        let id = item.id.clone();
        let (generation, tx) = self.next_request();
        self.loading = true;
        self.message = Some("loading metadata…".into());
        thread::spawn(move || {
            let result = client
                .metadata(&manifest, media_type, &id)
                .map_err(|_| "provider metadata request failed".to_owned())
                .and_then(|response| {
                    let item = response
                        .meta
                        .media_item()
                        .ok_or_else(|| "provider returned an unsupported media type".to_owned())?;
                    Ok((item, response.meta.episodes()))
                });
            let _ = tx.send(Reply::Detail(generation, result));
        });
    }

    pub fn selected_episode(&self) -> Option<&Episode> {
        self.seasons
            .get(self.season_selected)?
            .episodes
            .get(self.selected)
    }

    pub fn load_selected_sources(&mut self) {
        if self.loading {
            self.message = Some("provider request already in progress".into());
            return;
        }
        let Some(item) = self.detail.clone() else {
            return;
        };
        let id = if item.kind == crate::media::MediaKind::Series {
            let Some(episode) = self.selected_episode() else {
                self.message = Some("select an episode first".into());
                return;
            };
            episode.id.clone()
        } else {
            item.id.clone()
        };
        let media_type = match item.kind {
            crate::media::MediaKind::Movie => "movie",
            crate::media::MediaKind::Series => "series",
        };
        let (Some(client), Some(manifest)) = (self.client.clone(), self.manifest.clone()) else {
            return;
        };
        let (generation, tx) = self.next_request();
        self.loading = true;
        self.message = Some("checking configured source providers…".into());
        self.prepared_source = None;
        thread::spawn(move || {
            let result = client
                .streams(&manifest, media_type, &id)
                .map(|response| response.streams)
                .map_err(|_| "provider source request failed".to_owned());
            let _ = tx.send(Reply::Sources(generation, result));
        });
    }

    pub fn enter_episodes(&mut self) {
        if self
            .detail
            .as_ref()
            .is_some_and(|item| item.kind == crate::media::MediaKind::Series)
        {
            self.page = Page::Episodes;
            self.selected = 0;
            self.message = None;
        }
    }

    /// Resolve the selected direct candidate off-thread and reject any host
    /// whose complete DNS answer set contains a non-public address. This is
    /// deliberately not a playback handoff: mpv's normal URL loader performs
    /// its own DNS lookup, so a pinned transport is required before launch.
    pub fn prepare_selected_source(&mut self) {
        if self.loading {
            return;
        }
        let Some(stream) = self.sources.get(self.selected).cloned() else {
            return;
        };
        let Some(item) = self.detail.as_ref() else {
            return;
        };
        let raw_url = match stream.url.as_deref() {
            Some(url) => url.to_owned(),
            None => {
                self.message = Some("this source requires a transport Wim does not support".into());
                return;
            }
        };
        let kind = item.kind.clone();
        let media_id = item.id.clone();
        let media_title = item.title.clone();
        let episode_id = (kind == crate::media::MediaKind::Series)
            .then(|| self.selected_episode().map(|episode| episode.id.clone()))
            .flatten();
        let Some(manifest) = self.manifest.as_ref() else {
            return;
        };
        let provider = manifest.id.clone();
        let descriptor = stream
            .title
            .as_ref()
            .or(stream.name.as_ref())
            .cloned()
            .unwrap_or_else(|| format!("Source {}", self.selected + 1));
        let source_label = if self.spoilers_revealed {
            redact_source_label(&descriptor)
        } else {
            format!("Source {}", self.selected + 1)
        };
        let label = format!("{media_title} · {source_label}");
        // Add-on stream arrays have no stable direct-stream ID. Hash only
        // descriptive metadata (never the URL/signature), and scope it to the
        // movie or episode identity.
        let source_id = stable_source_id(&provider, &media_id, episode_id.as_deref(), &descriptor);
        let stable_id = source_id.clone();
        let (generation, tx) = self.next_request();
        self.loading = true;
        self.prepared_source = None;
        self.message = Some("checking source host DNS and address safety…".into());
        thread::spawn(move || {
            let result = resolve_external_stream_url(&raw_url)
                .map_err(|_| {
                    "source URL is invalid, unreachable, or resolves to a restricted address"
                        .to_owned()
                })
                .and_then(|(scheme, url, addresses)| {
                    let transport = match scheme {
                        "http" => crate::media::SourceTransport::Http,
                        "https" => crate::media::SourceTransport::Https,
                        _ => crate::media::SourceTransport::Unsupported(scheme.into()),
                    };
                    let source =
                        stream
                            .playable_source(&provider, &stable_id)
                            .ok_or_else(|| {
                                "source URL is not a supported direct HTTP(S) stream".to_owned()
                            })?;
                    let source = Source {
                        label,
                        transport,
                        ..source
                    };
                    Ok(PreparedSource {
                        identity: PlaybackIdentity {
                            media_id,
                            episode_id,
                            source_id,
                        },
                        source,
                        addresses,
                        _ephemeral_url: url,
                    })
                });
            let _ = tx.send(Reply::SourceResolved(generation, result));
        });
    }

    pub fn source_label(&self, index: usize) -> String {
        if !self.spoilers_revealed {
            return format!("Source {}", index + 1);
        }
        self.sources
            .get(index)
            .and_then(|stream| stream.title.as_ref().or(stream.name.as_ref()))
            .map(|label| redact_source_label(label))
            .unwrap_or_else(|| format!("Source {}", index + 1))
    }

    pub fn spoiler_policy(&self) -> SpoilerPolicy {
        SpoilerPolicy {
            reveal_episode_details: self.spoilers_revealed,
        }
    }

    pub fn enter_search(&mut self) {
        self.page = Page::Search;
        self.search_editing = true;
        self.generation = self.generation.wrapping_add(1);
        self.message = if self.loading {
            Some("waiting for the previous provider request to finish…".into())
        } else {
            None
        };
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = match self.page {
            Page::Search => self.results.len(),
            Page::Episodes => self
                .seasons
                .get(self.season_selected)
                .map_or(0, |season| season.episodes.len()),
            Page::Sources => self.sources.len(),
            Page::Detail => 0,
        };
        if len > 0 {
            let selected = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
            if selected != self.selected {
                self.prepared_source = None;
            }
            self.selected = selected;
        }
    }

    pub fn move_season(&mut self, delta: isize) {
        if self.seasons.is_empty() {
            return;
        }
        self.season_selected = (self.season_selected as isize + delta)
            .clamp(0, self.seasons.len() as isize - 1) as usize;
        self.selected = 0;
        self.prepared_source = None;
    }

    fn next_request(&mut self) -> (u64, Sender<Reply>) {
        self.generation = self.generation.wrapping_add(1);
        (self.generation, self.tx.clone())
    }
}

fn redact_source_label(label: &str) -> String {
    label
        .split_whitespace()
        .map(|word| {
            if word.contains("://") {
                "[URL hidden]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn stable_source_id(
    provider: &str,
    media_id: &str,
    episode_id: Option<&str>,
    descriptor: &str,
) -> String {
    let stable_parts = format!(
        "{provider}\0{media_id}\0{}\0{descriptor}",
        episode_id.unwrap_or("movie")
    );
    format!("addon:{:x}", Md5::digest(stable_parts.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration, Instant};

    fn episode(number: u32, spoiler: &str) -> Episode {
        Episode {
            id: format!("e{number}"),
            season: Some(1),
            number: Some(number),
            title: Some(spoiler.into()),
            overview: Some("secret overview".into()),
            thumbnail: Some("https://private.invalid/image".into()),
            air_date: None,
        }
    }

    #[test]
    fn source_and_episode_output_are_spoiler_safe_by_default() {
        let mut workspace = Workspace::new();
        workspace.seasons = vec![Season {
            number: Some(1),
            episodes: vec![episode(1, "HUGE PLOT TWIST")],
        }];
        workspace.sources = vec![Stream {
            name: Some("HUGE PLOT TWIST release".into()),
            title: None,
            url: Some("https://signed.invalid/?secret".into()),
            info_hash: None,
            file_idx: None,
            yt_id: None,
        }];
        let policy = workspace.spoiler_policy();
        assert_eq!(
            workspace.selected_episode().unwrap().display_label(policy),
            "S01E01 · Episode 1"
        );
        assert_eq!(
            workspace
                .selected_episode()
                .unwrap()
                .display_overview(policy),
            None
        );
        assert_eq!(
            workspace
                .selected_episode()
                .unwrap()
                .display_thumbnail(policy),
            None
        );
        assert_eq!(workspace.source_label(0), "Source 1");
        workspace.spoilers_revealed = true;
        assert!(workspace
            .selected_episode()
            .unwrap()
            .display_label(workspace.spoiler_policy())
            .contains("HUGE PLOT TWIST"));
        assert!(workspace.source_label(0).contains("HUGE PLOT TWIST"));
        assert_eq!(
            redact_source_label("1080p https://signed.invalid/?token=secret"),
            "1080p [URL hidden]"
        );
    }

    #[test]
    fn switching_seasons_resets_episode_selection_and_never_invents_numbering() {
        let mut workspace = Workspace::new();
        workspace.seasons = vec![
            Season {
                number: Some(1),
                episodes: vec![episode(1, "spoiler")],
            },
            Season {
                number: None,
                episodes: vec![Episode {
                    number: None,
                    season: None,
                    ..episode(1, "unknown")
                }],
            },
        ];
        workspace.selected = 4;
        workspace.move_season(1);
        assert_eq!(workspace.selected, 0);
        assert_eq!(
            workspace
                .selected_episode()
                .unwrap()
                .display_label(SpoilerPolicy::default()),
            "Episode · number unavailable"
        );
    }

    #[test]
    fn selected_source_preflight_keeps_signed_url_ephemeral_and_identity_stable() {
        let mut workspace = Workspace::new();
        workspace.client = Some(Arc::new(AddonClient::local_test_client("http://127.0.0.1")));
        workspace.manifest = Some(Manifest {
            id: "fixture.addon".into(),
            name: "Fixture".into(),
            description: String::new(),
            resources: vec![],
            types: vec![],
            catalogs: vec![],
        });
        workspace.detail = Some(MediaItem {
            id: "movie-7".into(),
            kind: crate::media::MediaKind::Movie,
            title: "Fixture Movie".into(),
            year: None,
            genres: vec![],
            poster: None,
            background: None,
            description: None,
        });
        workspace.sources = vec![Stream {
            name: Some("1080p WEB-DL".into()),
            title: None,
            url: Some("https://8.8.8.8:8443/video?signature=never-log-this".into()),
            info_hash: None,
            file_idx: None,
            yt_id: None,
        }];
        workspace.prepare_selected_source();
        wait_for(&mut workspace, |state| {
            !state.loading && state.prepared_source.is_some()
        });
        let prepared = workspace.prepared_source.as_ref().unwrap();
        assert_eq!(prepared.identity.media_id, "movie-7");
        assert_eq!(prepared.identity.episode_id, None);
        assert!(!prepared.identity.source_id.contains("never-log-this"));
        assert_eq!(prepared.addresses, vec!["8.8.8.8:8443".parse().unwrap()]);
        let debug = format!("{prepared:?}");
        assert!(!debug.contains("never-log-this"));
        assert!(debug.contains("ephemeral URL redacted"));
    }

    #[test]
    fn source_identity_survives_signed_url_refresh_but_changes_per_episode() {
        let first = stable_source_id("addon", "series", Some("ep-1"), "1080p WEB-DL");
        let refreshed = stable_source_id("addon", "series", Some("ep-1"), "1080p WEB-DL");
        let next_episode = stable_source_id("addon", "series", Some("ep-2"), "1080p WEB-DL");
        assert_eq!(first, refreshed);
        assert_ne!(first, next_episode);
        assert!(!first.contains("signature"));
    }

    #[test]
    fn mock_provider_walks_search_detail_episode_and_source_flow() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let responses = [
                r#"{"metas":[{"id":"tt1","type":"series","name":"Fixture Show","year":"2024"}]}"#,
                r#"{"meta":{"id":"tt1","type":"series","name":"Fixture Show","videos":[{"id":"tt1:1:1","season":1,"episode":1,"title":"SECRET PLOT TWIST","overview":"A SECRET","thumbnail":"https://img.invalid/spoiler.jpg"}]}}"#,
                r#"{"streams":[{"name":"SECRET PLOT TWIST release","url":"https://cdn.invalid/video?signature=ephemeral"}]}"#,
            ];
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let size = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(request.starts_with("GET "), "{request}");
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });

        let mut workspace = Workspace::new();
        workspace.client = Some(Arc::new(AddonClient::local_test_client(&format!(
            "http://{address}"
        ))));
        workspace.manifest = Some(Manifest {
            id: "fixture.addon".into(),
            name: "Fixture".into(),
            description: String::new(),
            resources: vec![
                crate::stremio::Resource::Detailed {
                    name: "catalog".into(),
                    types: vec!["series".into()],
                    id_prefixes: vec![],
                },
                crate::stremio::Resource::Detailed {
                    name: "meta".into(),
                    types: vec!["series".into()],
                    id_prefixes: vec![],
                },
                crate::stremio::Resource::Detailed {
                    name: "stream".into(),
                    types: vec!["series".into()],
                    id_prefixes: vec![],
                },
            ],
            types: vec!["series".into()],
            catalogs: vec![crate::stremio::Catalog {
                media_type: "series".into(),
                id: "shows".into(),
                name: None,
            }],
        });
        workspace.query = "fixture".into();
        workspace.search();
        workspace.search();
        assert!(workspace.loading);
        wait_for(&mut workspace, |state| {
            !state.loading && !state.results.is_empty()
        });
        assert_eq!(workspace.results[0].title, "Fixture Show");

        workspace.open_selected();
        wait_for(&mut workspace, |state| {
            !state.loading && state.page == Page::Detail
        });
        workspace.enter_episodes();
        assert_eq!(
            workspace
                .selected_episode()
                .unwrap()
                .display_label(workspace.spoiler_policy()),
            "S01E01 · Episode 1"
        );
        assert_eq!(
            workspace
                .selected_episode()
                .unwrap()
                .display_overview(workspace.spoiler_policy()),
            None
        );

        workspace.load_selected_sources();
        wait_for(&mut workspace, |state| {
            !state.loading && state.page == Page::Sources
        });
        assert_eq!(workspace.sources.len(), 1);
        assert_eq!(workspace.source_label(0), "Source 1");
        assert!(!workspace.source_label(0).contains("SECRET"));
        server.join().unwrap();
    }

    fn wait_for(workspace: &mut Workspace, ready: impl Fn(&Workspace) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            workspace.poll();
            if ready(workspace) {
                return;
            }
            thread::yield_now();
        }
        panic!("mock provider flow timed out: {:?}", workspace.message);
    }
}
