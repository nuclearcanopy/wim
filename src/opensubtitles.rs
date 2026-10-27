//! Auto-fetch subtitles for local video from OpenSubtitles. Behind
//! `config.opensubtitles_api_key`; `None` (the default) means this
//! whole module never gets touched. Needs a real key from
//! opensubtitles.com to actually live-test against; see
//! QUESTIONS.md. Best-effort guess at the REST API v1 shape from
//! general knowledge, not verified against live docs in this sandbox
//! (no network access here beyond what's already configured).

use std::io::Read;

use serde::Deserialize;

const BASE_URL: &str = "https://api.opensubtitles.com/api/v1";
const USER_AGENT: &str = "wim v0.1.0";

pub struct Client {
    api_key: String,
    agent: ureq::Agent,
}

#[derive(Debug, Clone)]
pub struct SubtitleResult {
    pub file_id: u64,
    pub language: String,
    pub release: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    data: Vec<SearchEntry>,
}

#[derive(Deserialize)]
struct SearchEntry {
    attributes: SearchAttributes,
}

#[derive(Deserialize)]
struct SearchAttributes {
    language: Option<String>,
    release: Option<String>,
    files: Vec<SearchFile>,
}

#[derive(Deserialize)]
struct SearchFile {
    file_id: u64,
}

#[derive(Deserialize)]
struct DownloadResponse {
    link: String,
}

impl Client {
    pub fn new(api_key: String) -> Self {
        Client {
            api_key,
            agent: ureq::AgentBuilder::new().user_agent(USER_AGENT).build(),
        }
    }

    /// Searches by free-text title (not file hash; wim doesn't compute
    /// one) plus a preferred language code (e.g. "en"). Best match is
    /// whichever the api sorts first; not re-ranked locally.
    pub fn search(&self, title: &str, language: &str) -> anyhow::Result<Vec<SubtitleResult>> {
        let resp: SearchResponse = self
            .agent
            .get(&format!("{BASE_URL}/subtitles"))
            .set("Api-Key", &self.api_key)
            .query("query", title)
            .query("languages", language)
            .call()?
            .into_json()?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(|entry| {
                let file_id = entry.attributes.files.first()?.file_id;
                Some(SubtitleResult {
                    file_id,
                    language: entry.attributes.language.unwrap_or_default(),
                    release: entry.attributes.release.unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Resolves a file id to an actual download link (consumes quota
    /// against the account the api key belongs to) and fetches the
    /// subtitle file's raw bytes from it.
    pub fn download(&self, file_id: u64) -> anyhow::Result<Vec<u8>> {
        let resp: DownloadResponse = self
            .agent
            .post(&format!("{BASE_URL}/download"))
            .set("Api-Key", &self.api_key)
            .send_json(ureq::json!({ "file_id": file_id }))?
            .into_json()?;
        let mut bytes = Vec::new();
        self.agent
            .get(&resp.link)
            .call()?
            .into_reader()
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}
