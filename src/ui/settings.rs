use super::{theme, wide};
use crate::{
    actions::Action,
    clients::{Client, ClientKey},
    config::{Config, RefreshMode},
    input::Device,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, Sender},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::*,
            HiDpi::*,
            Input::KeyboardAndMouse::{GetFocus, GetKeyState, SetFocus, VK_SHIFT, VK_TAB},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

const DEFERRED_MINIMIZE: u32 = WM_APP + 0x51;
const SETTINGS_WINDOW_STYLE: u32 =
    WS_POPUP | WS_BORDER | WS_SYSMENU | WS_MINIMIZEBOX | WS_VSCROLL | WS_CLIPCHILDREN;
const SETTINGS_EXTENDED_STYLE: u32 = WS_EX_CONTROLPARENT;

fn adjusted_window_rect(client_width: i32, client_height: i32, dpi: u32) -> RECT {
    let dpi = dpi.max(96);
    let mut bounds = RECT {
        left: 0,
        top: 0,
        right: theme::px_ceil(client_width, dpi),
        bottom: theme::px_ceil(client_height, dpi),
    };
    unsafe {
        AdjustWindowRectExForDpi(
            &mut bounds,
            SETTINGS_WINDOW_STYLE,
            0,
            SETTINGS_EXTENDED_STYLE,
            dpi,
        );
    }
    bounds
}

fn clamp_window_to_work_area(hwnd: HWND, mut bounds: RECT) -> RECT {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..std::mem::zeroed()
        };
        if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
            let work = info.rcWork;
            let width = (bounds.right - bounds.left).min(work.right - work.left);
            let height = (bounds.bottom - bounds.top).min(work.bottom - work.top);
            bounds.right = bounds.left + width;
            bounds.bottom = bounds.top + height;
            bounds.left = bounds.left.clamp(work.left, work.right - width);
            bounds.top = bounds.top.clamp(work.top, work.bottom - height);
            bounds.right = bounds.left + width;
            bounds.bottom = bounds.top + height;
        }
    }
    bounds
}

#[derive(Debug)]
pub enum SettingsEvent {
    Save {
        request_id: u64,
        config: Box<Config>,
        clients: Vec<Client>,
    },
    ClientEdits(Vec<Client>),
    Cancel,
    Refresh,
    CaptureController(Option<String>),
    EndCapture,
}

struct Control {
    hwnd: HWND,
    tab: i32,
    rect: [i32; 4],
    stretch: bool,
}
struct State {
    hwnd: HWND,
    draft: Config,
    clients: Vec<Client>,
    current: Option<ClientKey>,
    devices: Vec<Device>,
    sender: Sender<SettingsEvent>,
    controls: HashMap<i32, Control>,
    font: HFONT,
    section_font: HFONT,
    title_font: HFONT,
    theme: theme::Theme,
    dpi: u32,
    tab: i32,
    capturing: Option<usize>,
    gamepad_status: String,
    scroll: i32,
    next_save_request: u64,
    pending_save: Option<u64>,
    show_save_feedback: bool,
}

