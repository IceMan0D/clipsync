//! Sync engine: HLC last-write-wins ordering, dedupe, echo suppression, text limits.

use crate::transport::{self, Session};
use clipsync_clipboard::image::{validate_png, ImageError};
use clipsync_clipboard::{ClipboardBackend, ClipboardContent};
use sha2::{Digest, Sha256};
use std::sync::mpsc::Receiver;
use std::time::{SystemTime, UNIX_EPOCH};

pub const WINDOW_MS: u64 = 1000;
pub const SUPPRESS_MS: u64 = 500;
pub const DEDUPE_MS: u64 = 1000;
pub const MAX_TEXT_BYTES: usize = 1_048_576;

pub type NodeId = [u8; 32];

pub fn node_id_from_public_key(public: &[u8]) -> NodeId {
    Sha256::digest(public).into()
}

/// Hybrid logical clock stamp of a clipboard item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub ts_ms: u64,
    pub counter: u64,
    pub node: NodeId,
}

impl Stamp {
    /// True when `self` should replace `other`.
    pub fn wins_over(&self, other: &Stamp) -> bool {
        if self.ts_ms.abs_diff(other.ts_ms) > WINDOW_MS {
            return self.ts_ms > other.ts_ms;
        }
        (self.counter, self.node) > (other.counter, other.node)
    }
}

/// Reported to the UI layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    /// Text exceeded `MAX_TEXT_BYTES` and was skipped.
    TextTooLarge { bytes: usize },
    /// An image was dropped (size cap, megapixel cap, bad PNG or failed
    /// conversion); the local clipboard was left untouched.
    ImageDropped(ImageError),
}

/// What to do with a local clipboard change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    Send(Stamp),
    /// Same content was just sent.
    Duplicate,
    /// Change caused by applying a remote item.
    Echo,
    TooLarge,
    /// Content kind not synced by this engine yet.
    Unsupported,
}

/// What to do with a remote item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incoming {
    Apply,
    Reject,
}

fn content_hash(c: &ClipboardContent) -> [u8; 32] {
    match c {
        ClipboardContent::Text(t) => Sha256::digest(t.as_bytes()).into(),
        ClipboardContent::Image(b) => Sha256::digest(b).into(),
    }
}

/// Pure sync state machine; time is injected so behaviour is deterministic.
pub struct Engine {
    node: NodeId,
    counter: u64,
    latest: Option<Stamp>,
    last_sent: Option<([u8; 32], u64)>,
    last_local_write_hash: Option<([u8; 32], u64)>,
    events: Vec<SyncEvent>,
}

impl Engine {
    pub fn new(node: NodeId) -> Self {
        Engine {
            node,
            counter: 0,
            latest: None,
            last_sent: None,
            last_local_write_hash: None,
            events: Vec::new(),
        }
    }

    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// Handshake: `local_counter = max(local_counter, remote_counter)`.
    pub fn observe_remote_counter(&mut self, remote: u64) {
        self.counter = self.counter.max(remote);
    }

    pub fn take_events(&mut self) -> Vec<SyncEvent> {
        std::mem::take(&mut self.events)
    }

    fn push_event(&mut self, e: SyncEvent) {
        self.events.push(e);
    }

    fn check_size(&mut self, c: &ClipboardContent) -> bool {
        match c {
            ClipboardContent::Text(t) if t.len() > MAX_TEXT_BYTES => {
                self.events.push(SyncEvent::TextTooLarge { bytes: t.len() });
                false
            }
            ClipboardContent::Image(png) => match validate_png(png) {
                Ok(_) => true,
                Err(e) => {
                    self.events.push(SyncEvent::ImageDropped(e));
                    false
                }
            },
            _ => true,
        }
    }

    pub fn on_local_change(&mut self, content: &ClipboardContent, now_ms: u64) -> Outgoing {
        if !self.check_size(content) {
            return Outgoing::TooLarge;
        }
        let hash = content_hash(content);
        if let Some((h, at)) = self.last_local_write_hash {
            if h == hash && now_ms.saturating_sub(at) < SUPPRESS_MS {
                return Outgoing::Echo;
            }
        }
        if let Some((h, at)) = self.last_sent {
            if h == hash && now_ms.saturating_sub(at) < DEDUPE_MS {
                return Outgoing::Duplicate;
            }
        }
        self.counter += 1;
        let stamp = Stamp {
            ts_ms: now_ms,
            counter: self.counter,
            node: self.node,
        };
        self.latest = Some(stamp);
        self.last_sent = Some((hash, now_ms));
        Outgoing::Send(stamp)
    }

