//! Windows adapter over the Win32 clipboard API (windows-sys).
//! Formats are enumerated first; concealed markers are checked before any
//! text data is fetched. Not compiled on other platforms and untested on
//! real Windows so far (see the manual checklist in the PR/ticket).

use crate::conceal::windows_is_concealed;
use crate::retry::retry;
use crate::{ClipboardBackend, ClipboardContent, ClipboardError};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};

const CF_UNICODETEXT: u32 = 13;
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

fn read_text() -> Result<Option<String>, ClipboardError> {
    let open = Open::with_owner(null_mut())?;
    let formats = enumerate_formats(&open);
    let names: Vec<&str> = formats.iter().map(|(_, n)| n.as_str()).filter(|n| !n.is_empty()).collect();
    let concealed = windows_is_concealed(&names, |name| {
        let (id, _) = formats.iter().find(|(_, n)| n == name)?;
        format_bytes(&open, *id)
    });
    if concealed || !formats.iter().any(|(id, _)| *id == CF_UNICODETEXT) {
        return Ok(None);
    }
    let Some(bytes) = format_bytes(&open, CF_UNICODETEXT) else {
        return Ok(None);
    };
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Ok(Some(String::from_utf16_lossy(&units)))
}

/// SetClipboardData needs a real owner window after EmptyClipboard, so create
/// a throwaway message-only window for the duration of the write.
fn write_text(text: &str) -> Result<(), ClipboardError> {
    let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let owner = unsafe {
        CreateWindowExW(
            0, class.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), null_mut(), null(),
        )
    };
    if owner.is_null() {
        return Err(ClipboardError("could not create clipboard owner window".into()));
    }
    let result = write_text_owned(owner, text);
    unsafe { DestroyWindow(owner) };
    result
}

fn write_text_owned(owner: HWND, text: &str) -> Result<(), ClipboardError> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    let byte_len = units.len() * 2;

    let _open = Open::with_owner(owner)?;
    unsafe {
        if EmptyClipboard() == 0 {
            return Err(ClipboardError("EmptyClipboard failed".into()));
        }
        let h = GlobalAlloc(GMEM_MOVEABLE, byte_len);
        if h.is_null() {
            return Err(ClipboardError("GlobalAlloc failed".into()));
        }
        let p = GlobalLock(h) as *mut u16;
        if p.is_null() {
            GlobalFree(h);
            return Err(ClipboardError("GlobalLock failed".into()));
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
        GlobalUnlock(h);
        if SetClipboardData(CF_UNICODETEXT, h).is_null() {
            GlobalFree(h); // ownership was not transferred
            return Err(ClipboardError("SetClipboardData failed".into()));
        }
    }
    Ok(())
}

impl ClipboardBackend for WindowsClipboard {
    fn read(&self) -> Result<Option<ClipboardContent>, ClipboardError> {
        Ok(read_text()?.map(ClipboardContent::Text))
    }

    fn write(&mut self, content: ClipboardContent) -> Result<(), ClipboardError> {
        let ClipboardContent::Text(text) = content else {
            return Err(ClipboardError("only text is supported yet".into()));
        };
        write_text(&text)
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
                if let Ok(Some(text)) = read_text() {
                    if tx.send(ClipboardContent::Text(text)).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }
}
