//! Maps sync events to user-visible tray notices. Pure, so it is unit-testable.

use clipsync_clipboard::image::ImageError;
use clipsync_core::sync::SyncEvent;
use std::collections::VecDeque;

/// Notices kept in the tray menu.
const KEEP: usize = 3;

/// Human-readable notice for a sync event.
pub fn notice_message(ev: &SyncEvent) -> String {
    match ev {
        SyncEvent::TextTooLarge { bytes } => {
            format!("Text not synced: {} is over the 1 MiB limit", size(*bytes))
        }
        SyncEvent::ImageDropped(ImageError::TooLarge { bytes }) => {
            format!("Image not synced: {} is over the 10 MiB limit", size(*bytes))
        }
        SyncEvent::ImageDropped(ImageError::TooManyPixels { width, height }) => {
            format!("Image not synced: {width}x{height} is over the 64 megapixel limit")
        }
        SyncEvent::ImageDropped(ImageError::NotPng) => {
            "Image not synced: not a valid PNG".to_string()
        }
        SyncEvent::ImageDropped(ImageError::Conversion(why)) => {
            format!("Image not synced: conversion failed ({why})")
        }
    }
}

fn size(bytes: usize) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// The most recent notices, newest last.
#[derive(Default)]
pub struct Notices(VecDeque<String>);

impl Notices {
    pub fn push_event(&mut self, ev: &SyncEvent) {
        if self.0.len() == KEEP {
            self.0.pop_front();
        }
        self.0.push_back(notice_message(ev));
    }

    pub fn messages(&self) -> Vec<String> {
        self.0.iter().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_too_large_message() {
        let m = notice_message(&SyncEvent::TextTooLarge { bytes: 2 * 1024 * 1024 });
        assert_eq!(m, "Text not synced: 2.0 MiB is over the 1 MiB limit");
    }

    #[test]
    fn image_messages() {
        let m = notice_message(&SyncEvent::ImageDropped(ImageError::TooLarge { bytes: 11 * 1024 * 1024 }));
        assert_eq!(m, "Image not synced: 11.0 MiB is over the 10 MiB limit");
        let m = notice_message(&SyncEvent::ImageDropped(ImageError::TooManyPixels { width: 9000, height: 9000 }));
        assert_eq!(m, "Image not synced: 9000x9000 is over the 64 megapixel limit");
        let m = notice_message(&SyncEvent::ImageDropped(ImageError::Conversion("bad dib".into())));
        assert_eq!(m, "Image not synced: conversion failed (bad dib)");
        let m = notice_message(&SyncEvent::ImageDropped(ImageError::NotPng));
        assert_eq!(m, "Image not synced: not a valid PNG");
    }

    #[test]
    fn keeps_only_latest_three() {
        let mut n = Notices::default();
        for bytes in 1..=4usize {
            n.push_event(&SyncEvent::TextTooLarge { bytes: bytes * 1024 * 1024 });
        }
        let m = n.messages();
        assert_eq!(m.len(), 3);
        assert!(m[0].contains("2.0 MiB") && m[2].contains("4.0 MiB"));
        n.clear();
        assert!(n.messages().is_empty());
    }
}
