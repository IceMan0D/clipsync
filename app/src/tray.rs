//! Thin Tauri shell: runs the sync daemon in the background, renders `model` as a tray menu
//! and exposes the `commands::Controller` to the settings webview.

use crate::live::Live;
use clipsync_app::autostart::{self, Settings};
use clipsync_app::commands::{Backend, Controller, StateView};
use clipsync_app::notice::Notices;
use clipsync_app::model::{build_menu, tooltip, Device, MenuItem as Item, PauseState, ID_PAUSE, ID_QUIT, ID_SETTINGS};
use clipsync_clipboard::NativeClipboard;
use clipsync_core::daemon::{Daemon, DaemonConfig, DEFAULT_PORT};
use clipsync_core::discovery::{announce_addrs, system_interfaces, Discovery};
use clipsync_core::keystore::KeychainStore;
use clipsync_core::pairing::Node;
use clipsync_core::sync::node_id_from_public_key;
use clipsync_core::transport::Listener;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

type Ctl = Arc<Controller<Live>>;
const ID_DISMISS_NOTICES: &str = "dismiss_notices";

struct State {
    daemon: Arc<Daemon>,
    controller: Ctl,
    pause: Mutex<PauseState>,
    notices: Mutex<Notices>,
    discovery: Arc<Discovery>,
}

impl State {
    fn devices(&self) -> Vec<Device> {
        self.controller.backend().paired()
    }
}

#[tauri::command]
fn get_state(c: tauri::State<Ctl>) -> StateView {
    c.state()
}

#[tauri::command]
fn pair_start(c: tauri::State<Ctl>, address: Option<String>) -> Result<StateView, String> {
    c.pair_start(address.as_deref())
}

#[tauri::command]
fn pair_answer(c: tauri::State<Ctl>, confirmed: bool) -> StateView {
    c.pair_answer(confirmed)
}

#[tauri::command]
fn pair_cancel(c: tauri::State<Ctl>) -> StateView {
    c.pair_cancel()
}

#[tauri::command]
fn remove_device(c: tauri::State<Ctl>, key: String) -> Result<StateView, String> {
    c.remove_device(&key)
}

#[tauri::command]
fn add_address(c: tauri::State<Ctl>, key: String, address: String) -> Result<StateView, String> {
    c.add_address(&key, &address)
}

/// Everything the menu is built from. Compared between refreshes so the native menu is only
/// replaced when it actually changed (replacing it dismisses an open menu on macOS).
fn menu_signature(st: &State) -> (Vec<String>, Vec<Item>) {
    let paused = st.pause.lock().unwrap().is_paused();
    let notices = st.notices.lock().unwrap().messages();
    (notices, build_menu(&st.devices(), paused))
}

fn render(app: &AppHandle, st: &State) -> tauri::Result<Menu<tauri::Wry>> {
    let (notices, items) = menu_signature(st);
    let menu = Menu::new(app)?;
    if !notices.is_empty() {
        for m in notices.iter().rev() {
            menu.append(&MenuItem::new(app, format!("\u{26A0} {m}"), false, None::<&str>)?)?;
        }
        menu.append(&MenuItem::with_id(app, ID_DISMISS_NOTICES, "Dismiss notices", true, None::<&str>)?)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }
    for item in items {
        match item {
            Item::Label(t) => menu.append(&MenuItem::new(app, t, false, None::<&str>)?)?,
            Item::Separator => menu.append(&PredefinedMenuItem::separator(app)?)?,
            Item::Check { id, text, checked } => {
                menu.append(&CheckMenuItem::with_id(app, id, text, true, checked, None::<&str>)?)?
            }
            Item::Action { id, text } => {
                menu.append(&MenuItem::with_id(app, id, text, true, None::<&str>)?)?
            }
        }
    }
    Ok(menu)
}

fn start_daemon() -> Result<(Arc<Daemon>, Ctl, Arc<Discovery>), Box<dyn std::error::Error>> {
    let store = Arc::new(KeychainStore::new("clipsync"));
    let identity = store.load_or_create_identity()?;
    let node = Node::new(identity, store);
    let listener = Listener::bind(("0.0.0.0", DEFAULT_PORT), node.keypair().clone(), vec![])?;
    let addrs = announce_addrs(&system_interfaces());
    let id = node_id_from_public_key(&node.keypair().public);
    let discovery = Arc::new(Discovery::start(id, DEFAULT_PORT, &addrs)?);
    let book = Arc::new(super::load_book(Some(discovery.clone())));
    let daemon = Arc::new(Daemon::start(
        DaemonConfig { node, listener, resolver: book.clone(), auto_dial: true },
        NativeClipboard::new,
    ));
    let controller = Arc::new(Controller::new(Live::new(daemon.clone(), book, discovery.clone())));
    Ok((daemon, controller, discovery))
}

