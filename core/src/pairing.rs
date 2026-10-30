//! SAS pairing.

use crate::keystore::KeyStore;
use crate::transport::{connect, Error, Keypair, Listener, Result, Session};
use hkdf::Hkdf;
use sha2::Sha256;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Content type of the encrypted pairing decision message (payload: 1 = confirmed, 0 = rejected).
const CT_PAIR_DECISION: u8 = 0xF0;
pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);
pub const MAX_FAILED_ATTEMPTS: u32 = 3;

const SAS_INFO: &[u8] = b"Clipsync-SAS";

/// Derive the 6-digit Short Authentication String from a Noise handshake hash:
/// `HKDF(handshake_hash, "Clipsync-SAS") % 1_000_000`.
pub fn sas_code(handshake_hash: &[u8]) -> u32 {
    let hk = Hkdf::<Sha256>::new(None, handshake_hash);
    let mut out = [0u8; 4];
    hk.expand(SAS_INFO, &mut out)
        .expect("4 bytes is a valid HKDF length");
    u32::from_be_bytes(out) % 1_000_000
}

struct State {
    window: Duration,
    window_end: Option<Instant>,
    failed: u32,
    locked: bool,
    sessions: Vec<(Vec<u8>, TcpStream)>,
}

struct Inner {
    keypair: Keypair,
    store: Arc<dyn KeyStore>,
    state: Mutex<State>,
}

/// A device: its identity, its pinned-key store and the pairing state machine.
#[derive(Clone)]
pub struct Node {
    inner: Arc<Inner>,
}

/// Result of accepting a connection.
pub enum Incoming {
    /// The peer's key is pinned.
    Trusted(Session),
    /// An unpinned peer connected during the pairing window; awaiting SAS confirmation.
    Pairing(PairingAttempt),
}

/// A handshaken but unconfirmed pairing. Compare `sas()` on both screens, then `confirm`.
pub struct PairingAttempt {
    node: Node,
    session: Session,
    deadline: Instant,
}

impl Node {
    pub fn new(keypair: Keypair, store: Arc<dyn KeyStore>) -> Self {
        Node {
            inner: Arc::new(Inner {
                keypair,
                store,
                state: Mutex::new(State {
                    window: PAIRING_WINDOW,
                    window_end: None,
                    failed: 0,
                    locked: false,
                    sessions: Vec::new(),
                }),
            }),
        }
    }

    pub fn keypair(&self) -> &Keypair {
        &self.inner.keypair
    }

    pub fn set_pairing_window(&self, d: Duration) {
        self.inner.state.lock().unwrap().window = d;
    }

    /// Explicitly open a pairing window; resets failed attempts and any lock.
    pub fn start_pairing(&self) {
        let mut st = self.inner.state.lock().unwrap();
        st.window_end = Some(Instant::now() + st.window);
        st.failed = 0;
        st.locked = false;
    }

    pub fn pairing_locked(&self) -> bool {
        self.inner.state.lock().unwrap().locked
    }

    pub fn paired_devices(&self) -> Result<Vec<Vec<u8>>> {
        self.inner.store.load()
    }

    /// Delete a device's pinned key and terminate its active sessions.
    pub fn remove_device(&self, key: &[u8]) -> Result<()> {
        self.inner.store.remove(key)?;
        let mut st = self.inner.state.lock().unwrap();
        st.sessions.retain(|(k, s)| {
            if k == key {
                let _ = s.shutdown(Shutdown::Both);
                false
            } else {
                true
            }
        });
        Ok(())
    }

    /// Open pairing deadline, if pairing is currently possible.
    fn open_deadline(&self) -> Option<Instant> {
        let st = self.inner.state.lock().unwrap();
        match st.window_end {
            Some(end) if !st.locked && Instant::now() < end => Some(end),
            _ => None,
        }
    }

    fn register(&self, session: &Session) -> Result<()> {
        let handle = session.socket_handle()?;
        self.inner
            .state
            .lock()
            .unwrap()
            .sessions
            .push((session.remote_static().to_vec(), handle));
        Ok(())
    }

