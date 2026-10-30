use clipsync_core::transport::{connect, Keypair, Listener, MAX_CHUNK};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;

fn pair(allow_client: bool) -> (Listener, Keypair) {
    let server = Keypair::generate();
    let client = Keypair::generate();
    let allow = if allow_client {
        vec![client.public.clone()]
    } else {
        vec![]
    };
    let l = Listener::bind("127.0.0.1:0", server, allow).unwrap();
    (l, client)
}

#[test]
fn small_text_roundtrips_with_envelope_fields() {
    let (l, client) = pair(true);
    let addr = l.local_addr();
    let h = thread::spawn(move || {
        let mut s = l.accept().unwrap();
        s.recv().unwrap()
    });
    let mut c = connect(addr, client).unwrap();
    c.send(1, b"hello").unwrap();
    let m = h.join().unwrap();
    assert_eq!(m.content_type, 1);
    assert_eq!(m.payload, b"hello");
    assert_eq!(m.msg_id, 1);
    assert!(m.timestamp_ms > 1_600_000_000_000);
}

#[test]
fn one_mib_text_and_ten_mib_binary_cross_without_frame_errors() {
    let (l, client) = pair(true);
    let addr = l.local_addr();
    let text: Vec<u8> = (0..1_048_576).map(|i| b'a' + (i % 26) as u8).collect();
    let bin: Vec<u8> = (0..10 * 1_048_576u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    let (t2, b2) = (text.clone(), bin.clone());
    let h = thread::spawn(move || {
        let mut s = l.accept().unwrap();
        (s.recv().unwrap(), s.recv().unwrap())
    });
    let mut c = connect(addr, client).unwrap();
    c.send(1, &t2).unwrap();
    c.send(2, &b2).unwrap();
    let (a, b) = h.join().unwrap();
    assert_eq!((a.content_type, a.payload), (1, text));
    assert_eq!((b.content_type, b.payload), (2, bin));
    assert_eq!(b.msg_id, a.msg_id + 1);
}

#[test]
fn chunk_size_is_at_most_64_kib() {
    const { assert!(MAX_CHUNK <= 64 * 1024) };
}

#[test]
fn empty_and_chunk_boundary_payloads_roundtrip() {
    let (l, client) = pair(true);
    let addr = l.local_addr();
    let h = thread::spawn(move || {
        let mut s = l.accept().unwrap();
        (0..4)
            .map(|_| s.recv().unwrap().payload.len())
            .collect::<Vec<_>>()
    });
    let mut c = connect(addr, client).unwrap();
    for n in [0, MAX_CHUNK, MAX_CHUNK + 1, 2 * MAX_CHUNK] {
        c.send(1, &vec![7u8; n]).unwrap();
    }
    assert_eq!(
        h.join().unwrap(),
        vec![0, MAX_CHUNK, MAX_CHUNK + 1, 2 * MAX_CHUNK]
    );
}

#[test]
fn unknown_static_key_is_rejected() {
    let (l, client) = pair(false);
    let addr = l.local_addr();
    let h = thread::spawn(move || l.accept().err().map(|e| e.to_string()));
    let outcome = connect(addr, client).and_then(|mut c| {
        c.send(1, b"secret")?;
        c.recv() // server dropped us: must not succeed
    });
    assert!(outcome.is_err());
    let err = h.join().unwrap().expect("listener must reject");
    assert!(err.contains("allowlist"), "{err}");
}

/// Relays client->server traffic through a proxy that can mutate frames.
/// Client frames 0 and 1 are handshake messages; frame 2+ are data.
fn proxy(target: SocketAddr, mutate: fn(usize, Vec<u8>) -> Vec<Vec<u8>>) -> SocketAddr {
    let p = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = p.local_addr().unwrap();
    thread::spawn(move || {
        let (mut c, _) = p.accept().unwrap();
        let mut s = TcpStream::connect(target).unwrap();
        let mut s_back = s.try_clone().unwrap();
        let mut c_back = c.try_clone().unwrap();
        thread::spawn(move || {
            let _ = io::copy(&mut s_back, &mut c_back);
        });
        let mut i = 0;
        loop {
            let mut len = [0u8; 2];
            if c.read_exact(&mut len).is_err() {
                break;
            }
            let mut body = vec![0u8; u16::from_be_bytes(len) as usize];
            if c.read_exact(&mut body).is_err() {
                break;
            }
            for f in mutate(i, body) {
                let mut out = (f.len() as u16).to_be_bytes().to_vec();
                out.extend_from_slice(&f);
                if s.write_all(&out).is_err() {
                    return;
                }
            }
            i += 1;
        }
    });
    addr
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let (l, client) = pair(true);
    let addr = proxy(l.local_addr(), |i, mut f| {
        if i == 2 {
            let last = f.len() - 1;
            f[last] ^= 0x01;
        }
        vec![f]
    });
    let h = thread::spawn(move || l.accept().unwrap().recv());
    let mut c = connect(addr, client).unwrap();
    c.send(1, b"hello").unwrap();
    assert!(h.join().unwrap().is_err());
}

#[test]
fn replayed_message_is_rejected() {
    let (l, client) = pair(true);
    let addr = proxy(l.local_addr(), |i, f| {
        if i == 2 {
            vec![f.clone(), f]
        } else {
            vec![f]
        }
    });
    let h = thread::spawn(move || {
        let mut s = l.accept().unwrap();
        (s.recv().unwrap().payload, s.recv())
    });
    let mut c = connect(addr, client).unwrap();
    c.send(1, b"once").unwrap();
    let (first, second) = h.join().unwrap();
    assert_eq!(first, b"once");
    assert!(second.is_err(), "replayed frame must not be delivered");
}
