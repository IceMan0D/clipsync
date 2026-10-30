//! Candidate addresses for a paired peer: mDNS-discovered LAN addresses first, then
//! manually entered ones (Tailscale 100.x IP, MagicDNS name, anything else) in order.
//! Host names are re-resolved on every call, so every retry sees fresh DNS.

use crate::connection::Resolver;
use crate::daemon::DEFAULT_PORT;
use crate::sync::NodeId;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid address {0:?}")]
pub struct ParseError(pub String);

/// A user-entered address: an IP or host name with an optional port (default 43872).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAddr {
    Ip(SocketAddr),
    Host(String, u16),
}

impl PeerAddr {
    /// Accepts `1.2.3.4`, `1.2.3.4:port`, `name`, `name:port`, `[v6]`, `[v6]:port`, bare `v6`.
    pub fn parse(input: &str) -> Result<PeerAddr, ParseError> {
        let err = || ParseError(input.to_string());
        let s = input.trim();
        if s.is_empty() {
            return Err(err());
        }
        if let Ok(sa) = s.parse::<SocketAddr>() {
            return Ok(PeerAddr::Ip(sa));
        }
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(PeerAddr::Ip(SocketAddr::new(ip, DEFAULT_PORT)));
        }
        if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            let ip = inner.parse::<std::net::Ipv6Addr>().map_err(|_| err())?;
            return Ok(PeerAddr::Ip(SocketAddr::new(ip.into(), DEFAULT_PORT)));
        }
        let (host, port) = match s.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| err())?),
            None => (s, DEFAULT_PORT),
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        let valid = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|l| {
                !l.is_empty()
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if !valid || port == 0 {
            return Err(err());
        }
        Ok(PeerAddr::Host(host.to_string(), port))
    }
}

impl std::str::FromStr for PeerAddr {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, ParseError> {
        PeerAddr::parse(s)
    }
}

/// Host name lookup; injectable so tests need no real DNS.
pub trait HostLookup: Send + Sync {
    fn lookup(&self, host: &str, port: u16) -> Vec<SocketAddr>;
}

pub struct SystemLookup;

impl HostLookup for SystemLookup {
    fn lookup(&self, host: &str, port: u16) -> Vec<SocketAddr> {
        (host, port).to_socket_addrs().map(|i| i.collect()).unwrap_or_default()
    }
}

/// Resolver combining a primary resolver (mDNS) with per-peer manual addresses.
pub struct CandidateResolver {
    primary: Option<Arc<dyn Resolver>>,
    lookup: Arc<dyn HostLookup>,
    manual: Mutex<HashMap<NodeId, Vec<PeerAddr>>>,
}

impl CandidateResolver {
    pub fn new(primary: Option<Arc<dyn Resolver>>) -> Self {
        Self::with_lookup(primary, Arc::new(SystemLookup))
    }

    pub fn with_lookup(primary: Option<Arc<dyn Resolver>>, lookup: Arc<dyn HostLookup>) -> Self {
        CandidateResolver { primary, lookup, manual: Mutex::new(HashMap::new()) }
    }

    /// Replace the manual address list for a peer.
    pub fn set_manual(&self, node: NodeId, addrs: Vec<PeerAddr>) {
        self.manual.lock().unwrap().insert(node, addrs);
    }

    /// Append a manual address (ignored if already present).
    pub fn add_manual(&self, node: NodeId, addr: PeerAddr) {
        let mut g = self.manual.lock().unwrap();
        let list = g.entry(node).or_default();
        if !list.contains(&addr) {
            list.push(addr);
        }
    }

    pub fn manual(&self, node: &NodeId) -> Vec<PeerAddr> {
        self.manual.lock().unwrap().get(node).cloned().unwrap_or_default()
    }
}

impl Resolver for CandidateResolver {
    fn resolve(&self, node: &NodeId) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> =
            self.primary.as_ref().map(|p| p.resolve(node)).unwrap_or_default();
        for a in self.manual(node) {
            let found = match a {
                PeerAddr::Ip(sa) => vec![sa],
                PeerAddr::Host(h, p) => self.lookup.lookup(&h, p),
            };
            for sa in found {
                if !out.contains(&sa) {
                    out.push(sa);
                }
            }
        }
        out
    }
}

/// Plain-text persistence of manual addresses: one `<hex node id> <address>` per line.
impl CandidateResolver {
    pub fn to_text(&self) -> String {
        let g = self.manual.lock().unwrap();
        let mut ids: Vec<_> = g.keys().collect();
        ids.sort();
        let mut out = String::new();
        for id in ids {
            for a in &g[id] {
                let addr = match a {
                    PeerAddr::Ip(sa) => sa.to_string(),
                    PeerAddr::Host(h, p) => format!("{h}:{p}"),
                };
                out.push_str(&format!("{} {addr}\n", hex::encode(id)));
            }
        }
        out
    }

