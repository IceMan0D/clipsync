//! Clipboard abstraction: the `ClipboardBackend` trait and an in-memory mock.

use std::fmt;
use std::sync::mpsc::Receiver;

pub mod conceal;
pub mod image;
mod mock;
mod retry;
pub use mock::MockClipboard;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacClipboard;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::WindowsClipboard;

/// The native clipboard backend for the current OS.
#[cfg(target_os = "macos")]
pub type NativeClipboard = MacClipboard;
#[cfg(windows)]
pub type NativeClipboard = WindowsClipboard;

/// Content carried on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    /// PNG-encoded image bytes.
    Image(Vec<u8>),
}

#[derive(Debug)]
pub struct ClipboardError(pub String);

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "clipboard error: {}", self.0)
    }
}

impl std::error::Error for ClipboardError {}

/// Per-OS clipboard adapter.
pub trait ClipboardBackend {
    /// Current clipboard content, if any.
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError>;
    /// Replace the clipboard content.
    fn write(&mut self, content: ClipboardContent) -> Result<(), ClipboardError>;
    /// Subscribe to clipboard changes.
    fn watch(&mut self) -> Result<Receiver<ClipboardContent>, ClipboardError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_content_can_be_read_back() {
        let mut clip = MockClipboard::new();
        clip.write(ClipboardContent::Text("hello".into())).unwrap();
        assert_eq!(
            clip.read().unwrap(),
            Some(ClipboardContent::Text("hello".into()))
        );
    }

    #[test]
    fn empty_clipboard_reads_none() {
        assert_eq!(MockClipboard::new().read().unwrap(), None);
    }

    #[test]
    fn watchers_see_subsequent_writes() {
        let mut clip = MockClipboard::new();
        let rx = clip.watch().unwrap();
        clip.write(ClipboardContent::Text("a".into())).unwrap();
        assert_eq!(rx.try_recv().unwrap(), ClipboardContent::Text("a".into()));
    }

    #[test]
    fn content_moves_between_two_mocks() {
        let mut a = MockClipboard::new();
        let mut b = MockClipboard::new();
        a.write(ClipboardContent::Image(vec![1, 2, 3])).unwrap();
        b.write(a.read().unwrap().unwrap()).unwrap();
        assert_eq!(
            b.read().unwrap(),
            Some(ClipboardContent::Image(vec![1, 2, 3]))
        );
    }
}
