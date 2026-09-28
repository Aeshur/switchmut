//! Native GDI hold/select/release menu.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::OnceLock;

use crate::actions::Action;
use crate::clients::{ClientKey, is_live};
use crate::config::{MenuConfig, MenuItem};
use crate::platform::PaintBuffer;
use crate::ui::wide;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, CreatePen, CreateSolidBrush, DT_CENTER, DT_VCENTER, DT_WORDBREAK, DeleteObject,
    DrawTextW, Ellipse, FW_BOLD, FillRect, GetStockObject, HDC, InvalidateRect, LineTo, MoveToEx,
    NULL_BRUSH, PS_SOLID, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect,
    GetWindowLongPtrW, HWND_TOPMOST, IDC_ARROW, KillTimer, LWA_ALPHA, LWA_COLORKEY, LoadCursorW,
    RegisterClassW, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOSIZE, SetLayeredWindowAttributes,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN,
    WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SHOWWINDOW, WM_TIMER, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

const CLASS_NAME: &str = "Switchmut.NativeRadial.1";
const TIMER_ID: usize = 1;
const MENU_WIDTH: i32 = 300;
const MENU_HEIGHT: i32 = 300;
const MENU_POSITION_X: i32 = 0;
const MENU_POSITION_Y: i32 = 0;
const MENU_OPACITY: u16 = 80;
const MENU_SENSITIVITY: i32 = 5_000;
const MENU_REDRAW_INTERVAL_MS: u32 = 10;
const MENU_BACKGROUND: u32 = 0xff70_8090;
const MENU_BASE_COLOR: u32 = 0xffff_ffff;
const MENU_TEXT_COLOR: u32 = 0xff00_0000;
const MENU_SELECTOR_COLOR: u32 = 0xffff_ffff;
const ICON_RADIUS: f64 = 90.0;
const BASE_CIRCLE_RADIUS: i32 = 47;
const SELECTOR_RADIUS: f64 = 57.0;
const SELECTOR_HALF_WIDTH: f64 = 10.0;
static CLASS_REGISTERED: OnceLock<bool> = OnceLock::new();

struct RadialSlot {
    hwnd: Cell<HWND>,
    inner: RefCell<RadialWindow>,
}

struct RadialWindow {
    source: Option<ClientKey>,
    items: Vec<MenuItem>,
    selected: Option<usize>,
    direction_degrees: i32,
    is_open: bool,
    animation_step: u8,
    previous_selection: Option<usize>,
    error_reported: bool,
    pending_error: Option<String>,
}

impl RadialSlot {
    fn new(config: MenuConfig, source: Option<ClientKey>) -> Self {
        Self {
            hwnd: Cell::new(std::ptr::null_mut()),
            inner: RefCell::new(RadialWindow::new(config, source)),
        }
    }

    fn take_diagnostic(&self) -> Option<String> {
        self.inner.try_borrow_mut().ok()?.pending_error.take()
    }
}

impl Drop for RadialSlot {
    fn drop(&mut self) {
        if let Ok(mut state) = self.inner.try_borrow_mut() {
            state.destroy(self);
        }
    }
}

impl RadialWindow {
    fn new(config: MenuConfig, source: Option<ClientKey>) -> Self {
        let items: Vec<MenuItem> = if config.items.is_empty() {
            Action::ALL
                .into_iter()
                .filter(|action| *action != Action::Menu)
                .map(|action| MenuItem {
                    action,
                    label: action.menu_label().to_owned(),
                    enabled: true,
                })
                .collect()
        } else {
            config
                .items
                .iter()
                .filter(|item| item.enabled && item.action != Action::Menu)
                .cloned()
                .collect()
        };
        Self {
            source,
            items,
            selected: None,
            direction_degrees: 270,
            is_open: false,
            animation_step: 0,
            previous_selection: None,
            error_reported: false,
            pending_error: None,
        }
    }

