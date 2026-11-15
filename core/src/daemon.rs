//! The sync daemon: accepts and dials paired peers, resolves simultaneous dials,
//! reconnects with backoff, and drives the sync engine over the live sessions.

use crate::connection::{Backoff, Conn, ConnState, Offer, Registry, Resolver};
use crate::pairing::{Incoming, Node, PairingAttempt};
use crate::sync::{node_id_from_public_key, NodeId, SyncEvent, SyncNode};
use crate::transport::{Error, Listener, Result, Session};
use clipsync_clipboard::ClipboardBackend;
use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Default TCP listening port.
pub const DEFAULT_PORT: u16 = 43872;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const RECV_TIMEOUT: Duration = Duration::from_secs(10);
/// Application-level heartbeat period.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// A peer silent for this long is declared dead.
pub const DEAD_AFTER: Duration = Duration::from_secs(5);
const ACCEPTORS: usize = 4;
const POLL: Duration = Duration::from_millis(10);

pub struct DaemonConfig {
    pub node: Node,
    pub listener: Listener,
    pub resolver: Arc<dyn Resolver>,
    /// Keep dialing every paired device with backoff. Tests turn this off to dial manually.
    pub auto_dial: bool,
}

struct Handoff {
    session: Session,
    peer: NodeId,
    token: u64,
}

struct Shared {
    node: Node,
    local_id: NodeId,
    resolver: Arc<dyn Resolver>,
    registry: Registry,
    to_sync: Mutex<Sender<Handoff>>,
    stop: AtomicBool,
    /// Bumped by `network_changed`; dial loops wake and drop their backoff when it moves.
    kick: AtomicU64,
    established: AtomicU64,
    paused: AtomicBool,
    auto_dial: bool,
    /// Peers that have a running dial loop.
    dialers: Mutex<HashSet<Vec<u8>>>,
    /// Receives unpinned peers that connect during a pairing window.
    pairing_handler: Mutex<Option<PairingHandler>>,
    events: Mutex<Vec<SyncEvent>>,
}

/// Called with each incoming pairing attempt; must not block.
pub type PairingHandler = Box<dyn Fn(PairingAttempt) + Send + Sync>;