/// State outlives DestroyWindow. Reentrant native notifications use try_borrow_mut
/// and defer to DefWindowProc while an outer update owns the state. Control
/// colors are independent of that borrow because native painting is synchronous.
pub struct Settings {
    hwnd: HWND,
    state: Box<RefCell<State>>,
    pub events: Receiver<SettingsEvent>,
}
impl Settings {
    pub fn new(
        config: Config,
        clients: Vec<Client>,
        current: Option<ClientKey>,
        devices: Vec<Device>,
    ) -> Result<Self, String> {
        unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: GetModuleHandleW(null()),
                hIcon: super::app_icon(),
                lpszClassName: w!("Switchmut.Settings"),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: null_mut(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&class);
            let (sender, events) = mpsc::channel();
            let mut state = Box::new(RefCell::new(State {
                hwnd: null_mut(),
                draft: config,
                clients,
                current,
                devices,
                sender,
                controls: HashMap::new(),
                font: null_mut(),
                section_font: null_mut(),
                title_font: null_mut(),
                theme: theme::Theme::new(),
                dpi: 96,
                tab: 0,
                capturing: None,
                gamepad_status: String::new(),
                scroll: 0,
                next_save_request: 0,
                pending_save: None,
                show_save_feedback: false,
            }));
            let hwnd = CreateWindowExW(
                SETTINGS_EXTENDED_STYLE,
                class.lpszClassName,
                w!("Switchmut"),
                SETTINGS_WINDOW_STYLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                100,
                100,
                null_mut(),
                null_mut(),
                class.hInstance,
                (&mut *state as *mut RefCell<State>).cast(),
            );
            if hwnd.is_null() {
                return Err(format!(
                    "Settings window creation failed: {}",
                    GetLastError()
                ));
            }
            {
                let mut data = state.borrow_mut();
                data.hwnd = hwnd;
                data.dpi = GetDpiForWindow(hwnd).max(96);
                let bounds =
                    adjusted_window_rect(theme::WINDOW_WIDTH, theme::WINDOW_HEIGHT, data.dpi);
                let mut window = RECT::default();
                GetWindowRect(hwnd, &mut window);
                let target = clamp_window_to_work_area(
                    hwnd,
                    RECT {
                        left: window.left,
                        top: window.top,
                        right: window.left + bounds.right - bounds.left,
                        bottom: window.top + bounds.bottom - bounds.top,
                    },
                );
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    target.left,
                    target.top,
                    target.right - target.left,
                    target.bottom - target.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                data.create_controls();
                data.refresh_all();
                data.layout();
            }
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
            Ok(Self {
                hwnd,
                state,
                events,
            })
        }
    }
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
    pub fn show(&self) {
        unsafe {
            ShowWindow(self.hwnd, SW_RESTORE);
            SetForegroundWindow(self.hwnd);
        }
    }
    pub fn capturing(&self) -> bool {
        self.state.borrow().capturing.is_some()
    }
    pub fn capture_button(&self, button: u8) -> bool {
        let mut data = self.state.borrow_mut();
        let Some(index) = data.capturing.take() else {
            return false;
        };
        let Some(action) = Action::ALL.get(index).copied() else {
            return false;
        };
        let conflict = data
            .draft
            .bindings
            .iter()
            .find(|(other, assigned)| *other != action && *assigned == Some(button))
            .map(|(other, _)| other);
        if let Some(other) = conflict {
            data.status(&format!("Button already used by {}.", other.label()));
        } else {
            data.draft.bindings.set(action, Some(button));
            data.refresh_bindings(index);
            match data.emit_save() {
                Ok(()) => data.status("Saving button assignment..."),
                Err(error) => data.notice(&error),
            }
        }
        true
    }
    pub fn cancel_capture(&self, _reason: &str) {
        let mut data = self.state.borrow_mut();
        data.capturing = None;
        data.clear_status();
    }
    pub fn update_clients(&self, clients: &[Client], current: Option<ClientKey>) {
        let mut data = self.state.borrow_mut();
        let selected = data.selected_client_key();
        data.clients = clients.to_vec();
        data.current = current;
        data.refresh_clients_with_key(selected);
    }
    pub fn update_devices(&self, devices: Vec<Device>) {
        let mut data = self.state.borrow_mut();
        data.devices = devices;
        data.refresh_devices();
    }
    pub fn status(&self, text: &str) {
        self.state.borrow_mut().notice(text);
    }
    pub fn save_succeeded(&self, request_id: u64) {
        let mut data = self.state.borrow_mut();
        if data.pending_save == Some(request_id) {
            data.pending_save = None;
            if data.show_save_feedback {
                data.status("Saved.");
            }
            data.show_save_feedback = false;
        }
    }
    pub fn save_failed(&self, request_id: u64, error: &str) {
        let mut data = self.state.borrow_mut();
        if data.pending_save == Some(request_id) {
            data.pending_save = None;
            data.show_save_feedback = false;
            data.notice(&format!(
                "Save failed: {error} Changes remain unapplied; retry."
            ));
        }
    }
    pub fn dialog_message(&self, message: &MSG) -> bool {
        unsafe {
            let belongs = message.hwnd == self.hwnd
                || (!message.hwnd.is_null() && IsChild(self.hwnd, message.hwnd) != 0);
            if belongs && message.message == WM_KEYDOWN && message.wParam == VK_TAB as usize {
                let forward = GetKeyState(VK_SHIFT as i32) >= 0;
                self.state.borrow_mut().next_dialog_control(forward);
                true
            } else {
                IsDialogMessageW(self.hwnd, message) != 0
            }
        }
    }
}
impl Drop for Settings {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd);
            let font = self.state.borrow().font;
            if !font.is_null() {
                DeleteObject(font);
            }
            let section_font = self.state.borrow().section_font;
            if !section_font.is_null() {
                DeleteObject(section_font);
            }
            let title_font = self.state.borrow().title_font;
            if !title_font.is_null() {
                DeleteObject(title_font);
            }
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lp as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        // SetWindowTextW, ShowWindow and MoveWindow can request these colors
        // while an outer callback owns State. The DC brush needs no state or
        // owned GDI handle and remains valid for the native control's paint.
        if matches!(
            message,
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN
        ) {
            return control_color(message, wp as HDC) as isize;
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if !ptr.is_null() {
            if message == WM_NCHITTEST {
                let hit = DefWindowProcW(hwnd, message, wp, lp);
                if hit != HTCLIENT as isize {
                    return hit;
                }
                let point = POINT {
                    x: (lp as u32 as i16) as i32,
                    y: ((lp as u32 >> 16) as i16) as i32,
                };
                for id in [5, 6] {
                    let button = GetDlgItem(hwnd, id);
                    let mut rect = RECT::default();
                    if !button.is_null()
                        && GetWindowRect(button, &mut rect) != 0
                        && point.x >= rect.left
                        && point.x < rect.right
                        && point.y >= rect.top
                        && point.y < rect.bottom
                    {
                        return HTCLIENT as isize;
                    }
                }
                let mut client_point = point;
                ScreenToClient(hwnd, &mut client_point);
                let scale = GetDpiForWindow(hwnd).max(96) as i32;
                if client_point.y < theme::TITLE_HEIGHT * scale / 96 {
                    return HTCAPTION as isize;
                }
                return hit;
            }
            if message == DEFERRED_MINIMIZE
                || (message == WM_SYSCOMMAND && wp as u32 & 0xfff0 == SC_MINIMIZE)
                || (message == WM_SIZE && wp as u32 == SIZE_MINIMIZED)
            {
                minimize_to_tray(hwnd, &*ptr);
                return 0;
            }
            if let Ok(mut state) = (*ptr).try_borrow_mut() {
                match message {
                    WM_CLOSE => {
                        state.cancel();
                        return 0;
                    }
                    WM_NCPAINT => {
                        state.paint_non_client(hwnd, wp);
                        return 0;
                    }
                    WM_NCACTIVATE => {
                        state.paint_non_client(hwnd, 1);
                        return 1;
                    }
                    WM_PAINT => {
                        state.paint(hwnd);
                        return 0;
                    }
                    WM_ERASEBKGND => return 1,
                    WM_DRAWITEM => {
                        if state.draw_item(lp as *const DRAWITEMSTRUCT) {
                            return 1;
                        }
                    }
                    WM_MEASUREITEM => {
                        if state.measure_item(lp as *mut MEASUREITEMSTRUCT) {
                            return 1;
                        }
                    }
                    WM_NEXTDLGCTL => {
                        if lp != 0 {
                            let target = wp as HWND;
                            if !target.is_null() && IsChild(hwnd, target) != 0 {
                                SetFocus(target);
                                return 0;
                            }
                        } else {
                            state.next_dialog_control(wp == 0);
                            return 0;
                        }
                    }
                    WM_COMMAND => {
                        state.command((wp & 0xffff) as i32, ((wp >> 16) & 0xffff) as u32);
                        return 0;
                    }
                    WM_SIZE if !state.hwnd.is_null() => {
                        state.layout();
                        return 0;
                    }
                    WM_VSCROLL => {
                        let mut info = SCROLLINFO {
                            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                            fMask: SIF_ALL,
                            ..std::mem::zeroed()
                        };
                        GetScrollInfo(hwnd, SB_VERT, &mut info);
                        state.scroll = match (wp & 0xffff) as i32 {
                            SB_LINEUP => state.scroll - 30,
                            SB_LINEDOWN => state.scroll + 30,
                            SB_PAGEUP => state.scroll - 200,
                            SB_PAGEDOWN => state.scroll + 200,
                            SB_THUMBTRACK | SB_THUMBPOSITION => info.nTrackPos,
                            _ => state.scroll,
                        }
                        .clamp(0, (info.nMax - info.nPage as i32 + 1).max(0));
                        state.layout();
                        return 0;
                    }
                    WM_MOUSEWHEEL => {
                        let delta = ((wp >> 16) as i16) as i32;
                        state.scroll =
                            (state.scroll - delta / 120 * 72).clamp(0, state.scroll_limit());
                        state.layout();
                        return 0;
                    }
                    WM_DPICHANGED => {
                        state.dpi = (wp & 0xffff) as u32;
                        state.font_update();
                        state.update_list_heights();
                        let rect = &*(lp as *const RECT);
                        let target = clamp_window_to_work_area(
                            hwnd,
                            RECT {
                                left: rect.left,
                                top: rect.top,
                                right: rect.right,
                                bottom: rect.bottom,
                            },
                        );
                        SetWindowPos(
                            hwnd,
                            null_mut(),
                            target.left,
                            target.top,
                            target.right - target.left,
                            target.bottom - target.top,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                        );
                        state.layout();
                        return 0;
                    }
                    WM_NCDESTROY => {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    }
                    _ => {}
                }
            }
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }
}