    /// Load entries from `to_text` output; malformed lines are skipped.
    pub fn load_text(&self, text: &str) {
        for line in text.lines() {
            let Some((id, addr)) = line.split_once(' ') else { continue };
            let Ok(bytes) = hex::decode(id) else { continue };
            let Ok(id) = NodeId::try_from(bytes.as_slice()) else { continue };
            if let Ok(a) = PeerAddr::parse(addr) {
                self.add_manual(id, a);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_addresses_round_trip_through_text_preserving_order() {
        let r = CandidateResolver::new(None);
        let id = [3; 32];
        r.add_manual(id, PeerAddr::parse("100.64.0.1").unwrap());
        r.add_manual(id, PeerAddr::parse("box.ts.net:5000").unwrap());
        let r2 = CandidateResolver::new(None);
        r2.load_text(&format!("garbage\n{}", r.to_text()));
        assert_eq!(r2.manual(&id), r.manual(&id));
        assert_eq!(r2.manual(&id).len(), 2);
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_bare_and_ported_addresses_with_default_port_43872() {
        let cases = [
            ("100.64.1.2", PeerAddr::Ip(sa("100.64.1.2:43872"))),
            ("100.64.1.2:5000", PeerAddr::Ip(sa("100.64.1.2:5000"))),
            ("[fd7a::1]", PeerAddr::Ip(sa("[fd7a::1]:43872"))),
            ("fd7a::1", PeerAddr::Ip(sa("[fd7a::1]:43872"))),
            ("[fd7a::1]:9", PeerAddr::Ip(sa("[fd7a::1]:9"))),
            ("laptop.tail1234.ts.net", PeerAddr::Host("laptop.tail1234.ts.net".into(), 43872)),
            ("laptop:7000", PeerAddr::Host("laptop".into(), 7000)),
            ("  laptop.ts.net. ", PeerAddr::Host("laptop.ts.net".into(), 43872)),
        ];
        for (input, want) in cases {
            assert_eq!(PeerAddr::parse(input), Ok(want), "{input}");
        }
    }

    #[test]
    fn rejects_malformed_addresses() {
        for bad in [
            "", "host:", "host:99999", "host:0", ":43872", "a b", "-x.com", "a..b", "[1.2.3.4]",
            "host:port",
        ] {
            assert!(PeerAddr::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    const PEER: NodeId = [7; 32];

    struct Fixed(Vec<SocketAddr>);
    impl Resolver for Fixed {
        fn resolve(&self, _: &NodeId) -> Vec<SocketAddr> {
            self.0.clone()
        }
    }

    #[derive(Default)]
    struct FakeDns {
        table: Mutex<HashMap<String, Vec<IpAddr>>>,
        calls: Mutex<usize>,
    }
    impl FakeDns {
        fn set(&self, host: &str, ip: &str) {
            self.table.lock().unwrap().insert(host.into(), vec![ip.parse().unwrap()]);
        }
    }
    impl HostLookup for FakeDns {
        fn lookup(&self, host: &str, port: u16) -> Vec<SocketAddr> {
            *self.calls.lock().unwrap() += 1;
            let ips = self.table.lock().unwrap().get(host).cloned().unwrap_or_default();
            ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect()
        }
    }

    #[test]
    fn candidates_are_mdns_first_then_manual_entries_in_order_without_duplicates() {
        let dns = Arc::new(FakeDns::default());
        dns.set("box.ts.net", "100.64.0.9");
        let r = CandidateResolver::with_lookup(
            Some(Arc::new(Fixed(vec![sa("192.168.1.5:43872")]))),
            dns,
        );
        r.add_manual(PEER, PeerAddr::parse("100.64.0.1").unwrap());
        r.add_manual(PEER, PeerAddr::parse("box.ts.net").unwrap());
        r.add_manual(PEER, PeerAddr::parse("192.168.1.5").unwrap()); // dup of mDNS
        r.add_manual(PEER, PeerAddr::parse("100.64.0.1").unwrap()); // dup entry
        assert_eq!(
            r.resolve(&PEER),
            vec![sa("192.168.1.5:43872"), sa("100.64.0.1:43872"), sa("100.64.0.9:43872")]
        );
        assert_eq!(r.manual(&[1; 32]), vec![]);
    }

    #[test]
    fn magicdns_names_are_resolved_again_on_every_resolve() {
        let dns = Arc::new(FakeDns::default());
        let r = CandidateResolver::with_lookup(None, dns.clone());
        r.set_manual(PEER, vec![PeerAddr::parse("box.ts.net:5000").unwrap()]);
        assert!(r.resolve(&PEER).is_empty()); // not resolvable yet
        dns.set("box.ts.net", "100.64.0.2");
        assert_eq!(r.resolve(&PEER), vec![sa("100.64.0.2:5000")]);
        dns.set("box.ts.net", "100.64.0.3");
        assert_eq!(r.resolve(&PEER), vec![sa("100.64.0.3:5000")]);
        assert_eq!(*dns.calls.lock().unwrap(), 3);
    }
}
