use crate::{ClipboardBackend, ClipboardContent, ClipboardError};
use std::sync::mpsc::{channel, Receiver, Sender};

/// In-memory clipboard for headless tests.
#[derive(Default)]
pub struct MockClipboard {
    current: Option<ClipboardContent>,
    watchers: Vec<Sender<Result<ClipboardContent, ClipboardError>>>,
}

impl MockClipboard {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ClipboardBackend for MockClipboard {
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        Ok(self.current.clone())
    }

    fn write(&mut self, content: ClipboardContent) -> Result<(), ClipboardError> {
        self.current = Some(content.clone());
        self.watchers.retain(|w| w.send(Ok(content.clone())).is_ok());
        Ok(())
    }

    fn watch(&mut self) -> Result<Receiver<Result<ClipboardContent, ClipboardError>>, ClipboardError> {
        let (tx, rx) = channel();
        self.watchers.push(tx);
        Ok(rx)
    }
}
