//! Search/account interaction state shared by the frontends. Requests run
//! off-thread; frontends poll this state and render their own presentation.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;

use crate::torbox::{AccountItem, Client, MetadataHit, Service, TorrentRelease};

pub const ACCOUNT_PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Setup,
    Search,
    Releases,
    Account,
    Files,
}

enum Reply {
    Search(u64, Result<Vec<MetadataHit>, String>),
    Releases(u64, Result<Vec<TorrentRelease>, String>),
    Added(u64, Result<String, String>),
    Account(u64, Service, Result<Vec<AccountItem>, String>),
    Controlled(u64, Result<String, String>),
    Configured(u64, String, Result<Arc<Client>, String>),
}

/// Latest-request-wins model for TorBox search and add actions.
pub struct Workspace {
    client: Option<Arc<Client>>,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    generation: u64,
    pub page: Page,
    pub query: String,
    pub hits: Vec<MetadataHit>,
    pub releases: Vec<TorrentRelease>,
    pub selected: usize,
    pub loading: bool,
    pub message: Option<String>,
    confirm_add: Option<usize>,
    pub service: Service,
    pub account: Vec<AccountItem>,
    pub account_selected: Option<AccountItem>,
    confirm_delete: Option<(Service, u64)>,
    pub link_input: Option<String>,
    pub token_path_input: String,
    pub setup_busy: bool,
    configured_path: Option<String>,
    pub account_offset: usize,
    pub account_has_more: bool,
    pub search_editing: bool,
    setup_error: Option<String>,
}

