//! Settings window command handlers. The webview only calls these and renders the
//! returned `StateView`; all behaviour lives here and is testable with a fake `Backend`.

use crate::model::Device;
use crate::pairing_ui::{Observation, Phase, PairingUi};
use clipsync_core::addresses::PeerAddr;
use clipsync_core::pairing::{PairingStatus, MAX_FAILED_ATTEMPTS};
use serde::Serialize;
use std::sync::Mutex;

/// Everything the commands need from the running app.
pub trait Backend: Send + Sync {
    fn paired(&self) -> Vec<Device>;
    fn addresses(&self, key: &[u8]) -> Vec<String>;
    /// Delete the key and disconnect the device.
    fn remove(&self, key: &[u8]) -> Result<(), String>;
    /// Persist a manual address for a paired device; takes effect on the next dial.
    fn add_address(&self, key: &[u8], addr: PeerAddr) -> Result<(), String>;
    /// Open the pairing window and start looking for the other device. With an
    /// address, dial it directly (e.g. Tailscale); otherwise use LAN discovery.
    fn open_pairing(&self, addr: Option<PeerAddr>);
    /// Close the pairing window and drop any unanswered attempt.
    fn close_pairing(&self);
    fn pairing_status(&self) -> PairingStatus;
    fn pending_sas(&self) -> Option<String>;
    /// Answer the pending SAS prompt; the outcome is later returned by `take_finished`.
    fn decide(&self, confirmed: bool);
    fn take_finished(&self) -> Option<Result<(), String>>;
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct DeviceView {
    pub key: String,
    pub name: String,
    pub status: &'static str,
    pub addresses: Vec<String>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct PairingView {
    /// idle | searching | confirm | waiting | paired | failed | locked | expired
    pub phase: &'static str,
    pub sas: Option<String>,
    pub message: Option<String>,
    pub failed_attempts: u32,
    pub max_failed_attempts: u32,
    pub seconds_left: Option<u64>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct StateView {
    pub devices: Vec<DeviceView>,
    pub pairing: PairingView,
}

pub struct Controller<B: Backend> {
    backend: B,
    ui: Mutex<PairingUi>,
}

fn decode_key(hex_key: &str) -> Result<Vec<u8>, String> {
    hex::decode(hex_key).map_err(|_| "invalid device id".to_string())
}

fn phase_name(p: &Phase) -> &'static str {
    match p {
        Phase::Idle => "idle",
        Phase::Searching => "searching",
        Phase::Confirm { .. } => "confirm",
        Phase::Waiting { .. } => "waiting",
        Phase::Paired => "paired",
        Phase::Failed => "failed",
        Phase::Locked => "locked",
        Phase::Expired => "expired",
    }
}

impl<B: Backend> Controller<B> {
    pub fn new(backend: B) -> Self {
        Controller { backend, ui: Mutex::new(PairingUi::default()) }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Current snapshot; also advances the pairing state machine. Poll this.
    pub fn state(&self) -> StateView {
        let mut ui = self.ui.lock().unwrap();
        let was_active = ui.is_active();
        let status = self.backend.pairing_status();
        ui.update(&Observation {
            status,
            pending_sas: self.backend.pending_sas(),
            finished: self.backend.take_finished(),
        });
        if was_active && !ui.is_active() {
            self.backend.close_pairing();
        }
        let sas = match ui.phase() {
            Phase::Confirm { sas } | Phase::Waiting { sas } => Some(sas.clone()),
            _ => None,
        };
        let devices = self
            .backend
            .paired()
            .into_iter()
            .map(|d| DeviceView {
                key: hex::encode(&d.key),
                name: d.name(),
                status: d.status.label(),
                addresses: self.backend.addresses(&d.key),
            })
            .collect();
        StateView {
            devices,
            pairing: PairingView {
                phase: phase_name(ui.phase()),
                sas,
                message: ui.message().map(String::from),
                failed_attempts: status.failed,
                max_failed_attempts: MAX_FAILED_ATTEMPTS,
                seconds_left: status.open_for.map(|d| d.as_secs()),
            },
        }
    }

    /// "Pair New Device" (optionally with an address to dial).
    pub fn pair_start(&self, address: Option<&str>) -> Result<StateView, String> {
        let addr = match address.map(str::trim).filter(|a| !a.is_empty()) {
            Some(a) => Some(PeerAddr::parse(a).map_err(|_| format!("Invalid address: {a}"))?),
            None => None,
        };
        self.ui.lock().unwrap().start();
        self.backend.open_pairing(addr);
        Ok(self.state())
    }

    pub fn pair_answer(&self, confirmed: bool) -> StateView {
        if self.ui.lock().unwrap().answer(confirmed) {
            self.backend.decide(confirmed);
        }
        self.state()
    }

    pub fn pair_cancel(&self) -> StateView {
        self.ui.lock().unwrap().cancel();
        self.backend.close_pairing();
        self.state()
    }

    pub fn remove_device(&self, hex_key: &str) -> Result<StateView, String> {
        let key = decode_key(hex_key)?;
        if !self.backend.paired().iter().any(|d| d.key == key) {
            return Err("No such paired device.".into());
        }
        self.backend.remove(&key)?;
        Ok(self.state())
    }

    pub fn add_address(&self, hex_key: &str, address: &str) -> Result<StateView, String> {
        let key = decode_key(hex_key)?;
        if !self.backend.paired().iter().any(|d| d.key == key) {
            return Err("No such paired device.".into());
        }
        let addr = PeerAddr::parse(address).map_err(|_| format!("Invalid address: {}", address.trim()))?;
        self.backend.add_address(&key, addr)?;
        Ok(self.state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceStatus;
    use std::sync::Mutex;
    use std::time::Duration;

    struct DeviceExt;
    impl DeviceExt {
        fn offline(key: Vec<u8>) -> Device {
            Device { key, status: DeviceStatus::Offline }
        }
    }

    #[derive(Default)]
    struct Fake {
        devices: Mutex<Vec<Device>>,
        addrs: Mutex<Vec<(Vec<u8>, PeerAddr)>>,
        status: Mutex<Option<PairingStatus>>,
        sas: Mutex<Option<String>>,
        finished: Mutex<Option<Result<(), String>>>,
        log: Mutex<Vec<String>>,
    }

    impl Fake {
        fn open_status() -> PairingStatus {
            PairingStatus { open_for: Some(Duration::from_secs(90)), failed: 0, locked: false }
        }
    }

    impl Backend for &Fake {
        fn paired(&self) -> Vec<Device> {
            self.devices.lock().unwrap().clone()
        }
        fn addresses(&self, key: &[u8]) -> Vec<String> {
            self.addrs
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, a)| format!("{a:?}"))
                .collect()
        }
        fn remove(&self, key: &[u8]) -> Result<(), String> {
            self.devices.lock().unwrap().retain(|d| d.key != key);
            self.log.lock().unwrap().push("remove".into());
            Ok(())
        }
        fn add_address(&self, key: &[u8], addr: PeerAddr) -> Result<(), String> {
            self.addrs.lock().unwrap().push((key.to_vec(), addr));
            Ok(())
        }
        fn open_pairing(&self, addr: Option<PeerAddr>) {
            self.log.lock().unwrap().push(format!("open {}", addr.is_some()));
            *self.status.lock().unwrap() = Some(Fake::open_status());
        }
        fn close_pairing(&self) {
            self.log.lock().unwrap().push("close".into());
        }
        fn pairing_status(&self) -> PairingStatus {
            self.status
                .lock()
                .unwrap()
                .unwrap_or(PairingStatus { open_for: None, failed: 0, locked: false })
        }
        fn pending_sas(&self) -> Option<String> {
            self.sas.lock().unwrap().clone()
        }
        fn decide(&self, confirmed: bool) {
            self.log.lock().unwrap().push(format!("decide {confirmed}"));
            *self.sas.lock().unwrap() = None;
        }
        fn take_finished(&self) -> Option<Result<(), String>> {
            self.finished.lock().unwrap().take()
        }
    }

    fn key(b: u8) -> Vec<u8> {
        vec![b, 0xab, 0xcd, 0xef, 1, 2]
    }

    #[test]
    fn lists_paired_devices_with_hex_id_status_and_addresses() {
        let f = Fake::default();
        f.devices.lock().unwrap().push(Device { key: key(1), status: DeviceStatus::Connected });
        let c = Controller::new(&f);
        let s = c.state();
        assert_eq!(s.devices.len(), 1);
        assert_eq!(s.devices[0].key, "01abcdef0102");
        assert_eq!(s.devices[0].status, "Connected");
        assert_eq!(s.pairing.phase, "idle");
    }

    #[test]
    fn pairing_flow_shows_sas_then_waits_then_pairs() {
        let f = Fake::default();
        let c = Controller::new(&f);
        assert_eq!(c.pair_start(None).unwrap().pairing.phase, "searching");
        assert_eq!(f.log.lock().unwrap()[0], "open false");

        *f.sas.lock().unwrap() = Some("042517".into());
        let s = c.state();
        assert_eq!((s.pairing.phase, s.pairing.sas.as_deref()), ("confirm", Some("042517")));

        let s = c.pair_answer(true);
        assert_eq!((s.pairing.phase, s.pairing.sas.as_deref()), ("waiting", Some("042517")));
        assert!(f.log.lock().unwrap().contains(&"decide true".to_string()));

        f.devices.lock().unwrap().push(DeviceExt::offline(key(2)));
        *f.finished.lock().unwrap() = Some(Ok(()));
        let s = c.state();
        assert_eq!(s.pairing.phase, "paired");
        assert_eq!(s.devices.len(), 1);
        assert!(f.log.lock().unwrap().contains(&"close".to_string()), "window closed after pairing");
    }

    #[test]
    fn answering_with_no_prompt_does_not_reach_the_backend() {
        let f = Fake::default();
        let c = Controller::new(&f);
        c.pair_start(None).unwrap();
        c.pair_answer(true);
        assert!(!f.log.lock().unwrap().iter().any(|l| l.starts_with("decide")));
    }

    #[test]
    fn lockout_after_three_failures_is_visible() {
        let f = Fake::default();
        let c = Controller::new(&f);
        c.pair_start(None).unwrap();
        *f.sas.lock().unwrap() = Some("111111".into());
        c.state();
        c.pair_answer(true);
        *f.status.lock().unwrap() = Some(PairingStatus { open_for: None, failed: 3, locked: true });
        *f.finished.lock().unwrap() = Some(Err("codes did not match".into()));
        let s = c.state();
        assert_eq!(s.pairing.phase, "locked");
        assert_eq!((s.pairing.failed_attempts, s.pairing.max_failed_attempts), (3, 3));
        assert!(s.pairing.message.unwrap().contains("locked"));
    }

    #[test]
    fn failure_count_is_shown_while_still_open() {
        let f = Fake::default();
        let c = Controller::new(&f);
        c.pair_start(None).unwrap();
        *f.status.lock().unwrap() = Some(PairingStatus { failed: 2, ..Fake::open_status() });
        *f.finished.lock().unwrap() = Some(Err("x".into()));
        let s = c.state();
        assert_eq!((s.pairing.phase, s.pairing.failed_attempts), ("failed", 2));
    }

    #[test]
    fn pair_start_validates_the_optional_address() {
        let f = Fake::default();
        let c = Controller::new(&f);
        assert!(c.pair_start(Some("not a host!")).is_err());
        assert!(f.log.lock().unwrap().is_empty());
        c.pair_start(Some(" box.ts.net:5000 ")).unwrap();
        assert_eq!(f.log.lock().unwrap()[0], "open true");
        // blank address means LAN discovery
        c.pair_start(Some("  ")).unwrap();
        assert_eq!(f.log.lock().unwrap()[1], "open false");
    }

    #[test]
    fn cancel_closes_the_window_and_goes_idle() {
        let f = Fake::default();
        let c = Controller::new(&f);
        c.pair_start(None).unwrap();
        *f.status.lock().unwrap() = None; // backend closed it
        let s = c.pair_cancel();
        assert_eq!(s.pairing.phase, "idle");
        assert!(f.log.lock().unwrap().contains(&"close".to_string()));
    }

    #[test]
    fn remove_device_deletes_it_via_the_backend() {
        let f = Fake::default();
        f.devices.lock().unwrap().push(Device { key: key(1), status: DeviceStatus::Connected });
        let c = Controller::new(&f);
        let s = c.remove_device("01abcdef0102").unwrap();
        assert!(s.devices.is_empty());
        assert_eq!(f.log.lock().unwrap().as_slice(), ["remove"]);
    }

    #[test]
    fn remove_rejects_bad_or_unknown_ids() {
        let f = Fake::default();
        let c = Controller::new(&f);
        assert!(c.remove_device("zz").is_err());
        assert!(c.remove_device("0102").is_err());
        assert!(f.log.lock().unwrap().is_empty());
    }

    #[test]
    fn add_address_parses_and_attaches_to_a_paired_device() {
        let f = Fake::default();
        f.devices.lock().unwrap().push(DeviceExt::offline(key(1)));
        let c = Controller::new(&f);
        let s = c.add_address("01abcdef0102", "100.64.0.9").unwrap();
        assert_eq!(s.devices[0].addresses.len(), 1);
        assert!(c.add_address("01abcdef0102", "bad address").is_err());
        assert!(c.add_address("ffff", "100.64.0.9").is_err());
        assert_eq!(f.addrs.lock().unwrap().len(), 1);
    }
}
