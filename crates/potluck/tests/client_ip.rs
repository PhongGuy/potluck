//! Client address resolution behind trusted proxies.

use std::net::IpAddr;

use potluck::api::client_ip::{ClientIp, resolve};

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