    pub fn on_remote(&mut self, stamp: Stamp, content: &ClipboardContent, now_ms: u64) -> Incoming {
        self.observe_remote_counter(stamp.counter);
        if !self.check_size(content) {
            return Incoming::Reject;
        }
        if let Some(latest) = &self.latest {
            if !stamp.wins_over(latest) {
                return Incoming::Reject;
            }
        }
        self.latest = Some(stamp);
        self.last_local_write_hash = Some((content_hash(content), now_ms));
        Incoming::Apply
    }
}

const TYPE_HELLO: u8 = 0;
const TYPE_TEXT: u8 = 1;
const TYPE_IMAGE: u8 = 2;
/// Item payload: ts_ms(8) counter(8) node(32) body.
const ITEM_HEADER: usize = 48;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("transport: {0}")]
    Transport(#[from] transport::Error),
    #[error("clipboard: {0}")]
    Clipboard(#[from] clipsync_clipboard::ClipboardError),
    #[error("malformed sync message")]
    Malformed,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A clipboard bound to the sync engine; drives one `Session` at a time.
pub struct SyncNode<C: ClipboardBackend> {
    engine: Engine,
    clipboard: C,
    watch: Receiver<ClipboardContent>,
    paused: bool,
}

impl<C: ClipboardBackend> SyncNode<C> {
    pub fn new(mut clipboard: C, public_key: &[u8]) -> Self {
        let watch = clipboard.watch().expect("clipboard watch");
        SyncNode {
            engine: Engine::new(node_id_from_public_key(public_key)),
            clipboard,
            watch,
            paused: false,
        }
    }

    /// While paused, local changes are discarded (not replayed on resume) and
    /// remote items are received but not applied.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn clipboard(&self) -> &C {
        &self.clipboard
    }

    pub fn clipboard_mut(&mut self) -> &mut C {
        &mut self.clipboard
    }

    pub fn take_events(&mut self) -> Vec<SyncEvent> {
        self.engine.take_events()
    }

    /// Exchange logical counters. Clipboard changes made while disconnected are
    /// discarded: nothing is sent on connect (live sync only).
    pub fn handshake(&mut self, session: &mut Session) -> Result<(), SyncError> {
        while self.watch.try_recv().is_ok() {}
        session.send(TYPE_HELLO, &self.engine.counter().to_be_bytes())?;
        let m = session.recv()?;
        let bytes: [u8; 8] = m
            .payload
            .as_slice()
            .try_into()
            .map_err(|_| SyncError::Malformed)?;
        if m.content_type != TYPE_HELLO {
            return Err(SyncError::Malformed);
        }
        self.engine
            .observe_remote_counter(u64::from_be_bytes(bytes));
        Ok(())
    }

    /// Send all pending local clipboard changes; returns how many items were sent.
    pub fn pump_local(&mut self, session: &mut Session) -> Result<usize, SyncError> {
        let mut sent = 0;
        if self.paused {
            while self.watch.try_recv().is_ok() {}
            return Ok(0);
        }
        while let Ok(content) = self.watch.try_recv() {
            if let Outgoing::Send(stamp) = self.engine.on_local_change(&content, now_ms()) {
                let (kind, body) = match &content {
                    ClipboardContent::Text(t) => (TYPE_TEXT, t.as_bytes()),
                    ClipboardContent::Image(png) => (TYPE_IMAGE, png.as_slice()),
                };
                let mut payload = Vec::with_capacity(ITEM_HEADER + body.len());
                payload.extend_from_slice(&stamp.ts_ms.to_be_bytes());
                payload.extend_from_slice(&stamp.counter.to_be_bytes());
                payload.extend_from_slice(&stamp.node);
                payload.extend_from_slice(body);
                session.send(kind, &payload)?;
                sent += 1;
            }
        }
        Ok(sent)
    }

    /// Block for one remote message and apply it if it wins.
    pub fn recv_one(&mut self, session: &mut Session) -> Result<(), SyncError> {
        let m = session.recv()?;
        match m.content_type {
            TYPE_HELLO => {
                let b: [u8; 8] = m
                    .payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| SyncError::Malformed)?;
                self.engine.observe_remote_counter(u64::from_be_bytes(b));
            }
            TYPE_TEXT | TYPE_IMAGE => {
                if m.payload.len() < ITEM_HEADER {
                    return Err(SyncError::Malformed);
                }
                let p = &m.payload;
                let stamp = Stamp {
                    ts_ms: u64::from_be_bytes(p[0..8].try_into().unwrap()),
                    counter: u64::from_be_bytes(p[8..16].try_into().unwrap()),
                    node: p[16..48].try_into().unwrap(),
                };
                let content = if m.content_type == TYPE_IMAGE {
                    ClipboardContent::Image(p[ITEM_HEADER..].to_vec())
                } else {
                    ClipboardContent::Text(
                        String::from_utf8(p[ITEM_HEADER..].to_vec())
                            .map_err(|_| SyncError::Malformed)?,
                    )
                };
                if !self.paused
                    && self.engine.on_remote(stamp, &content, now_ms()) == Incoming::Apply
                {
                    let is_image = matches!(content, ClipboardContent::Image(_));
                    match self.clipboard.write(content) {
                        Ok(()) => {}
                        // A failed image conversion is dropped, not fatal.
                        Err(e) if is_image => self
                            .engine
                            .push_event(SyncEvent::ImageDropped(ImageError::Conversion(e.0))),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(ts_ms: u64, counter: u64, n: u8) -> Stamp {
        Stamp {
            ts_ms,
            counter,
            node: [n; 32],
        }
    }

    #[test]
    fn newer_timestamp_wins_outside_window() {
        assert!(st(5_000, 1, 1).wins_over(&st(3_000, 99, 9)));
        assert!(!st(3_000, 99, 9).wins_over(&st(5_000, 1, 1)));
    }

    #[test]
    fn higher_counter_wins_inside_window() {
        assert!(st(3_000, 5, 1).wins_over(&st(3_900, 4, 9)));
        assert!(!st(3_900, 4, 9).wins_over(&st(3_000, 5, 1)));
        // exactly 1000 ms is still inside the window
        assert!(st(4_000, 2, 1).wins_over(&st(3_000, 1, 9)));
        assert!(!st(3_000, 1, 9).wins_over(&st(4_000, 2, 1)));
    }

    #[test]
    fn equal_counters_go_to_higher_node_id() {
        assert!(st(3_000, 2, 9).wins_over(&st(3_100, 2, 1)));
        assert!(!st(3_100, 2, 1).wins_over(&st(3_000, 2, 9)));
    }

    #[test]
    fn node_id_is_sha256_of_public_key() {
        // SHA-256("abc") = ba7816bf...f20015ad
        let id = node_id_from_public_key(b"abc");
        assert_eq!(id[0], 0xba);
        assert_eq!(id[1], 0x78);
        assert_eq!(id[31], 0xad);
    }

    fn eng(n: u8) -> Engine {
        Engine::new([n; 32])
    }
    fn text(s: &str) -> ClipboardContent {
        ClipboardContent::Text(s.into())
    }

    #[test]
    fn local_change_is_stamped_with_increasing_counter() {
        let mut e = eng(1);
        let Outgoing::Send(a) = e.on_local_change(&text("a"), 10_000) else {
            panic!()
        };
        let Outgoing::Send(b) = e.on_local_change(&text("b"), 10_001) else {
            panic!()
        };
        assert_eq!((a.ts_ms, a.counter, a.node), (10_000, 1, [1; 32]));
        assert_eq!(b.counter, 2);
    }

    #[test]
    fn rapid_duplicate_copies_are_sent_once() {
        let mut e = eng(1);
        assert!(matches!(
            e.on_local_change(&text("a"), 10_000),
            Outgoing::Send(_)
        ));
        assert_eq!(e.on_local_change(&text("a"), 10_300), Outgoing::Duplicate);
        // after the window the same text is sent again
        assert!(matches!(
            e.on_local_change(&text("a"), 11_500),
            Outgoing::Send(_)
        ));
        // different content is never a duplicate
        assert!(matches!(
            e.on_local_change(&text("b"), 11_501),
            Outgoing::Send(_)
        ));
    }

    #[test]
    fn applied_remote_item_is_not_echoed_within_suppression_window() {
        let mut e = eng(1);
        let remote = Stamp {
            ts_ms: 10_000,
            counter: 1,
            node: [2; 32],
        };
        assert_eq!(e.on_remote(remote, &text("x"), 10_000), Incoming::Apply);
        assert_eq!(e.on_local_change(&text("x"), 10_200), Outgoing::Echo);
        // outside the window a genuine re-copy goes out
        assert!(matches!(
            e.on_local_change(&text("x"), 10_600),
            Outgoing::Send(_)
        ));
    }

    #[test]
    fn oversize_text_is_skipped_with_event() {
        let mut e = eng(1);
        let big = ClipboardContent::Text("a".repeat(MAX_TEXT_BYTES + 1));
        assert_eq!(e.on_local_change(&big, 10_000), Outgoing::TooLarge);
        assert_eq!(
            e.take_events(),
            vec![SyncEvent::TextTooLarge {
                bytes: MAX_TEXT_BYTES + 1
            }]
        );
        let max = ClipboardContent::Text("a".repeat(MAX_TEXT_BYTES));
        assert!(matches!(e.on_local_change(&max, 10_001), Outgoing::Send(_)));
        assert!(e.take_events().is_empty());
    }

    #[test]
    fn oversize_remote_text_is_dropped_with_event() {
        let mut e = eng(1);
        let remote = Stamp {
            ts_ms: 10_000,
            counter: 1,
            node: [2; 32],
        };
        let big = ClipboardContent::Text("a".repeat(MAX_TEXT_BYTES + 1));
        assert_eq!(e.on_remote(remote, &big, 10_000), Incoming::Reject);
        assert_eq!(e.take_events().len(), 1);
    }

    #[test]
    fn older_remote_item_loses_to_newer_local_item() {
        let mut e = eng(1);
        e.on_local_change(&text("mine"), 20_000);
        let old = Stamp {
            ts_ms: 10_000,
            counter: 50,
            node: [2; 32],
        };
        assert_eq!(e.on_remote(old, &text("theirs"), 20_001), Incoming::Reject);
    }

    #[test]
    fn simultaneous_copies_converge_on_the_same_winner() {
        let mut a = eng(1);
        let mut b = eng(2);
        let Outgoing::Send(sa) = a.on_local_change(&text("from-a"), 50_000) else {
            panic!()
        };
        let Outgoing::Send(sb) = b.on_local_change(&text("from-b"), 50_100) else {
            panic!()
        };
        let ra = a.on_remote(sb, &text("from-b"), 50_110);
        let rb = b.on_remote(sa, &text("from-a"), 50_110);
        // equal counters inside the window: higher node id (b) wins on both sides
        assert_eq!((ra, rb), (Incoming::Apply, Incoming::Reject));
    }

    #[test]
    fn handshake_counters_align_to_the_max() {
        let mut e = eng(1);
        e.on_local_change(&text("a"), 10_000);
        e.observe_remote_counter(40);
        assert_eq!(e.counter(), 40);
        e.observe_remote_counter(3);
        assert_eq!(e.counter(), 40);
        let Outgoing::Send(s) = e.on_local_change(&text("b"), 10_001) else {
            panic!()
        };
        assert_eq!(s.counter, 41);
    }

    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    #[test]
    fn small_image_is_sent_and_applied() {
        let img = ClipboardContent::Image(png_header(10, 10));
        let mut a = eng(1);
        let Outgoing::Send(s) = a.on_local_change(&img, 10_000) else {
            panic!()
        };
        let mut b = eng(2);
        assert_eq!(b.on_remote(s, &img, 10_001), Incoming::Apply);
    }

    #[test]
    fn oversize_local_image_is_dropped_with_event() {
        let mut e = eng(1);
        let mut big = png_header(10, 10);
        big.resize(10 * 1024 * 1024 + 1, 0);
        assert_eq!(
            e.on_local_change(&ClipboardContent::Image(big), 10_000),
            Outgoing::TooLarge
        );
        assert!(matches!(
            e.take_events()[..],
            [SyncEvent::ImageDropped(ImageError::TooLarge { .. })]
        ));
    }

    #[test]
    fn remote_image_over_64_megapixels_is_rejected_with_event() {
        let mut e = eng(1);
        let remote = Stamp {
            ts_ms: 10_000,
            counter: 1,
            node: [2; 32],
        };
        let bomb = ClipboardContent::Image(png_header(20_000, 20_000));
        assert_eq!(e.on_remote(remote, &bomb, 10_000), Incoming::Reject);
        assert!(matches!(
            e.take_events()[..],
            [SyncEvent::ImageDropped(ImageError::TooManyPixels { .. })]
        ));
    }

    #[test]
    fn remote_non_png_image_is_rejected_with_event() {
        let mut e = eng(1);
        let remote = Stamp {
            ts_ms: 10_000,
            counter: 1,
            node: [2; 32],
        };
        let junk = ClipboardContent::Image(vec![1, 2, 3]);
        assert_eq!(e.on_remote(remote, &junk, 10_000), Incoming::Reject);
        assert_eq!(
            e.take_events(),
            vec![SyncEvent::ImageDropped(ImageError::NotPng)]
        );
    }
}
