//! The client address of a request: the TCP peer, or what a trusted proxy forwarded.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use super::AppState;

/// Where a request came from, for rate limiting. An IPv4-mapped IPv6 address is held as IPv4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub(crate) enum ClientIp {
    /// A TCP peer, or the client a trusted proxy forwarded for.
    Peer(IpAddr),
    /// The admin socket: no network address.
    Local,
}

impl ClientIp {
    /// The rate-limiter key: `local`; `ip:<a.b.c.d>` for IPv4 (an IPv4-mapped IPv6 address is
    /// unmapped first); `ip:<prefix>::/64` for IPv6, the address masked to its first 64 bits, so
    /// one host's whole /64 shares a window.
    pub(crate) fn key(&self) -> String {
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
pub(crate) fn resolve(
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

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::{ClientIp, resolve};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn trusted(list: &'static [&'static str]) -> impl Fn(IpAddr) -> bool {
        move |a| list.iter().any(|c| ip(c) == a)
    }

    #[test]
    fn without_a_peer_it_is_local() {
        assert_eq!(resolve(None, &["1.2.3.4"], &|_| true), ClientIp::Local);
        assert_eq!(ClientIp::Local.key(), "local");
    }

    #[test]
    fn an_untrusted_peer_is_the_client_whatever_it_forwards() {
        let peer = ip("203.0.113.9");
        assert_eq!(
            resolve(Some(peer), &["1.2.3.4"], &|_| false),
            ClientIp::Peer(peer)
        );
        assert_eq!(ClientIp::Peer(peer).key(), "ip:203.0.113.9");
    }

    #[test]
    fn addresses_in_one_ipv6_64_share_a_key() {
        let a = ClientIp::Peer(ip("2001:db8:1:2:3:4:5:6")).key();
        let b = ClientIp::Peer(ip("2001:db8:1:2:ffff:ffff:ffff:ffff")).key();
        assert_eq!(a, "ip:2001:db8:1:2::/64");
        assert_eq!(a, b, "one /64 is one window");
        assert_ne!(
            a,
            ClientIp::Peer(ip("2001:db8:1:3::1")).key(),
            "the next /64 is another"
        );
        assert_eq!(ClientIp::Peer(ip("2001:db8::5")).key(), "ip:2001:db8::/64");
    }

    #[test]
    fn a_trusted_proxy_yields_the_forwarded_client() {
        let t = trusted(&["10.0.0.1", "10.0.0.2"]);
        let proxy = Some(ip("10.0.0.1"));
        let client = ClientIp::Peer(ip("198.51.100.7"));
        assert_eq!(resolve(proxy, &["198.51.100.7"], &t), client);
        assert_eq!(
            resolve(proxy, &["198.51.100.7, 10.0.0.2"], &t),
            client,
            "trusted hops are skipped from the right"
        );
        assert_eq!(
            resolve(proxy, &["1.1.1.1, 198.51.100.7"], &t),
            client,
            "a client-supplied left entry is ignored"
        );
        assert_eq!(
            resolve(proxy, &[" 2001:db8::5 "], &t),
            ClientIp::Peer(ip("2001:db8::5")),
            "whitespace and IPv6"
        );
    }

    #[test]
    fn header_lines_are_joined_in_order() {
        let t = trusted(&["10.0.0.1"]);
        let proxy = Some(ip("10.0.0.1"));
        // The attacker sent the first line; the proxy appended its own with the real client.
        assert_eq!(
            resolve(proxy, &["1.1.1.1", "198.51.100.7"], &t),
            ClientIp::Peer(ip("198.51.100.7"))
        );
        assert_eq!(
            resolve(proxy, &["1.1.1.1, 2.2.2.2", "198.51.100.7, 10.0.0.1"], &t),
            ClientIp::Peer(ip("198.51.100.7"))
        );
        // A line holding only a trusted hop must not hide the client named on an earlier line.
        assert_eq!(
            resolve(proxy, &["198.51.100.7", "10.0.0.1"], &t),
            ClientIp::Peer(ip("198.51.100.7"))
        );
        assert_eq!(resolve(proxy, &[], &t), ClientIp::Peer(ip("10.0.0.1")));
    }

    #[test]
    fn a_trusted_proxy_without_a_usable_header_is_itself_the_client() {
        let t = trusted(&["10.0.0.1"]);
        let proxy = ip("10.0.0.1");
        assert_eq!(resolve(Some(proxy), &[""], &t), ClientIp::Peer(proxy));
        assert_eq!(
            resolve(Some(proxy), &["not-an-ip"], &t),
            ClientIp::Peer(proxy)
        );
        assert_eq!(
            resolve(Some(proxy), &["10.0.0.1"], &t),
            ClientIp::Peer(proxy),
            "only trusted hops"
        );
        assert_eq!(
            resolve(Some(proxy), &["198.51.100.7, garbage"], &t),
            ClientIp::Peer(proxy),
            "a malformed hop ends the walk"
        );
    }

    #[test]
    fn a_port_suffix_is_malformed() {
        let t = trusted(&["10.0.0.1"]);
        let proxy = ip("10.0.0.1");
        assert_eq!(
            resolve(Some(proxy), &["198.51.100.7:5678"], &t),
            ClientIp::Peer(proxy)
        );
        assert_eq!(
            resolve(Some(proxy), &["[2001:db8::5]"], &t),
            ClientIp::Peer(proxy)
        );
    }

    #[test]
    fn an_ipv4_mapped_peer_is_keyed_as_ipv4() {
        let t = trusted(&["10.0.0.1"]);
        assert_eq!(
            resolve(Some(ip("::ffff:203.0.113.9")), &[], &|_| false),
            ClientIp::Peer(ip("203.0.113.9"))
        );
        assert_eq!(
            ClientIp::Peer(ip("::ffff:203.0.113.9")).key(),
            "ip:203.0.113.9",
            "key() unmaps too"
        );
        assert_eq!(
            resolve(Some(ip("::ffff:10.0.0.1")), &["198.51.100.7"], &t),
            ClientIp::Peer(ip("198.51.100.7")),
            "a mapped proxy is trusted"
        );
    }
}
