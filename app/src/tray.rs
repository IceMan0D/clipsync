//! Thin Tauri shell: runs the sync daemon in the background and renders `model` as a tray menu.

use clipsync_app::autostart::{self, Settings};
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

const ID_DISMISS_NOTICES: &str = "dismiss_notices";

struct State {
    daemon: Daemon,
    node: Node,
    pause: Mutex<PauseState>,
    notices: Mutex<Notices>,
}

impl State {
    fn devices(&self) -> Vec<Device> {
        self.node
            .paired_devices()
            .unwrap_or_default()
            .into_iter()
            .map(|key| Device { status: self.daemon.peer_state(&key).into(), key })
            .collect()
    }
}

fn render(app: &AppHandle, st: &State) -> tauri::Result<Menu<tauri::Wry>> {
    let paused = st.pause.lock().unwrap().is_paused();
    let menu = Menu::new(app)?;
    let notices = st.notices.lock().unwrap().messages();
    if !notices.is_empty() {
        for m in notices.iter().rev() {
            menu.append(&MenuItem::new(app, format!("\u{26A0} {m}"), false, None::<&str>)?)?;
        }
        menu.append(&MenuItem::with_id(app, ID_DISMISS_NOTICES, "Dismiss notices", true, None::<&str>)?)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }
    for item in build_menu(&st.devices(), paused) {
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

fn start_daemon() -> Result<(Daemon, Node), Box<dyn std::error::Error>> {
    let store = Arc::new(KeychainStore::new("clipsync"));
    let identity = store.load_or_create_identity()?;
    let node = Node::new(identity.clone(), store.clone());
    let listener = Listener::bind(("0.0.0.0", DEFAULT_PORT), node.keypair().clone(), vec![])?;
    let addrs = announce_addrs(&system_interfaces());
    let id = node_id_from_public_key(&node.keypair().public);
    let discovery = Arc::new(Discovery::start(id, DEFAULT_PORT, &addrs)?);
    let daemon = Daemon::start(
        DaemonConfig { node, listener, resolver: Arc::new(super::load_book(Some(discovery))), auto_dial: true },
        NativeClipboard::new,
    );
    Ok((daemon, Node::new(identity, store)))
}

fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("ClipSync Settings")
        .inner_size(480.0, 320.0)
        .build();
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
    let (daemon, node) = match start_daemon() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let state = Arc::new(State { daemon, node, pause: Mutex::new(PauseState::default()),
        notices: Mutex::new(Notices::default()),
    });

    // Make the OS login registration match the saved setting (on by default).
    if let Err(e) = autostart::apply(Settings::load(&super::settings_path()).autostart) {
        eprintln!("autostart: {e}");
    }

    let built = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_autostart, set_autostart])
        .setup(move |app| {
            // Menu bar only: no Dock icon on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

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
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(2));
                {
                    let mut n = st.notices.lock().unwrap();
                    for ev in st.daemon.take_events() {
                        n.push_event(&ev);
                    }
                }
                let (h, st2, tray) = (handle.clone(), st.clone(), tray.clone());
                let _ = handle.run_on_main_thread(move || {
                    if let Ok(m) = render(&h, &st2) {
                        let _ = tray.set_menu(Some(m));
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
