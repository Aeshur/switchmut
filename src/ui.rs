pub mod settings;
mod theme;
pub mod tray;

use std::ptr::{null, null_mut};
use windows_sys::Win32::{
    Foundation::HWND, System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*,
};

/// LoadIcon returns a shared resource; the process owns it until shutdown.
pub(crate) fn app_icon() -> HICON {
    unsafe {
        // Win32 encodes resource ID 1 as a pointer-sized integer, not an address to read.
        let icon = LoadIconW(GetModuleHandleW(null()), std::ptr::without_provenance(1));
        if icon.is_null() {
            // Test harnesses do not link the application's icon resource.
            LoadIconW(null_mut(), IDI_APPLICATION)
        } else {
            icon
        }
    }
}

/// Load an owned icon at the exact size used by the compact title bar.
pub(crate) fn app_icon_at(size: i32) -> Option<HICON> {
    unsafe {
        let icon = LoadImageW(
            GetModuleHandleW(null()),
            std::ptr::without_provenance(1),
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        ) as HICON;
        (!icon.is_null()).then_some(icon)
    }
}

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
// HWND is an opaque, OS-validated handle; Rust never dereferences it.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn message(hwnd: HWND, text: &str) {
    unsafe {
        MessageBoxW(
            hwnd,
            wide(text).as_ptr(),
            wide("Switchmut").as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}
