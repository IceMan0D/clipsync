use clipsync_core::keystore::{KeyStore, MemoryKeyStore};
use clipsync_core::pairing::{sas_code, Incoming, Node};
use clipsync_core::transport::{Error, Keypair, Listener};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
fn sas_matches_independent_hkdf_vector() {
    // Vector computed independently (python hmac/sha256 HKDF, salt = 32 zero bytes,
    // info = "Clipsync-SAS", first 4 output bytes big-endian, mod 1_000_000).
    let hash: Vec<u8> = (0u8..32).collect();
    assert_eq!(sas_code(&hash), 685_867);
}

struct Peer {
    node: Arc<Node>,
    store: Arc<MemoryKeyStore>,
    public: Vec<u8>,
}

fn peer() -> Peer {
    let kp = Keypair::generate();
    let public = kp.public.clone();
    let store = Arc::new(MemoryKeyStore::default());
    let node = Arc::new(Node::new(kp, store.clone()));
    Peer { node, store, public }
}

fn listener(p: &Peer) -> Listener {
    Listener::bind("127.0.0.1:0", p.node.keypair().clone(), vec![]).unwrap()
}

/// Run a pairing between `a` (listener) and `b` (dialer); each side's user answers
/// with the given decision. Returns (a_result_ok, b_result_ok, a_sas, b_sas).
fn run_pairing(a: &Peer, b: &Peer, a_says: bool, b_says: bool) -> (bool, bool, String, String) {
    let l = listener(a);
    let addr = l.local_addr();
    let an = a.node.clone();
    let h = thread::spawn(move || match an.accept(&l).unwrap() {
        Incoming::Pairing(att) => {
            let sas = att.sas();
            (an_confirm(att, a_says), sas)
        }
        Incoming::Trusted(_) => panic!("unexpected trusted"),
    });
    let att = b.node.dial_pairing(addr).unwrap();
    let b_sas = att.sas();
    let b_ok = att.confirm(b_says).is_ok();
    let (a_ok, a_sas) = h.join().unwrap();
    (a_ok, b_ok, a_sas, b_sas)
}

fn an_confirm(att: clipsync_core::pairing::PairingAttempt, say: bool) -> bool {
    att.confirm(say).is_ok()
}

#[test]
fn two_nodes_pair_with_matching_sas_and_pin_each_other() {
    let (a, b) = (peer(), peer());
    a.node.start_pairing();
    b.node.start_pairing();
    let (a_ok, b_ok, a_sas, b_sas) = run_pairing(&a, &b, true, true);
    assert!(a_ok && b_ok);
    assert_eq!(a_sas, b_sas);
    assert_eq!(a_sas.len(), 6);
    assert_eq!(a.store.load().unwrap(), vec![b.public.clone()]);
    assert_eq!(b.store.load().unwrap(), vec![a.public.clone()]);
}

#[test]
fn rejection_by_either_side_stores_nothing_on_both() {
    for (a_says, b_says) in [(true, false), (false, true), (false, false)] {
        let (a, b) = (peer(), peer());
        a.node.start_pairing();
        b.node.start_pairing();
        let (a_ok, b_ok, _, _) = run_pairing(&a, &b, a_says, b_says);
        assert!(!a_ok && !b_ok);
        assert!(a.store.load().unwrap().is_empty());
        assert!(b.store.load().unwrap().is_empty());
    }
}

#[test]
fn unpaired_device_is_rejected_outside_pairing_window() {
    let (a, b) = (peer(), peer());
    // Listener never opened a window.
    b.node.start_pairing();
    let l = listener(&a);
    let addr = l.local_addr();
    let an = a.node.clone();
    let h = thread::spawn(move || an.accept(&l).err());
    let _ = b.node.dial_pairing(addr);
    assert!(matches!(h.join().unwrap(), Some(Error::UnknownPeer)));
    assert!(a.store.load().unwrap().is_empty());
}

#[test]
fn unpaired_device_is_rejected_after_window_expires() {
    let a = peer();
    let b = peer();
    a.node.set_pairing_window(Duration::from_millis(50));
    a.node.start_pairing();
    b.node.start_pairing();
    thread::sleep(Duration::from_millis(100));
    let l = listener(&a);
    let addr = l.local_addr();
    let an = a.node.clone();
    let h = thread::spawn(move || an.accept(&l).err());
    let _ = b.node.dial_pairing(addr);
    assert!(matches!(h.join().unwrap(), Some(Error::UnknownPeer)));
}