    fn record_failure(&self) {
        let mut st = self.inner.state.lock().unwrap();
        st.failed += 1;
        if st.failed >= MAX_FAILED_ATTEMPTS {
            st.locked = true;
        }
    }

    fn is_pinned(&self, key: &[u8]) -> Result<bool> {
        Ok(self.inner.store.load()?.iter().any(|k| k == key))
    }

    /// Accept one connection: pinned peers are trusted; unpinned peers are only
    /// admitted (for SAS confirmation) while the pairing window is open and unlocked.
    pub fn accept(&self, listener: &Listener) -> Result<Incoming> {
        let session = listener.accept_unauthenticated()?;
        if self.is_pinned(session.remote_static())? {
            self.register(&session)?;
            return Ok(Incoming::Trusted(session));
        }
        match self.open_deadline() {
            Some(deadline) => Ok(Incoming::Pairing(PairingAttempt {
                node: self.clone(),
                session,
                deadline,
            })),
            None => Err(Error::UnknownPeer),
        }
    }

    /// Connect to a pinned peer; the peer's key must already be pinned.
    pub fn dial_trusted(&self, addr: impl ToSocketAddrs) -> Result<Session> {
        let session = connect(addr, self.inner.keypair.clone())?;
        if !self.is_pinned(session.remote_static())? {
            return Err(Error::UnknownPeer);
        }
        self.register(&session)?;
        Ok(session)
    }

    /// Like `dial_trusted`, with a TCP connect timeout.
    pub fn dial_trusted_timeout(
        &self,
        addr: &std::net::SocketAddr,
        timeout: Duration,
    ) -> Result<Session> {
        let session = crate::transport::connect_timeout(addr, self.inner.keypair.clone(), timeout)?;
        if !self.is_pinned(session.remote_static())? {
            return Err(Error::UnknownPeer);
        }
        self.register(&session)?;
        Ok(session)
    }

    /// Connect to a peer to pair with it. Requires this node's own pairing window to be open.
    pub fn dial_pairing(&self, addr: impl ToSocketAddrs) -> Result<PairingAttempt> {
        let deadline = self.open_deadline().ok_or(Error::PairingClosed)?;
        let session = connect(addr, self.inner.keypair.clone())?;
        Ok(PairingAttempt {
            node: self.clone(),
            session,
            deadline,
        })
    }
}

impl PairingAttempt {
    /// The 6-digit code both users compare.
    pub fn sas(&self) -> String {
        format!("{:06}", sas_code(self.session.handshake_hash()))
    }

    /// Record this user's decision and exchange it with the peer. The peer's key is
    /// pinned only if both sides confirmed within the pairing window; otherwise
    /// nothing is stored and a failed attempt is counted.
    pub fn confirm(mut self, confirmed: bool) -> Result<Session> {
        let outcome = self.exchange(confirmed);
        match outcome {
            Ok(()) => {
                let key = self.session.remote_static().to_vec();
                let stored = self
                    .node
                    .inner
                    .store
                    .add(&key)
                    .and_then(|_| self.node.register(&self.session));
                if let Err(e) = stored {
                    let _ = self.node.inner.store.remove(&key);
                    return Err(e);
                }
                let _ = self.session.set_read_timeout(None);
                Ok(self.session)
            }
            Err(e) => {
                self.node.record_failure();
                Err(e)
            }
        }
    }

    fn exchange(&mut self, confirmed: bool) -> Result<()> {
        let confirmed = confirmed && Instant::now() < self.deadline;
        // Best effort: let the peer know even when we are failing.
        let sent = self
            .session
            .send(CT_PAIR_DECISION, &[confirmed as u8])
            .is_ok();
        if !confirmed || !sent {
            return Err(Error::PairingFailed);
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::PairingFailed);
        }
        self.session.set_read_timeout(Some(remaining))?;
        let reply = self.session.recv().map_err(|_| Error::PairingFailed)?;
        if reply.content_type == CT_PAIR_DECISION
            && reply.payload == [1]
            && Instant::now() < self.deadline
        {
            Ok(())
        } else {
            Err(Error::PairingFailed)
        }
    }
}
