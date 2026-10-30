//! OS hooks that tell the daemon the world changed under it: network address changes
//! (portable polling of local addresses) and sleep/wake (native per platform).

use crate::discovery::{announce_addrs, system_interfaces};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How often local addresses are sampled.
pub const NETWORK_POLL: Duration = Duration::from_secs(2);

/// The set of addresses we would announce, sorted so snapshots compare equal.
pub fn current_addrs() -> Vec<IpAddr> {
    let mut a = announce_addrs(&system_interfaces());
    a.sort();
    a
}

/// Calls `on_change` whenever `snapshot()` differs from the previous sample.
pub struct NetworkWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NetworkWatcher {
    pub fn start<S, F>(snapshot: S, interval: Duration, on_change: F) -> Self
    where
        S: Fn() -> Vec<IpAddr> + Send + 'static,
        F: Fn() + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || {
            let mut last = snapshot();
            while !flag.load(Ordering::SeqCst) {
                thread::sleep(interval);
                let now = snapshot();
                if now != last {
                    last = now;
                    on_change();
                }
            }
        });
        NetworkWatcher { stop, thread: Some(thread) }
    }

    /// Watch the real network interfaces.
    pub fn start_system<F: Fn() + Send + 'static>(on_change: F) -> Self {
        Self::start(current_addrs, NETWORK_POLL, on_change)
    }
}

impl Drop for NetworkWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Native sleep/wake notification.
#[cfg(target_os = "macos")]
pub mod wake {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObjectProtocol, ProtocolObject};
    use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification};
    use objc2_foundation::{NSNotification, NSRunLoop};
    use std::ptr::NonNull;

    /// Keeps the observer registered while alive.
    pub struct WakeWatcher {
        _observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    }

    /// Register for `NSWorkspaceDidWakeNotification`. Notifications are delivered
    /// while the main run loop runs; call `run_event_loop` from the main thread.
    pub fn watch(on_wake: impl Fn() + 'static) -> WakeWatcher {
        let block = RcBlock::new(move |_n: NonNull<NSNotification>| on_wake());
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidWakeNotification),
                None,
                None,
                &block,
            )
        };
        WakeWatcher { _observer: observer }
    }

    /// Run the current thread's run loop forever (use on the main thread).
    pub fn run_event_loop() {
        NSRunLoop::currentRunLoop().run();
    }
}

/// Native resume notification via `WM_POWERBROADCAST` on a hidden top-level window
/// (message-only windows do not receive broadcast messages).
#[cfg(windows)]
pub mod wake {
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
        TranslateMessage, MSG, WM_POWERBROADCAST, WNDCLASSW,
    };

    const PBT_APMRESUMESUSPEND: usize = 0x0007;
    const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

    static CALLBACK: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

    unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if msg == WM_POWERBROADCAST && (w == PBT_APMRESUMESUSPEND || w == PBT_APMRESUMEAUTOMATIC) {
            if let Some(cb) = CALLBACK.get() {
                cb();
            }
            return 1; // TRUE
        }
        DefWindowProcW(h, msg, w, l)
    }

    /// Spawn a thread owning a hidden window that calls `on_wake` on every resume.
    /// Only one watcher per process is supported.
    pub fn watch(on_wake: impl Fn() + Send + Sync + 'static) {
        if CALLBACK.set(Box::new(on_wake)).is_err() {
            return;
        }
        std::thread::spawn(|| unsafe {
            let class: Vec<u16> = "ClipsyncPower\0".encode_utf16().collect();
            let hinst = GetModuleHandleW(null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: null_mut(),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                hinst,
                null(),
            );
            if hwnd.is_null() {
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    #[test]
    fn fires_once_per_address_set_change_and_not_while_stable() {
        let addrs: Arc<Mutex<Vec<IpAddr>>> =
            Arc::new(Mutex::new(vec!["192.168.1.5".parse().unwrap()]));
        let fired = Arc::new(AtomicUsize::new(0));
        let (a, f) = (addrs.clone(), fired.clone());
        let _w = NetworkWatcher::start(
            move || a.lock().unwrap().clone(),
            Duration::from_millis(20),
            move || {
                f.fetch_add(1, Ordering::SeqCst);
            },
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(fired.load(Ordering::SeqCst), 0);
        *addrs.lock().unwrap() = vec!["10.0.0.7".parse().unwrap()];
        thread::sleep(Duration::from_millis(100));
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }
}
