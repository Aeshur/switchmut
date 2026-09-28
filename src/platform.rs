//! Windows window activation helpers.

use crate::clients::{ClientKey, is_live};
use std::thread;
use std::time::{Duration, Instant};
use std::{
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread::JoinHandle,
};

const FOREGROUND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const FOREGROUND_POLL_LIMIT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivationResult {
    Activated,
    Stale,
    ForegroundDenied,
    TimedOut,
}

/// Runs the potentially slow foreground acknowledgement away from the owner
/// message loop. The bounded request channel makes repeated commands explicit:
/// one request may be in flight and one queued request is retained at most.
pub struct ActivationWorker {
    sender: Option<SyncSender<ClientKey>>,
    results: Receiver<(ClientKey, ActivationResult)>,
    worker: Option<JoinHandle<()>>,
}

impl ActivationWorker {
    pub fn start() -> Result<Self, String> {
        let (sender, requests) = mpsc::sync_channel(1);
        let (results, result_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("switchmut-activation".to_owned())
            .spawn(move || {
                while let Ok(key) = requests.recv() {
                    let result = activate(key);
                    if results.send((key, result)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("could not start the activation worker: {error}"))?;
        Ok(Self {
            sender: Some(sender),
            results: result_receiver,
            worker: Some(worker),
        })
    }

    pub fn request(&self, key: ClientKey) -> bool {
        let Some(sender) = &self.sender else {
            return false;
        };
        match sender.try_send(key) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn try_result(&self) -> Option<(ClientKey, ActivationResult)> {
        self.results.try_recv().ok()
    }
}

impl Drop for ActivationWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Restore and activate a client, then verify that Windows actually made it
/// foreground. Every input queue attached for the activation attempt is
/// detached before foreground polling begins.
pub fn activate(key: ClientKey) -> ActivationResult {
    #[cfg(windows)]
    {
        activate_windows(key)
    }
    #[cfg(not(windows))]
    {
        let _ = key;
        ActivationResult::Stale
    }
}

/// Encode a nul-terminated UTF-16 string for Win32 APIs.
pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn activate_windows(key: ClientKey) -> ActivationResult {
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetActiveWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE,
        SW_SHOW, SetForegroundWindow, ShowWindow, ShowWindowAsync,
    };

    if !is_live(key) {
        return ActivationResult::Stale;
    }

    let hwnd = key.hwnd as windows_sys::Win32::Foundation::HWND;
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_thread = if !foreground.is_null() {
        unsafe { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) }
    } else {
        0
    };

    let current_thread = unsafe { GetCurrentThreadId() };
    let mut target_pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut target_pid) };
    let same_process = target_pid == unsafe { GetCurrentProcessId() };
    let foreground_call_ok;
    let start = Instant::now();

    {
        let _attachment = InputAttachment::attach(current_thread, foreground_thread);
        if !is_live(key) {
            return ActivationResult::Stale;
        }
        unsafe {
            BringWindowToTop(hwnd);
            if IsIconic(hwnd) != 0 {
                if same_process {
                    ShowWindow(hwnd, SW_RESTORE);
                } else {
                    ShowWindowAsync(hwnd, SW_RESTORE);
                }
            } else {
                if same_process {
                    ShowWindow(hwnd, SW_SHOW);
                } else {
                    ShowWindowAsync(hwnd, SW_SHOW);
                }
            }
            foreground_call_ok = SetForegroundWindow(hwnd) != 0;
            SetActiveWindow(hwnd);
        }
    }

    loop {
        if !is_live(key) {
            return ActivationResult::Stale;
        }
        if unsafe { GetForegroundWindow() } == hwnd && unsafe { IsIconic(hwnd) } == 0 {
            return if is_live(key) {
                ActivationResult::Activated
            } else {
                ActivationResult::Stale
            };
        }
        if start.elapsed() >= FOREGROUND_POLL_LIMIT {
            return if foreground_call_ok {
                ActivationResult::TimedOut
            } else {
                ActivationResult::ForegroundDenied
            };
        }
        thread::sleep(FOREGROUND_POLL_INTERVAL);
    }
}

#[cfg(windows)]
struct InputAttachment {
    attached: Option<(u32, u32)>,
}

#[cfg(windows)]
impl InputAttachment {
    fn attach(current_thread: u32, other_thread: u32) -> Self {
        use windows_sys::Win32::System::Threading::AttachThreadInput;

        let attached = (other_thread != 0
            && other_thread != current_thread
            && unsafe { AttachThreadInput(current_thread, other_thread, 1) } != 0)
            .then_some((current_thread, other_thread));
        Self { attached }
    }
}

#[cfg(windows)]
impl Drop for InputAttachment {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Threading::AttachThreadInput;
        if let Some((first, second)) = self.attached {
            unsafe { AttachThreadInput(first, second, 0) };
        }
    }
}

/// Owns a compatible bitmap/DC for one paint, borrowing only the destination DC.
/// The selected bitmap is restored before either GDI object is released.
pub(crate) struct PaintBuffer {
    target: windows_sys::Win32::Graphics::Gdi::HDC,
    memory: windows_sys::Win32::Graphics::Gdi::HDC,
    bitmap: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    previous: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
    width: i32,
    height: i32,
}
impl PaintBuffer {
    /// Safety: target must be a valid DC until this buffer is dropped.
    pub(crate) unsafe fn new(
        target: windows_sys::Win32::Graphics::Gdi::HDC,
        width: i32,
        height: i32,
    ) -> Option<Self> {
        use windows_sys::Win32::Graphics::Gdi::*;
        if target.is_null() || width <= 0 || height <= 0 {
            return None;
        }
        unsafe {
            let memory = CreateCompatibleDC(target);
            if memory.is_null() {
                return None;
            }
            let bitmap = CreateCompatibleBitmap(target, width, height);
            if bitmap.is_null() {
                DeleteDC(memory);
                return None;
            }
            let previous = SelectObject(memory, bitmap);
            if previous.is_null() || previous as isize == -1 {
                DeleteObject(bitmap);
                DeleteDC(memory);
                return None;
            }
            Some(Self {
                target,
                memory,
                bitmap,
                previous,
                width,
                height,
            })
        }
    }
    pub(crate) fn dc(&self) -> windows_sys::Win32::Graphics::Gdi::HDC {
        self.memory
    }
    pub(crate) fn present(&self) -> bool {
        unsafe {
            windows_sys::Win32::Graphics::Gdi::BitBlt(
                self.target,
                0,
                0,
                self.width,
                self.height,
                self.memory,
                0,
                0,
                windows_sys::Win32::Graphics::Gdi::SRCCOPY,
            ) != 0
        }
    }
}
impl Drop for PaintBuffer {
    fn drop(&mut self) {
        use windows_sys::Win32::Graphics::Gdi::*;
        unsafe {
            SelectObject(self.memory, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.memory);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_encoding_includes_one_terminator() {
        assert_eq!(wide("A").as_slice(), &[b'A' as u16, 0]);
        assert_eq!(wide("").as_slice(), &[0]);
    }
}
