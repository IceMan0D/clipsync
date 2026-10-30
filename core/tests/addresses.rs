//! Daemon fallback across candidate addresses (loopback, no real network).

use clipsync_clipboard::MockClipboard;
use clipsync_core::addresses::{CandidateResolver, PeerAddr};
use clipsync_core::connection::{ConnState, Resolver};
use clipsync_core::daemon::{Daemon, DaemonConfig};
use clipsync_core::keystore::{KeyStore, MemoryKeyStore};
use clipsync_core::pairing::Node;
use clipsync_core::sync::node_id_from_public_key;
use clipsync_core::transport::{Keypair, Listener};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn node(own: Keypair, trusted: &Keypair) -> (Node, Listener) {
    let store = Arc::new(MemoryKeyStore::default());
    store.add(&trusted.public).unwrap();
    let node = Node::new(own, store);
    let l = Listener::bind("127.0.0.1:0", node.keypair().clone(), vec![]).unwrap();
    (node, l)
}

#[test]
fn dialer_falls_through_dead_candidates_to_a_working_manual_address() {
    let (ka, kb) = (Keypair::generate(), Keypair::generate());
    let (na, la) = node(ka.clone(), &kb);
    let (nb, lb) = node(kb.clone(), &ka);
    let b_id = node_id_from_public_key(&kb.public);
    let b_port = lb.local_addr().port();

    // a dead port: bind then drop
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let resolver = Arc::new(CandidateResolver::new(None));
    resolver.add_manual(b_id, PeerAddr::parse(&format!("127.0.0.1:{dead}")).unwrap());
    resolver.add_manual(b_id, PeerAddr::parse(&format!("127.0.0.1:{b_port}")).unwrap());
    assert_eq!(resolver.resolve(&b_id).len(), 2);

    let da = Daemon::start(
        DaemonConfig { node: na, listener: la, resolver: resolver.clone(), auto_dial: false },
        MockClipboard::default,
    );
    let db = Daemon::start(
        DaemonConfig {
            node: nb,
            listener: lb,
            resolver: Arc::new(CandidateResolver::new(None)),
            auto_dial: false,
        },
        MockClipboard::default,
    );
    da.dial_now(&kb.public).expect("second candidate should work");
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end && da.peer_state(&kb.public) != ConnState::Connected {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(da.peer_state(&kb.public), ConnState::Connected);
    da.shutdown();
    db.shutdown();
}
