//! macOS adapter over NSPasteboard. Concealed markers are checked on the
//! pasteboard type list before any string data is read.

use crate::conceal::macos_is_concealed;
use crate::image::{validate_png, ImageError};
use crate::{ClipboardBackend, ClipboardContent, ClipboardError};
use objc2::AnyThread;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard};
use objc2_foundation::{NSData, NSDictionary, NSString};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

const TEXT_TYPE: &str = "public.utf8-plain-text";
const PNG_TYPE: &str = "public.png";
const TIFF_TYPE: &str = "public.tiff";
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
pub struct MacClipboard;

impl MacClipboard {
    pub fn new() -> Self {
        Self
    }
}

fn tiff_to_png(tiff: &[u8]) -> Result<Vec<u8>, ImageError> {
    let fail = |m: &str| ImageError::Conversion(m.into());
    let data = NSData::with_bytes(tiff);
    let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)
        .ok_or_else(|| fail("unreadable TIFF"))?;
    let props = NSDictionary::new();
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }
        .ok_or_else(|| fail("PNG encode failed"))?;
    Ok(png.to_vec())
}

fn read_image(pb: &NSPasteboard) -> Result<Option<Vec<u8>>, ClipboardError> {
    if let Some(png) = pb.dataForType(&NSString::from_str(PNG_TYPE)) {
        return Ok(Some(png.to_vec()));
    }
    let Some(tiff) = pb.dataForType(&NSString::from_str(TIFF_TYPE)) else { return Ok(None) };
    tiff_to_png(&tiff.to_vec()).map(Some).map_err(|e| ClipboardError(e.to_string()))
}

/// Text or image on the pasteboard (text preferred), or `None` when empty,
/// unsupported or concealed. Concealed markers are checked before any data
/// is read.
fn read_content(pb: &NSPasteboard) -> Result<Option<ClipboardContent>, ClipboardError> {
    let types: Vec<String> = pb
        .types()
        .map(|arr| arr.iter().map(|t| t.to_string()).collect())
        .unwrap_or_default();
    if macos_is_concealed(&types) {
        return Ok(None);
    }
    if let Some(s) = pb.stringForType(&NSString::from_str(TEXT_TYPE)) {
        return Ok(Some(ClipboardContent::Text(s.to_string())));
    }
    Ok(read_image(pb)?.map(ClipboardContent::Image))
}

impl ClipboardBackend for MacClipboard {
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        let pb = NSPasteboard::generalPasteboard();
        read_content(&pb)
    }

    fn write(&mut self, content: ClipboardContent) -> Result<(), ClipboardError> {
        let pb = NSPasteboard::generalPasteboard();
        let text = match content {
            ClipboardContent::Text(t) => t,
            ClipboardContent::Image(png) => {
                // Validate first so a bad image never clears the clipboard.
                validate_png(&png).map_err(|e| ClipboardError(e.to_string()))?;
                pb.clearContents();
                return if pb.setData_forType(
                    Some(&NSData::with_bytes(&png)),
                    &NSString::from_str(PNG_TYPE),
                ) {
                    Ok(())
                } else {
                    Err(ClipboardError("NSPasteboard rejected the image".into()))
                };
            }
        };
        pb.clearContents();
        if pb.setString_forType(&NSString::from_str(&text), &NSString::from_str(TEXT_TYPE)) {
            Ok(())
        } else {
            Err(ClipboardError("NSPasteboard rejected the write".into()))
        }
    }

    fn watch(&mut self) -> Result<Receiver<Result<ClipboardContent, ClipboardError>>, ClipboardError> {
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
                match read_content(&pb) {
                    Ok(None) => {}
                    Ok(Some(content)) => if tx.send(Ok(content)).is_err() { return; },
                    Err(error) => if tx.send(Err(error)).is_err() { return; },
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
    fn image_roundtrips_as_public_png() {
        let png = crate::image::rgba_to_png(2, 1, &[255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        let mut clip = MacClipboard::new();
        clip.write(ClipboardContent::Image(png)).unwrap();
        let Some(ClipboardContent::Image(back)) = clip.read().unwrap() else {
            panic!("expected image");
        };
        let (_, _, rgba) = crate::image::png_to_rgba(&back).unwrap();
        assert_eq!(rgba.len(), 8);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    #[ignore]
    fn write_then_read_roundtrip_and_concealed_is_hidden() {
        let mut clip = MacClipboard::new();
        clip.write(ClipboardContent::Text("clipsync-test".into()))
            .unwrap();
        assert_eq!(
            clip.read().unwrap(),
            Some(ClipboardContent::Text("clipsync-test".into()))
        );

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