    fn ensure_window(&mut self, owner: &RadialSlot) -> bool {
        if !owner.hwnd.get().is_null() {
            return true;
        }
        if !register_class() {
            self.record_error("could not register the radial menu window class");
            return false;
        }
        let mut class_name = wide(CLASS_NAME);
        let mut title = wide("Switchmut Radial Menu");
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name.as_mut_ptr(),
                title.as_mut_ptr(),
                WS_POPUP,
                MENU_POSITION_X,
                MENU_POSITION_Y,
                MENU_WIDTH,
                MENU_HEIGHT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                owner as *const RadialSlot as *mut c_void,
            )
        };
        if hwnd.is_null() {
            owner.hwnd.set(std::ptr::null_mut());
            self.record_error("could not create the radial menu window");
            return false;
        }
        let alpha = ((MENU_OPACITY * 255) / 100) as u8;
        let layered = unsafe {
            SetLayeredWindowAttributes(hwnd, 0x00ff00ff, alpha, LWA_ALPHA | LWA_COLORKEY)
        };
        let positioned = unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                MENU_POSITION_X,
                MENU_POSITION_Y,
                MENU_WIDTH,
                MENU_HEIGHT,
                SWP_NOACTIVATE | SWP_NOSIZE,
            )
        };
        if layered == 0 {
            self.record_error("could not set radial menu opacity");
        } else if positioned == 0 {
            self.record_error("could not position the radial menu window");
        } else {
            self.clear_error();
        }
        true
    }

    fn open(&mut self, owner: &RadialSlot) -> bool {
        if self.source.is_some_and(|source| !is_live(source)) {
            return false;
        }
        if !self.ensure_window(owner) {
            return false;
        }
        self.is_open = true;
        self.selected = None;
        self.previous_selection = None;
        self.animation_step = 0;
        let hwnd = owner.hwnd.get();
        let timer = unsafe { SetTimer(hwnd, TIMER_ID, MENU_REDRAW_INTERVAL_MS, None) };
        if timer == 0 {
            self.record_error("could not start the radial menu redraw timer");
        }
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                MENU_POSITION_X,
                MENU_POSITION_Y,
                MENU_WIDTH,
                MENU_HEIGHT,
                SWP_NOACTIVATE,
            );
            ShowWindow(hwnd, SW_SHOW);
            InvalidateRect(hwnd, std::ptr::null(), 0);
        }
        true
    }

    fn close(&mut self, owner: &RadialSlot) {
        if !self.is_open {
            return;
        }
        self.is_open = false;
        self.selected = None;
        let hwnd = owner.hwnd.get();
        if !hwnd.is_null() {
            unsafe {
                KillTimer(hwnd, TIMER_ID);
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    fn update_axes(&mut self, owner: &RadialSlot, x: i32, y: i32) {
        if self.source.is_some_and(|source| !is_live(source)) {
            self.close(owner);
            return;
        }
        let selected = select_sector(x, y, self.items.len());
        if selected != self.selected {
            self.animation_step = 0;
            self.previous_selection = self.selected;
            self.selected = selected;
            let hwnd = owner.hwnd.get();
            if self.is_open && !hwnd.is_null() {
                unsafe {
                    SetTimer(hwnd, TIMER_ID, MENU_REDRAW_INTERVAL_MS, None);
                }
            }
        }
        if self.selected.is_some() {
            let dx = x.clamp(0, 65_535) - 32_767;
            let dy = y.clamp(0, 65_535) - 32_767;
            self.direction_degrees = (dy as f64).atan2(dx as f64).to_degrees() as i32;
            self.direction_degrees = self.direction_degrees.rem_euclid(360);
        }
        let hwnd = owner.hwnd.get();
        if self.is_open && !hwnd.is_null() {
            unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
        }
    }

    fn selected_action(&self) -> Option<Action> {
        self.selected
            .and_then(|index| self.items.get(index))
            .map(|item| item.action)
    }

    fn draw(&mut self, owner: &RadialSlot, dc: HDC) {
        let mut client = RECT::default();
        if unsafe { GetClientRect(owner.hwnd.get(), &mut client) } == 0 {
            self.record_error("could not read the radial menu client area");
            return;
        }
        let mut draw_failed = false;
        let background = unsafe { CreateSolidBrush(colorref(MENU_BACKGROUND)) };
        if !background.is_null() {
            if unsafe { FillRect(dc, &client, background) } == 0 {
                draw_failed = true;
                self.record_error("could not fill the radial menu background");
            }
            unsafe { DeleteObject(background) };
        } else {
            draw_failed = true;
            self.record_error("could not allocate the radial menu background brush");
        }
        if self.animation_step < 20 {
            self.animation_step = (self.animation_step + 4).min(20);
            if self.animation_step == 20 {
                unsafe { KillTimer(owner.hwnd.get(), TIMER_ID) };
            }
        }
        let cx = (client.right - client.left) / 2;
        let cy = (client.bottom - client.top) / 2;
        let base_color = composite_color(MENU_BASE_COLOR, MENU_BACKGROUND);
        let selector_color = composite_color(MENU_SELECTOR_COLOR, MENU_BACKGROUND);
        let text_color = composite_color(MENU_TEXT_COLOR, MENU_BACKGROUND);
        let base_pen = unsafe { CreatePen(PS_SOLID, 1, base_color) };
        let selector_pen = unsafe { CreatePen(PS_SOLID, 2, selector_color) };
        let base_circle_pen = unsafe { CreatePen(PS_SOLID, 8, base_color) };
        let font = unsafe {
            CreateFontW(
                -18,
                0,
                0,
                0,
                FW_BOLD as i32,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                wide("Arial").as_ptr(),
            )
        };
        for (resource, error) in [
            (base_pen, "could not allocate the radial menu outline pen"),
            (
                selector_pen,
                "could not allocate the radial menu selector pen",
            ),
            (
                base_circle_pen,
                "could not allocate the radial menu ring pen",
            ),
            (font, "could not allocate the radial menu font"),
        ] {
            if resource.is_null() {
                draw_failed = true;
                self.record_error(error);
            }
        }
        let old_text = unsafe { SetTextColor(dc, text_color) };
        let old_mode = unsafe { SetBkMode(dc, TRANSPARENT as i32) };
        let old_pen = if !base_pen.is_null() {
            unsafe { SelectObject(dc, base_pen) }
        } else {
            std::ptr::null_mut()
        };
        let old_font = if !font.is_null() {
            unsafe { SelectObject(dc, font) }
        } else {
            std::ptr::null_mut()
        };
        let old_brush = unsafe { SelectObject(dc, GetStockObject(NULL_BRUSH)) };
        unsafe {
            Ellipse(
                dc,
                cx - BASE_CIRCLE_RADIUS,
                cy - BASE_CIRCLE_RADIUS,
                cx + BASE_CIRCLE_RADIUS,
                cy + BASE_CIRCLE_RADIUS,
            );
        }
        for (index, item) in self.items.iter().enumerate() {
            let step = 360.0 / self.items.len() as f64;
            let radians = (step * index as f64 - 90.0).to_radians();
            let mut radial = ICON_RADIUS;
            if self.previous_selection == Some(index) && self.selected != Some(index) {
                radial += f64::from(20 - self.animation_step);
            }
            if self.selected == Some(index) {
                radial += f64::from(self.animation_step);
            }
            let x = cx + (radians.cos() * radial).round() as i32;
            let y = cy + (radians.sin() * radial).round() as i32;
            unsafe { SetTextColor(dc, base_color) };
            unsafe { draw_icon(dc, item.action, x, y) };
        }
        if let Some(index) = self
            .selected
            .and_then(|index| self.items.get(index).map(|_| index))
        {
            let mut text = wide(&self.items[index].label);
            let mut bounds = RECT {
                left: cx - 42,
                top: cy - 42,
                right: cx + 42,
                bottom: cy + 42,
            };
            unsafe {
                SetTextColor(dc, text_color);
                let text_result = DrawTextW(
                    dc,
                    text.as_mut_ptr(),
                    (text.len() - 1) as i32,
                    &mut bounds,
                    DT_CENTER | DT_VCENTER | DT_WORDBREAK,
                );
                SetTextColor(dc, text_color);
                if text_result == 0 {
                    draw_failed = true;
                    self.record_error("could not draw the radial menu label");
                }
            }
            if !selector_pen.is_null() {
                unsafe { SelectObject(dc, selector_pen) };
                draw_selector(dc, cx, cy, f64::from(self.direction_degrees));
            }
        }
        if !base_circle_pen.is_null() {
            unsafe {
                SelectObject(dc, base_circle_pen);
                Ellipse(
                    dc,
                    cx - BASE_CIRCLE_RADIUS,
                    cy - BASE_CIRCLE_RADIUS,
                    cx + BASE_CIRCLE_RADIUS,
                    cy + BASE_CIRCLE_RADIUS,
                );
            }
        }
        if !old_pen.is_null() {
            unsafe { SelectObject(dc, old_pen) };
        }
        if !old_font.is_null() {
            unsafe { SelectObject(dc, old_font) };
        }
        if !old_brush.is_null() {
            unsafe { SelectObject(dc, old_brush) };
        }
        unsafe {
            SetTextColor(dc, old_text);
            SetBkMode(dc, old_mode);
            if !base_pen.is_null() {
                DeleteObject(base_pen);
            }
            if !selector_pen.is_null() {
                DeleteObject(selector_pen);
            }
            if !base_circle_pen.is_null() {
                DeleteObject(base_circle_pen);
            }
            if !font.is_null() {
                DeleteObject(font);
            }
        }
        if !draw_failed {
            self.clear_error();
        }
    }

    fn destroy(&mut self, owner: &RadialSlot) {
        let hwnd = owner.hwnd.get();
        if hwnd.is_null() {
            return;
        }
        owner.hwnd.set(std::ptr::null_mut());
        unsafe {
            KillTimer(hwnd, TIMER_ID);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DestroyWindow(hwnd);
        }
    }

    fn record_error(&mut self, message: &str) {
        if !self.error_reported {
            self.error_reported = true;
            self.pending_error = Some(message.to_owned());
        }
    }

    fn clear_error(&mut self) {
        self.error_reported = false;
    }
}

/// A radial menu window for one interaction. The parent owns input polling and executes actions.
pub struct Radial {
    window: Box<RadialSlot>,
}

impl Radial {
    pub fn new(config: MenuConfig, source: Option<ClientKey>) -> Self {
        Self {
            window: Box::new(RadialSlot::new(config, source)),
        }
    }

    pub fn open(&mut self) -> bool {
        self.window
            .inner
            .try_borrow_mut()
            .is_ok_and(|mut window| window.open(&self.window))
    }

    pub fn update_axes(&mut self, x: i32, y: i32) {
        if let Ok(mut window) = self.window.inner.try_borrow_mut() {
            window.update_axes(&self.window, x, y);
        }
    }

    pub fn release(&mut self) -> Option<Action> {
        let mut window = self.window.inner.try_borrow_mut().ok()?;
        if !window.is_open {
            return None;
        }
        if window.source.is_some_and(|source| !is_live(source)) {
            window.close(&self.window);
            return None;
        }
        let action = window.selected_action();
        window.close(&self.window);
        action
    }

    pub fn cancel(&mut self) {
        if let Ok(mut window) = self.window.inner.try_borrow_mut() {
            window.close(&self.window);
        }
    }

    pub fn is_open(&self) -> bool {
        self.window
            .inner
            .try_borrow()
            .is_ok_and(|window| window.is_open)
    }

    pub fn take_diagnostics(&mut self) -> Vec<String> {
        self.window.take_diagnostic().into_iter().collect()
    }
}

fn select_sector(x: i32, y: i32, item_count: usize) -> Option<usize> {
    if item_count == 0 || item_count > 360 {
        return None;
    }
    let dx = x.clamp(0, 65_535) - 32_767;
    let dy = y.clamp(0, 65_535) - 32_767;
    if ((i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)) as f64).sqrt()
        <= f64::from(MENU_SENSITIVITY)
    {
        return None;
    }
    let mut direction = (dy as f64).atan2(dx as f64).to_degrees() as i32;
    direction = direction.rem_euclid(360);
    let chunk = 360 / item_count as i32;
    let normalized = (direction + 90 + chunk / 2).rem_euclid(360);
    Some((normalized / chunk) as usize)
}

fn colorref(argb: u32) -> u32 {
    let red = (argb >> 16) & 0xff;
    let green = (argb >> 8) & 0xff;
    let blue = argb & 0xff;
    red | (green << 8) | (blue << 16)
}

// The menu has a solid form background. GDI pens/text use the component color
// composited over that surface; whole-window opacity is applied separately.
fn composite_color(argb: u32, background: u32) -> u32 {
    let alpha = argb >> 24;
    let mut rgb = 0;
    for shift in [0, 8, 16] {
        let foreground = (argb >> shift) & 255;
        let behind = (background >> shift) & 255;
        rgb |= ((foreground * alpha + behind * (255 - alpha) + 127) / 255) << shift;
    }
    colorref(rgb)
}

fn draw_selector(dc: HDC, cx: i32, cy: i32, direction_degrees: f64) {
    let radians = direction_degrees.to_radians();
    let ux = radians.cos();
    let uy = radians.sin();
    let px = -uy;
    let py = ux;
    let tip_x = cx + (ux * SELECTOR_RADIUS).round() as i32;
    let tip_y = cy + (uy * SELECTOR_RADIUS).round() as i32;
    for sign in [-1.0, 1.0] {
        let tail_x =
            cx + (ux * (SELECTOR_RADIUS - 5.0) + px * SELECTOR_HALF_WIDTH * sign).round() as i32;
        let tail_y =
            cy + (uy * (SELECTOR_RADIUS - 5.0) + py * SELECTOR_HALF_WIDTH * sign).round() as i32;
        unsafe {
            MoveToEx(dc, tip_x, tip_y, std::ptr::null_mut());
            LineTo(dc, tail_x, tail_y);
        }
    }
}

unsafe fn draw_icon(dc: HDC, action: Action, x: i32, y: i32) {
    unsafe {
        match action {
            Action::Next => {
                MoveToEx(dc, x - 10, y, std::ptr::null_mut());
                LineTo(dc, x + 9, y);
                LineTo(dc, x + 2, y - 7);
                MoveToEx(dc, x + 9, y, std::ptr::null_mut());
                LineTo(dc, x + 2, y + 7);
            }
            Action::Previous => {
                MoveToEx(dc, x + 10, y, std::ptr::null_mut());
                LineTo(dc, x - 9, y);
                LineTo(dc, x - 2, y - 7);
                MoveToEx(dc, x - 9, y, std::ptr::null_mut());
                LineTo(dc, x - 2, y + 7);
            }
            Action::Menu => {}
        }
    }
}

fn register_class() -> bool {
    *CLASS_REGISTERED.get_or_init(|| {
        let mut name = wide(CLASS_NAME);
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: crate::ui::app_icon(),
            hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: name.as_mut_ptr(),
        };
        let atom = unsafe { RegisterClassW(&class) };
        atom != 0 || unsafe { GetLastError() } == 1410
    })
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe {
            &*(lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW)
        };
        let slot = create.lpCreateParams as *mut RadialSlot;
        if slot.is_null() {
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        unsafe {
            (*slot).hwnd.set(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, slot as isize);
        }
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let slot_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RadialSlot };
    if slot_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    if message == WM_NCDESTROY {
        unsafe {
            KillTimer(hwnd, TIMER_ID);
            (*slot_ptr).hwnd.set(std::ptr::null_mut());
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let slot = unsafe { &*slot_ptr };
    if message == WM_ERASEBKGND {
        return 1;
    }
    if message == WM_PAINT {
        let mut paint = windows_sys::Win32::Graphics::Gdi::PAINTSTRUCT::default();
        let dc = unsafe { windows_sys::Win32::Graphics::Gdi::BeginPaint(hwnd, &mut paint) };
        if !dc.is_null() {
            let mut client = RECT::default();
            unsafe {
                GetClientRect(hwnd, &mut client);
            }
            let buffer = unsafe { PaintBuffer::new(dc, client.right, client.bottom) };
            let draw_dc = buffer.as_ref().map_or(dc, PaintBuffer::dc);
            if let Ok(mut state) = slot.inner.try_borrow_mut() {
                state.draw(slot, draw_dc);
                match buffer.as_ref() {
                    Some(buffer) if !buffer.present() => {
                        state.record_error("could not present the radial paint buffer")
                    }
                    None => state.record_error("could not allocate the radial paint buffer"),
                    _ => {}
                }
            }
            drop(buffer);
            unsafe { windows_sys::Win32::Graphics::Gdi::EndPaint(hwnd, &paint) };
        } else if let Ok(mut state) = slot.inner.try_borrow_mut() {
            state.record_error("could not begin radial menu painting");
        }
        return 0;
    }
    let Ok(mut state) = slot.inner.try_borrow_mut() else {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    match message {
        WM_SHOWWINDOW => {
            state.is_open = wparam != 0;
            if wparam == 0 {
                unsafe { KillTimer(hwnd, TIMER_ID) };
            }
            0
        }
        WM_TIMER => {
            if state.animation_step < 20 {
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            } else {
                unsafe { KillTimer(hwnd, TIMER_ID) };
            }
            0
        }
        WM_KEYDOWN if wparam as u16 == VK_ESCAPE => {
            state.close(slot);
            0
        }
        WM_CLOSE => {
            state.close(slot);
            0
        }
        _ => {
            drop(state);
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb_component_alpha_composites_over_menu_background() {
        assert_eq!(
            composite_color(0x00ffffff, 0xff102030),
            colorref(0xff102030)
        );
        assert_eq!(
            composite_color(0xffffffff, 0xff102030),
            colorref(0xffffffff)
        );
        assert_eq!(
            composite_color(0x80ffffff, 0xff000000),
            colorref(0xff808080)
        );
    }

    fn menu_item(action: Action, label: &str, enabled: bool) -> MenuItem {
        MenuItem {
            action,
            label: label.to_owned(),
            enabled,
        }
    }

    #[test]
    fn analog_selection_starts_at_top_and_moves_clockwise() {
        assert_eq!(select_sector(32_767, 20_000, 4), Some(0));
        assert_eq!(select_sector(45_000, 32_767, 4), Some(1));
        assert_eq!(select_sector(32_767, 45_000, 4), Some(2));
        assert_eq!(select_sector(20_000, 32_767, 4), Some(3));
    }

    #[test]
    fn center_and_disabled_items_do_not_release_actions() {
        assert_eq!(select_sector(32_767, 32_767, 5), None);
        let menu = RadialWindow::new(
            MenuConfig {
                items: vec![menu_item(Action::Next, "Next", false)],
            },
            None,
        );
        assert!(menu.items.is_empty());
        assert_eq!(menu.selected_action(), None);
    }

    #[test]
    fn enabled_custom_items_keep_order_and_disabled_items_take_no_sector() {
        let mut window = RadialWindow::new(
            MenuConfig {
                items: vec![
                    menu_item(Action::Previous, "Back", true),
                    menu_item(Action::Next, "Disabled", false),
                    menu_item(Action::Menu, "Menu", true),
                ],
            },
            None,
        );
        assert_eq!(
            window
                .items
                .iter()
                .map(|item| item.action)
                .collect::<Vec<_>>(),
            [Action::Previous]
        );
        assert_eq!(select_sector(32_767, 20_000, window.items.len()), Some(0));
        window.selected = Some(0);
        assert_eq!(window.selected_action(), Some(Action::Previous));
        window.selected = None;
        assert_eq!(window.selected_action(), None);
    }

    #[test]
    fn only_an_empty_custom_list_uses_the_builtin_items() {
        let mut default = RadialWindow::new(MenuConfig { items: Vec::new() }, None);
        let disabled = RadialWindow::new(
            MenuConfig {
                items: vec![menu_item(Action::Next, "Disabled", false)],
            },
            None,
        );
        assert_eq!(
            default
                .items
                .iter()
                .map(|item| item.action)
                .collect::<Vec<_>>(),
            [Action::Next, Action::Previous]
        );
        default.selected = Some(0);
        assert_eq!(default.selected_action(), Some(Action::Next));
        default.selected = Some(1);
        assert_eq!(default.selected_action(), Some(Action::Previous));
        assert!(disabled.items.is_empty());
    }

    #[test]
    fn diagnostics_remain_deduplicated_until_a_successful_frame() {
        let slot = RadialSlot::new(menu_config(), None);
        {
            let mut window = slot.inner.borrow_mut();
            window.record_error("could not create the radial menu window");
        }
        assert_eq!(
            slot.take_diagnostic().as_deref(),
            Some("could not create the radial menu window")
        );
        slot.inner
            .borrow_mut()
            .record_error("could not create the radial menu window");
        assert_eq!(slot.take_diagnostic(), None);
        {
            let mut window = slot.inner.borrow_mut();
            window.clear_error();
            window.record_error("could not create the radial menu window");
        }
        assert_eq!(
            slot.take_diagnostic().as_deref(),
            Some("could not create the radial menu window")
        );
    }

    fn menu_config() -> MenuConfig {
        MenuConfig { items: Vec::new() }
    }
}
