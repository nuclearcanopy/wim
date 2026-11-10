//! Loopback HTTP CONNECT/forward proxy for mpv network streams. The proxy
//! resolves each destination once, rejects restricted address sets, and
//! connects to the checked socket addresses directly. Redirects naturally
//! return through mpv and are checked as new proxy connections.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::stremio::is_public_address;

const MAX_HEADERS: usize = 32 * 1024;
const MAX_CONNECTIONS: usize = 32;

#[derive(Clone, Debug)]
struct AllowedOrigin {
    host: String,
    port: u16,
}

/// A temporary local relay. Its address is loopback-only and its debug
/// representation contains no destination or media URL.
pub struct MediaProxy {
    address: SocketAddr,
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
                            let spawned = thread::Builder::new()
                                .name("wim-media-relay".into())
                                .spawn(move || {
                                    let _guard = ActiveGuard(connection_active);
                                    let _ = handle_client(stream, &origins);
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
            stop,
            worker: Some(worker),
        })
    }

    pub fn proxy_url(&self) -> String {
        format!("http://{}:{}", self.address.ip(), self.address.port())
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

fn handle_client(mut client: TcpStream, origins: &[AllowedOrigin]) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(15)))?;
    client.set_write_timeout(Some(Duration::from_secs(15)))?;
    let head = read_head(&mut client)?;
    let request = std::str::from_utf8(&head).map_err(invalid_data)?;
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
    while head.len() < MAX_HEADERS {
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
        && addresses
            .iter()
            .all(|address| is_public_address(address.ip()) || origin_allowed)
}

fn tunnel(mut client: TcpStream, mut upstream: TcpStream) -> io::Result<()> {
    client.set_read_timeout(None)?;
    client.set_write_timeout(None)?;
    upstream.set_read_timeout(None)?;
    upstream.set_write_timeout(None)?;
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
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
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
        write!(client, "GET http://127.0.0.1:{port}/movie.mkv?range=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        assert!(response.ends_with(b"OK"));
        server.join().unwrap();
    }
}
