//! LAN discovery: which interface addresses to announce, and mDNS announce/browse.

use crate::connection::Resolver;
use crate::sync::NodeId;
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::thread;

pub const SERVICE_TYPE: &str = "_clipsync._tcp.local.";
const TXT_ID: &str = "id";

/// A network interface address, as reported by the OS (or injected by tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceInfo {
    pub name: String,
    pub ip: IpAddr,
}

/// Name fragments (lowercase) of virtual, bridge, tunnel and VM adapters.
const VIRTUAL_NAME_MARKERS: &[&str] = &[
    "vethernet", "hyper-v", "wsl", "vmware", "vmnet", "vmenet", "virtualbox", "vboxnet",
    "docker", "veth", "virbr", "br-", "bridge", "utun", "awdl", "llw", "anpi", "tun", "tap",
    "loopback", "tailscale",
];

/// Whether an interface name looks like a virtual/bridge/VM adapter.
pub fn is_virtual_interface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    VIRTUAL_NAME_MARKERS.iter().any(|m| n.contains(m))
}

/// Link-local (169.254.0.0/16, fe80::/10) or loopback addresses.
pub fn is_unannounceable_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local() || v4.is_loopback() || v4.is_unspecified(),
        IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80 || v6.is_loopback() || v6.is_unspecified()
        }
    }
}

/// Addresses to announce over mDNS: physical interfaces only, no link-local.
pub fn announce_addrs(ifaces: &[IfaceInfo]) -> Vec<IpAddr> {
    ifaces
        .iter()
        .filter(|i| !is_virtual_interface(&i.name) && !is_unannounceable_ip(&i.ip))
        .map(|i| i.ip)
        .collect()
}

/// Interfaces that are operationally up, as reported by the OS.
pub fn system_interfaces() -> Vec<IfaceInfo> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.is_oper_up())
        .map(|i| IfaceInfo { name: i.name.clone(), ip: i.ip() })
        .collect()
}

#[derive(Debug, thiserror::Error)]
#[error("mdns: {0}")]
pub struct DiscoveryError(String);

impl From<mdns_sd::Error> for DiscoveryError {
    fn from(e: mdns_sd::Error) -> Self {
        DiscoveryError(e.to_string())
    }
}

type Peers = Arc<Mutex<HashMap<String, (NodeId, Vec<SocketAddr>)>>>;

/// mDNS announcer and browser for `_clipsync._tcp`. Implements `Resolver` over the
/// continuously refreshed set of peers seen on the LAN.
pub struct Discovery {
    daemon: ServiceDaemon,
    peers: Peers,
}

impl Discovery {
    /// Announce this node on exactly `addrs` (use `announce_addrs(&system_interfaces())`)
    /// and start browsing for peers. mDNS traffic is restricted to the interfaces of
    /// those addresses, so virtual and link-local adapters are never used.
    pub fn start(local_id: NodeId, port: u16, addrs: &[IpAddr]) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        daemon.disable_interface(IfKind::All)?;
        for a in addrs {
            daemon.enable_interface(IfKind::Addr(*a))?;
        }
        let hex_id = hex::encode(local_id);
        let instance = &hex_id[..16];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            &format!("clipsync-{instance}.local."),
            addrs,
            port,
            &[(TXT_ID, hex_id.as_str())][..],
        )?;
        daemon.register(info)?;

        let peers: Peers = Arc::default();
        let rx = daemon.browse(SERVICE_TYPE)?;
        let map = peers.clone();
        thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                match ev {
                    ServiceEvent::ServiceResolved(svc) => {
                        let Some(id) = svc
                            .get_property_val_str(TXT_ID)
                            .and_then(|h| hex::decode(h).ok())
                            .and_then(|b| NodeId::try_from(b).ok())
                        else {
                            continue;
                        };
                        if id == local_id {
                            continue;
                        }
                        let addrs: Vec<SocketAddr> = svc
                            .get_addresses_v4()
                            .into_iter()
                            .filter(|a| !a.is_link_local())
                            .map(|a| SocketAddr::new(IpAddr::V4(a), svc.get_port()))
                            .collect();
                        map.lock().unwrap().insert(svc.get_fullname().to_string(), (id, addrs));
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        map.lock().unwrap().remove(&fullname);
                    }
                    _ => {}
                }
            }
        });
        Ok(Discovery { daemon, peers })
    }
}

impl Resolver for Discovery {
    fn resolve(&self, node: &NodeId) -> Vec<SocketAddr> {
        self.peers
            .lock()
            .unwrap()
            .values()
            .filter(|(id, _)| id == node)
            .flat_map(|(_, a)| a.clone())
            .collect()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, ip: &str) -> IfaceInfo {
        IfaceInfo {
            name: name.into(),
            ip: ip.parse().unwrap(),
        }
    }

    #[test]
    fn announces_ordinary_lan_interfaces() {
        let ifs = [iface("en0", "192.168.1.20"), iface("Ethernet", "10.0.0.5")];
        assert_eq!(
            announce_addrs(&ifs),
            vec!["192.168.1.20".parse::<IpAddr>().unwrap(), "10.0.0.5".parse().unwrap()]
        );
    }

    #[test]
    fn skips_link_local_and_loopback() {
        let ifs = [
            iface("en0", "169.254.7.7"),
            iface("en1", "fe80::1"),
            iface("lo0", "127.0.0.1"),
            iface("en2", "192.168.1.9"),
        ];
        assert_eq!(announce_addrs(&ifs), vec!["192.168.1.9".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn skips_virtual_and_bridge_interfaces() {
        let names = [
            "vEthernet (WSL)",
            "vEthernet (Default Switch)",
            "VMware Network Adapter VMnet8",
            "VirtualBox Host-Only Network",
            "Hyper-V Virtual Ethernet Adapter",
            "docker0",
            "br-1a2b3c",
            "bridge100",
            "vmenet0",
            "utun3",
            "awdl0",
            "veth12ab",
            "virbr0",
            "vboxnet0",
        ];
        for n in names {
            assert!(
                announce_addrs(&[iface(n, "192.168.99.1")]).is_empty(),
                "{n} should be skipped"
            );
        }
        assert_eq!(announce_addrs(&[iface("Wi-Fi", "192.168.1.2")]).len(), 1);
    }
}
