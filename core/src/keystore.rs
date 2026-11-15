//! Pinned peer key storage behind a trait, so the OS keychain can be swapped
//! for an in-memory store in tests.

use crate::transport::{Error, Result};
use std::sync::Mutex;

pub trait KeyStore: Send + Sync {
    /// All pinned peer static public keys.
    fn load(&self) -> Result<Vec<Vec<u8>>>;
    /// Pin a key (idempotent).
    fn add(&self, key: &[u8]) -> Result<()>;
    /// Delete a pinned key (no-op if absent).
    fn remove(&self, key: &[u8]) -> Result<()>;
}

#[derive(Default)]
pub struct MemoryKeyStore {
    keys: Mutex<Vec<Vec<u8>>>,
}

impl KeyStore for MemoryKeyStore {
    fn load(&self) -> Result<Vec<Vec<u8>>> {
        Ok(self.keys.lock().unwrap().clone())
    }

    fn add(&self, key: &[u8]) -> Result<()> {
        let mut keys = self.keys.lock().unwrap();
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_vec());
        }
        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        self.keys.lock().unwrap().retain(|k| k != key);
        Ok(())
    }
}

/// OS keychain backed store (macOS Keychain, Windows Credential Manager).
/// The pinned list is kept as one hex-encoded, comma-separated secret since
/// keychains cannot be enumerated portably.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub struct KeychainStore {
    service: String,
    lock: Mutex<()>,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl KeychainStore {
    pub fn new(service: impl Into<String>) -> Self {
        KeychainStore {
            service: service.into(),
            lock: Mutex::new(()),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, account).map_err(|e| Error::Store(e.to_string()))
    }

    fn read(&self, account: &str) -> Result<Option<String>> {
        match self.entry(account)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Store(e.to_string())),
        }
    }

    fn write(&self, account: &str, value: &str) -> Result<()> {
        self.entry(account)?
            .set_password(value)
            .map_err(|e| Error::Store(e.to_string()))
    }

    fn save_keys(&self, keys: &[Vec<u8>]) -> Result<()> {
        let joined: Vec<String> = keys.iter().map(hex::encode).collect();
        self.write("pinned-keys", &joined.join(","))
    }

    /// This device's own static keypair, created on first use.
    pub fn load_or_create_identity(&self) -> Result<crate::transport::Keypair> {
        let _g = self.lock.lock().unwrap();
        if let Some(v) = self.read("identity")? {
            if let Some((p, s)) = v.split_once(':') {
                if let (Ok(p), Ok(s)) = (hex::decode(p), hex::decode(s)) {
                    return Ok(crate::transport::Keypair::from_parts(p, s));
                }
            }
        }
        let kp = crate::transport::Keypair::generate();
        self.write(
            "identity",
            &format!(
                "{}:{}",
                hex::encode(&kp.public),
                hex::encode(kp.private_bytes())
            ),
        )?;
        Ok(kp)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl KeyStore for KeychainStore {
    fn load(&self) -> Result<Vec<Vec<u8>>> {
        let _g = self.lock.lock().unwrap();
        Ok(self
            .read("pinned-keys")?
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| hex::decode(s).ok())
            .collect())
    }

    fn add(&self, key: &[u8]) -> Result<()> {
        let mut keys = self.load()?;
        let _g = self.lock.lock().unwrap();
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_vec());
            self.save_keys(&keys)?;
        }
        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        let mut keys = self.load()?;
        let _g = self.lock.lock().unwrap();
        keys.retain(|k| k != key);
        self.save_keys(&keys)
    }
}
