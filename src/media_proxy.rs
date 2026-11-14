//! Loopback HTTP CONNECT/forward proxy for mpv network streams. The proxy
//! resolves each destination once, rejects restricted address sets, and
//! connects to the checked socket addresses directly. Redirects naturally
//! return through mpv and are checked as new proxy connections.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::stremio::is_public_address;

const MAX_HEADERS: usize = 32 * 1024;
const MAX_CONNECTIONS: usize = 32;
/// The whole request head must arrive in this long; a per-byte timeout
/// let one client hold a slot for MAX_HEADERS x 15s.
const HEAD_DEADLINE: Duration = Duration::from_secs(15);
/// An established tunnel with no traffic either way for this long is
/// dropped; mpv reconnects a paused stream on demand.
const TUNNEL_IDLE: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
struct AllowedOrigin {
    host: String,
    port: u16,
}

/// A temporary local relay. Its address is loopback-only and its debug
/// representation contains no destination or media URL.
pub struct MediaProxy {
    address: SocketAddr,
    /// Basic-auth password every request must present (user `wim`), so a
    /// local process that merely finds the port cannot use the relay.
    token: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for MediaProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaProxy")
            .field("address", &"[loopback proxy]")
            .finish()
    }
}

impl MediaProxy {
    pub fn start(trusted_origins: &[String]) -> io::Result<Self> {
        let origins = trusted_origins
            .iter()
            .filter_map(|raw| parse_trusted_origin(raw))
            .collect::<Vec<_>>();
        Self::start_with_origins(origins)
    }

    fn start_with_origins(origins: Vec<AllowedOrigin>) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let token = {
            use rand::Rng;
            let mut bytes = [0u8; 18];
            rand::thread_rng().fill(&mut bytes);
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        };
        let expected_auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("wim:{token}"))
        );
        let worker = thread::Builder::new()
            .name("wim-media-proxy".into())
            .spawn(move || {
                let active = Arc::new(AtomicUsize::new(0));
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                                active.fetch_sub(1, Ordering::AcqRel);
                                drop(stream);
                                continue;
                            }
                            let connection_active = Arc::clone(&active);
                            let origins = origins.clone();
                            let expected_auth = expected_auth.clone();
                            let spawned = thread::Builder::new()
                                .name("wim-media-relay".into())
                                .spawn(move || {
                                    let _guard = ActiveGuard(connection_active);
                                    let _ = handle_client(stream, &origins, &expected_auth);
                                });
                            if spawned.is_err() {
                                active.fetch_sub(1, Ordering::AcqRel);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(50)),
                    }
                }
            })?;
        Ok(Self {
            address,
            token,
            stop,
            worker: Some(worker),
        })
    }

    #[cfg(test)]
    fn auth_header(&self) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("wim:{}", self.token))
        )
    }

    pub fn proxy_url(&self) -> String {
        format!(
            "http://wim:{}@{}:{}",
            self.token,
            self.address.ip(),
            self.address.port()
        )
    }
}

impl Drop for MediaProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn parse_trusted_origin(raw: &str) -> Option<AllowedOrigin> {
    let url = url::Url::parse(raw).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(())?;
    Some(AllowedOrigin {
        host: url.host_str()?.trim_end_matches('.').to_ascii_lowercase(),
        port: url.port_or_known_default()?,
    })
}

fn handle_client(
    mut client: TcpStream,
    origins: &[AllowedOrigin],
    expected_auth: &str,
) -> io::Result<()> {
    client.set_write_timeout(Some(Duration::from_secs(15)))?;
    let head = read_head(&mut client)?;
    let request = std::str::from_utf8(&head).map_err(invalid_data)?;
    let authorized = request.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("proxy-authorization") && value.trim() == expected_auth
        })
    });
    if !authorized {
        client.write_all(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"wim\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "missing proxy credentials",
        ));
    }
    let request_line = request
        .lines()
        .next()
        .ok_or_else(|| invalid_data("missing request line"))?;
    let mut fields = request_line.split_ascii_whitespace();
    let method = fields
        .next()
        .ok_or_else(|| invalid_data("missing method"))?;
    let target = fields
        .next()
        .ok_or_else(|| invalid_data("missing target"))?;
    match method {
        "CONNECT" => relay_connect(client, target, origins),
        "GET" | "HEAD" => relay_http(client, &head, target, method, origins),
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsupported proxy method",
        )),
    }
}

fn read_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0];
    let deadline = Instant::now() + HEAD_DEADLINE;
    while head.len() < MAX_HEADERS {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "proxy request head too slow",
            ));
        }
        stream.set_read_timeout(Some(left))?;
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "proxy headers too large",
    ))
}

