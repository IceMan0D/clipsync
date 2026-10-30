//! Headless end-to-end tests of the daemon over loopback TCP with mock clipboards
//! and an injected address resolver.

use clipsync_clipboard::{
    ClipboardBackend, ClipboardContent, ClipboardError, MockClipboard,
};
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
    fn watch(&mut self) -> Result<Receiver<ClipboardContent>, ClipboardError> {
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
    assert!(wait_until(Duration::from_secs(5), || {
        a.d().peer_state(&b.public) == ConnState::Connected
            && b.d().peer_state(&a.public) == ConnState::Connected
    }));

    let t = Instant::now();
    a.copy("from a");
    assert!(wait_until(Duration::from_secs(1), || b.pasted()
        == Some(ClipboardContent::Text("from a".into()))));
    assert!(t.elapsed() < Duration::from_secs(1));

    b.copy("from b");
    assert!(wait_until(Duration::from_secs(1), || a.pasted()
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
    assert!(wait_until(Duration::from_secs(1), || b.pasted()
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
    thread::sleep(Duration::from_millis(800));
    assert_eq!(a.d().connection_count(), 0);
    assert_eq!(b.d().connection_count(), 0);
}

fn a_pub(a: &Side) -> Keypair {
    Keypair::from_parts(a.public.clone(), vec![0; 32])
}

#[test]
fn pausing_stops_sending_and_applying_and_resuming_restores_sync() {
    let (mut a, mut b) = two_sides();
    a.start(true);
    b.start(true);
    assert!(wait_until(Duration::from_secs(5), || {
        a.d().peer_state(&b.public) == ConnState::Connected
            && b.d().peer_state(&a.public) == ConnState::Connected
    }));

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
    assert!(wait_until(Duration::from_secs(1), || b.pasted()
        == Some(ClipboardContent::Text("after resume".into()))));
    b.copy("b after resume");
    assert!(wait_until(Duration::from_secs(1), || a.pasted()
        == Some(ClipboardContent::Text("b after resume".into()))));
}