/// Bring a window in front of other apps. A tray-only app is not frontmost, so the OS would
/// otherwise open/raise the window behind the current app.
fn bring_to_front(w: &tauri::WebviewWindow) {
    let _ = w.unminimize();
    let _ = w.show();
    let _ = w.set_focus();
    // Windows may refuse SetForegroundWindow; a brief always-on-top toggle forces it forward.
    let _ = w.set_always_on_top(true);
    let _ = w.set_always_on_top(false);
    let _ = w.set_focus();
}

fn open_settings(app: &AppHandle) {
    let existing = app.get_webview_window("settings");
    let window = match existing {
        Some(w) => Some(w),
        None => WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
            .title("ClipSync Settings")
            .inner_size(520.0, 560.0)
            .focused(true)
            .build()
            .map_err(|e| eprintln!("settings window: {e}"))
            .ok(),
    };
    if let Some(w) = window {
        bring_to_front(&w);
    }
}

#[tauri::command]
fn get_autostart() -> bool {
    Settings::load(&super::settings_path()).autostart
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    autostart::apply(enabled).map_err(|e| e.to_string())?;
    Settings { autostart: enabled }.save(&super::settings_path()).map_err(|e| e.to_string())
}

pub fn run() {
    let (daemon, controller, discovery) = match start_daemon() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let state = Arc::new(State {
        daemon,
        controller: controller.clone(),
        pause: Mutex::new(PauseState::default()),
        notices: Mutex::new(Notices::default()),
        discovery,
    });

    // Make the OS login registration match the saved setting (on by default).
    if let Err(e) = autostart::apply(Settings::load(&super::settings_path()).autostart) {
        eprintln!("autostart: {e}");
    }

    let built = tauri::Builder::default()
        .manage(controller)
        .invoke_handler(tauri::generate_handler![
            get_autostart,
            set_autostart,
            get_state,
            pair_start,
            pair_answer,
            pair_cancel,
            remove_device,
            add_address
        ])
        .setup(move |app| {
            // Menu bar only: no Dock icon on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let hook = state.daemon.change_hook();
            let discovery = state.discovery.clone();
            clipsync_core::power::install(Arc::new(move || {
                if let Err(e) = discovery.refresh(&clipsync_core::power::current_addrs()) { eprintln!("mDNS refresh: {e}"); }
                hook();
            }));

            let handle = app.handle().clone();
            let menu = render(&handle, &state)?;
            let st = state.clone();
            let tray = TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().expect("window icon").clone())
                .tooltip("ClipSync")
                .menu(&menu)
                .on_menu_event(move |app, ev| match ev.id().as_ref() {
                    ID_PAUSE => {
                        let paused = st.pause.lock().unwrap().toggle();
                        st.daemon.set_paused(paused);
                    }
                    ID_SETTINGS => open_settings(app),
                    ID_DISMISS_NOTICES => st.notices.lock().unwrap().clear(),
                    ID_QUIT => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Refresh device statuses and the pause check mark.
            let st = state.clone();
            let mut last = menu_signature(&state);
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(2));
                {
                    let mut n = st.notices.lock().unwrap();
                    for ev in st.daemon.take_events() {
                        n.push_event(&ev);
                    }
                }
                let sig = menu_signature(&st);
                let changed = sig != last;
                last = sig;
                let (h, st2, tray) = (handle.clone(), st.clone(), tray.clone());
                let _ = handle.run_on_main_thread(move || {
                    if changed {
                        if let Ok(m) = render(&h, &st2) {
                            let _ = tray.set_menu(Some(m));
                        }
                    }
                    let paused = st2.pause.lock().unwrap().is_paused();
                    let _ = tray.set_tooltip(Some(tooltip(&st2.devices(), paused)));
                });
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tray app");

    // Closing the settings window must not quit the tray app.
    built.run(|_app, ev| {
        if let tauri::RunEvent::ExitRequested { api, code, .. } = ev {
            if code.is_none() {
                api.prevent_exit();
            }
        }
    });
}