fn relay_connect(
    mut client: TcpStream,
    authority: &str,
    origins: &[AllowedOrigin],
) -> io::Result<()> {
    let target = url::Url::parse(&format!("https://{authority}/")).map_err(invalid_data)?;
    if !target.username().is_empty() || target.password().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "credentials in CONNECT authority are not allowed",
        ));
    }
    let host = target
        .host_str()
        .ok_or_else(|| invalid_data("missing CONNECT host"))?;
    let port = target
        .port_or_known_default()
        .ok_or_else(|| invalid_data("missing CONNECT port"))?;
    let upstream = connect_checked(host, port, origins)?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    tunnel(client, upstream)
}

fn relay_http(
    client: TcpStream,
    head: &[u8],
    target: &str,
    method: &str,
    origins: &[AllowedOrigin],
) -> io::Result<()> {
    let url = url::Url::parse(target).map_err(invalid_data)?;
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "only absolute HTTP URLs are accepted",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| invalid_data("missing HTTP host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| invalid_data("missing HTTP port"))?;
    let mut upstream = connect_checked(host, port, origins)?;
    let path = if url.query().is_some() {
        format!("{}?{}", url.path(), url.query().unwrap_or_default())
    } else {
        url.path().to_owned()
    };
    write!(upstream, "{method} {path} HTTP/1.1\r\n")?;
    let headers = std::str::from_utf8(head).map_err(invalid_data)?;
    let mut host_seen = false;
    for line in headers.lines().skip(1) {
        if line.is_empty() {
            continue;
        }
        let (name, _) = line
            .split_once(':')
            .ok_or_else(|| invalid_data("invalid proxy header"))?;
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        host_seen |= name.eq_ignore_ascii_case("host");
        writeln!(upstream, "{line}")?;
    }
    if !host_seen {
        write!(upstream, "Host: {}\r\n", host_header(&url))?;
    }
    upstream.write_all(b"Connection: close\r\n\r\n")?;
    tunnel(client, upstream)
}

fn host_header(url: &url::Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => String::new(),
    }
}

fn connect_checked(host: &str, port: u16, origins: &[AllowedOrigin]) -> io::Result<TcpStream> {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    let origin_allowed = origins
        .iter()
        .any(|origin| origin.host == normalized && origin.port == port);
    // an IPv6 literal arrives bracketed from the url crate
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        (host, port).to_socket_addrs()?.collect::<Vec<_>>()
    };
    if !validate_addresses(&addresses, origin_allowed) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "destination resolves to a restricted address",
        ));
    }
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, Duration::from_secs(10)) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AddrNotAvailable, "no destination addresses")
    }))
}

fn validate_addresses(addresses: &[SocketAddr], origin_allowed: bool) -> bool {
    !addresses.is_empty()
        && addresses.iter().all(|address| {
            is_public_address(address.ip()) || (origin_allowed && trusted_may_use(address.ip()))
        })
}

/// A configured server may live on the LAN or loopback, but never on a
/// link-local (cloud metadata), unspecified, multicast or broadcast
/// address, whatever its DNS name claims.
fn trusted_may_use(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_link_local() || ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast())
        }
        IpAddr::V6(ip) => {
            let link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
            !(link_local || ip.is_unspecified() || ip.is_multicast())
        }
    }
}

