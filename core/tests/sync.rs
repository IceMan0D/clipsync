use clipsync_clipboard::{ClipboardBackend, ClipboardContent, MockClipboard};
use clipsync_core::sync::{SyncEvent, SyncNode, MAX_TEXT_BYTES};
use clipsync_core::transport::{connect, Keypair, Listener, Session};
use std::thread;

fn text(s: &str) -> ClipboardContent {
    ClipboardContent::Text(s.into())
}

/// Two authenticated sessions over loopback: (server side, client side).
fn link(server: &Keypair, client: &Keypair) -> (Session, Session) {
    let l = Listener::bind("127.0.0.1:0", server.clone(), vec![client.public.clone()]).unwrap();
    let addr = l.local_addr();
    let h = thread::spawn(move || l.accept().unwrap());
    let c = connect(addr, client.clone()).unwrap();
    (h.join().unwrap(), c)
}

/// Run the counter handshake on both ends concurrently.
fn handshake(
    a: SyncNode<MockClipboard>,
    sa: Session,
    b: SyncNode<MockClipboard>,
    sb: Session,
) -> (
    SyncNode<MockClipboard>,
    Session,
    SyncNode<MockClipboard>,
    Session,
) {
    let h = thread::spawn(move || {
        let (mut a, mut sa) = (a, sa);
        a.handshake(&mut sa).unwrap();
        (a, sa)
    });
    let (mut b, mut sb) = (b, sb);
    b.handshake(&mut sb).unwrap();
    let (a, sa) = h.join().unwrap();
    (a, sa, b, sb)
}

struct Pair {
    a: SyncNode<MockClipboard>,
    sa: Session,
    b: SyncNode<MockClipboard>,
    sb: Session,
}

fn pair() -> (Pair, Keypair, Keypair) {
    let (ka, kb) = (Keypair::generate(), Keypair::generate());
    let (sa, sb) = link(&ka, &kb);
    let a = SyncNode::new(MockClipboard::new(), &ka.public);
    let b = SyncNode::new(MockClipboard::new(), &kb.public);
    let (a, sa, b, sb) = handshake(a, sa, b, sb);
    (Pair { a, sa, b, sb }, ka, kb)
}

#[test]
fn text_copied_on_one_node_appears_on_the_other_without_echo() {
    let (mut p, _, _) = pair();
    p.a.clipboard_mut().write(text("hello")).unwrap();
    assert_eq!(p.a.pump_local(&mut p.sa).unwrap(), 1);
    p.b.recv_one(&mut p.sb).unwrap();
    assert_eq!(p.b.clipboard().read().unwrap(), Some(text("hello")));
    // b's clipboard write raised a change event; it must not be echoed back
    assert_eq!(p.b.pump_local(&mut p.sb).unwrap(), 0);
}

#[test]
fn rapid_duplicate_copies_are_transmitted_once() {
    let (mut p, _, _) = pair();
    for _ in 0..3 {
        p.a.clipboard_mut().write(text("same")).unwrap();
    }
    assert_eq!(p.a.pump_local(&mut p.sa).unwrap(), 1);
}

#[test]
fn simultaneous_copies_converge_on_the_same_item() {
    let (mut p, ka, kb) = pair();
    p.a.clipboard_mut().write(text("from-a")).unwrap();
    p.b.clipboard_mut().write(text("from-b")).unwrap();
    assert_eq!(p.a.pump_local(&mut p.sa).unwrap(), 1);
    assert_eq!(p.b.pump_local(&mut p.sb).unwrap(), 1);
    p.a.recv_one(&mut p.sa).unwrap();
    p.b.recv_one(&mut p.sb).unwrap();
    let ra = p.a.clipboard().read().unwrap();
    let rb = p.b.clipboard().read().unwrap();
    assert_eq!(ra, rb);
    // equal counters within the window: the higher node id wins
    let a_wins = clipsync_core::sync::node_id_from_public_key(&ka.public)
        > clipsync_core::sync::node_id_from_public_key(&kb.public);
    assert_eq!(ra, Some(text(if a_wins { "from-a" } else { "from-b" })));
}

#[test]
fn oversize_text_is_skipped_and_reported() {
    let (mut p, _, _) = pair();
    p.a.clipboard_mut()
        .write(text(&"a".repeat(MAX_TEXT_BYTES + 1)))
        .unwrap();
    assert_eq!(p.a.pump_local(&mut p.sa).unwrap(), 0);
    assert_eq!(
        p.a.take_events(),
        vec![SyncEvent::TextTooLarge {
            bytes: MAX_TEXT_BYTES + 1
        }]
    );
}

#[test]
fn one_mib_text_is_transmitted() {
    let (mut p, _, _) = pair();
    let big = "a".repeat(MAX_TEXT_BYTES);
    let expect = big.clone();
    let h = thread::spawn(move || {
        p.a.clipboard_mut().write(text(&big)).unwrap();
        p.a.pump_local(&mut p.sa).unwrap();
        p.a
    });
    p.b.recv_one(&mut p.sb).unwrap();
    h.join().unwrap();
    assert_eq!(p.b.clipboard().read().unwrap(), Some(text(&expect)));
}

#[test]
fn reconnecting_node_receives_nothing_it_missed() {
    let (ka, kb) = (Keypair::generate(), Keypair::generate());
    let a = SyncNode::new(MockClipboard::new(), &ka.public);
    let mut b = SyncNode::new(MockClipboard::new(), &kb.public);
    // b copies while no connection exists
    b.clipboard_mut().write(text("missed")).unwrap();
    let (sa, sb) = link(&ka, &kb);
    let (mut a, mut sa, mut b, mut sb) = handshake(a, sa, b, sb);
    // nothing is sent on connect
    assert_eq!(b.pump_local(&mut sb).unwrap(), 0);
    assert_eq!(a.clipboard().read().unwrap(), None);
    // live sync still works afterwards
    b.clipboard_mut().write(text("live")).unwrap();
    assert_eq!(b.pump_local(&mut sb).unwrap(), 1);
    a.recv_one(&mut sa).unwrap();
    assert_eq!(a.clipboard().read().unwrap(), Some(text("live")));
}
