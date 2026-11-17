//! Headless end-to-end tests of the daemon over loopback TCP with mock clipboards
//! and an injected address resolver.

use clipsync_clipboard::{ClipboardBackend, ClipboardContent, ClipboardError, MockClipboard};
use clipsync_core::connection::{ConnState, Resolver};
use clipsync_core::daemon::{Daemon, DaemonConfig};
use clipsync_core::keystore::{KeyStore, MemoryKeyStore};
use clipsync_core::pairing::Node;
use clipsync_core::sync::{node_id_from_public_key, NodeId};
use clipsync_core::transport::{Keypair, Listener};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
struct SharedClip(Arc<Mutex<MockClipboard>>);

impl ClipboardBackend for SharedClip {
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        self.0.lock().unwrap().read()
    }
    fn write(&mut self, c: ClipboardContent) -> Result<(), ClipboardError> {
        self.0.lock().unwrap().write(c)
    }
    fn watch(&mut self) -> Result<Receiver<Result<ClipboardContent, ClipboardError>>, ClipboardError> {
        self.0.lock().unwrap().watch()
    }
}

#[derive(Default)]
struct StaticResolver(Mutex<HashMap<NodeId, Vec<SocketAddr>>>);

impl StaticResolver {
    fn set(&self, id: NodeId, addrs: Vec<SocketAddr>) {
        self.0.lock().unwrap().insert(id, addrs);
    }
}

impl Resolver for StaticResolver {
    fn resolve(&self, id: &NodeId) -> Vec<SocketAddr> {
        self.0.lock().unwrap().get(id).cloned().unwrap_or_default()
    }
}

struct Side {
    public: Vec<u8>,
    id: NodeId,
    addr: SocketAddr,
    clip: SharedClip,
    resolver: Arc<StaticResolver>,
    daemon: Option<Daemon>,
    // held until the daemon is started
    pending: Option<(Node, Listener)>,
}

fn side(trusted: &[&Keypair], kp: Keypair) -> Side {
    let store = Arc::new(MemoryKeyStore::default());
    for t in trusted {
        store.add(&t.public).unwrap();
    }
    let public = kp.public.clone();
    let node = Node::new(kp, store);
    let listener = Listener::bind("127.0.0.1:0", node.keypair().clone(), vec![]).unwrap();
    Side {
        id: node_id_from_public_key(&public),
        public,
        addr: listener.local_addr(),
        clip: SharedClip::default(),
        resolver: Arc::new(StaticResolver::default()),
        daemon: None,
        pending: Some((node, listener)),
    }
}

impl Side {
    fn start(&mut self, auto_dial: bool) {
        let (node, listener) = self.pending.take().unwrap();
        let clip = self.clip.clone();
        self.daemon = Some(Daemon::start(
            DaemonConfig {
                node,
                listener,
                resolver: self.resolver.clone(),
                auto_dial,
            },
            move || clip,
        ));
    }

    fn d(&self) -> &Daemon {
        self.daemon.as_ref().unwrap()
    }

    fn copy(&self, s: &str) {
        self.clip
            .clone()
            .write(ClipboardContent::Text(s.into()))
            .unwrap();
    }

    fn pasted(&self) -> Option<ClipboardContent> {
        self.clip.read().unwrap()
    }
}

/// The spec's one-second sync target holds locally; shared CI runners (notably Windows) are
/// too jittery to assert it, so they get a looser bound.
fn sync_deadline() -> Duration {
    if std::env::var_os("CI").is_some() {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(1)
    }
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    f()
}

/// Wait until both sides are connected, then until the connection has stopped churning.
/// When both sides dial at once, the loser of the simultaneous-dial tie-break is torn down
/// shortly after the first `Connected`; on slow runners (Windows CI) that can land in the
/// middle of a test and look like a lost sync or a spurious reconnect.
fn connect_and_settle(a: &Side, b: &Side) {
    assert!(wait_until(Duration::from_secs(10), || {
        a.d().peer_state(&b.public) == ConnState::Connected
            && b.d().peer_state(&a.public) == ConnState::Connected
    }));
    let total = || a.d().connections_established() + b.d().connections_established();
    let mut last = total();
    let mut stable_since = Instant::now();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && stable_since.elapsed() < Duration::from_millis(1500) {
        thread::sleep(Duration::from_millis(50));
        let now = total();
        if now != last {
            last = now;
            stable_since = Instant::now();
        }
    }
    assert!(wait_until(Duration::from_secs(5), || {
        a.d().peer_state(&b.public) == ConnState::Connected
            && b.d().peer_state(&a.public) == ConnState::Connected
    }));
}

fn two_sides() -> (Side, Side) {
    let (ka, kb) = (Keypair::generate(), Keypair::generate());
    let a = side(&[&kb], ka.clone());
    let b = side(&[&ka], kb);
    a.resolver.set(b.id, vec![b.addr]);
    b.resolver.set(a.id, vec![a.addr]);
    (a, b)
}

