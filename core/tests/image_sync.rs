use clipsync_clipboard::image::{rgba_to_png, ImageError, MAX_IMAGE_BYTES};
use clipsync_clipboard::{ClipboardBackend, ClipboardContent, ClipboardError, MockClipboard};
use clipsync_core::sync::{SyncEvent, SyncNode};
use clipsync_core::transport::{connect, Keypair, Listener, Session};
use std::sync::mpsc::Receiver;
use std::thread;

/// Mock clipboard whose image writes can be made to fail.
struct Flaky {
    inner: MockClipboard,
    fail_images: bool,
}

impl ClipboardBackend for Flaky {
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        self.inner.read()
    }
    fn write(&mut self, c: ClipboardContent) -> Result<(), ClipboardError> {
        if self.fail_images && matches!(c, ClipboardContent::Image(_)) {
            return Err(ClipboardError("cannot convert PNG".into()));
        }
        self.inner.write(c)
    }
    fn watch(&mut self) -> Result<Receiver<Result<ClipboardContent, ClipboardError>>, ClipboardError> {
        self.inner.watch()
    }
}

fn flaky(fail_images: bool) -> Flaky {
    Flaky {
        inner: MockClipboard::new(),
        fail_images,
    }
}

fn setup(fail_b: bool) -> (SyncNode<Flaky>, Session, SyncNode<Flaky>, Session) {
    let (ka, kb) = (Keypair::generate(), Keypair::generate());
    let l = Listener::bind("127.0.0.1:0", ka.clone(), vec![kb.public.clone()]).unwrap();
    let addr = l.local_addr();
    let h = thread::spawn(move || l.accept().unwrap());
    let mut sb = connect(addr, kb.clone()).unwrap();
    let mut sa = h.join().unwrap();
    let mut a = SyncNode::new(flaky(false), &ka.public);
    let mut b = SyncNode::new(flaky(fail_b), &kb.public);
    let t = thread::spawn(move || {
        a.handshake(&mut sa).unwrap();
        (a, sa)
    });
    b.handshake(&mut sb).unwrap();
    let (a, sa) = t.join().unwrap();
    (a, sa, b, sb)
}

fn real_png() -> Vec<u8> {
    // 3x1: opaque red, half-alpha green, fully transparent
    rgba_to_png(3, 1, &[255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 0, 0]).unwrap()
}

fn padded_png(len: usize) -> Vec<u8> {
    let mut v = real_png();
    v.resize(len, 0);
    v
}

#[test]
fn image_with_alpha_crosses_the_link() {
    let (mut a, mut sa, mut b, mut sb) = setup(false);
    let png = real_png();
    a.clipboard_mut()
        .write(ClipboardContent::Image(png.clone()))
        .unwrap();
    assert_eq!(a.pump_local(&mut sa).unwrap(), 1);
    b.recv_one(&mut sb).unwrap();
    assert_eq!(
        b.clipboard().read().unwrap(),
        Some(ClipboardContent::Image(png))
    );
}

#[test]
fn ten_mib_image_transfers() {
    let (mut a, mut sa, mut b, mut sb) = setup(false);
    let big = padded_png(MAX_IMAGE_BYTES);
    let expect = big.clone();
    let h = thread::spawn(move || {
        a.clipboard_mut()
            .write(ClipboardContent::Image(big))
            .unwrap();
        assert_eq!(a.pump_local(&mut sa).unwrap(), 1);
    });
    b.recv_one(&mut sb).unwrap();
    h.join().unwrap();
    assert_eq!(
        b.clipboard().read().unwrap(),
        Some(ClipboardContent::Image(expect))
    );
}

#[test]
fn image_over_10_mib_is_dropped_with_event() {
    let (mut a, mut sa, _b, _sb) = setup(false);
    a.clipboard_mut()
        .write(ClipboardContent::Image(padded_png(MAX_IMAGE_BYTES + 1)))
        .unwrap();
    assert_eq!(a.pump_local(&mut sa).unwrap(), 0);
    assert_eq!(
        a.take_events(),
        vec![SyncEvent::ImageDropped(ImageError::TooLarge {
            bytes: MAX_IMAGE_BYTES + 1
        })]
    );
}

#[test]
fn failed_conversion_on_receiver_leaves_clipboard_unchanged() {
    let (mut a, mut sa, mut b, mut sb) = setup(true);
    b.clipboard_mut()
        .write(ClipboardContent::Text("keep me".into()))
        .unwrap();
    a.clipboard_mut()
        .write(ClipboardContent::Image(real_png()))
        .unwrap();
    assert_eq!(a.pump_local(&mut sa).unwrap(), 1);
    b.recv_one(&mut sb).unwrap();
    assert_eq!(
        b.clipboard().read().unwrap(),
        Some(ClipboardContent::Text("keep me".into()))
    );
    assert!(matches!(
        b.take_events()[..],
        [SyncEvent::ImageDropped(ImageError::Conversion(_))]
    ));
}
