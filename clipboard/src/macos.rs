//! macOS adapter over NSPasteboard. Concealed markers are checked on the
//! pasteboard type list before any string data is read.

use crate::conceal::macos_is_concealed;
use crate::{ClipboardBackend, ClipboardContent, ClipboardError};
use objc2_app_kit::NSPasteboard;
use objc2_foundation::NSString;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

const TEXT_TYPE: &str = "public.utf8-plain-text";
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
pub struct MacClipboard;

impl MacClipboard {
    pub fn new() -> Self {
        Self
    }
}

/// Text on the pasteboard, or `None` when empty, non-text or concealed.
fn read_text(pb: &NSPasteboard) -> Option<String> {
    let types: Vec<String> = pb
        .types()
        .map(|arr| arr.iter().map(|t| t.to_string()).collect())
        .unwrap_or_default();
    if macos_is_concealed(&types) {
        return None;
    }
    pb.stringForType(&NSString::from_str(TEXT_TYPE))
        .map(|s| s.to_string())
}

impl ClipboardBackend for MacClipboard {
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        let pb = NSPasteboard::generalPasteboard();
        Ok(read_text(&pb).map(ClipboardContent::Text))
    }

    fn write(&mut self, content: ClipboardContent) -> Result<(), ClipboardError> {
        let ClipboardContent::Text(text) = content else {
            return Err(ClipboardError("only text is supported yet".into()));
        };
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        if pb.setString_forType(&NSString::from_str(&text), &NSString::from_str(TEXT_TYPE)) {
            Ok(())
        } else {
            Err(ClipboardError("NSPasteboard rejected the write".into()))
        }
    }

    fn watch(&mut self) -> Result<Receiver<ClipboardContent>, ClipboardError> {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let pb = NSPasteboard::generalPasteboard();
            let mut last = pb.changeCount();
            loop {
                std::thread::sleep(POLL_INTERVAL);
                let now = pb.changeCount();
                if now == last {
                    continue;
                }
                last = now;
                if let Some(text) = read_text(&pb) {
                    if tx.send(ClipboardContent::Text(text)).is_err() {
                        return; // receiver dropped
                    }
                }
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSArray;

    /// Touches the real system pasteboard, so it is opt-in:
    /// `cargo test -p clipsync-clipboard -- --ignored`
    #[test]
    #[ignore]
    fn write_then_read_roundtrip_and_concealed_is_hidden() {
        let mut clip = MacClipboard::new();
        clip.write(ClipboardContent::Text("clipsync-test".into())).unwrap();
        assert_eq!(clip.read().unwrap(), Some(ClipboardContent::Text("clipsync-test".into())));

        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let marker = NSString::from_str(crate::conceal::MACOS_MARKERS[0]);
        let text = NSString::from_str(TEXT_TYPE);
        let types = NSArray::from_retained_slice(&[text.clone(), marker.clone()]);
        let _ = unsafe { pb.declareTypes_owner(&types, None) };
        pb.setString_forType(&NSString::from_str("secret"), &text);
        pb.setString_forType(&NSString::from_str(""), &marker);
        assert_eq!(clip.read().unwrap(), None);
    }
}