#[test]
fn paired_daemons_find_each_other_and_sync_text_both_ways_within_a_second() {
    let (mut a, mut b) = two_sides();
    a.start(true);
    b.start(true);
    connect_and_settle(&a, &b);

    let t = Instant::now();
    a.copy("from a");
    assert!(wait_until(sync_deadline(), || b.pasted()
        == Some(ClipboardContent::Text("from a".into()))));
    assert!(t.elapsed() < sync_deadline());

    b.copy("from b");
    assert!(wait_until(sync_deadline(), || a.pasted()
        == Some(ClipboardContent::Text("from b".into()))));
}

#[test]
fn simultaneous_dials_leave_exactly_one_connection_initiated_by_the_lower_node_id() {
    let (mut a, mut b) = two_sides();
    a.start(false);
    b.start(false);
    let (da, db) = (a.d(), b.d());
    thread::scope(|s| {
        s.spawn(|| da.dial_now(&b.public).unwrap());
        s.spawn(|| db.dial_now(&a.public).unwrap());
    });
    assert!(wait_until(Duration::from_secs(3), || {
        da.peer_state(&b.public) == ConnState::Connected
            && db.peer_state(&a.public) == ConnState::Connected
    }));
    // let the loser be torn down on both sides
    thread::sleep(Duration::from_millis(500));
    assert_eq!(da.connection_count(), 1);
    assert_eq!(db.connection_count(), 1);

    let a_is_lower = a.id < b.id;
    assert_eq!(da.active_dialed(&b.public), Some(a_is_lower));
    assert_eq!(db.active_dialed(&a.public), Some(!a_is_lower));

    // the surviving connection carries data
    a.copy("after glare");
    assert!(wait_until(sync_deadline(), || b.pasted()
        == Some(ClipboardContent::Text("after glare".into()))));
}

#[test]
fn dialer_retries_with_fresh_addresses_until_the_peer_appears() {
    let (mut a, mut b) = two_sides();
    // a cannot resolve b yet
    a.resolver.set(b.id, vec![]);
    a.start(true);
    thread::sleep(Duration::from_millis(300));
    assert_ne!(a.d().peer_state(&b.public), ConnState::Connected);

    // b shows up in "mDNS"; a's next attempt (backoff 1 s) must re-resolve it
    a.resolver.set(b.id, vec![b.addr]);
    b.resolver.set(a.id, vec![]);
    b.start(false);
    assert!(wait_until(Duration::from_secs(4), || {
        a.d().peer_state(&b.public) == ConnState::Connected
    }));
}

