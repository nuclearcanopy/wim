//! Parsing and capability checks for the safe, client-consumable subset of the
//! Stremio add-on protocol. Network transport is deliberately kept outside
//! this module so callers can enforce their configured provider/privacy policy.

use serde::Deserialize;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::time::Duration;

use crate::media::{Episode, MediaItem, MediaKind, Source, SourceTransport, SubtitleCandidate};

const RESPONSE_LIMIT: u64 = 2 * 1024 * 1024;

/// An opt-in client for an explicitly configured add-on. Call it from a
/// background worker: methods are synchronous and bounded, not UI-thread safe.
pub struct AddonClient {
    base: String,
    agent: ureq::Agent,
    request_lock: Mutex<()>,
}

impl std::fmt::Debug for AddonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddonClient")
            .field("base", &"[configured provider URL]")
            .finish()
    }
}

impl AddonClient {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        let (base, addresses) = checked_provider_url(base)?;
        let resolver_addresses = addresses.clone();
        let base = base.trim_end_matches('/');
        let base = base.strip_suffix("/manifest.json").unwrap_or(base);
        Ok(Self {
            base: base.to_owned(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(12))
                .redirects(0)
                .try_proxy_from_env(false)
                .https_only(true)
                .resolver(move |_netloc: &str| -> std::io::Result<Vec<SocketAddr>> {
                    Ok(resolver_addresses.clone())
                })
                .build(),
            request_lock: Mutex::new(()),
        })
    }

    #[cfg(test)]
    pub(crate) fn local_test_client(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(2))
                .redirects(0)
                .build(),
            request_lock: Mutex::new(()),
        }
    }

    pub fn manifest(&self) -> anyhow::Result<Manifest> {
        self.get("/manifest.json")
    }

    pub fn search(
        &self,
        manifest: &Manifest,
        media_type: &str,
        catalog_id: &str,
        query: &str,
    ) -> anyhow::Result<CatalogResponse> {
        anyhow::ensure!(
            manifest.supports("catalog", media_type, None),
            "provider does not support this catalog type"
        );
        anyhow::ensure!(
            manifest.catalogs.is_empty()
                || manifest.catalogs.iter().any(|catalog| {
                    catalog.media_type == media_type && catalog.id == catalog_id
                }),
            "provider does not declare this catalog"
        );
        let path = format!(
            "/catalog/{}/{}/search={}.json",
            encode_path_segment(media_type),
            encode_path_segment(catalog_id),
            encode_path_segment(query)
        );
        self.get(&path)
    }

    pub fn metadata(
        &self,
        manifest: &Manifest,
        media_type: &str,
        id: &str,
    ) -> anyhow::Result<MetaResponse> {
        anyhow::ensure!(
            manifest.supports("meta", media_type, Some(id)),
            "provider does not support metadata for this ID"
        );
        self.get(&format!(
            "/meta/{}/{}.json",
            encode_path_segment(media_type),
            encode_path_segment(id)
        ))
    }

    pub fn streams(
        &self,
        manifest: &Manifest,
        media_type: &str,
        id: &str,
    ) -> anyhow::Result<StreamResponse> {
        anyhow::ensure!(
            manifest.supports("stream", media_type, Some(id)),
            "provider does not support streams for this ID"
        );
        self.get(&format!(
            "/stream/{}/{}.json",
            encode_path_segment(media_type),
            encode_path_segment(id)
        ))
    }

    pub fn subtitles(
        &self,
        manifest: &Manifest,
        media_type: &str,
        id: &str,
    ) -> anyhow::Result<SubtitlesResponse> {
        anyhow::ensure!(
            manifest.supports("subtitles", media_type, Some(id)),
            "provider does not support subtitles for this ID"
        );
        self.get(&format!(
            "/subtitles/{}/{}.json",
            encode_path_segment(media_type),
            encode_path_segment(id)
        ))
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        // One in-flight request per configured add-on keeps independent UI
        // consumers from overwhelming an extension server.
        let _guard = self
            .request_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let response = self
            .agent
            .get(&format!("{}{path}", self.base))
            .call()
            .map_err(|_| {
                anyhow::anyhow!("provider request failed (network, timeout, or HTTP error)")
            })?;
        anyhow::ensure!(
            (200..300).contains(&response.status()),
            "provider returned a non-success HTTP status"
        );
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(RESPONSE_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= RESPONSE_LIMIT,
            "provider response exceeds 2 MiB limit"
        );
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("provider returned an invalid response"))
    }
}

#[cfg(test)]
fn validate_provider_url(raw: &str) -> anyhow::Result<()> {
    checked_provider_url(raw).map(|_| ())
}

