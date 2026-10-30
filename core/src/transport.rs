//! Encrypted, chunked transport: `Noise_XX_25519_ChaChaPoly_SHA256` over TCP with
//! 2-byte big-endian length framing and a typed, chunked message envelope.

use snow::{Builder, HandshakeState, TransportState};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
/// Noise's maximum transport message length.
const MAX_FRAME: usize = 65_535;
const TAG_LEN: usize = 16;
/// Envelope header: msg_id(8) content_type(1) timestamp_ms(8) payload_size(4) chunk_index(4) total_chunks(4).
const HEADER_LEN: usize = 29;
/// Largest chunk of payload that fits one Noise frame (always <= 64 KiB).
pub const MAX_CHUNK: usize = MAX_FRAME - TAG_LEN - HEADER_LEN;
/// Receiver-side memory guard on a reassembled payload.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("peer static key is not on the allowlist")]
    UnknownPeer,
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("replayed or out-of-order message")]
    Replay,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Keypair {
    pub public: Vec<u8>,
    private: Vec<u8>,
}

impl Keypair {
    pub fn generate() -> Self {
        let kp = Builder::new(NOISE_PARAMS.parse().unwrap())
            .generate_keypair()
            .expect("keypair generation");
        Keypair {
            public: kp.public,
            private: kp.private,
        }
    }
}

/// A fully reassembled application message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_id: u64,
    pub content_type: u8,
    pub timestamp_ms: u64,
    pub payload: Vec<u8>,
}

pub struct Session {
    stream: TcpStream,
    noise: TransportState,
    remote_static: Vec<u8>,
    next_send_id: u64,
    last_recv_id: u64,
}

pub struct Listener {
    listener: TcpListener,
    keypair: Keypair,
    allowlist: Vec<Vec<u8>>,
}

impl Listener {
    pub fn bind(
        addr: impl ToSocketAddrs,
        keypair: Keypair,
        allowlist: Vec<Vec<u8>>,
    ) -> Result<Self> {
        Ok(Listener {
            listener: TcpListener::bind(addr)?,
            keypair,
            allowlist,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().expect("bound listener")
    }

    /// Accept one connection. Handshakes whose static key is not allowlisted
    /// are dropped and reported as `Error::UnknownPeer`.
    pub fn accept(&self) -> Result<Session> {
        let (mut stream, _) = self.listener.accept()?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let mut hs = builder(&self.keypair)?.build_responder()?;
        let mut buf = vec![0u8; MAX_FRAME];
        handshake_read(&mut hs, &mut stream, &mut buf)?;
        handshake_write(&mut hs, &mut stream, &mut buf)?;
        handshake_read(&mut hs, &mut stream, &mut buf)?;
        let remote = remote_static(&hs)?;
        if !self.allowlist.contains(&remote) {
            return Err(Error::UnknownPeer);
        }
        finish(stream, hs, remote)
    }
}

pub fn connect(addr: impl ToSocketAddrs, keypair: Keypair) -> Result<Session> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut hs = builder(&keypair)?.build_initiator()?;
    let mut buf = vec![0u8; MAX_FRAME];
    handshake_write(&mut hs, &mut stream, &mut buf)?;
    handshake_read(&mut hs, &mut stream, &mut buf)?;
    handshake_write(&mut hs, &mut stream, &mut buf)?;
    let remote = remote_static(&hs)?;
    finish(stream, hs, remote)
}

fn builder(kp: &Keypair) -> Result<Builder<'_>> {
    Ok(Builder::new(NOISE_PARAMS.parse()?).local_private_key(&kp.private)?)
}

fn remote_static(hs: &HandshakeState) -> Result<Vec<u8>> {
    hs.get_remote_static()
        .map(<[u8]>::to_vec)
        .ok_or(Error::Protocol("no remote static key"))
}

fn finish(stream: TcpStream, hs: HandshakeState, remote_static: Vec<u8>) -> Result<Session> {
    stream.set_read_timeout(None)?;
    Ok(Session {
        stream,
        noise: hs.into_transport_mode()?,
        remote_static,
        next_send_id: 1,
        last_recv_id: 0,
    })
}

fn handshake_write(hs: &mut HandshakeState, s: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    let n = hs.write_message(&[], buf)?;
    write_frame(s, &buf[..n])
}

