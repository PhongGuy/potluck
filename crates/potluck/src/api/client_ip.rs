//! The client address of a request: the TCP peer, or what a trusted proxy forwarded.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use super::AppState;

/// Where a request came from, for rate limiting. An IPv4-mapped IPv6 address is held as IPv4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClientIp {
    /// A TCP peer, or the client a trusted proxy forwarded for.
    Peer(IpAddr),
    /// The admin socket: no network address.
    Local,
}

impl ClientIp {
    /// The rate-limiter key: `local`; `ip:<a.b.c.d>` for IPv4 (an IPv4-mapped IPv6 address is
    /// unmapped first); `ip:<prefix>::/64` for IPv6, the address masked to its first 64 bits, so
    /// one host's whole /64 shares a window.
    pub fn key(&self) -> String {
        match self {
            ClientIp::Local => "local".to_owned(),
            ClientIp::Peer(addr) => match unmap(*addr) {
                IpAddr::V4(v4) => format!("ip:{v4}"),
                // The zeroed interface half prints as a trailing `::`, giving `2001:db8::/64`.
                IpAddr::V6(v6) => {
                    let prefix = Ipv6Addr::from_bits(v6.to_bits() & !0xffff_ffff_ffff_ffff);
                    format!("ip:{prefix}/64")
                }
            },
        }
    }
}

/// The client. Without a peer: `Local`. With an untrusted peer: the peer. With a trusted peer:
/// `forwarded_for` (every `X-Forwarded-For` line, in order) is joined with commas and walked from
/// the right; the first entry that is not trusted is the client; the peer again when there is no
/// entry, every entry is trusted, or an entry does not parse as an `IpAddr`. Peer and entries
/// are unmapped (`::ffff:a.b.c.d` becomes `a.b.c.d`) before `trusts` sees them and before keying.
pub fn resolve(
    peer: Option<IpAddr>,
    forwarded_for: &[&str],
    trusts: &dyn Fn(IpAddr) -> bool,
) -> ClientIp {
    let Some(peer) = peer.map(unmap) else {
        return ClientIp::Local;
    };
    if !trusts(peer) {
        return ClientIp::Peer(peer);
    }
    // Each proxy appends the address it saw, so only the right end is trustworthy: skip our own
    // proxies from the right and stop at the first hop we did not add ourselves.
    let joined = forwarded_for.join(",");
    for entry in joined.rsplit(',') {
        let Ok(hop) = entry.trim().parse::<IpAddr>().map(unmap) else {
            break;
        };
        if !trusts(hop) {
            return ClientIp::Peer(hop);
        }
    }
    ClientIp::Peer(peer)
}

fn unmap(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        IpAddr::V4(_) => addr,
    }
}

/// Peer from `ConnectInfo<SocketAddr>` in the extensions (absent over the socket), every
/// `x-forwarded-for` value via `HeaderMap::get_all`, `state.config().trusts_proxy`.
impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Read the extension directly: the `ConnectInfo` extractor would fail over the socket.
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip());
        // A line that is not valid text cannot be walked, so it reads as an unparsable hop.
        let lines: Vec<&str> = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .map(|value| value.to_str().unwrap_or("!"))
            .collect();
        let config = state.config();
        Ok(resolve(peer, &lines, &|ip| config.trusts_proxy(ip)))
    }
}
