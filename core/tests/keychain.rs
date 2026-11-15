#![cfg(any(target_os = "macos", target_os = "windows"))]
use clipsync_core::keystore::{KeyStore, KeychainStore};

/// Touches the real OS keychain; run with `cargo test -- --ignored`.
#[test]
#[ignore]
fn os_keychain_persists_and_deletes_pinned_keys() {
    let service = format!("clipsync-test-{}", std::process::id());
    let s = KeychainStore::new(service.clone());
    s.add(&[1, 2, 3]).unwrap();
    s.add(&[4, 5]).unwrap();
    // A fresh handle sees the persisted keys.
    let again = KeychainStore::new(service);
    assert_eq!(again.load().unwrap(), vec![vec![1, 2, 3], vec![4, 5]]);
    again.remove(&[1, 2, 3]).unwrap();
    assert_eq!(s.load().unwrap(), vec![vec![4, 5]]);
    s.remove(&[4, 5]).unwrap();
}
