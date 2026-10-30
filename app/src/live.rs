//! The real `Backend`: drives the running daemon, discovery and address book.
//! Thin glue over core; the logic worth testing lives in `commands` / `pairing_ui`.

use clipsync_app::commands::Backend;
use clipsync_app::model::Device;
use clipsync_core::addresses::{CandidateResolver, PeerAddr};
use clipsync_core::daemon::Daemon;
use clipsync_core::discovery::Discovery;
use clipsync_core::pairing::{PairingAttempt, PairingStatus};
use clipsync_core::sync::{node_id_from_public_key, NodeId};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DIAL_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Pairing {
    pending: Mutex<Option<PairingAttempt>>,
    /// A confirm exchange is in flight.
    deciding: AtomicBool,
    finished: Mutex<Option<Result<(), String>>>,
    /// Bumped to cancel the dial thread of a previous window.
    generation: AtomicU64,
}

pub struct Live {
    daemon: Arc<Daemon>,
    book: Arc<CandidateResolver>,
    discovery: Arc<Discovery>,
    pairing: Arc<Pairing>,
}

impl Live {
    pub fn new(daemon: Arc<Daemon>, book: Arc<CandidateResolver>, discovery: Arc<Discovery>) -> Self {
        let pairing = Arc::new(Pairing::default());
        let p = pairing.clone();
        daemon.set_pairing_handler(Box::new(move |att| {
            let mut pending = p.pending.lock().unwrap();
            if pending.is_none() && !p.deciding.load(Ordering::SeqCst) {
                *pending = Some(att);
            }
        }));
        Live { daemon, book, discovery, pairing }
    }
}

fn fmt_addr(a: &PeerAddr) -> String {
    match a {
        PeerAddr::Ip(sa) => sa.to_string(),
        PeerAddr::Host(h, p) => format!("{h}:{p}"),
    }
}

fn targets(addr: &PeerAddr) -> Vec<SocketAddr> {
    match addr {
        PeerAddr::Ip(sa) => vec![*sa],
        PeerAddr::Host(h, p) => (h.as_str(), *p).to_socket_addrs().map(|i| i.collect()).unwrap_or_default(),
    }
}

/// Unpaired LAN peers this side should dial: only the lower node id dials, so exactly
/// one side initiates and both users see the same attempt.
fn lan_targets(live: &Live) -> Vec<SocketAddr> {
    let node = live.daemon.node();
    let me = node_id_from_public_key(&node.keypair().public);
    let paired: Vec<NodeId> = node
        .paired_devices()
        .unwrap_or_default()
        .iter()
        .map(|k| node_id_from_public_key(k))
        .collect();
    live.discovery
        .discovered()
        .into_iter()
        .filter(|(id, _)| me < *id && !paired.contains(id))
        .flat_map(|(_, addrs)| addrs)
        .collect()
}

impl Backend for Live {
    fn paired(&self) -> Vec<Device> {
        self.daemon
            .node()
            .paired_devices()
            .unwrap_or_default()
            .into_iter()
            .map(|key| Device { status: self.daemon.peer_state(&key).into(), key })
            .collect()
    }

    fn addresses(&self, key: &[u8]) -> Vec<String> {
        self.book.manual(&node_id_from_public_key(key)).iter().map(fmt_addr).collect()
    }

    fn remove(&self, key: &[u8]) -> Result<(), String> {
        self.daemon.remove_peer(key).map_err(|e| e.to_string())?;
        self.book.set_manual(node_id_from_public_key(key), vec![]);
        super::save_book(&self.book).map_err(|e| e.to_string())
    }

    fn add_address(&self, key: &[u8], addr: PeerAddr) -> Result<(), String> {
        self.book.add_manual(node_id_from_public_key(key), addr);
        super::save_book(&self.book).map_err(|e| e.to_string())
    }

    fn open_pairing(&self, addr: Option<PeerAddr>) {
        let p = self.pairing.clone();
        let gen = p.generation.fetch_add(1, Ordering::SeqCst) + 1;
        p.pending.lock().unwrap().take();
        p.finished.lock().unwrap().take();
        self.daemon.node().start_pairing();

        let (daemon, discovery) = (self.daemon.clone(), self.discovery.clone());
        let helper = Live {
            daemon: daemon.clone(),
            book: self.book.clone(),
            discovery,
            pairing: p.clone(),
        };
        thread::spawn(move || {
            let node = daemon.node().clone();
            while p.generation.load(Ordering::SeqCst) == gen && node.pairing_status().open_for.is_some() {
                let busy = p.pending.lock().unwrap().is_some() || p.deciding.load(Ordering::SeqCst);
                if !busy {
                    let candidates = match &addr {
                        Some(a) => targets(a),
                        None => lan_targets(&helper),
                    };
                    for sa in candidates {
                        let Ok(att) = node.dial_pairing_timeout(&sa, DIAL_TIMEOUT) else { continue };
                        // The peer drops us right away when its window is closed.
                        if att.peer_closed() {
                            continue;
                        }
                        if p.generation.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        let mut pending = p.pending.lock().unwrap();
                        if pending.is_none() {
                            *pending = Some(att);
                        }
                        break;
                    }
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    }

    fn close_pairing(&self) {
        self.pairing.generation.fetch_add(1, Ordering::SeqCst);
        self.daemon.node().stop_pairing();
        self.pairing.pending.lock().unwrap().take();
    }

    fn pairing_status(&self) -> PairingStatus {
        self.daemon.node().pairing_status()
    }

    fn pending_sas(&self) -> Option<String> {
        self.pairing.pending.lock().unwrap().as_ref().map(|a| a.sas())
    }

    fn decide(&self, confirmed: bool) {
        let Some(att) = self.pairing.pending.lock().unwrap().take() else { return };
        self.pairing.deciding.store(true, Ordering::SeqCst);
        let (p, daemon) = (self.pairing.clone(), self.daemon.clone());
        thread::spawn(move || {
            let key = att.peer_key();
            let result = match att.confirm(confirmed) {
                Ok(_session) => {
                    daemon.add_peer(&key);
                    Ok(())
                }
                Err(_) => Err("Pairing failed: the codes were not confirmed on both devices in time.".to_string()),
            };
            *p.finished.lock().unwrap() = Some(result);
            p.deciding.store(false, Ordering::SeqCst);
        });
    }

    fn take_finished(&self) -> Option<Result<(), String>> {
        self.pairing.finished.lock().unwrap().take()
    }
}
