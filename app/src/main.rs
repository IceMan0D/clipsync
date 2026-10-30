// Entry point: tray app by default, plus a CLI for SAS pairing and device management.
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

const DEFAULT_PORT: &str = "43872";

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
                .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));
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
            let _daemon = Daemon::start(
                DaemonConfig {
                    node,
                    listener,
                    resolver: Arc::new(load_book(Some(discovery.clone()))),
                    auto_dial: true,
                },
                NativeClipboard::new,
            );
            println!("Syncing with {paired} paired device(s). Ctrl-C to quit.");
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

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
