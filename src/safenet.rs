//! Guard rails for fetching things a remote response told us to fetch,
//! and for reading bodies without trusting their size.

use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{bail, Result};
use serde::de::DeserializeOwned;

use crate::stremio::is_public_address;

/// Reads a response body as JSON, refusing one larger than `limit` bytes
/// instead of buffering whatever the server sends.
pub fn json_capped<T: DeserializeOwned>(response: ureq::Response, limit: u64) -> Result<T> {
    let mut body = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > limit {
        bail!("response larger than {} KiB", limit >> 10);
    }
    Ok(serde_json::from_slice(&body)?)
}

/// Same cap for a raw body.
pub fn bytes_capped(response: ureq::Response, limit: u64) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > limit {
        bail!("response larger than {} KiB", limit >> 10);
    }
    Ok(body)
}

/// An agent for urls that came out of a response body: https only, and
/// every hop (redirects included) resolves to public addresses only, so a
/// hostile or compromised api cannot aim wim at localhost or the LAN.
pub fn public_https_agent(user_agent: &str) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(user_agent)
        .https_only(true)
        .redirects(3)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .resolver(|netloc: &str| -> std::io::Result<Vec<SocketAddr>> {
            let public: Vec<SocketAddr> = netloc
                .to_socket_addrs()?
                .filter(|addr| is_public_address(addr.ip()))
                .collect();
            if public.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "host resolves to a private or special-use address",
                ));
            }
            Ok(public)
        })
        .build()
}

#[cfg(test)]
mod public_agent_tests {
    use super::*;

    #[test]
    fn loopback_and_plain_http_are_refused_before_any_request_is_sent() {
        let agent = public_https_agent("wim-test");
        assert!(agent.get("https://127.0.0.1:1/x").call().is_err());
        assert!(agent.get("http://example.com/x").call().is_err());
    }
}
