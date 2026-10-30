// Stub for the Tauri shell; the real UI arrives in a later ticket.
// For now: a CLI for SAS pairing and device management.
use clipsync_core::keystore::KeychainStore;
use clipsync_core::pairing::{Incoming, Node, PairingAttempt};
use clipsync_core::transport::Listener;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

const DEFAULT_PORT: &str = "43872";

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
        ["devices"] => {
            for key in node.paired_devices()? {
                println!("{}", hex::encode(key));
            }
        }
        ["remove", key] => {
            node.remove_device(&hex::decode(key)?)?;
            println!("Removed.");
        }
        _ => eprintln!("usage: clipsync pair-listen [addr] | pair-connect <addr> | devices | remove <hex-key>"),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