unsafe fn minimize_to_tray(hwnd: HWND, state: &RefCell<State>) {
    if let Ok(mut state) = state.try_borrow_mut() {
        if state.capturing.take().is_some() {
            let _ = state.sender.send(SettingsEvent::EndCapture);
        }
        state.clear_status();
        drop(state);
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
    } else {
        unsafe {
            PostMessageW(hwnd, DEFERRED_MINIMIZE, 0, 0);
        }
    }
}

impl State {
    fn cancel(&mut self) {
        if self.capturing.take().is_some() {
            let _ = self.sender.send(SettingsEvent::EndCapture);
        }
        self.clear_status();
        let _ = self.sender.send(SettingsEvent::Cancel);
    }

    fn tab_order(&self) -> Vec<i32> {
        match self.tab {
            0 => vec![100, 101, 111, 3, 114, 112, 113, 116],
            1 => vec![100, 101, 211, 213, 214, 215, 216],
            _ => vec![100, 101],
        }
    }

    fn next_dialog_control(&mut self, forward: bool) {
        let order = self.tab_order();
        let current = unsafe { GetFocus() };
        let current_id = self
            .controls
            .iter()
            .find_map(|(id, control)| (control.hwnd == current).then_some(*id));
        let index = current_id
            .and_then(|id| order.iter().position(|candidate| *candidate == id))
            .unwrap_or(if forward {
                order.len().saturating_sub(1)
            } else {
                0
            });
        let next = if forward {
            order[(index + 1) % order.len()]
        } else {
            order[(index + order.len() - 1) % order.len()]
        };
        self.scroll_to_control(next);
        unsafe {
            SetFocus(self.hwnd(next));
        }
    }

    fn scroll_to_control(&mut self, id: i32) {
        let Some(control) = self.controls.get(&id) else {
            return;
        };
        if control.tab != self.tab {
            return;
        }
        let visible_height = if control.rect[3] > 200 && [116, 211].contains(&id) {
            theme::FIELD_HEIGHT
        } else {
            control.rect[3]
        };
        let mut client = RECT::default();
        unsafe {
            GetClientRect(self.hwnd, &mut client);
        }
        let height = theme::logical(client.bottom, self.dpi);
        let viewport_bottom = height;
        let top = control.rect[1];
        let bottom = top + visible_height;
        if top - self.scroll < theme::CONTENT_TOP {
            self.scroll = top - theme::CONTENT_TOP;
        } else if bottom - self.scroll > viewport_bottom {
            self.scroll = bottom - viewport_bottom;
        }
        self.scroll = self.scroll.clamp(0, self.scroll_limit());
        self.layout();
    }