#[test]
fn confirming_after_window_expiry_stores_nothing() {
    let (a, b) = (peer(), peer());
    a.node.set_pairing_window(Duration::from_millis(300));
    a.node.start_pairing();
    b.node.start_pairing();
    let l = listener(&a);
    let addr = l.local_addr();
    let an = a.node.clone();
    let h = thread::spawn(move || match an.accept(&l).unwrap() {
        Incoming::Pairing(att) => {
            thread::sleep(Duration::from_millis(500));
            att.confirm(true).is_ok()
        }
        _ => panic!(),
    });
    let att = b.node.dial_pairing(addr).unwrap();
    let b_ok = att.confirm(true).is_ok();
    assert!(!h.join().unwrap());
    assert!(!b_ok);
    assert!(a.store.load().unwrap().is_empty());
    assert!(b.store.load().unwrap().is_empty());
}

#[test]
fn three_failed_attempts_lock_the_pairing_session() {
    let (a, b) = (peer(), peer());
    a.node.start_pairing();
    b.node.set_pairing_window(Duration::from_secs(120));
    for _ in 0..3 {
        b.node.start_pairing(); // dialer resets its own state each time
        assert!(!a.node.pairing_locked());
        let (a_ok, _, _, _) = run_pairing(&a, &b, false, false);
        assert!(!a_ok);
    }
    assert!(a.node.pairing_locked());
    // A fourth attempt, even with matching confirmations, is refused.
    b.node.start_pairing();
    let (a_ok, b_ok, _, _) = run_pairing_expect_refused(&a, &b);
    assert!(!a_ok && !b_ok);
    assert!(a.store.load().unwrap().is_empty());
    // Explicitly restarting pairing unlocks.
    a.node.start_pairing();
    assert!(!a.node.pairing_locked());
}

fn run_pairing_expect_refused(a: &Peer, b: &Peer) -> (bool, bool, (), ()) {
    let l = listener(a);
    let addr = l.local_addr();
    let an = a.node.clone();
    let h = thread::spawn(move || an.accept(&l).is_ok());
    let b_ok = b.node.dial_pairing(addr).and_then(|att| att.confirm(true)).is_ok();
    (h.join().unwrap(), b_ok, (), ())
}

#[test]
fn pinned_keys_persist_and_authorize_later_connections() {
    let (a, b) = (peer(), peer());
    a.node.start_pairing();
    b.node.start_pairing();
    let (a_ok, b_ok, _, _) = run_pairing(&a, &b, true, true);
    assert!(a_ok && b_ok);

    // "Restart": new nodes with the same keys over the same stores.
    let a2 = Arc::new(Node::new(a.node.keypair().clone(), a.store.clone()));
    let b2 = Arc::new(Node::new(b.node.keypair().clone(), b.store.clone()));
    let l = listener(&a);
    let addr = l.local_addr();
    let h = thread::spawn(move || match a2.accept(&l).unwrap() {
        Incoming::Trusted(mut s) => s.recv().unwrap().payload,
        _ => panic!(),
    });
    let mut s = b2.dial_trusted(addr).unwrap();
    s.send(1, b"hi").unwrap();
    assert_eq!(h.join().unwrap(), b"hi");
}

#[test]
fn removing_a_device_deletes_key_and_drops_its_session() {
    let (a, b) = (peer(), peer());
    a.node.set_pairing_window(Duration::from_millis(400));
    a.node.start_pairing();
    b.node.start_pairing();
    run_pairing(&a, &b, true, true);

    let l = listener(&a);
    let addr = l.local_addr();
    let an = a.node.clone();
    let h = thread::spawn(move || match an.accept(&l).unwrap() {
        Incoming::Trusted(mut s) => {
            // Blocks until the session is terminated by removal.
            s.recv().is_err()
        }
        _ => panic!(),
    });
    let _client = b.node.dial_trusted(addr).unwrap();
    // Give the accept side time to register the session.
    thread::sleep(Duration::from_millis(200));
    a.node.remove_device(&b.public).unwrap();
    assert!(h.join().unwrap());
    assert!(a.store.load().unwrap().is_empty());

    // And the removed device is now rejected (once a's pairing window has closed).
    thread::sleep(Duration::from_millis(400));
    let l = listener(&a);
    let addr = l.local_addr();
    let an = a.node.clone();
    let h = thread::spawn(move || an.accept(&l).err());
    let _ = b.node.dial_trusted(addr);
    assert!(matches!(h.join().unwrap(), Some(Error::UnknownPeer)));
}