fn handshake_read(hs: &mut HandshakeState, s: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    let frame = read_frame(s)?;
    hs.read_message(&frame, buf)?;
    Ok(())
}

fn write_frame(s: &mut TcpStream, data: &[u8]) -> Result<()> {
    let len = u16::try_from(data.len()).map_err(|_| Error::Protocol("frame too large"))?;
    let mut out = Vec::with_capacity(2 + data.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(data);
    s.write_all(&out)?;
    Ok(())
}

fn read_frame(s: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    s.read_exact(&mut len)?;
    let mut data = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut data)?;
    Ok(data)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Session {
    /// Static public key of the authenticated peer.
    pub fn remote_static(&self) -> &[u8] {
        &self.remote_static
    }

    /// Send a payload of up to `MAX_PAYLOAD` bytes, chunked as needed. Returns the message id.
    pub fn send(&mut self, content_type: u8, payload: &[u8]) -> Result<u64> {
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Protocol("payload too large"));
        }
        let msg_id = self.next_send_id;
        self.next_send_id += 1;
        let ts = now_ms();
        let chunks: Vec<&[u8]> = if payload.is_empty() {
            vec![&[][..]]
        } else {
            payload.chunks(MAX_CHUNK).collect()
        };
        let total = chunks.len() as u32;
        let mut plain = Vec::with_capacity(MAX_FRAME);
        let mut cipher = vec![0u8; MAX_FRAME];
        for (i, chunk) in chunks.iter().enumerate() {
            plain.clear();
            plain.extend_from_slice(&msg_id.to_be_bytes());
            plain.push(content_type);
            plain.extend_from_slice(&ts.to_be_bytes());
            plain.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            plain.extend_from_slice(&(i as u32).to_be_bytes());
            plain.extend_from_slice(&total.to_be_bytes());
            plain.extend_from_slice(chunk);
            let n = self.noise.write_message(&plain, &mut cipher)?;
            write_frame(&mut self.stream, &cipher[..n])?;
        }
        Ok(msg_id)
    }

    /// Receive and reassemble the next message. Any error is fatal for the session.
    pub fn recv(&mut self) -> Result<Message> {
        let mut plain = vec![0u8; MAX_FRAME];
        let mut msg: Option<(Message, u32, usize)> = None; // message, total_chunks, declared size
        let mut next_idx = 0u32;
        loop {
            let frame = read_frame(&mut self.stream)?;
            // Tampered ciphertext and replayed frames (stale nonce) both fail here.
            let n = self.noise.read_message(&frame, &mut plain)?;
            let p = &plain[..n];
            if n < HEADER_LEN {
                return Err(Error::Protocol("short envelope"));
            }
            let msg_id = u64::from_be_bytes(p[0..8].try_into().unwrap());
            let content_type = p[8];
            let timestamp_ms = u64::from_be_bytes(p[9..17].try_into().unwrap());
            let size = u32::from_be_bytes(p[17..21].try_into().unwrap()) as usize;
            let idx = u32::from_be_bytes(p[21..25].try_into().unwrap());
            let total = u32::from_be_bytes(p[25..29].try_into().unwrap());
            let data = &p[HEADER_LEN..];

            if msg.is_none() {
                if msg_id <= self.last_recv_id {
                    return Err(Error::Replay);
                }
                if size > MAX_PAYLOAD || total as usize != size.div_ceil(MAX_CHUNK).max(1) {
                    return Err(Error::Protocol("bad envelope"));
                }
                msg = Some((
                    Message {
                        msg_id,
                        content_type,
                        timestamp_ms,
                        payload: Vec::with_capacity(size),
                    },
                    total,
                    size,
                ));
            }
            let (m, t, s) = msg.as_mut().unwrap();
            if msg_id != m.msg_id || total != *t || size != *s || idx != next_idx {
                return Err(Error::Protocol("inconsistent chunk"));
            }
            next_idx += 1;
            m.payload.extend_from_slice(data);
            if m.payload.len() > *s {
                return Err(Error::Protocol("payload exceeds declared size"));
            }
            if next_idx == *t {
                if m.payload.len() != *s {
                    return Err(Error::Protocol("payload size mismatch"));
                }
                let (m, _, _) = msg.take().unwrap();
                self.last_recv_id = m.msg_id;
                return Ok(m);
            }
        }
    }
}