    // Keep the native control's identity, page, style and layout together.
    #[allow(clippy::too_many_arguments)]
    fn control(
        &mut self,
        id: i32,
        tab: i32,
        class: &str,
        title: &str,
        style: u32,
        rect: [i32; 4],
        stretch: bool,
    ) {
        unsafe {
            let extended_style = if class == "EDIT" || class == "LISTBOX" || class == "COMBOBOX" {
                WS_EX_CLIENTEDGE
            } else {
                0
            };
            let hwnd = CreateWindowExW(
                extended_style,
                wide(class).as_ptr(),
                wide(title).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                0,
                0,
                self.hwnd,
                id as usize as HMENU,
                GetModuleHandleW(null()),
                null(),
            );
            SendMessageW(hwnd, WM_SETFONT, self.font as usize, 1);
            self.controls.insert(
                id,
                Control {
                    hwnd,
                    tab,
                    rect,
                    stretch,
                },
            );
        }
    }
    fn label(&mut self, id: i32, tab: i32, text: &str, x: i32, y: i32, width: i32) {
        self.control(id, tab, "STATIC", text, 0, [x, y, width, 18], width >= 200);
    }
    fn button(&mut self, id: i32, tab: i32, text: &str, x: i32, y: i32, width: i32) {
        let height = if id == 5 || id == 6 {
            theme::TITLE_BUTTON_SIZE
        } else if (100..=101).contains(&id) {
            theme::TAB_BUTTON_HEIGHT
        } else {
            theme::BUTTON_HEIGHT
        };
        let tab_stop = if id == 5 || id == 6 { 0 } else { WS_TABSTOP };
        self.control(
            id,
            tab,
            "BUTTON",
            text,
            tab_stop | BS_OWNERDRAW as u32,
            [x, y, width, height],
            id == 113 || id == 114,
        );
    }
    fn combo(&mut self, id: i32, tab: i32, x: i32, y: i32, width: i32) {
        self.control(
            id,
            tab,
            "COMBOBOX",
            "",
            WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
            [x, y, width, 250],
            true,
        );
    }
    fn list(&mut self, id: i32, tab: i32, x: i32, y: i32, width: i32, height: i32) {
        self.control(
            id,
            tab,
            "LISTBOX",
            "",
            WS_TABSTOP
                | WS_VSCROLL
                | LBS_NOTIFY as u32
                | LBS_NOINTEGRALHEIGHT as u32
                | LBS_OWNERDRAWFIXED as u32
                | LBS_HASSTRINGS as u32,
            [x, y, width, height],
            true,
        );
        self.update_list_heights();
    }
    fn hwnd(&self, id: i32) -> HWND {
        self.controls.get(&id).map_or(null_mut(), |c| c.hwnd)
    }
    fn selection(&self, id: i32, combo: bool) -> Option<usize> {
        let n = unsafe {
            SendMessageW(
                self.hwnd(id),
                if combo { CB_GETCURSEL } else { LB_GETCURSEL },
                0,
                0,
            )
        };
        (n >= 0).then_some(n as usize)
    }
    fn select(&self, id: i32, index: usize, combo: bool) {
        unsafe {
            SendMessageW(
                self.hwnd(id),
                if combo { CB_SETCURSEL } else { LB_SETCURSEL },
                index,
                0,
            );
        }
    }
    fn add(&self, id: i32, text: &str, combo: bool) {
        unsafe {
            SendMessageW(
                self.hwnd(id),
                if combo { CB_ADDSTRING } else { LB_ADDSTRING },
                0,
                wide(text).as_ptr() as isize,
            );
        }
    }
    fn clear(&self, id: i32, combo: bool) {
        unsafe {
            SendMessageW(
                self.hwnd(id),
                if combo {
                    CB_RESETCONTENT
                } else {
                    LB_RESETCONTENT
                },
                0,
                0,
            );
        }
    }
    fn update_list_heights(&self) {
        unsafe {
            for id in [111, 213] {
                let hwnd = self.hwnd(id);
                if !hwnd.is_null() {
                    SendMessageW(hwnd, LB_SETITEMHEIGHT, 0, theme::px(22, self.dpi) as LPARAM);
                }
            }
        }
    }
    fn status(&mut self, text: &str) {
        self.gamepad_status.clear();
        if self.tab == 1 {
            self.gamepad_status.push_str(text);
        }
        self.update_status_control();
    }
    fn notice(&mut self, text: &str) {
        // Errors and migration/recovery notices must be visible even when
        // their source is General. General itself never owns a status line.
        self.tab = 1;
        self.scroll = 0;
        self.status(text);
    }
    fn clear_status(&mut self) {
        self.show_save_feedback = false;
        self.status("");
    }
    fn update_status_control(&self) {
        unsafe {
            let hwnd = self.hwnd(90);
            if !hwnd.is_null() {
                SetWindowTextW(hwnd, wide(&self.gamepad_status).as_ptr());
                // Layout owns page and viewport visibility, including after
                // a text update while the control is scrolled out of view.
                self.layout();
            }
        }
    }
    fn font_update(&mut self) {
        unsafe {
            let font = CreateFontW(
                -(12 * self.dpi as i32 / 96),
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                0,
                0,
                CLEARTYPE_QUALITY as u32,
                0,
                w!("Segoe UI"),
            );
            if !font.is_null() {
                for c in self.controls.values() {
                    SendMessageW(c.hwnd, WM_SETFONT, font as usize, 1);
                }
                if !self.font.is_null() {
                    DeleteObject(self.font);
                }
                self.font = font;
            }
            let section_font = CreateFontW(
                -(12 * self.dpi as i32 / 96),
                0,
                0,
                0,
                FW_BOLD as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                0,
                0,
                CLEARTYPE_QUALITY as u32,
                0,
                w!("Segoe UI"),
            );
            if !section_font.is_null() {
                if !self.section_font.is_null() {
                    DeleteObject(self.section_font);
                }
                self.section_font = section_font;
            }
            let title_font = CreateFontW(
                -(12 * self.dpi as i32 / 96),
                0,
                0,
                0,
                FW_BOLD as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                0,
                0,
                CLEARTYPE_QUALITY as u32,
                0,
                w!("Segoe UI"),
            );
            if !title_font.is_null() {
                if !self.title_font.is_null() {
                    DeleteObject(self.title_font);
                }
                self.title_font = title_font;
            }
        }
    }
    fn create_controls(&mut self) {
        self.font_update();
        self.button(5, -1, "Minimize", 0, 3, 24);
        self.button(6, -1, "Close", 0, 3, 24);
        self.button(100, -1, "&General", 13, 42, 132);
        self.button(101, -1, "Game&pad", 145, 42, 132);
        self.list(111, 0, 16, 108, 258, 88);
        self.button(3, 0, "&Refresh", 16, 205, 125);
        self.button(114, 0, "Allow / Deny", 149, 205, 125);
        self.button(112, 0, "Move &Up", 16, 239, 125);
        self.button(113, 0, "Move &Down", 149, 239, 125);
        self.label(115, 0, "Refresh mode", 16, 316, 258);
        self.combo(116, 0, 16, 340, 258);
        for t in ["Manual", "Periodic", "On switch"] {
            self.add(116, t, true);
        }

        self.label(210, 1, "Active controller", 16, 108, 258);
        self.combo(211, 1, 16, 132, 258);
        self.list(213, 1, 16, 204, 258, 90);
        self.button(214, 1, "&Assign", 16, 302, 81);
        self.button(215, 1, "&Unassign", 105, 302, 81);
        self.button(216, 1, "&Cancel", 194, 302, 80);
        self.label(90, 1, "", 16, 338, 258);
    }
    fn scroll_limit(&self) -> i32 {
        let content_end = match self.tab {
            0 => theme::CONTENT_BOTTOM,
            1 => theme::CONTENT_BOTTOM,
            _ => 0,
        };
        let mut client = RECT::default();
        unsafe {
            GetClientRect(self.hwnd, &mut client);
        }
        let height = theme::logical(client.bottom, self.dpi);
        (content_end - height).max(0)
    }
    fn layout(&self) {
        unsafe {
            let mut r = RECT::default();
            GetClientRect(self.hwnd, &mut r);
            let scale = self.dpi as i32;
            let initial_height = theme::logical(r.bottom, scale as u32);
            let initial_scroll = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: (self.scroll_content_height() - 1).max(0),
                nPage: (initial_height - theme::CONTENT_TOP).max(1) as u32,
                nPos: self.scroll,
                nTrackPos: 0,
            };
            SetScrollInfo(self.hwnd, SB_VERT, &initial_scroll, 1);
            GetClientRect(self.hwnd, &mut r);
            let logical_width = theme::logical(r.right, scale as u32);
            let width_delta = logical_width - theme::WINDOW_WIDTH;
            let height = theme::logical(r.bottom, scale as u32);
            let content_bottom = height;
            let scroll_limit = self.scroll_limit();
            let scroll = self.scroll.clamp(0, scroll_limit);
            for (&id, c) in &self.controls {
                let [mut x, mut y, mut width, h] = c.rect;
                if c.stretch {
                    width = (width + width_delta).max(1);
                }
                if (100..=101).contains(&id) {
                    let inset =
                        theme::CONTENT_MARGIN + theme::TABBAR_BORDER + theme::TABBAR_PADDING;
                    let slot = (logical_width - 2 * inset).max(2) / 2;
                    x = inset + (id - 100) * slot;
                    y = theme::TABBAR_TOP + theme::TABBAR_BORDER + theme::TABBAR_PADDING;
                    width = slot;
                } else if id == 5 {
                    x = logical_width - 57;
                    y = 3;
                    width = theme::TITLE_BUTTON_SIZE;
                } else if id == 6 {
                    x = logical_width - 29;
                    y = 3;
                    width = theme::TITLE_BUTTON_SIZE;
                } else if matches!(id, 3 | 112 | 114 | 113) {
                    let inset = theme::CONTENT_MARGIN + theme::SECTION_PADDING;
                    let slot = (logical_width - 2 * inset - 8).max(2) / 2;
                    let order = if matches!(id, 3 | 112) { 0 } else { 1 };
                    x = inset + order * (slot + 8);
                    width = slot;
                } else if (214..=216).contains(&id) {
                    let inset = theme::CONTENT_MARGIN + theme::SECTION_PADDING;
                    let slot = (logical_width - 2 * inset - 16).max(3) / 3;
                    x = inset + (id - 214) * (slot + 8);
                    width = slot;
                }
                if c.tab >= 0 {
                    y -= scroll;
                }
                MoveWindow(
                    c.hwnd,
                    theme::px(x, self.dpi),
                    theme::px(y, self.dpi),
                    theme::px(width, self.dpi),
                    theme::px(h, self.dpi),
                    1,
                );
                let visible_height = if h > 200 && [116, 211].contains(&id) {
                    theme::FIELD_HEIGHT
                } else {
                    h
                };
                ShowWindow(
                    c.hwnd,
                    if id == 90 && self.gamepad_status.is_empty() {
                        SW_HIDE
                    } else if c.tab < 0
                        || (c.tab == self.tab
                            && y >= theme::CONTENT_TOP
                            && y + visible_height <= content_bottom)
                    {
                        SW_SHOW
                    } else {
                        SW_HIDE
                    },
                );
            }
            let info = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: (self.scroll_content_height() - 1).max(0),
                nPage: (content_bottom - theme::CONTENT_TOP).max(1) as u32,
                nPos: scroll,
                nTrackPos: 0,
            };
            SetScrollInfo(self.hwnd, SB_VERT, &info, 1);
            // Layout can synchronously request owner-drawing while State is
            // borrowed. Repaint children after returning to the message loop.
            RedrawWindow(
                self.hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ALLCHILDREN,
            );
        }
    }
    fn paint_non_client(&self, hwnd: HWND, region: WPARAM) {
        unsafe {
            DefWindowProcW(hwnd, WM_NCPAINT, region, 0);
            let dc = GetWindowDC(hwnd);
            if dc.is_null() {
                return;
            }
            let mut window = RECT::default();
            let mut client = RECT::default();
            let mut client_origin = POINT { x: 0, y: 0 };
            GetWindowRect(hwnd, &mut window);
            GetClientRect(hwnd, &mut client);
            ClientToScreen(hwnd, &mut client_origin);
            let width = window.right - window.left;
            let height = window.bottom - window.top;
            let left = client_origin.x - window.left;
            let top = client_origin.y - window.top;
            let right = left + client.right;
            let bottom = top + client.bottom;
            let scrollbar_left = right;
            let scrollbar_right = (width - left).max(scrollbar_left);
            for rect in [
                RECT {
                    left: 0,
                    top: 0,
                    right: scrollbar_left,
                    bottom: top,
                },
                RECT {
                    left: 0,
                    top: bottom,
                    right: scrollbar_left,
                    bottom: height,
                },
                RECT {
                    left: 0,
                    top,
                    right: left,
                    bottom,
                },
                RECT {
                    left: scrollbar_right,
                    top,
                    right: width,
                    bottom,
                },
            ] {
                if rect.left < rect.right && rect.top < rect.bottom {
                    FillRect(dc, &rect, self.theme.border);
                }
            }
            ReleaseDC(hwnd, dc);
        }
    }
    fn scroll_content_height(&self) -> i32 {
        let content_end = match self.tab {
            0 => theme::CONTENT_BOTTOM,
            1 => theme::CONTENT_BOTTOM,
            _ => 0,
        };
        (content_end - theme::CONTENT_TOP).max(1)
    }
    fn paint(&self, hwnd: HWND) {
        unsafe {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client);
            let px = |value| theme::px(value, self.dpi);
            FillRect(dc, &client, self.theme.backdrop);

            let header = RECT {
                left: 0,
                top: 0,
                right: client.right,
                bottom: px(theme::TITLE_HEIGHT),
            };
            FillRect(dc, &header, self.theme.topbar);
            let header_rule = RECT {
                left: 0,
                top: px(theme::TITLE_HEIGHT - 1),
                right: client.right,
                bottom: px(theme::TITLE_HEIGHT),
            };
            FillRect(dc, &header_rule, self.theme.border);

            let title_icon = super::app_icon_at(px(22));
            DrawIconEx(
                dc,
                px(8),
                px(4),
                title_icon.unwrap_or_else(super::app_icon),
                px(22),
                px(22),
                0,
                null_mut(),
                DI_NORMAL,
            );
            if let Some(icon) = title_icon {
                DestroyIcon(icon);
            }
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, theme::TEXT);
            let old_title = SelectObject(dc, self.title_font);
            let title = wide("Switchmut");
            let mut title_rect = RECT {
                left: px(49),
                top: 0,
                right: client.right - px(70),
                bottom: px(theme::TITLE_HEIGHT),
            };
            SetTextCharacterExtra(dc, px(1));
            DrawTextW(
                dc,
                title.as_ptr(),
                -1,
                &mut title_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
            SetTextCharacterExtra(dc, 0);
            SelectObject(dc, old_title);

            let tabs = RECT {
                left: 0,
                top: px(theme::TITLE_HEIGHT),
                right: client.right,
                bottom: px(theme::CONTENT_TOP),
            };
            FillRect(dc, &tabs, self.theme.sunk);
            let tabbar = RECT {
                left: px(theme::CONTENT_MARGIN),
                top: px(theme::TABBAR_TOP),
                right: client.right - px(theme::CONTENT_MARGIN),
                bottom: px(theme::CONTENT_TOP),
            };
            FillRect(dc, &tabbar, self.theme.sunk);
            FrameRect(dc, &tabbar, self.theme.border);
            let saved = SaveDC(dc);
            if saved == 0 {
                EndPaint(hwnd, &paint);
                return;
            }
            IntersectClipRect(dc, 0, px(theme::CONTENT_TOP), client.right, client.bottom);
            let right = client.right - px(theme::CONTENT_MARGIN);
            let left = px(theme::CONTENT_MARGIN);
            let scroll = self.scroll.clamp(0, self.scroll_limit());
            let y = |logical: i32| px(logical - scroll);
            match self.tab {
                0 => {
                    self.theme.section(
                        dc,
                        RECT {
                            left,
                            top: y(84),
                            right,
                            bottom: y(276),
                        },
                        "CLIENTS",
                        self.section_font,
                        self.dpi,
                    );
                    self.theme.section(
                        dc,
                        RECT {
                            left,
                            top: y(284),
                            right,
                            bottom: y(372),
                        },
                        "REFRESH",
                        self.section_font,
                        self.dpi,
                    );
                }
                1 => {
                    self.theme.section(
                        dc,
                        RECT {
                            left,
                            top: y(84),
                            right,
                            bottom: y(172),
                        },
                        "CONTROLLER",
                        self.section_font,
                        self.dpi,
                    );
                    let actions = RECT {
                        left,
                        top: y(180),
                        right,
                        bottom: y(372),
                    };
                    self.theme
                        .section(dc, actions, "ACTIONS", self.section_font, self.dpi);
                }
                _ => {}
            }
            RestoreDC(dc, saved);
            EndPaint(hwnd, &paint);
        }
    }
    fn draw_item(&self, item_ptr: *const DRAWITEMSTRUCT) -> bool {
        if item_ptr.is_null() {
            return false;
        }
        unsafe {
            let item = &*item_ptr;
            if item.CtlType == ODT_LISTBOX {
                self.draw_list_item(item);
                return true;
            }
            if item.CtlType != ODT_BUTTON {
                return false;
            }
            self.draw_button(item);
            true
        }
    }
    unsafe fn draw_button(&self, item: &DRAWITEMSTRUCT) {
        unsafe {
            let id = item.CtlID as i32;
            let selected = item.itemState & ODS_SELECTED != 0;
            let disabled = item.itemState & ODS_DISABLED != 0;
            let focused = item.itemState & ODS_FOCUS != 0;
            if id == 5 || id == 6 {
                let dc = item.hDC;
                let px = |value| theme::px(value, self.dpi);
                FillRect(dc, &item.rcItem, self.theme.topbar);
                let old_brush = SelectObject(dc, GetStockObject(DC_BRUSH));
                let old_pen = SelectObject(dc, GetStockObject(DC_PEN));
                let old_brush_color = SetDCBrushColor(dc, theme::COMPACT);
                let old_pen_color = SetDCPenColor(dc, theme::BORDER);
                let close_hover = id == 6 && selected;
                SetDCBrushColor(
                    dc,
                    if close_hover {
                        theme::DANGER
                    } else {
                        theme::COMPACT
                    },
                );
                SetDCPenColor(dc, theme::BORDER);
                Ellipse(
                    dc,
                    item.rcItem.left + px(1),
                    item.rcItem.top + px(1),
                    item.rcItem.right - px(1),
                    item.rcItem.bottom - px(1),
                );
                SetDCPenColor(dc, theme::TEXT);
                let cx = (item.rcItem.left + item.rcItem.right) / 2;
                let cy = (item.rcItem.top + item.rcItem.bottom) / 2;
                if id == 5 {
                    MoveToEx(dc, cx - px(4), cy, null_mut());
                    LineTo(dc, cx + px(4), cy);
                } else {
                    MoveToEx(dc, cx - px(3), cy - px(3), null_mut());
                    LineTo(dc, cx + px(3), cy + px(3));
                    MoveToEx(dc, cx + px(3), cy - px(3), null_mut());
                    LineTo(dc, cx - px(3), cy + px(3));
                }
                SetDCBrushColor(dc, old_brush_color);
                SetDCPenColor(dc, old_pen_color);
                SelectObject(dc, old_pen);
                SelectObject(dc, old_brush);
                return;
            }

            let brush = if (100..=101).contains(&id) {
                if id - 100 == self.tab {
                    self.theme.compact
                } else {
                    self.theme.sunk
                }
            } else if selected {
                self.theme.panel
            } else {
                self.theme.compact
            };
            FillRect(item.hDC, &item.rcItem, brush);
            if !(100..=101).contains(&id) {
                FrameRect(item.hDC, &item.rcItem, self.theme.border);
            }
            if focused {
                FrameRect(item.hDC, &item.rcItem, self.theme.accent_strong);
            }
            SetBkMode(item.hDC, TRANSPARENT as i32);
            SetTextColor(
                item.hDC,
                if disabled {
                    theme::SECONDARY
                } else if (100..=101).contains(&id) && id - 100 == self.tab {
                    theme::ACCENT_TEXT
                } else {
                    theme::TEXT
                },
            );
            let button_font = if (100..=101).contains(&id) {
                self.section_font
            } else {
                self.font
            };
            let old_font = SelectObject(item.hDC, button_font);
            let length = GetWindowTextLengthW(item.hwndItem).max(0) as usize;
            let mut text = vec![0u16; length + 1];
            GetWindowTextW(item.hwndItem, text.as_mut_ptr(), text.len() as i32);
            let mut display = Vec::with_capacity(length + 1);
            let mut prefix = false;
            for &unit in &text[..length] {
                if unit == b'&' as u16 && !prefix {
                    prefix = true;
                    continue;
                }
                prefix = false;
                display.push(unit);
            }
            display.push(0);
            let padding = theme::px(6, self.dpi);
            let mut text_rect = item.rcItem;
            text_rect.left += padding;
            text_rect.right -= padding;
            DrawTextW(
                item.hDC,
                display.as_ptr(),
                -1,
                &mut text_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(item.hDC, old_font);
        }
    }
    unsafe fn draw_list_item(&self, item: &DRAWITEMSTRUCT) {
        unsafe {
            if item.itemID == u32::MAX {
                return;
            }
            let selected = item.itemState & ODS_SELECTED != 0;
            FillRect(
                item.hDC,
                &item.rcItem,
                if selected {
                    self.theme.selected
                } else {
                    self.theme.input
                },
            );
            SetBkMode(item.hDC, TRANSPARENT as i32);
            SetTextColor(item.hDC, theme::INPUT_TEXT);
            let length = SendMessageW(item.hwndItem, LB_GETTEXTLEN, item.itemID as usize, 0);
            if length < 0 {
                return;
            }
            let old_font = SelectObject(item.hDC, self.font);
            let mut text = vec![0u16; length as usize + 1];
            SendMessageW(
                item.hwndItem,
                LB_GETTEXT,
                item.itemID as usize,
                text.as_mut_ptr() as LPARAM,
            );
            let mut text_rect = item.rcItem;
            text_rect.left += theme::px(6, self.dpi);
            text_rect.right -= theme::px(4, self.dpi);
            DrawTextW(
                item.hDC,
                text.as_ptr(),
                -1,
                &mut text_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SelectObject(item.hDC, old_font);
            if item.itemState & ODS_FOCUS != 0 {
                FrameRect(item.hDC, &item.rcItem, self.theme.accent);
            }
        }
    }
    fn measure_item(&self, item: *mut MEASUREITEMSTRUCT) -> bool {
        if item.is_null() {
            return false;
        }
        unsafe {
            let item = &mut *item;
            if item.CtlType == ODT_LISTBOX && [111, 213].contains(&(item.CtlID as i32)) {
                item.itemHeight = (22 * self.dpi / 96).max(18);
                return true;
            }
        }
        false
    }
    fn refresh_all(&mut self) {
        self.select(
            116,
            match self.draft.refresh_mode {
                RefreshMode::Manual => 0,
                RefreshMode::Periodic => 1,
                RefreshMode::OnSwitch => 2,
            },
            true,
        );
        self.refresh_clients();
        self.refresh_devices();
        self.refresh_bindings(0);
    }
    fn refresh_clients(&self) {
        self.refresh_clients_with_key(self.selected_client_key());
    }
    fn selected_client_key(&self) -> Option<ClientKey> {
        self.selection(111, false)
            .and_then(|index| self.clients.get(index).map(|client| client.key))
    }
    fn refresh_clients_with_key(&self, selected_key: Option<ClientKey>) {
        let selected = selected_key
            .and_then(|key| self.clients.iter().position(|client| client.key == key))
            .or_else(|| {
                self.current.and_then(|current| {
                    self.clients.iter().position(|client| client.key == current)
                })
            })
            .unwrap_or(0);
        self.clear(111, false);
        for c in &self.clients {
            self.add(
                111,
                &format!(
                    "{} {} | PID {} | {}",
                    if Some(c.key) == self.current {
                        "*"
                    } else {
                        " "
                    },
                    if c.allowed { "Allow" } else { "Deny" },
                    c.key.pid,
                    c.title
                ),
                false,
            );
        }
        if !self.clients.is_empty() {
            self.select(111, selected.min(self.clients.len() - 1), false);
        }
    }
    fn refresh_devices(&self) {
        self.clear(211, true);
        self.add(211, "Automatic", true);
        for d in &self.devices {
            self.add(211, &d.name, true);
        }
        let selection = self
            .draft
            .controller
            .as_ref()
            .and_then(|id| self.devices.iter().position(|d| &d.id == id))
            .map(|i| i + 1);
        if self.draft.controller.is_some() && selection.is_none() {
            self.add(211, "Selected controller (disconnected)", true);
            self.select(211, self.devices.len() + 1, true);
        } else {
            self.select(211, selection.unwrap_or(0), true);
        }
    }
    fn refresh_bindings(&self, index: usize) {
        self.clear(213, false);
        for action in Action::ALL {
            let binding = self
                .draft
                .bindings
                .get(action)
                .map_or_else(|| "Unassigned".into(), |b| button_name(b, false));
            self.add(213, &format!("{}: {binding}", action.label()), false);
        }
        self.select(213, index, false);
    }
    fn read_fields(&mut self) -> Result<(), String> {
        self.draft.refresh_mode = match self.selection(116, true) {
            Some(0) => RefreshMode::Manual,
            Some(1) => RefreshMode::Periodic,
            _ => RefreshMode::OnSwitch,
        };
        self.draft.validate()
    }
    fn emit_save(&mut self) -> Result<(), String> {
        self.read_fields()?;
        self.next_save_request = self.next_save_request.wrapping_add(1);
        if self.next_save_request == 0 {
            self.next_save_request = 1;
        }
        let request_id = self.next_save_request;
        self.pending_save = Some(request_id);
        self.show_save_feedback = self.tab == 1;
        self.status("Saving changes...");
        if let Err(error) = self.sender.send(SettingsEvent::Save {
            request_id,
            config: Box::new(self.draft.clone()),
            clients: self.clients.clone(),
        }) {
            self.pending_save = None;
            self.show_save_feedback = false;
            return Err(format!("could not deliver save request: {error}"));
        }
        Ok(())
    }
    fn emit_client_edits(&mut self) -> Result<(), String> {
        self.sender
            .send(SettingsEvent::ClientEdits(self.clients.clone()))
            .map_err(|error| format!("could not deliver client changes: {error}"))
    }
    fn command(&mut self, id: i32, notification: u32) {
        match id {
            5 => unsafe {
                PostMessageW(self.hwnd, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0);
            },
            6 => unsafe {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            },
            2 => self.cancel(),
            3 => {
                let _ = self.sender.send(SettingsEvent::Refresh);
            }
            100..=101 => {
                if self.capturing.take().is_some() {
                    let _ = self.sender.send(SettingsEvent::EndCapture);
                }
                self.clear_status();
                self.tab = id - 100;
                self.scroll = 0;
                self.layout();
            }
            112 | 113 => {
                if let Some(i) = self.selection(111, false) {
                    let j = if id == 112 {
                        i.saturating_sub(1)
                    } else {
                        (i + 1).min(self.clients.len().saturating_sub(1))
                    };
                    if i < self.clients.len() && i != j {
                        self.clients.swap(i, j);
                        self.refresh_clients();
                        self.select(111, j, false);
                        if let Err(error) = self.emit_client_edits() {
                            self.notice(&error);
                        }
                    }
                }
            }
            114 => {
                if let Some(i) = self.selection(111, false)
                    && let Some(c) = self.clients.get_mut(i)
                {
                    c.allowed = !c.allowed;
                    self.refresh_clients();
                    if let Err(error) = self.emit_client_edits() {
                        self.notice(&error);
                    }
                }
            }
            116 if notification == CBN_SELCHANGE => {
                if let Err(error) = self.emit_save() {
                    self.notice(&error);
                }
            }
            211 if notification == CBN_SELCHANGE => {
                let previous = self.draft.controller.clone();
                if let Some(i) = self.selection(211, true) {
                    if i == 0 {
                        self.draft.controller = None;
                    } else if let Some(d) = self.devices.get(i - 1) {
                        self.draft.controller = Some(d.id.clone());
                    }
                }
                if self.draft.controller != previous {
                    if self.capturing.is_some() {
                        let _ = self.sender.send(SettingsEvent::CaptureController(
                            self.draft.controller.clone(),
                        ));
                    }
                    if let Err(error) = self.emit_save() {
                        self.notice(&error);
                    }
                }
                self.refresh_bindings(self.selection(213, false).unwrap_or(0));
            }
            214 => {
                if let Some(i) = self.selection(213, false) {
                    self.capturing = Some(i);
                    let _ = self.sender.send(SettingsEvent::CaptureController(
                        self.draft.controller.clone(),
                    ));
                    self.status("Press a button to assign it.");
                }
            }
            215 => {
                let mut changed = false;
                if let Some(i) = self.selection(213, false) {
                    if let Some(action) = Action::ALL.get(i) {
                        changed = self.draft.bindings.get(*action).is_some();
                        self.draft.bindings.set(*action, None);
                    }
                    self.refresh_bindings(i);
                }
                if self.capturing.take().is_some() {
                    let _ = self.sender.send(SettingsEvent::EndCapture);
                }
                if changed {
                    if let Err(error) = self.emit_save() {
                        self.notice(&error);
                    }
                } else {
                    self.clear_status();
                }
            }
            216 => {
                if self.capturing.take().is_some() {
                    let _ = self.sender.send(SettingsEvent::EndCapture);
                }
                self.clear_status();
            }
            _ => {}
        }
    }
}

fn control_color(message: u32, dc: HDC) -> HBRUSH {
    unsafe {
        let input = matches!(message, WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX);
        let background = if input { theme::INPUT } else { theme::PANEL };
        SetBkMode(dc, if input { OPAQUE } else { TRANSPARENT } as i32);
        SetBkColor(dc, background);
        SetTextColor(
            dc,
            if input {
                theme::INPUT_TEXT
            } else if message == WM_CTLCOLORBTN {
                theme::TEXT
            } else {
                theme::MUTED
            },
        );
        SetDCBrushColor(dc, background);
        GetStockObject(DC_BRUSH)
    }
}

pub fn button_name(button: u8, _xinput: bool) -> String {
    format!("Button {button}")
}

#[cfg(test)]
mod tests {
    use super::button_name;

    #[test]
    fn binding_labels_use_numeric_button_names() {
        assert_eq!(button_name(9, true), "Button 9");
        assert_eq!(button_name(27, false), "Button 27");
    }
}