impl Workspace {
    pub fn new(client: Result<Client, String>) -> Self {
        let (tx, rx) = mpsc::channel();
        let (client, message) = match client {
            Ok(client) => (Some(Arc::new(client)), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            client,
            tx,
            rx,
            generation: 0,
            page: Page::Search,
            query: String::new(),
            hits: Vec::new(),
            releases: Vec::new(),
            selected: 0,
            loading: false,
            message,
            confirm_add: None,
            service: Service::Torrent,
            account: Vec::new(),
            account_selected: None,
            confirm_delete: None,
            link_input: None,
            token_path_input: String::new(),
            setup_busy: false,
            configured_path: None,
            account_offset: 0,
            account_has_more: false,
            search_editing: true,
            setup_error: None,
        }
    }

    pub fn poll(&mut self) {
        loop {
            let reply = match self.rx.try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            match reply {
                Reply::Search(generation, result) if generation == self.generation => {
                    self.loading = false;
                    self.page = Page::Search;
                    match result {
                        Ok(hits) => {
                            self.hits = hits;
                            self.selected = 0;
                            self.message = if self.hits.is_empty() {
                                Some("no metadata results".into())
                            } else {
                                None
                            };
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
                Reply::Releases(generation, result) if generation == self.generation => {
                    self.loading = false;
                    self.page = Page::Releases;
                    match result {
                        Ok(releases) => {
                            self.releases = releases;
                            self.selected = 0;
                            self.message = if self.releases.is_empty() {
                                Some("no torrent releases found".into())
                            } else {
                                None
                            };
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
                Reply::Added(generation, result) if generation == self.generation => {
                    self.loading = false;
                    self.confirm_add = None;
                    self.message = Some(match result {
                        Ok(message) => message,
                        Err(error) => error,
                    });
                }
                Reply::Account(generation, service, result) if generation == self.generation => {
                    self.loading = false;
                    self.page = Page::Account;
                    self.service = service;
                    match result {
                        Ok(items) => {
                            self.account_has_more = items.len() == ACCOUNT_PAGE_SIZE;
                            self.account.extend(items);
                            self.selected = self.selected.min(self.account.len().saturating_sub(1));
                            self.message = if self.account.is_empty() {
                                Some("no account items".into())
                            } else {
                                None
                            };
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
                Reply::Controlled(generation, result) if generation == self.generation => {
                    self.loading = false;
                    self.confirm_delete = None;
                    self.message = Some(result.unwrap_or_else(|error| error));
                    self.load_account(self.service);
                }
                Reply::Configured(generation, path, result) if generation == self.generation => {
                    self.setup_busy = false;
                    match result {
                        Ok(client) => {
                            self.client = Some(client);
                            self.configured_path = Some(path.clone());
                            self.token_path_input = path;
                            self.page = Page::Search;
                            self.search_editing = false;
                            self.message =
                                Some("TorBox connection verified; token path saved".into());
                        }
                        Err(error) => {
                            self.setup_error = Some(error.clone());
                            self.message = Some(error);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn item_count(&self) -> usize {
        match self.page {
            Page::Setup => 0,
            Page::Search => self.hits.len(),
            Page::Releases => self.releases.len(),
            Page::Account => self.account.len(),
            Page::Files => match (&self.account_selected, &self.client) {
                (Some(item), Some(client)) => item.playable_items(client.clone()).len(),
                _ => 0,
            },
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.item_count();
        if len > 0 {
            self.selected = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
        }
        self.confirm_add = None;
        self.confirm_delete = None;
    }

    pub fn search(&mut self) {
        let query = self.query.trim().to_owned();
        if query.is_empty() {
            self.message = Some("type a title, then press enter to search TorBox".into());
            return;
        }
        let Some(client) = self.client.clone() else {
            self.message = Some("TorBox is not configured: set torbox_token_file".into());
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        self.search_editing = false;
        let (generation, tx) = (self.generation, self.tx.clone());
        self.loading = true;
        self.message = Some(format!("searching TorBox for {query:?}…"));
        thread::spawn(move || {
            let result = client.search_hits(&query).map_err(|_| {
                "TorBox search failed; check the token, network, and account plan".to_string()
            });
            let _ = tx.send(Reply::Search(generation, result));
        });
    }

    pub fn load_account(&mut self, service: Service) {
        self.account_offset = 0;
        self.account.clear();
        self.account_has_more = false;
        self.fetch_account_page(service, false);
    }

    pub fn load_more_account(&mut self) {
        if self.account_has_more && !self.loading {
            self.account_offset = self.account_offset.saturating_add(ACCOUNT_PAGE_SIZE);
            self.fetch_account_page(self.service, true);
        }
    }

    fn fetch_account_page(&mut self, service: Service, append: bool) {
        let Some(client) = self.client.clone() else {
            self.message = Some("TorBox is not configured: set torbox_token_file".into());
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        let (generation, tx) = (self.generation, self.tx.clone());
        let offset = self.account_offset;
        self.loading = true;
        self.service = service;
        self.confirm_delete = None;
        self.confirm_add = None;
        self.message = Some(format!("loading TorBox {} account…", service.as_str()));
        thread::spawn(move || {
            let result = client
                .account_items(service, offset, ACCOUNT_PAGE_SIZE)
                .map_err(|_| "TorBox account list failed; check token, network, and plan".into());
            let _ = tx.send(Reply::Account(generation, service, result));
        });
        if !append {
            self.account.clear();
        }
    }

    pub fn begin_setup(&mut self, configured_path: Option<&str>) {
        self.page = Page::Setup;
        self.token_path_input = std::env::var("WIM_TORBOX_TOKEN_FILE")
            .ok()
            .or_else(|| configured_path.map(str::to_owned))
            .unwrap_or_default();
        self.message = Some("Enter the path to a protected TorBox API token file. The token itself is never saved in Wim config.".into());
    }

    pub fn commit_setup(&mut self) {
        let path = self.token_path_input.trim().to_owned();
        if path.is_empty() {
            self.message = Some("enter a token-file path, or press Esc to cancel".into());
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let (generation, tx) = (self.generation, self.tx.clone());
        self.setup_busy = true;
        self.message = Some("checking token file and contacting TorBox…".into());
        thread::spawn(move || {
            let result = Client::from_token_file(&path)
                .map_err(|_| "Cannot read this token file. Check that the path exists and is readable.".to_string())
                .and_then(|client| {
                    client.account_items(Service::Torrent, 0, 1)
                        .map(|_| Arc::new(client))
                        .map_err(|error| {
                            let text = error.to_string();
                            if text.contains("HTTP 401") || text.contains("HTTP 403") {
                                "TorBox rejected this token. Verify that the file contains a current API token.".to_string()
                            } else if text.contains("network error") {
                                "Could not reach TorBox. Check network/VPN routing; TorBox may reject Mullvad VPN exits.".to_string()
                            } else {
                                "TorBox could not verify the account response. Check account access and try again.".to_string()
                            }
                        })
                });
            let _ = tx.send(Reply::Configured(generation, path, result));
        });
    }

    pub fn take_configured_path(&mut self) -> Option<String> {
        self.configured_path.take()
    }

    pub fn take_setup_error(&mut self) -> Option<String> {
        self.setup_error.take()
    }

    pub fn cancel_setup(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.setup_busy = false;
        self.page = Page::Search;
    }

    pub fn begin_search_edit(&mut self) {
        self.page = Page::Search;
        self.search_editing = true;
        self.message = None;
    }

    pub fn is_configured(&self) -> bool {
        self.client.is_some()
    }

    pub fn open_account_item(&mut self) {
        self.account_selected = self.account.get(self.selected).cloned();
        if self.account_selected.is_some() {
            self.page = Page::Files;
            self.selected = 0;
            self.message = None;
        }
    }

    pub fn selected_playable(&self) -> Option<crate::search::SearchItem> {
        let client = self.client.as_ref()?;
        let item = self.account_selected.as_ref()?;
        item.playable_items(Arc::clone(client))
            .into_iter()
            .nth(self.selected)
    }

    pub fn control_selected(&mut self, operation: &str) {
        let Some(item) = self.account.get(self.selected).cloned() else {
            return;
        };
        if operation == "delete" && self.confirm_delete != Some((item.service, item.id)) {
            self.confirm_delete = Some((item.service, item.id));
            self.message = Some(format!("delete '{}' permanently? press d again", item.name));
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        let (generation, tx) = (self.generation, self.tx.clone());
        let operation = operation.to_owned();
        self.loading = true;
        self.message = Some(format!("{operation} {}…", item.name));
        thread::spawn(move || {
            let result = client
                .control(item.service, item.id, &operation)
                .and_then(|r| r.ensure_success())
                .map(|_| format!("{operation} complete: {}", item.name))
                .map_err(|_| format!("TorBox could not {operation} this item"));
            let _ = tx.send(Reply::Controlled(generation, result));
        });
    }

    pub fn begin_add(&mut self) {
        self.link_input = Some(String::new());
        self.message = Some(match self.service {
            Service::Torrent => "paste magnet:? link, then press Enter".into(),
            Service::Usenet => "paste HTTP(S) NZB link, then press Enter".into(),
            Service::WebDownload => "paste HTTP(S) download link, then press Enter".into(),
        });
    }

    pub fn commit_add(&mut self) {
        let Some(link) = self.link_input.take() else {
            return;
        };
        if link.trim().is_empty() {
            self.message = Some("link cannot be empty".into());
            return;
        }
        self.add_link(link.trim().to_owned());
    }

    pub fn add_link(&mut self, link: String) {
        let Some(client) = self.client.clone() else {
            self.message = Some("TorBox is not configured: set torbox_token_file".into());
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        let (generation, tx, service) = (self.generation, self.tx.clone(), self.service);
        self.loading = true;
        self.message = Some(format!("adding {} link to TorBox…", service.as_str()));
        thread::spawn(move || {
            let result = match service {
                Service::Torrent => client.add_magnet(&link),
                Service::Usenet => client.add_usenet_link(&link),
                Service::WebDownload => client.add_web_download(&link),
            }
            .and_then(|r| r.ensure_success())
            .map(|_| format!("added {} item; refresh account list", service.as_str()))
            .map_err(|_| format!("TorBox could not add this {} link", service.as_str()));
            let _ = tx.send(Reply::Added(generation, result));
        });
    }

    pub fn open_releases(&mut self) {
        let Some(hit) = self.hits.get(self.selected).cloned() else {
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        let (generation, tx) = (self.generation, self.tx.clone());
        self.loading = true;
        self.message = Some(format!("finding torrent sources for {}…", hit.title));
        thread::spawn(move || {
            let result = client
                .releases_for_metadata(&hit.source, &hit.id)
                .map_err(|_| "TorBox could not load torrent sources".to_string());
            let _ = tx.send(Reply::Releases(generation, result));
        });
    }

    /// First press on an uncached result arms confirmation; the next press
    /// submits it. Cached results still require an intentional add keypress.
    pub fn add_selected(&mut self) {
        let Some(release) = self.releases.get(self.selected).cloned() else {
            return;
        };
        let Some(magnet) = release.magnet.clone() else {
            self.message = Some("this release has no magnet link".into());
            return;
        };
        if release.cached == Some(false) && self.confirm_add != Some(self.selected) {
            self.confirm_add = Some(self.selected);
            self.message =
                Some("uncached item may use account capacity; press 'a' again to add".into());
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        let (generation, tx) = (self.generation, self.tx.clone());
        self.loading = true;
        self.message = Some(format!("adding {} to TorBox…", release.name));
        thread::spawn(move || {
            let result = client
                .add_magnet(&magnet)
                .and_then(|response| response.ensure_success())
                .map(|_| "added to TorBox; refresh the account list to see its files".to_string())
                .map_err(|_| "TorBox could not add this magnet; check account limits".to_string());
            let _ = tx.send(Reply::Added(generation, result));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torbox::{AccountFile, AccountItem};

    #[test]
    fn selected_account_file_becomes_a_stable_torbox_queue_identity() {
        let token_path = std::env::temp_dir().join(format!("wim-tbx-{}", std::process::id()));
        std::fs::write(&token_path, "not-a-live-token").unwrap();
        let client = Client::from_token_file(&token_path).unwrap();
        let mut workspace = Workspace::new(Ok(client));
        workspace.account_selected = Some(AccountItem {
            service: Service::WebDownload,
            id: 81,
            name: "movie".into(),
            status: "completed".into(),
            progress: Some(1.0),
            files: vec![AccountFile {
                id: 7,
                name: "movie.mp4".into(),
                size: 1234,
            }],
        });
        workspace.page = Page::Files;
        let item = workspace.selected_playable().unwrap();
        assert!(matches!(
            item,
            crate::search::SearchItem::TorBoxFile {
                service: Service::WebDownload,
                item_id: 81,
                file_id: 7,
                video: true,
                resolved_url: None,
                ..
            }
        ));
        std::fs::remove_file(token_path).ok();
    }

    #[test]
    fn deletion_requires_a_second_explicit_press_for_same_selection() {
        let token_path =
            std::env::temp_dir().join(format!("wim-tbx-delete-{}", std::process::id()));
        std::fs::write(&token_path, "test").unwrap();
        let mut workspace = Workspace::new(Ok(Client::from_token_file(&token_path).unwrap()));
        workspace.account.push(AccountItem {
            service: Service::Torrent,
            id: 12,
            name: "item".into(),
            status: "completed".into(),
            progress: None,
            files: Vec::new(),
        });
        workspace.control_selected("delete");
        assert!(workspace
            .message
            .as_deref()
            .unwrap()
            .contains("press d again"));
        assert!(!workspace.loading);
        std::fs::remove_file(token_path).ok();
    }
}