fn tunnel(mut client: TcpStream, mut upstream: TcpStream) -> io::Result<()> {
    client.set_read_timeout(Some(TUNNEL_IDLE))?;
    client.set_write_timeout(Some(TUNNEL_IDLE))?;
    upstream.set_read_timeout(Some(TUNNEL_IDLE))?;
    upstream.set_write_timeout(Some(TUNNEL_IDLE))?;
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let outgoing = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });
    let result = io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(std::net::Shutdown::Write);
    let _ = outgoing.join();
    result.map(|_| ())
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_configured_origins_may_resolve_to_private_addresses() {
        let origins = vec![AllowedOrigin {
            host: "nas.home".into(),
            port: 4533,
        }];
        assert!(connect_checked("127.0.0.1", 80, &origins).is_err());
        assert!(origins
            .iter()
            .any(|origin| origin.host == "nas.home" && origin.port == 4533));
        assert!(!is_public_address("100.64.0.1".parse().unwrap()));
        assert!(!is_public_address("2001:db8::1".parse().unwrap()));
        assert!(is_public_address("8.8.8.8".parse().unwrap()));
        let mixed = vec![
            "8.8.8.8:443".parse().unwrap(),
            "10.0.0.2:443".parse().unwrap(),
        ];
        assert!(!validate_addresses(&mixed, false));
        assert!(validate_addresses(&mixed, true));
    }

    #[test]
    fn proxy_binds_only_to_loopback_and_has_no_url_in_debug() {
        let proxy = MediaProxy::start(&[]).unwrap();
        assert!(proxy.address.ip().is_loopback());
        assert!(!format!("{proxy:?}").contains("http://"));
        assert_eq!(proxy.proxy_url().split(':').next(), Some("http"));
        assert!(!format!("{proxy:?}").contains(&proxy.token));
    }

    #[test]
    fn a_request_without_the_token_is_refused_with_407() {
        let proxy = MediaProxy::start(&[]).unwrap();
        let mut client = TcpStream::connect(proxy.address).unwrap();
        write!(
            client,
            "GET http://8.8.8.8/ HTTP/1.1\r\nHost: 8.8.8.8\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 407"), "{response}");
    }

    #[test]
    fn trusted_hosts_still_cannot_reach_link_local_or_unspecified() {
        for bad in [
            "169.254.169.254:80",
            "0.0.0.0:80",
            "[fe80::1]:80",
            "224.0.0.1:80",
        ] {
            let addr: SocketAddr = bad.parse().unwrap();
            assert!(!validate_addresses(&[addr], true), "{bad}");
        }
        assert!(validate_addresses(
            &["192.168.1.5:4533".parse().unwrap()],
            true
        ));
    }

    #[test]
    fn connect_tunnel_uses_the_configured_socket_and_relays_bytes() {
        let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = origin.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = origin.accept().unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"PING");
            stream.write_all(b"PONG").unwrap();
        });
        let proxy = MediaProxy::start_with_origins(vec![AllowedOrigin {
            host: "127.0.0.1".into(),
            port,
        }])
        .unwrap();
        let mut client = TcpStream::connect(proxy.address).unwrap();
        write!(
            client,
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nProxy-Authorization: {}\r\n\r\n",
            proxy.auth_header()
        )
        .unwrap();
        let response = read_head(&mut client).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 Connection Established"));
        client.write_all(b"PING").unwrap();
        let mut response = [0; 4];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"PONG");
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn absolute_http_requests_are_rewritten_to_origin_form() {
        let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = origin.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = origin.accept().unwrap();
            let request = read_head(&mut stream).unwrap();
            assert!(
                String::from_utf8_lossy(&request).starts_with("GET /movie.mkv?range=1 HTTP/1.1")
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
        });
        let proxy = MediaProxy::start_with_origins(vec![AllowedOrigin {
            host: "127.0.0.1".into(),
            port,
        }])
        .unwrap();
        let mut client = TcpStream::connect(proxy.address).unwrap();
        write!(client, "GET http://127.0.0.1:{port}/movie.mkv?range=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nProxy-Authorization: {}\r\n\r\n", proxy.auth_header()).unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        assert!(response.ends_with(b"OK"));
        server.join().unwrap();
    }

    /// mpv must accept the credentialed proxy url and answer the 407
    /// challenge itself; run with `--ignored` where mpv is installed.
    #[test]
    #[ignore = "spawns a real mpv"]
    fn real_mpv_streams_through_the_authenticated_relay() {
        let mut wav = Vec::new();
        let samples = 4000u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + samples * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[1, 0, 1, 0]);
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&[2, 0, 16, 0]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples * 2).to_le_bytes());
        wav.extend(std::iter::repeat_n(0u8, (samples * 2) as usize));
        let origin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = origin.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&hits);
        thread::spawn(move || {
            for stream in origin.incoming() {
                let Ok(mut stream) = stream else { break };
                let wav = wav.clone();
                let served = Arc::clone(&served);
                thread::spawn(move || {
                    if read_head(&mut stream).is_ok() {
                        served.fetch_add(1, Ordering::SeqCst);
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            wav.len()
                        );
                        let _ = stream.write_all(&wav);
                    }
                });
            }
        });
        let proxy = MediaProxy::start_with_origins(vec![AllowedOrigin {
            host: "127.0.0.1".into(),
            port,
        }])
        .unwrap();
        let status = std::process::Command::new("mpv")
            .args(["--no-config", "--no-video", "--ao=null", "--vo=null"])
            .arg(format!("--http-proxy={}", proxy.proxy_url()))
            .arg(format!("http://127.0.0.1:{port}/a.wav"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mpv exited {status}");
        assert!(hits.load(Ordering::SeqCst) >= 1, "origin never reached");
    }
}
