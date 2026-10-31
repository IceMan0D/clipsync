//! Real mDNS announce/browse between two instances in one process on the host's own
//! announceable interface. Skips itself when the host has none, and on CI.

use clipsync_core::connection::Resolver;
use clipsync_core::discovery::{announce_addrs, system_interfaces, Discovery};
use std::time::{Duration, Instant};

#[test]
fn announced_node_is_resolved_by_another_browser() {
    // CI runners' virtualised interfaces don't loop multicast back; run this locally.
    if std::env::var_os("CI").is_some() {
        eprintln!("CI runner without multicast loopback; skipping");
        return;
    }
    let addrs = announce_addrs(&system_interfaces());
    let Some(ip) = addrs.iter().find(|a| a.is_ipv4()).copied() else {
        eprintln!("no announceable interface; skipping");
        return;
    };
    let (a, b) = ([0xAA; 32], [0xBB; 32]);
    let da = Discovery::start(a, 43872, &[ip]).unwrap();
    let db = Discovery::start(b, 43999, &[ip]).unwrap();

    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && (da.resolve(&b).is_empty() || db.resolve(&a).is_empty()) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(da.resolve(&b), vec![std::net::SocketAddr::new(ip, 43999)]);
    assert_eq!(db.resolve(&a), vec![std::net::SocketAddr::new(ip, 43872)]);
    assert!(da.resolve(&a).is_empty(), "must not resolve itself");
    // A wake/network refresh must retain a working browser and reannounce.
    da.refresh(&[ip]).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && (da.resolve(&b).is_empty() || db.resolve(&a).is_empty()) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(da.resolve(&b), vec![std::net::SocketAddr::new(ip, 43999)]);
    assert_eq!(db.resolve(&a), vec![std::net::SocketAddr::new(ip, 43872)]);
}
