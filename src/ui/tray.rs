use super::wide;
use crate::actions::Action;
use std::{mem::size_of, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::*,
    UI::{Shell::*, WindowsAndMessaging::*},
};

pub const TRAY_MESSAGE: u32 = WM_APP + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayCallback {
    OpenSettings,
    OpenContextMenu,
}

/// Decode the legacy Shell notification contract. We do not call
/// NIM_SETVERSION, so Explorer places the mouse message directly in LPARAM.
pub fn decode_callback(lparam: LPARAM) -> Option<TrayCallback> {
    match lparam as u32 {
        WM_LBUTTONDBLCLK => Some(TrayCallback::OpenSettings),
        WM_RBUTTONUP | WM_CONTEXTMENU => Some(TrayCallback::OpenContextMenu),
        _ => None,
    }
}

pub enum TrayCommand {
    Settings,
    Refresh,
    Exit,
    Action(Action),
}
pub struct Tray {
    owner: HWND,
    data: NOTIFYICONDATAW,
}
impl Tray {
    pub fn new(owner: HWND) -> Result<Self, String> {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = owner;
        data.uID = 1;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = TRAY_MESSAGE;
        data.hIcon = super::app_icon();
        let tip = wide("Switchmut");
        data.szTip[..tip.len()].copy_from_slice(&tip);
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            return Err("Could not create the notification icon".into());
        }
        Ok(Self { owner, data })
    }
    pub fn restore(&self) -> Result<(), u32> {
        if unsafe { Shell_NotifyIconW(NIM_ADD, &self.data) } == 0 {
            Err(unsafe { GetLastError() })
        } else {
            Ok(())
        }
    }
    pub fn popup(&self) -> Option<TrayCommand> {
        unsafe {
            let root = CreatePopupMenu();
            if root.is_null() {
                return None;
            }
            append(root, 1, "&Settings...");
            append(root, 2, "&Refresh clients");
            separator(root);
            for (i, action) in [Action::Next, Action::Previous].into_iter().enumerate() {
                append(root, 100 + i, action.label());
            }
            separator(root);
            append(root, 5, "E&xit");
            let mut point = POINT::default();
            GetCursorPos(&mut point);
            SetForegroundWindow(self.owner);
            let chosen = TrackPopupMenu(
                root,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.owner,
                null_mut(),
            ) as usize;
            PostMessageW(self.owner, WM_NULL, 0, 0);
            DestroyMenu(root); // Also owns and destroys its submenus.
            match chosen {
                1 => Some(TrayCommand::Settings),
                2 => Some(TrayCommand::Refresh),
                5 => Some(TrayCommand::Exit),
                100..=101 => Some(TrayCommand::Action(
                    [Action::Next, Action::Previous][chosen - 100],
                )),
                _ => None,
            }
        }
    }
}
unsafe fn append(menu: HMENU, id: usize, label: &str) {
    unsafe {
        AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
    }
}
unsafe fn separator(menu: HMENU) {
    unsafe {
        AppendMenuW(menu, MF_SEPARATOR, 0, null_mut());
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &self.data);
        }
    }
}
