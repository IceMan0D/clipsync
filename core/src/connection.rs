//! Connection management: states, backoff, simultaneous-dial resolution.

use crate::sync::NodeId;
use std::collections::{HashMap, HashSet};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

/// Looks up the current candidate addresses of a peer (mDNS, MagicDNS, static...).
/// Called afresh on every connection attempt.
pub trait Resolver: Send + Sync {
    fn resolve(&self, node: &NodeId) -> Vec<SocketAddr>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Handshaking,
    Connected,
}

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Exponential reconnect delay: 1 s doubling up to 30 s.
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    pub fn new() -> Self {
        Backoff { next: BACKOFF_MIN }
    }

    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(BACKOFF_MAX);
        d
    }

    pub fn reset(&mut self) {
        self.next = BACKOFF_MIN;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// An authenticated connection as seen from the local node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conn {
    pub local: NodeId,
    pub remote: NodeId,
    /// True if the local node initiated (dialed) the connection.
    pub dialed: bool,
}

impl Conn {
    fn initiator(&self) -> &NodeId {
        if self.dialed {
            &self.local
        } else {
            &self.remote
        }
    }

    /// Simultaneous-dial resolution: the connection initiated by the lower node ID
    /// wins. Both peers compute the same verdict for the same pair of connections.
    pub fn beats(&self, other: &Conn) -> bool {
        self.initiator() < other.initiator()
    }
}

/// Outcome of offering a freshly authenticated connection to the registry.
#[derive(Debug, PartialEq, Eq)]
pub enum Offer {
    /// Kept; the token identifies it for later `mark_connected` / `release`.
    Accepted(u64),
    /// Lost the simultaneous-dial resolution (the socket has been closed).
    Rejected,
}

struct Slot {
    conn: Conn,
    token: u64,
    phase: ConnState,
    handle: TcpStream,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<NodeId, Slot>,
    dialing: HashSet<NodeId>,
    next_token: u64,
}

/// At most one live connection per peer; resolves simultaneous dials.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self, peer: &NodeId) -> ConnState {
        let g = self.inner.lock().unwrap();
        match g.slots.get(peer) {
            Some(s) => s.phase,
            None if g.dialing.contains(peer) => ConnState::Connecting,
            None => ConnState::Disconnected,
        }
    }

    pub fn set_dialing(&self, peer: NodeId, dialing: bool) {
        let mut g = self.inner.lock().unwrap();
        if dialing {
            g.dialing.insert(peer);
        } else {
            g.dialing.remove(&peer);
        }
    }

    /// Register an authenticated connection. If one already exists for this peer, the
    /// one initiated by the lower node ID is kept and the other is closed. A duplicate
    /// in the same direction is a reconnect, so the newer replaces the older.
    pub fn offer(&self, conn: Conn, handle: TcpStream) -> Offer {
        let mut g = self.inner.lock().unwrap();
        if let Some(old) = g.slots.get(&conn.remote) {
            if conn.dialed != old.conn.dialed && !conn.beats(&old.conn) {
                let _ = handle.shutdown(Shutdown::Both);
                return Offer::Rejected;
            }
            let _ = old.handle.shutdown(Shutdown::Both);
        }
        g.next_token += 1;
        let token = g.next_token;
        g.slots.insert(
            conn.remote,
            Slot { conn, token, phase: ConnState::Handshaking, handle },
        );
        Offer::Accepted(token)
    }

    pub fn mark_connected(&self, peer: &NodeId, token: u64) {
        if let Some(s) = self.inner.lock().unwrap().slots.get_mut(peer) {
            if s.token == token {
                s.phase = ConnState::Connected;
            }
        }
    }

    /// Drop a connection that ended. No-op if it was already replaced.
    pub fn release(&self, peer: &NodeId, token: u64) {
        let mut g = self.inner.lock().unwrap();
        if g.slots.get(peer).is_some_and(|s| s.token == token) {
            g.slots.remove(peer);
        }
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().slots.len()
    }

    /// Whether the live connection to `peer` was dialed by the local node.
    pub fn dialed(&self, peer: &NodeId) -> Option<bool> {
        self.inner.lock().unwrap().slots.get(peer).map(|s| s.conn.dialed)
    }

    pub fn close_all(&self) {
        for s in self.inner.lock().unwrap().slots.values() {
            let _ = s.handle.shutdown(Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_one_second_to_thirty_and_resets() {
        let mut b = Backoff::new();
        let secs: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    const LOW: NodeId = [1; 32];
    const HIGH: NodeId = [9; 32];

    #[test]
    fn connection_initiated_by_lower_node_id_is_preferred() {
        // (dialed_by_us, local, remote)
        let low_dialed = Conn { local: LOW, remote: HIGH, dialed: true };
        let high_dialed = Conn { local: LOW, remote: HIGH, dialed: false };
        assert!(low_dialed.beats(&high_dialed));
        assert!(!high_dialed.beats(&low_dialed));
        // same connection seen from the other side gives the same verdict
        let low_dialed_peer_view = Conn { local: HIGH, remote: LOW, dialed: false };
        let high_dialed_peer_view = Conn { local: HIGH, remote: LOW, dialed: true };
        assert!(low_dialed_peer_view.beats(&high_dialed_peer_view));
    }

    #[test]
    fn same_direction_duplicate_does_not_beat_existing() {
        let a = Conn { local: LOW, remote: HIGH, dialed: true };
        assert!(!a.beats(&a));
    }

    fn sock() -> TcpStream {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let _ = l.accept().unwrap();
        c
    }

    #[test]
    fn registry_keeps_the_lower_initiated_connection_in_either_arrival_order() {
        let low_dialed = Conn { local: LOW, remote: HIGH, dialed: true };
        let high_dialed = Conn { local: LOW, remote: HIGH, dialed: false };
        for first_is_winner in [true, false] {
            let r = Registry::new();
            let (first, second) = if first_is_winner {
                (low_dialed, high_dialed)
            } else {
                (high_dialed, low_dialed)
            };
            assert!(matches!(r.offer(first, sock()), Offer::Accepted(_)));
            let second_offer = r.offer(second, sock());
            assert_eq!(second_offer == Offer::Rejected, first_is_winner);
            assert_eq!(r.count(), 1);
            assert_eq!(r.dialed(&HIGH), Some(true));
        }
    }

    #[test]
    fn release_of_a_replaced_connection_does_not_drop_its_successor() {
        let r = Registry::new();
        let c = Conn { local: LOW, remote: HIGH, dialed: true };
        let Offer::Accepted(old) = r.offer(c, sock()) else { panic!() };
        let Offer::Accepted(new) = r.offer(c, sock()) else { panic!() };
        r.release(&HIGH, old);
        assert_eq!(r.count(), 1);
        r.mark_connected(&HIGH, new);
        assert_eq!(r.state(&HIGH), ConnState::Connected);
        r.release(&HIGH, new);
        assert_eq!(r.state(&HIGH), ConnState::Disconnected);
    }
}