fn checked_provider_url(raw: &str) -> anyhow::Result<(String, Vec<SocketAddr>)> {
    reject_dot_segments(raw)?;
    let url = url::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid provider URL"))?;
    anyhow::ensure!(url.scheme() == "https", "provider URL must use HTTPS");
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "provider URL must not embed credentials"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "provider URL must not contain a query or fragment"
    );
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("provider URL has no host"))?;
    let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
    anyhow::ensure!(
        normalized_host != "localhost"
            && !normalized_host.ends_with(".localhost")
            && !normalized_host.ends_with(".local"),
        "local provider hosts are not allowed"
    );
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("provider URL has no port"))?;
    let addresses: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        (host, port)
            .to_socket_addrs()
            .map_err(|_| anyhow::anyhow!("provider host did not resolve"))?
            .collect()
    };
    let addresses = checked_public_addresses(addresses)?;
    Ok((url.to_string(), addresses))
}

fn checked_public_addresses(addresses: Vec<SocketAddr>) -> anyhow::Result<Vec<SocketAddr>> {
    anyhow::ensure!(
        !addresses.is_empty()
            && addresses
                .iter()
                .all(|address| is_public_address(address.ip())),
        "provider host resolves to a private or special-use address"
    );
    Ok(addresses)
}

fn reject_dot_segments(raw: &str) -> anyhow::Result<()> {
    let authority_start = raw.find("://").map(|index| index + 3).unwrap_or(0);
    anyhow::ensure!(
        !raw[authority_start..].contains('\\'),
        "provider URL path contains a forbidden separator"
    );
    let path_start = raw[authority_start..]
        .find('/')
        .map(|index| authority_start + index);
    if let Some(path_start) = path_start {
        let path = raw[path_start..]
            .split(['?', '#'])
            .next()
            .unwrap_or_default();
        let path_lower = path.to_ascii_lowercase();
        anyhow::ensure!(
            !path_lower.contains("%2f") && !path_lower.contains("%5c"),
            "provider URL path contains an encoded separator"
        );
        for segment in path.split('/') {
            let decoded = decode_percent_dots(segment);
            anyhow::ensure!(
                decoded != "." && decoded != "..",
                "provider URL must not contain dot segments"
            );
        }
    }
    Ok(())
}

fn decode_percent_dots(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = String::with_capacity(segment.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&segment[index + 1..index + 3], 16) {
                decoded.push(value as char);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index] as char);
        index += 1;
    }
    decoded
}

/// Parse untrusted stream URLs before exposing them as direct-play candidates.
/// Actual playback must repeat address checks and use a pinned resolver.
pub fn validate_external_stream_url(raw: &str) -> anyhow::Result<&'static str> {
    reject_dot_segments(raw)?;
    let url = url::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid stream URL"))?;
    let scheme = match url.scheme() {
        "http" => "http",
        "https" => "https",
        _ => anyhow::bail!("unsupported stream URL scheme"),
    };
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "stream URL must not embed credentials"
    );
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("stream URL has no host"))?;
    let host_lower = host.trim_end_matches('.').to_ascii_lowercase();
    anyhow::ensure!(
        host_lower != "localhost"
            && !host_lower.ends_with(".localhost")
            && !host_lower.ends_with(".local"),
        "local stream hosts are not allowed"
    );
    if let Ok(address) = host.parse::<IpAddr>() {
        anyhow::ensure!(
            is_public_address(address),
            "private or special-use stream address is not allowed"
        );
    }
    Ok(scheme)
}

/// Validate a provider-supplied URL and resolve it exactly once. Consumers
/// that open a socket must use these addresses rather than resolving the host
/// again; ordinary mpv URL playback cannot currently accept a pinned resolver,
/// so this result is not yet a playback handoff.
pub fn resolve_external_stream_url(
    raw: &str,
) -> anyhow::Result<(&'static str, String, Vec<SocketAddr>)> {
    reject_dot_segments(raw)?;
    let scheme = validate_external_stream_url(raw)?;
    let url = url::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid stream URL"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("stream URL has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("stream URL has no port"))?;
    let addresses: Vec<SocketAddr> = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, port)]
    } else {
        (host, port)
            .to_socket_addrs()
            .map_err(|_| anyhow::anyhow!("stream host did not resolve"))?
            .collect()
    };
    let addresses = checked_public_addresses(addresses)
        .map_err(|_| anyhow::anyhow!("stream host resolves to a private or special-use address"))?;
    Ok((scheme, url.to_string(), addresses))
}

