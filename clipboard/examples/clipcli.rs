//! Manual clipboard check.
//!   cargo run -p clipsync-clipboard --example clipcli -- watch
//!   cargo run -p clipsync-clipboard --example clipcli -- read
//!   cargo run -p clipsync-clipboard --example clipcli -- write "some text"
//! Prints clipboard text to stdout; for manual testing only.

#[cfg(any(target_os = "macos", windows))]
fn main() {
    use clipsync_clipboard::{ClipboardBackend, ClipboardContent, NativeClipboard};
    let mut clip = NativeClipboard::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("read") => println!("{:?}", clip.read().unwrap()),
        Some("write") => clip
            .write(ClipboardContent::Text(
                args.get(1).cloned().unwrap_or_default(),
            ))
            .unwrap(),
        Some("watch") => {
            let rx = clip.watch().unwrap();
            eprintln!("watching; copy something (Ctrl-C to stop)");
            for c in rx {
                println!("{c:?}");
            }
        }
        _ => eprintln!("usage: clipcli read | write <text> | watch"),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("no native clipboard backend for this OS");
}
