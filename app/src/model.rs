//! Pure tray-menu model: device status and menu layout. No UI toolkit here, so it is unit-testable.

use clipsync_core::connection::ConnState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceStatus {
    Connected,
    Connecting,
    Offline,
}

impl From<ConnState> for DeviceStatus {
    fn from(s: ConnState) -> Self {
        match s {
            ConnState::Connected => DeviceStatus::Connected,
            ConnState::Connecting | ConnState::Handshaking => DeviceStatus::Connecting,
            ConnState::Disconnected => DeviceStatus::Offline,
        }
    }
}

impl DeviceStatus {
    pub fn label(self) -> &'static str {
        match self {
            DeviceStatus::Connected => "Connected",
            DeviceStatus::Connecting => "Connecting…",
            DeviceStatus::Offline => "Offline",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub key: Vec<u8>,
    pub status: DeviceStatus,
}

impl Device {
    /// Short human-readable name derived from the public key (no names are stored yet).
    pub fn name(&self) -> String {
        let hex: String = self.key.iter().take(4).map(|b| format!("{b:02x}")).collect();
        format!("Device {hex}")
    }
}

/// Stable ids for actionable menu entries.
pub const ID_PAUSE: &str = "pause";
pub const ID_SETTINGS: &str = "settings";
pub const ID_QUIT: &str = "quit";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuItem {
    /// Non-clickable text line.
    Label(String),
    Separator,
    /// Checkable item; `checked` true means sync is paused.
    Check { id: &'static str, text: String, checked: bool },
    Action { id: &'static str, text: String },
}

/// Pause state with toggle semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PauseState(bool);

impl PauseState {
    pub fn is_paused(self) -> bool {
        self.0
    }
    /// Flip and return the new value.
    pub fn toggle(&mut self) -> bool {
        self.0 = !self.0;
        self.0
    }
}

pub fn build_menu(devices: &[Device], paused: bool) -> Vec<MenuItem> {
    let mut m = Vec::new();
    if devices.is_empty() {
        m.push(MenuItem::Label("No paired devices".into()));
    } else {
        for d in devices {
            m.push(MenuItem::Label(format!("{} — {}", d.name(), d.status.label())));
        }
    }
    m.push(MenuItem::Separator);
    m.push(MenuItem::Check { id: ID_PAUSE, text: "Pause sync".into(), checked: paused });
    m.push(MenuItem::Action { id: ID_SETTINGS, text: "Settings…".into() });
    m.push(MenuItem::Separator);
    m.push(MenuItem::Action { id: ID_QUIT, text: "Quit ClipSync".into() });
    m
}

/// Tray tooltip summarising state.
pub fn tooltip(devices: &[Device], paused: bool) -> String {
    let n = devices.iter().filter(|d| d.status == DeviceStatus::Connected).count();
    if paused {
        "ClipSync — paused".into()
    } else {
        format!("ClipSync — {n} device(s) connected")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(b: u8, status: DeviceStatus) -> Device {
        Device { key: vec![b, 0xab, 0xcd, 0xef, 1, 2], status }
    }

    #[test]
    fn conn_states_map_to_user_facing_status() {
        assert_eq!(DeviceStatus::from(ConnState::Connected), DeviceStatus::Connected);
        assert_eq!(DeviceStatus::from(ConnState::Connecting), DeviceStatus::Connecting);
        assert_eq!(DeviceStatus::from(ConnState::Handshaking), DeviceStatus::Connecting);
        assert_eq!(DeviceStatus::from(ConnState::Disconnected), DeviceStatus::Offline);
    }

    #[test]
    fn menu_lists_each_device_with_status_then_pause_settings_quit() {
        let m = build_menu(&[dev(1, DeviceStatus::Connected), dev(2, DeviceStatus::Offline)], false);
        assert_eq!(
            m,
            vec![
                MenuItem::Label("Device 01abcdef — Connected".into()),
                MenuItem::Label("Device 02abcdef — Offline".into()),
                MenuItem::Separator,
                MenuItem::Check { id: ID_PAUSE, text: "Pause sync".into(), checked: false },
                MenuItem::Action { id: ID_SETTINGS, text: "Settings…".into() },
                MenuItem::Separator,
                MenuItem::Action { id: ID_QUIT, text: "Quit ClipSync".into() },
            ]
        );
    }

    #[test]
    fn empty_device_list_says_so() {
        let m = build_menu(&[], false);
        assert_eq!(m[0], MenuItem::Label("No paired devices".into()));
    }

    #[test]
    fn pause_item_reflects_state() {
        let m = build_menu(&[], true);
        assert!(m.contains(&MenuItem::Check { id: ID_PAUSE, text: "Pause sync".into(), checked: true }));
    }

    #[test]
    fn pause_state_toggles_back_and_forth() {
        let mut p = PauseState::default();
        assert!(!p.is_paused());
        assert!(p.toggle());
        assert!(p.is_paused());
        assert!(!p.toggle());
    }

    #[test]
    fn tooltip_counts_connected_or_says_paused() {
        let d = [dev(1, DeviceStatus::Connected), dev(2, DeviceStatus::Connecting)];
        assert_eq!(tooltip(&d, false), "ClipSync — 1 device(s) connected");
        assert_eq!(tooltip(&d, true), "ClipSync — paused");
    }
}