pub(crate) fn is_public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || a == 0
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                || !(0x2000..=0x3fff).contains(&first)
                || (first == 0x2001 && ip.segments()[1] == 0x0db8)
                || first == 0x2002
                || (first == 0x3fff && ip.segments()[1] & 0xf000 == 0)
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| !is_public_address(IpAddr::V4(v4))))
        }
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub catalogs: Vec<Catalog>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    #[serde(rename = "type")]
    pub media_type: String,
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Resource {
    Name(String),
    Detailed {
        name: String,
        #[serde(default)]
        types: Vec<String>,
        #[serde(rename = "idPrefixes", default)]
        id_prefixes: Vec<String>,
    },
}

impl Manifest {
    pub fn supports(&self, resource: &str, media_type: &str, id: Option<&str>) -> bool {
        self.resources.iter().any(|candidate| match candidate {
            Resource::Name(name) => name == resource && self.types.iter().any(|t| t == media_type),
            Resource::Detailed {
                name,
                types,
                id_prefixes,
            } => {
                name == resource
                    && (types.is_empty() || types.iter().any(|t| t == media_type))
                    && id.is_none_or(|id| {
                        id_prefixes.is_empty()
                            || id_prefixes.iter().any(|prefix| id.starts_with(prefix))
                    })
            }
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MetaResponse {
    pub meta: Meta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogResponse {
    #[serde(default)]
    pub metas: Vec<MetaPreview>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MetaPreview {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub year: Option<String>,
    pub poster: Option<String>,
    #[serde(rename = "posterShape")]
    pub poster_shape: Option<String>,
    pub description: Option<String>,
}

impl MetaPreview {
    pub fn media_item(&self) -> Option<MediaItem> {
        let kind = match self.kind.as_str() {
            "movie" => MediaKind::Movie,
            "series" => MediaKind::Series,
            _ => return None,
        };
        Some(MediaItem {
            id: self.id.clone(),
            kind,
            title: self.name.clone(),
            year: self
                .year
                .as_deref()
                .and_then(|year| year.get(..4)?.parse().ok()),
            genres: Vec::new(),
            poster: self.poster.clone(),
            background: None,
            description: self.description.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub year: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub videos: Vec<Video>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Video {
    pub id: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub thumbnail: Option<String>,
    #[serde(rename = "released", alias = "releaseInfo")]
    pub air_date: Option<String>,
}

impl Meta {
    pub fn media_item(&self) -> Option<MediaItem> {
        let kind = match self.kind.as_str() {
            "movie" => MediaKind::Movie,
            "series" => MediaKind::Series,
            _ => return None,
        };
        Some(MediaItem {
            id: self.id.clone(),
            kind,
            title: self.name.clone(),
            year: self
                .year
                .as_deref()
                .and_then(|year| year.get(..4)?.parse().ok()),
            genres: self.genres.clone(),
            poster: self.poster.clone(),
            background: self.background.clone(),
            description: self.description.clone(),
        })
    }

    pub fn episodes(&self) -> Vec<Episode> {
        self.videos
            .iter()
            .map(|video| Episode {
                id: video.id.clone(),
                season: video.season,
                number: video.episode,
                title: video.title.clone(),
                overview: video.overview.clone(),
                thumbnail: video.thumbnail.clone(),
                air_date: video.air_date.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamResponse {
    #[serde(default)]
    pub streams: Vec<Stream>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Stream {
    pub name: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    #[serde(rename = "infoHash")]
    pub info_hash: Option<String>,
    #[serde(rename = "fileIdx")]
    pub file_idx: Option<u32>,
    #[serde(rename = "ytId")]
    pub yt_id: Option<String>,
}

impl Stream {
    /// Convert only direct HTTP(S) links here. Torrent/YouTube identifiers
    /// require separate resolvers and must not be mistaken for playable URLs.
    pub fn playable_source(&self, provider: &str, stable_id: &str) -> Option<Source> {
        let url = self.url.as_deref()?;
        let transport = match validate_external_stream_url(url).ok()? {
            "http" => SourceTransport::Http,
            "https" => SourceTransport::Https,
            _ => return None,
        };
        Some(Source {
            // The URL is intentionally not retained in the stable identity:
            // add-ons may return signed/temporary links. A caller supplies an
            // identity scoped to the provider response and resolves the URL
            // only when playback is explicitly selected.
            id: stable_id.to_owned(),
            provider: provider.to_owned(),
            label: self
                .title
                .clone()
                .or_else(|| self.name.clone())
                .unwrap_or_else(|| "Direct stream".into()),
            quality: None,
            size_bytes: None,
            cached: None,
            transport,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubtitlesResponse {
    #[serde(default)]
    pub subtitles: Vec<Subtitle>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Subtitle {
    pub id: String,
    pub url: String,
    pub lang: String,
    pub label: Option<String>,
}

impl Subtitle {
    pub fn candidate(&self, provider: &str) -> SubtitleCandidate {
        SubtitleCandidate {
            id: self.id.clone(),
            provider: provider.to_owned(),
            language: self.lang.clone(),
            label: self.label.clone(),
            format: None,
            url: Some(self.url.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{seasons_from_episodes, SpoilerPolicy};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]).into_owned();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            request
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn manifest_capabilities_respect_resource_type_and_prefix() {
        let manifest: Manifest = serde_json::from_str(
            r#"{
            "id":"test.addon","name":"Fixture","resources":[
                {"name":"meta","types":["series"],"idPrefixes":["tt"]},
                "catalog"],"types":["movie","series"]
        }"#,
        )
        .unwrap();
        assert!(manifest.supports("meta", "series", Some("tt123")));
        assert!(!manifest.supports("meta", "movie", Some("tt123")));
        assert!(!manifest.supports("meta", "series", Some("other:1")));
        assert!(manifest.supports("catalog", "movie", None));
    }

    #[test]
    fn addon_client_requests_catalog_and_honors_the_manifest() {
        let (base, server) = serve_once(r#"{"metas":[]}"#);
        let client = AddonClient::local_test_client(&base);
        let manifest: Manifest = serde_json::from_str(
            r#"{
            "id":"fixture","name":"Fixture","types":["series"],
            "resources":[{"name":"catalog","types":["series"]}],
            "catalogs":[{"type":"series","id":"series","name":"Shows"}]
        }"#,
        )
        .unwrap();
        client
            .search(&manifest, "series", "series", "hello world")
            .unwrap();
        let request = server.join().unwrap();
        assert!(
            request.starts_with("GET /catalog/series/series/search=hello%20world.json HTTP/1.1"),
            "{request}"
        );
        assert!(client
            .search(&manifest, "series", "not-declared", "x")
            .is_err());
        assert!(client.search(&manifest, "movie", "movie", "x").is_err());
    }

    #[test]
    fn provider_urls_reject_credentials_http_and_non_public_addresses() {
        assert!(validate_provider_url("http://example.org").is_err());
        assert!(validate_provider_url("https://user:pass@example.org").is_err());
        assert!(validate_provider_url("https://127.0.0.1").is_err());
        assert!(validate_provider_url("https://192.0.2.1").is_err());
        assert!(validate_provider_url("https://[::1]").is_err());
        assert!(reject_dot_segments("https://provider.example/addon/../private").is_err());
        assert!(reject_dot_segments("https://provider.example/addon/%2e%2e/private").is_err());
        assert!(reject_dot_segments("https://provider.example/addon%2f..%2fprivate").is_err());
        assert!(reject_dot_segments("https://provider.example\\private").is_err());
    }

    #[test]
    fn mixed_dns_answers_and_special_ipv6_ranges_are_rejected() {
        let mixed = vec![
            "8.8.8.8:443".parse().unwrap(),
            "10.0.0.1:443".parse().unwrap(),
        ];
        assert!(checked_public_addresses(mixed).is_err());
        let empty: Vec<SocketAddr> = Vec::new();
        assert!(checked_public_addresses(empty).is_err());
        for address in [
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            assert!(!is_public_address(address.parse().unwrap()), "{address}");
        }
        for address in ["2001:4860:4860::8888", "2606:4700:4700::1111"] {
            assert!(is_public_address(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn stream_urls_need_a_valid_safe_http_authority() {
        assert_eq!(
            validate_external_stream_url("https://cdn.example/video?signature=secret").unwrap(),
            "https"
        );
        assert_eq!(
            validate_external_stream_url("http://cdn.example/video").unwrap(),
            "http"
        );
        for url in [
            "https://user:pass@cdn.example/video",
            "https://127.0.0.1/video",
            "https://localhost/video",
            "https://cdn.example/addon/../private",
            "file:///etc/passwd",
            "https://",
        ] {
            assert!(validate_external_stream_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn stream_resolution_returns_only_public_pinned_addresses() {
        let (scheme, normalized, addresses) =
            resolve_external_stream_url("https://8.8.8.8:8443/video?signature=private").unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(normalized, "https://8.8.8.8:8443/video?signature=private");
        assert_eq!(addresses, vec!["8.8.8.8:8443".parse().unwrap()]);
        assert!(resolve_external_stream_url("https://10.0.0.1/video").is_err());
        assert!(resolve_external_stream_url("https://[::1]/video").is_err());
    }

    #[test]
    fn addon_client_does_not_follow_redirects_or_leak_provider_urls_in_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://secret.invalid/redirect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client = AddonClient::local_test_client(&format!("http://{address}"));
        let error = client.manifest().unwrap_err().to_string();
        server.join().unwrap();
        assert_eq!(error, "provider returned a non-success HTTP status");
        assert!(!error.contains("secret.invalid"));
    }

    #[test]
    fn addon_client_rejects_oversized_responses() {
        let body: &'static str = Box::leak(
            format!("{}{{}}", " ".repeat((RESPONSE_LIMIT + 1) as usize)).into_boxed_str(),
        );
        let (base, server) = serve_once(body);
        let client = AddonClient::local_test_client(&base);
        let error = client.manifest().unwrap_err().to_string();
        let _ = server.join().unwrap();
        assert_eq!(error, "provider response exceeds 2 MiB limit");
    }

    #[test]
    fn catalog_search_preview_normalizes_movies_and_series() {
        let response: CatalogResponse = serde_json::from_str(r#"{"metas":[
            {"id":"tt001","type":"movie","name":"Film","year":"2023-01-01","poster":"https://img.invalid/p.jpg"},
            {"id":"tt002","type":"series","name":"Show","year":"2024","posterShape":"poster"}
        ]}"#).unwrap();
        let items: Vec<_> = response.metas.iter().map(MetaPreview::media_item).collect();
        assert_eq!(items[0].as_ref().unwrap().year, Some(2023));
        assert_eq!(items[1].as_ref().unwrap().kind, MediaKind::Series);
        assert_eq!(items[1].as_ref().unwrap().poster, None);
    }

    #[test]
    fn series_metadata_converts_and_stays_spoiler_safe() {
        let response: MetaResponse = serde_json::from_str(
            r#"{"meta":{
            "id":"tt123","type":"series","name":"Fixture Show","year":"2024",
            "poster":"https://img.invalid/poster.jpg","videos":[
                {"id":"tt123:2:3","season":2,"episode":3,"title":"A HUGE SPOILER",
                 "overview":"A SECRET","thumbnail":"https://img.invalid/episode.jpg"},
                {"id":"tt123:2:1","season":2,"episode":1}
            ]}}"#,
        )
        .unwrap();
        let media = response.meta.media_item().unwrap();
        assert_eq!(media.year, Some(2024));
        assert_eq!(media.kind, MediaKind::Series);
        let seasons = seasons_from_episodes(response.meta.episodes());
        assert_eq!(
            seasons[0].episodes[0].display_label(SpoilerPolicy::default()),
            "S02E01 · Episode 1"
        );
        assert!(!seasons[0].episodes[1]
            .display_label(SpoilerPolicy::default())
            .contains("SPOILER"));
        assert_eq!(
            seasons[0].episodes[1].display_thumbnail(SpoilerPolicy::default()),
            None
        );
    }

    #[test]
    fn unsupported_stream_transports_are_not_advertised_as_direct_playback() {
        let torrent: Stream = serde_json::from_str(r#"{"infoHash":"abc","fileIdx":2}"#).unwrap();
        let yt: Stream = serde_json::from_str(r#"{"ytId":"abc"}"#).unwrap();
        assert!(torrent.playable_source("fixture", "s1").is_none());
        assert!(yt.playable_source("fixture", "s2").is_none());
        let unsafe_url: Stream = serde_json::from_str(r#"{"url":"file:///etc/passwd"}"#).unwrap();
        assert!(unsafe_url.playable_source("fixture", "s3").is_none());
    }

    #[test]
    fn stable_source_identity_never_contains_a_temporary_url() {
        let stream: Stream =
            serde_json::from_str(r#"{"url":"https://cdn.invalid/video?sig=secret"}"#).unwrap();
        let source = stream.playable_source("fixture", "result-0").unwrap();
        assert_eq!(source.id, "result-0");
        assert!(!source.id.contains("secret"));
    }

    #[test]
    fn subtitles_convert_without_requiring_an_account_model() {
        let result: SubtitlesResponse = serde_json::from_str(
            r#"{"subtitles":[
            {"id":"en-1","url":"https://sub.invalid/en.vtt","lang":"eng","label":"English CC"}
        ]}"#,
        )
        .unwrap();
        let candidate = result.subtitles[0].candidate("fixture");
        assert_eq!(candidate.language, "eng");
        assert_eq!(candidate.provider, "fixture");
    }
}
