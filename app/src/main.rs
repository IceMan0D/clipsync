// Entry point: tray app by default, plus a CLI for SAS pairing and device management.
// GUI subsystem on Windows so launching the tray app doesn't open a console window;
// CLI subcommands re-attach to the parent console (see `attach_parent_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]
mod live;
mod tray;

use clipsync_clipboard::NativeClipboard;
use clipsync_core::addresses::{CandidateResolver, PeerAddr};
use clipsync_core::connection::Resolver;
use clipsync_core::daemon::{Daemon, DaemonConfig, DEFAULT_PORT as PORT};
use clipsync_core::discovery::{announce_addrs, system_interfaces, Discovery};
use clipsync_core::keystore::KeychainStore;
use clipsync_core::pairing::{Incoming, Node, PairingAttempt};
use clipsync_core::sync::node_id_from_public_key;
use clipsync_core::transport::Listener;
use std::io::{self, BufRead, Write};
use std::sync::Arc;


fn book_path() -> std::path::PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")))
        .unwrap_or_default();
    base.join("clipsync").join("addresses.txt")
}

fn settings_path() -> std::path::PathBuf {
    book_path().with_file_name("settings.txt")
}

fn load_book(primary: Option<Arc<dyn Resolver>>) -> CandidateResolver {
    let book = CandidateResolver::new(primary);
    if let Ok(text) = std::fs::read_to_string(book_path()) {
        book.load_text(&text);
    }
    book
}

fn save_book(book: &CandidateResolver) -> io::Result<()> {
    let p = book_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, book.to_text())
}

fn prompt_confirm(att: &PairingAttempt) -> bool {
    println!("Pairing code: {}", att.sas());
    print!("Does the other device show the same code? [y/N] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    matches!(line.trim(), "y" | "Y" | "yes")
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Launched with no subcommand (e.g. from the app bundle): run as the tray app.
    if args.is_empty() || args[0] == "app" {
        tray::run();
        return Ok(());
    }
    let store = Arc::new(KeychainStore::new("clipsync"));
    let node = Node::new(store.load_or_create_identity()?, store);
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["pair-listen"] | ["pair-listen", _] => {
            let addr = args
                .get(1)
                .cloned()
                .unwrap_or_else(|| format!("0.0.0.0:{PORT}"));
            let listener = Listener::bind(addr, node.keypair().clone(), vec![])?;
            node.start_pairing();
            println!("Pairing window open for 2 minutes on {}", listener.local_addr());
            match node.accept(&listener)? {
                Incoming::Pairing(att) => {
                    let ok = prompt_confirm(&att);
                    att.confirm(ok)?;
                    println!("Paired.");
                }
                Incoming::Trusted(_) => println!("Peer is already paired."),
            }
        }
        ["pair-connect", addr] => {
            node.start_pairing();
            let att = node.dial_pairing(addr.to_string())?;
            let ok = prompt_confirm(&att);
            att.confirm(ok)?;
            println!("Paired.");
        }
        ["run"] | ["run", _] => {
            let port: u16 = match args.get(1) {
                Some(p) => p.parse()?,
                None => PORT,
            };
            let listener = Listener::bind(("0.0.0.0", port), node.keypair().clone(), vec![])?;
            let addrs = announce_addrs(&system_interfaces());
            let id = node_id_from_public_key(&node.keypair().public);
            let discovery = Arc::new(Discovery::start(id, port, &addrs)?);
            println!("Listening on port {port}; announcing on {addrs:?}");
            let paired = node.paired_devices()?.len();
            if paired == 0 {
                println!("No paired devices yet: run `clipsync pair-listen` on one machine and `pair-connect` on the other.");
            }
            let daemon = Daemon::start(
                DaemonConfig {
                    node,
                    listener,
                    resolver: Arc::new(load_book(Some(discovery.clone()))),
                    auto_dial: true,
                },
                NativeClipboard::new,
            );
            let hook = daemon.change_hook();
            clipsync_core::power::install(Arc::new(move || {
                if let Err(e) = discovery.refresh(&clipsync_core::power::current_addrs()) { eprintln!("mDNS refresh: {e}"); }
                hook();
            }));
            println!("Syncing with {paired} paired device(s). Ctrl-C to quit.");
            #[cfg(target_os = "macos")]
            clipsync_core::power::wake::run_event_loop();
            loop {
                std::thread::park();
            }
        }
        ["add-address", key, addr] => {
            let key = hex::decode(key)?;
            if !node.paired_devices()?.contains(&key) {
                return Err("no such paired device (see `clipsync devices`)".into());
            }
            let book = load_book(None);
            book.add_manual(node_id_from_public_key(&key), PeerAddr::parse(addr)?);
            save_book(&book)?;
            println!("Added.");
        }
        ["devices"] => {
            for key in node.paired_devices()? {
                println!("{}", hex::encode(key));
            }
        }
        ["remove", key] => {
            node.remove_device(&hex::decode(key)?)?;
            println!("Removed.");
        }
        _ => eprintln!("usage: clipsync [app] | run [port] | pair-listen [addr] | pair-connect <addr> | devices | add-address <hex-key> <addr> | remove <hex-key>"),
    }
    Ok(())
}

/// A GUI-subsystem exe has no console; when run from a terminal with a subcommand,
/// attach to the parent's console and point the std handles at it so output shows up.
#[cfg(windows)]
fn attach_parent_console() {
    use std::os::windows::io::AsRawHandle;
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn SetStdHandle(which: u32, handle: *mut std::ffi::c_void) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let open = |name: &str, write: bool| {
            std::fs::OpenOptions::new().read(!write).write(write).open(name).ok()
        };
        if let Some(f) = open("CONIN$", false) {
            SetStdHandle(STD_INPUT_HANDLE, f.as_raw_handle() as _);
            std::mem::forget(f);
        }
        if let Some(f) = open("CONOUT$", true) {
            SetStdHandle(STD_OUTPUT_HANDLE, f.as_raw_handle() as _);
            SetStdHandle(STD_ERROR_HANDLE, f.as_raw_handle() as _);
            std::mem::forget(f);
        }
    }
}

fn main() {
    #[cfg(windows)]
    if std::env::args().nth(1).is_some_and(|a| a != "app") {
        attach_parent_console();
    }
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
