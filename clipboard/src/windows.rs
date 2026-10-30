//! Windows adapter over the Win32 clipboard API (windows-sys).
//! Formats are enumerated first; concealed markers are checked before any
//! text data is fetched. Not compiled on other platforms and untested on
//! real Windows so far (see the manual checklist in the PR/ticket).

use crate::conceal::windows_is_concealed;
use crate::image::{dib_to_png, png_to_dib, png_to_dibv5, validate_png};
use crate::retry::retry;
use crate::{ClipboardBackend, ClipboardContent, ClipboardError};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// About 5 attempts over 250 ms.
const OPEN_ATTEMPTS: u32 = 5;
const OPEN_DELAY: Duration = Duration::from_millis(60);

#[derive(Default)]
pub struct WindowsClipboard;

impl WindowsClipboard {
    pub fn new() -> Self {
        Self
    }
}

/// RAII guard: the clipboard is open while this lives.
struct Open;

impl Open {
    fn with_owner(owner: HWND) -> Result<Open, ClipboardError> {
        retry(OPEN_ATTEMPTS, OPEN_DELAY, || {
            if unsafe { OpenClipboard(owner) } == 0 {
                Err(ClipboardError("OpenClipboard failed (busy)".into()))
            } else {
                Ok(Open)
            }
        })
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

/// Requires the clipboard to be open. Returns (id, registered name) pairs;
/// standard formats (below 0xC000) have an empty name.
fn enumerate_formats(_open: &Open) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut fmt = 0u32;
    loop {
        fmt = unsafe { EnumClipboardFormats(fmt) };
        if fmt == 0 {
            break;
        }
        let mut name = String::new();
        if fmt >= 0xC000 {
            let mut buf = [0u16; 256];
            let n = unsafe { GetClipboardFormatNameW(fmt, buf.as_mut_ptr(), buf.len() as i32) };
            if n > 0 {
                name = String::from_utf16_lossy(&buf[..n as usize]);
            }
        }
        out.push((fmt, name));
    }
    out
}

/// Raw bytes of a clipboard format. Requires the clipboard to be open.
fn format_bytes(_open: &Open, fmt: u32) -> Option<Vec<u8>> {
    unsafe {
        let h = GetClipboardData(fmt);
        if h.is_null() {
            return None;
        }
        let size = GlobalSize(h);
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(p, size).to_vec();
        GlobalUnlock(h);
        Some(bytes)
    }
}

/// Text or image on the clipboard (text preferred), or `None` when empty,
/// unsupported, concealed or unconvertible. Concealed markers are checked
/// before any data is fetched. Images: `PNG`, then `CF_DIBV5`, then `CF_DIB`.
fn read_content() -> Result<Option<ClipboardContent>, ClipboardError> {
    let open = Open::with_owner(null_mut())?;
    let formats = enumerate_formats(&open);
    let names: Vec<&str> = formats
        .iter()
        .map(|(_, n)| n.as_str())
        .filter(|n| !n.is_empty())
        .collect();
    let concealed = windows_is_concealed(&names, |name| {
        let (id, _) = formats.iter().find(|(_, n)| n == name)?;
        format_bytes(&open, *id)
    });
    if concealed {
        return Ok(None);
    }
    let has = |id: u32| formats.iter().any(|(f, _)| *f == id);
    if has(CF_UNICODETEXT) {
        let Some(bytes) = format_bytes(&open, CF_UNICODETEXT) else {
            return Ok(None);
        };
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        return Ok(Some(ClipboardContent::Text(String::from_utf16_lossy(
            &units,
        ))));
    }
    if let Some((id, _)) = formats.iter().find(|(_, n)| n == "PNG") {
        if let Some(png) = format_bytes(&open, *id) {
            return Ok(Some(ClipboardContent::Image(png)));
        }
    }
    for dib_fmt in [CF_DIBV5, CF_DIB] {
        if has(dib_fmt) {
            // A DIB that cannot be converted leaves the clipboard unsynced.
            return Ok(format_bytes(&open, dib_fmt)
                .and_then(|d| dib_to_png(&d).ok())
                .map(ClipboardContent::Image));
        }
    }
    Ok(None)
}

/// One clipboard format to publish: id plus payload bytes.
struct Format {
    id: u32,
    bytes: Vec<u8>,
}

fn utf16_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn register_format(name: &str) -> u32 {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
}

/// All formats are converted before the clipboard is touched, so a failed
/// conversion leaves the existing content intact.
fn formats_for(content: &ClipboardContent) -> Result<Vec<Format>, ClipboardError> {
    match content {
        ClipboardContent::Text(t) => Ok(vec![Format {
            id: CF_UNICODETEXT,
            bytes: utf16_bytes(t),
        }]),
        ClipboardContent::Image(png) => {
            validate_png(png).map_err(|e| ClipboardError(e.to_string()))?;
            let err = |e: crate::image::ImageError| ClipboardError(e.to_string());
            Ok(vec![
                Format {
                    id: CF_DIBV5,
                    bytes: png_to_dibv5(png).map_err(err)?,
                },
                Format {
                    id: CF_DIB,
                    bytes: png_to_dib(png).map_err(err)?,
                },
                Format {
                    id: register_format("PNG"),
                    bytes: png.clone(),
                },
            ])
        }
    }
}

/// SetClipboardData needs a real owner window after EmptyClipboard, so create
/// a throwaway message-only window for the duration of the write.
fn write_formats(formats: &[Format]) -> Result<(), ClipboardError> {
    let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let owner = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            null_mut(),
            null(),
        )
    };
    if owner.is_null() {
        return Err(ClipboardError(
            "could not create clipboard owner window".into(),
        ));
    }
    let result = write_formats_owned(owner, formats);
    unsafe { DestroyWindow(owner) };
    result
}

fn write_formats_owned(owner: HWND, formats: &[Format]) -> Result<(), ClipboardError> {
    let _open = Open::with_owner(owner)?;
    unsafe {
        if EmptyClipboard() == 0 {
            return Err(ClipboardError("EmptyClipboard failed".into()));
        }
        for f in formats {
            let h = GlobalAlloc(GMEM_MOVEABLE, f.bytes.len());
            if h.is_null() {
                return Err(ClipboardError("GlobalAlloc failed".into()));
            }
            let p = GlobalLock(h) as *mut u8;
            if p.is_null() {
                GlobalFree(h);
                return Err(ClipboardError("GlobalLock failed".into()));
            }
            std::ptr::copy_nonoverlapping(f.bytes.as_ptr(), p, f.bytes.len());
            GlobalUnlock(h);
            if SetClipboardData(f.id, h).is_null() {
                GlobalFree(h); // ownership was not transferred
                return Err(ClipboardError("SetClipboardData failed".into()));
            }
        }
    }
    Ok(())
}

impl ClipboardBackend for WindowsClipboard {
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        read_content()
    }

    fn write(&mut self, content: ClipboardContent) -> Result<(), ClipboardError> {
        write_formats(&formats_for(&content)?)
    }

    fn watch(&mut self) -> Result<Receiver<ClipboardContent>, ClipboardError> {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut last = unsafe { GetClipboardSequenceNumber() };
            loop {
                std::thread::sleep(POLL_INTERVAL);
                let now = unsafe { GetClipboardSequenceNumber() };
                if now == last {
                    continue;
                }
                last = now;
                if let Ok(Some(content)) = read_content() {
                    if tx.send(content).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }
}