#[test]
fn unpaired_peer_never_connects() {
    let (ka, kb) = (Keypair::generate(), Keypair::generate());
    let mut a = side(&[], ka); // trusts nobody
    let mut b = side(&[&a_pub(&a)], kb);
    b.resolver.set(a.id, vec![a.addr]);
    a.start(false);
    b.start(true);
    // Count only handshaken sessions: a connection attempt may sit in the registry for a
    // moment before the untrusting side rejects it.
    let end = Instant::now() + Duration::from_millis(800);
    while Instant::now() < end {
        assert_ne!(a.d().peer_state(&b.public), ConnState::Connected);
        assert_ne!(b.d().peer_state(&a.public), ConnState::Connected);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(a.d().connections_established(), 0);
    assert_eq!(b.d().connections_established(), 0);
}

fn a_pub(a: &Side) -> Keypair {
    Keypair::from_parts(a.public.clone(), vec![0; 32])
}

#[test]
fn pausing_stops_sending_and_applying_and_resuming_restores_sync() {
    let (mut a, mut b) = two_sides();
    a.start(true);
    b.start(true);
    connect_and_settle(&a, &b);

    // b paused: a's copy is not applied on b
    b.d().set_paused(true);
    assert!(b.d().is_paused());
    thread::sleep(Duration::from_millis(200));
    a.copy("while b paused");
    thread::sleep(Duration::from_millis(500));
    assert_ne!(b.pasted(), Some(ClipboardContent::Text("while b paused".into())));

    // b paused: b's local copy is not sent to a
    b.copy("b local while paused");
    thread::sleep(Duration::from_millis(500));
    assert_ne!(a.pasted(), Some(ClipboardContent::Text("b local while paused".into())));

    // resume: new copies flow again, paused-era copies are not replayed
    b.d().set_paused(false);
    thread::sleep(Duration::from_millis(200));
    assert_ne!(a.pasted(), Some(ClipboardContent::Text("b local while paused".into())));
    a.copy("after resume");
    assert!(wait_until(sync_deadline(), || b.pasted()
        == Some(ClipboardContent::Text("after resume".into()))));
    b.copy("b after resume");
    assert!(wait_until(sync_deadline(), || a.pasted()
        == Some(ClipboardContent::Text("b after resume".into()))));
}

#[test]
fn idle_connection_stays_up_past_the_dead_peer_timeout_thanks_to_heartbeats() {
    let (mut a, mut b) = two_sides();
    a.start(true);
    b.start(true);
    connect_and_settle(&a, &b);
    let before = a.d().connections_established() + b.d().connections_established();
    thread::sleep(Duration::from_secs(7));
    assert_eq!(a.d().peer_state(&b.public), ConnState::Connected);
    assert_eq!(b.d().peer_state(&a.public), ConnState::Connected);
    assert_eq!(
        a.d().connections_established() + b.d().connections_established(),
        before,
        "no reconnects while idle"
    );
}

#[test]
fn silent_peer_is_declared_dead_within_five_seconds() {
    let (ka, kb) = (Keypair::generate(), Keypair::generate());
    let mut a = side(&[&kb], ka.clone());
    a.start(false);
    // A paired peer that completes the handshake and then says nothing (e.g. suspended).
    let peer = Node::new(kb.clone(), {
        let s = Arc::new(MemoryKeyStore::default());
        s.add(&ka.public).unwrap();
        s
    });
    let mut session = peer
        .dial_trusted_timeout(&a.addr, Duration::from_secs(3))
        .unwrap();
    session.send(0, &0u64.to_be_bytes()).unwrap(); // hello
    let b_pub = kb.public.clone();
    assert!(wait_until(Duration::from_secs(2), || {
        a.d().peer_state(&b_pub) == ConnState::Connected
    }));
    let t = Instant::now();
    assert!(wait_until(Duration::from_secs(6), || {
        a.d().peer_state(&b_pub) == ConnState::Disconnected
    }));
    let elapsed = t.elapsed();
    assert!(elapsed >= Duration::from_secs(4), "too eager: {elapsed:?}");
    assert!(elapsed <= Duration::from_millis(5600), "too slow: {elapsed:?}");
}

#[test]
fn network_change_forces_immediate_reconnect_and_drops_items_copied_while_down() {
    let (mut a, mut b) = two_sides();
    a.start(true);
    b.start(false);
    connect_and_settle(&a, &b);

    // The network goes away: a cannot find b, and tears down on the change notification.
    a.resolver.set(b.id, vec![]);
    a.d().network_changed();
    // b only notices when its socket read sees the close, which is slow on Windows CI
    assert!(wait_until(Duration::from_secs(10), || {
        a.d().peer_state(&b.public) == ConnState::Disconnected
            && b.d().peer_state(&a.public) == ConnState::Disconnected
    }));
    // let a's backoff grow so only the notification can make the next dial prompt
    thread::sleep(Duration::from_millis(3500));
    a.copy("while offline");

    // The network returns.
    a.resolver.set(b.id, vec![b.addr]);
    let t = Instant::now();
    a.d().network_changed();
    assert!(wait_until(Duration::from_secs(3), || {
        a.d().peer_state(&b.public) == ConnState::Connected
            && b.d().peer_state(&a.public) == ConnState::Connected
    }));
    assert!(t.elapsed() < Duration::from_secs(3));

    thread::sleep(Duration::from_millis(500));
    assert_eq!(b.pasted(), None, "offline copy must not be delivered");
    a.copy("after reconnect");
    assert!(wait_until(sync_deadline(), || b.pasted()
        == Some(ClipboardContent::Text("after reconnect".into()))));
}

#[test]
fn removing_a_peer_disconnects_it_and_it_is_not_redialed() {
    let (mut a, mut b) = two_sides();
    a.start(true);
    b.start(true);
    connect_and_settle(&a, &b);

    a.d().remove_peer(&b.public).unwrap();
    assert!(a.d().node().paired_devices().unwrap().is_empty());
    assert!(wait_until(Duration::from_secs(3), || {
        a.d().peer_state(&b.public) == ConnState::Disconnected
    }));
    // b keeps dialing but a no longer trusts it; a does not dial b either
    thread::sleep(Duration::from_millis(2500));
    assert_eq!(a.d().peer_state(&b.public), ConnState::Disconnected);
    a.copy("secret");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(b.pasted(), None);
}

#[test]
fn a_device_paired_while_running_is_dialed_without_restart() {
    let (ka, kb) = (Keypair::generate(), Keypair::generate());
    let (mut a, mut b) = (side(&[], ka), side(&[], kb));
    a.resolver.set(b.id, vec![b.addr]);
    b.resolver.set(a.id, vec![a.addr]);
    a.start(true);
    b.start(true);

    // a's daemon hands incoming pairing attempts to the handler
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    a.d().set_pairing_handler(Box::new(move |att| {
        tx.lock().unwrap().send(att).unwrap();
    }));
    a.d().node().start_pairing();
    b.d().node().start_pairing();
    let att_b = b.d().node().dial_pairing(a.addr).unwrap();
    let att_a = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(att_a.sas(), att_b.sas());
    let h = thread::spawn(move || att_a.confirm(true).is_ok());
    assert!(att_b.confirm(true).is_ok());
    assert!(h.join().unwrap());

    a.d().add_peer(&b.public);
    b.d().add_peer(&a.public);
    connect_and_settle(&a, &b);
}