pub struct Daemon {
    shared: Arc<Shared>,
    local_addr: std::net::SocketAddr,
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Shared {
    fn network_changed(&self) {
        self.kick.fetch_add(1, Ordering::SeqCst);
        self.registry.reset();
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Sleep up to `d`, returning early (true) if the daemon is stopping.
    fn sleep(&self, d: Duration) -> bool {
        let end = std::time::Instant::now() + d;
        while std::time::Instant::now() < end {
            if self.stopped() {
                return true;
            }
            thread::sleep(Duration::from_millis(50).min(d));
        }
        self.stopped()
    }

    /// Like `sleep`, but also returns early (true) when `network_changed` was called
    /// since the kick generation `seen`.
    fn sleep_unless_kicked(&self, d: Duration, seen: u64) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.stopped() || self.kick.load(Ordering::SeqCst) != seen {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.stopped() || self.kick.load(Ordering::SeqCst) != seen
    }

    fn offer(&self, session: Session, dialed: bool) {
        let peer = node_id_from_public_key(session.remote_static());
        let Ok(handle) = session.socket_handle() else {
            return;
        };
        let conn = Conn {
            local: self.local_id,
            remote: peer,
            dialed,
        };
        if let Offer::Accepted(token) = self.registry.offer(conn, handle) {
            let _ = self.to_sync.lock().unwrap().send(Handoff {
                session,
                peer,
                token,
            });
        }
    }

    /// One dial attempt: re-resolve the peer's addresses, try each in order.
    fn dial_once(&self, peer_key: &[u8]) -> Result<()> {
        let peer = node_id_from_public_key(peer_key);
        self.registry.set_dialing(peer, true);
        let result = self.try_addrs(peer_key, &peer);
        self.registry.set_dialing(peer, false);
        let session = result?;
        self.offer(session, true);
        Ok(())
    }

    fn try_addrs(&self, peer_key: &[u8], peer: &NodeId) -> Result<Session> {
        let mut last = Error::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "no address for peer",
        ));
        for addr in self.resolver.resolve(peer) {
            match self.node.dial_trusted_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(s) if s.remote_static() == peer_key => return Ok(s),
                Ok(_) => last = Error::UnknownPeer, // someone else lives at that address
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

impl Daemon {
    /// Start the daemon. `make_clipboard` runs on the sync thread, so backends that are
    /// not `Send` (native OS handles) are fine.
    pub fn start<C, F>(cfg: DaemonConfig, make_clipboard: F) -> Daemon
    where
        C: ClipboardBackend + 'static,
        F: FnOnce() -> C + Send + 'static,
    {
        let (tx, rx) = channel();
        let public = cfg.node.keypair().public.clone();
        let shared = Arc::new(Shared {
            local_id: node_id_from_public_key(&public),
            node: cfg.node,
            resolver: cfg.resolver,
            registry: Registry::new(),
            to_sync: Mutex::new(tx),
            stop: AtomicBool::new(false),
            kick: AtomicU64::new(0),
            established: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            auto_dial: cfg.auto_dial,
            dialers: Mutex::new(HashSet::new()),
            pairing_handler: Mutex::new(None),
            events: Mutex::new(Vec::new()),
        });
        let local_addr = cfg.listener.local_addr();
        let mut threads = Vec::new();

        let s = shared.clone();
        threads.push(thread::spawn(move || {
            sync_loop(&s, SyncNode::new(make_clipboard(), &public), rx)
        }));

        // Several acceptors: a stalled half-open connection must not block the others
        // (a handshake can take up to 5 s to time out).
        let listener = Arc::new(cfg.listener);
        for _ in 0..ACCEPTORS {
            let (s, l) = (shared.clone(), listener.clone());
            threads.push(thread::spawn(move || accept_loop(&s, &l)));
        }

        let daemon = Daemon {
            shared,
            local_addr,
            threads: Arc::new(Mutex::new(threads)),
        };
        for key in daemon.shared.node.paired_devices().unwrap_or_default() {
            daemon.add_peer(&key);
        }
        daemon
    }

    /// The daemon's node: use it (not a copy with a separate identity) to drive pairing.
    pub fn node(&self) -> &Node {
        &self.shared.node
    }

    /// Route unpinned peers that connect while a pairing window is open to `handler`.
    pub fn set_pairing_handler(&self, handler: PairingHandler) {
        *self.shared.pairing_handler.lock().unwrap() = Some(handler);
    }

    /// Start dialing a device that was paired after startup (no-op if already dialed
    /// or when auto-dial is off).
    pub fn add_peer(&self, peer_key: &[u8]) {
        if !self.shared.auto_dial || !self.shared.dialers.lock().unwrap().insert(peer_key.to_vec())
        {
            return;
        }
        let s = self.shared.clone();
        let key = peer_key.to_vec();
        self.threads
            .lock()
            .unwrap()
            .push(thread::spawn(move || dial_loop(&s, key)));
    }

    /// Unpair a device: delete its pinned key and drop its live connection. Its dial
    /// loop notices the missing key and stops.
    pub fn remove_peer(&self, peer_key: &[u8]) -> Result<()> {
        self.shared.node.remove_device(peer_key)
    }

    /// Drain sync events (oversize or dropped items) raised since the last call.
    pub fn take_events(&self) -> Vec<SyncEvent> {
        std::mem::take(&mut *self.shared.events.lock().unwrap())
    }

    /// Pause or resume clipboard sync. Connections stay up; nothing is sent or applied while paused.
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::SeqCst)
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    pub fn peer_state(&self, peer_key: &[u8]) -> ConnState {
        self.shared
            .registry
            .state(&node_id_from_public_key(peer_key))
    }

    /// Number of live connections (at most one per peer).
    pub fn connection_count(&self) -> usize {
        self.shared.registry.count()
    }

    /// Whether the live connection to the peer was dialed by us (None if not connected).
    pub fn active_dialed(&self, peer_key: &[u8]) -> Option<bool> {
        self.shared
            .registry
            .dialed(&node_id_from_public_key(peer_key))
    }

    /// Total number of connections that completed the handshake since start.
    pub fn connections_established(&self) -> u64 {
        self.shared.established.load(Ordering::SeqCst)
    }

    /// The OS reported sleep/wake or a network change: tear down every connection now
    /// and make the dial loops re-probe immediately, skipping any backoff delay.
    pub fn network_changed(&self) {
        self.shared.network_changed();
    }

    /// A callback equivalent to `network_changed`, for OS hooks that outlive borrows of the daemon.
    pub fn change_hook(&self) -> Arc<dyn Fn() + Send + Sync> {
        let shared = self.shared.clone();
        Arc::new(move || {
            shared.network_changed();
        })
    }

    /// Make a single dial attempt right now.
    pub fn dial_now(&self, peer_key: &[u8]) -> Result<()> {
        self.shared.dial_once(peer_key)
    }

    pub fn shutdown(self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.registry.close_all();
        // Wake each blocking acceptor before joining all daemon threads.
        for _ in 0..ACCEPTORS {
            let _ = std::net::TcpStream::connect_timeout(&self.local_addr, Duration::from_millis(500));
        }
        let threads: Vec<_> = self.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }
}

fn accept_loop(s: &Shared, listener: &Listener) {
    while !s.stopped() {
        match s.node.accept(listener) {
            Ok(Incoming::Trusted(session)) => s.offer(session, false),
            Ok(Incoming::Pairing(att)) => {
                if let Some(h) = s.pairing_handler.lock().unwrap().as_ref() {
                    h(att);
                }
            }
            Err(Error::Io(_)) => {
                s.sleep(Duration::from_millis(50));
            }
            Err(_) => {}
        }
    }
}

fn dial_loop(s: &Shared, peer_key: Vec<u8>) {
    let peer = node_id_from_public_key(&peer_key);
    let mut backoff = Backoff::new();
    let mut seen = s.kick.load(Ordering::SeqCst);
    while !s.stopped() {
        let now = s.kick.load(Ordering::SeqCst);
        if now != seen {
            seen = now;
            backoff.reset();
        }
        if !s.node.paired_devices().is_ok_and(|d| d.contains(&peer_key)) {
            // Unpaired. Re-check under the lock so a concurrent re-pair is not lost.
            let mut dialers = s.dialers.lock().unwrap();
            if !s.node.paired_devices().is_ok_and(|d| d.contains(&peer_key)) {
                dialers.remove(&peer_key);
                return;
            }
            continue;
        }
        if s.registry.state(&peer) != ConnState::Disconnected {
            s.sleep(Duration::from_millis(100));
            continue;
        }
        match s.dial_once(&peer_key) {
            Ok(()) => {
                backoff.reset();
                // Give the new session time to establish before judging it lost.
                s.sleep(Duration::from_millis(100));
            }
            Err(_) => {
                s.sleep_unless_kicked(backoff.next_delay(), seen);
            }
        }
    }
}

struct Active {
    session: Session,
    peer: NodeId,
    token: u64,
    last_heard: Instant,
    last_ping: Instant,
}

fn sync_loop<C: ClipboardBackend>(s: &Shared, mut node: SyncNode<C>, rx: Receiver<Handoff>) {
    let mut active: Vec<Active> = Vec::new();
    while !s.stopped() {
        node.set_paused(s.paused.load(Ordering::SeqCst));
        let evs = node.take_events();
        if !evs.is_empty() {
            s.events.lock().unwrap().extend(evs);
        }
        loop {
            let h = if active.is_empty() {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(h) => h,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match rx.try_recv() {
                    Ok(h) => h,
                    Err(_) => break,
                }
            };
            let mut session = h.session;
            let _ = session.set_read_timeout(Some(HELLO_TIMEOUT));
            if node.handshake(&mut session).is_ok() {
                let _ = session.set_read_timeout(None);
                s.registry.mark_connected(&h.peer, h.token);
                s.established.fetch_add(1, Ordering::SeqCst);
                let now = Instant::now();
                active.push(Active {
                    session,
                    peer: h.peer,
                    token: h.token,
                    last_heard: now,
                    last_ping: now,
                });
            } else {
                s.registry.release(&h.peer, h.token);
            }
        }

        if active.is_empty() { node.discard_local(); }
        let mut i = 0;
        while i < active.len() {
            let a = &mut active[i];
            if a.last_ping.elapsed() >= HEARTBEAT_INTERVAL {
                a.last_ping = Instant::now();
                if node.ping(&mut a.session).is_err() {
                    let dead = active.swap_remove(i);
                    s.registry.release(&dead.peer, dead.token);
                    continue;
                }
            }
            let ok = a.last_heard.elapsed() < DEAD_AFTER
                && node.pump_local(&mut a.session).is_ok()
                && match a.session.wait_readable(POLL) {
                    Ok(false) => true,
                    Ok(true) => {
                        let _ = a.session.set_read_timeout(Some(RECV_TIMEOUT));
                        let r = node.recv_one(&mut a.session).is_ok();
                        a.last_heard = Instant::now();
                        r
                    }
                    Err(_) => false,
                };
            if ok {
                i += 1;
            } else {
                let dead = active.swap_remove(i);
                s.registry.release(&dead.peer, dead.token);
            }
        }
    }
}
