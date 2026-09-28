use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{CreateSolidBrush, DeleteObject, HBRUSH},
};

pub const BACKDROP: u32 = color(0x08, 0x7f, 0xaf);
pub const TOPBAR: u32 = color(0x05, 0x3b, 0x59);
pub const PANEL: u32 = color(0x05, 0x4e, 0x6e);
pub const COMPACT: u32 = color(0x06, 0x5a, 0x7a);
pub const SUNK: u32 = color(0x03, 0x2b, 0x40);
pub const BORDER: u32 = color(0x2a, 0x71, 0x8e);
pub const ACCENT: u32 = color(0x39, 0xc8, 0xf0);
pub const ACCENT_STRONG: u32 = color(0x78, 0xdd, 0xf8);
pub const ACCENT_TEXT: u32 = color(0xbc, 0xee, 0xff);
pub const TEXT: u32 = color(0xed, 0xf8, 0xfb);
pub const MUTED: u32 = color(0xd7, 0xef, 0xf7);
pub const SECONDARY: u32 = color(0xa9, 0xd2, 0xe0);
pub const INPUT: u32 = color(0xf4, 0xfc, 0xff);
pub const INPUT_TEXT: u32 = color(0x18, 0x36, 0x45);
pub const INPUT_BORDER: u32 = color(0x00, 0x46, 0x6d);
pub const SELECTED: u32 = ACCENT_STRONG;
pub const DANGER: u32 = color(0xbd, 0x33, 0x2b);

pub(crate) const WINDOW_WIDTH: i32 = 290;
pub(crate) const WINDOW_HEIGHT: i32 = 420;
pub(crate) const TITLE_HEIGHT: i32 = 30;
pub(crate) const TITLE_BUTTON_SIZE: i32 = 24;
pub(crate) const TAB_BUTTON_HEIGHT: i32 = 34;
pub(crate) const TABBAR_TOP: i32 = 37;
pub(crate) const TABBAR_BORDER: i32 = 1;
pub(crate) const TABBAR_PADDING: i32 = 4;
pub(crate) const FIELD_HEIGHT: i32 = 28;
pub(crate) const BUTTON_HEIGHT: i32 = 29;
pub(crate) const CONTENT_MARGIN: i32 = 8;
pub(crate) const SECTION_PADDING: i32 = 8;
pub(crate) const CONTENT_TOP: i32 = 81;
pub(crate) const CONTENT_BOTTOM: i32 = 404;

const fn color(red: u32, green: u32, blue: u32) -> u32 {
    red | (green << 8) | (blue << 16)
}

pub(crate) fn px(value: i32, dpi: u32) -> i32 {
    value * dpi.max(96) as i32 / 96
}

pub(crate) fn logical(value: i32, dpi: u32) -> i32 {
    let scale = dpi.max(96) as i32;
    (value * 96 + scale / 2) / scale
}

pub(crate) fn px_ceil(value: i32, dpi: u32) -> i32 {
    (value * dpi.max(96) as i32 + 95) / 96
}

pub struct Theme {
    pub backdrop: HBRUSH,
    pub topbar: HBRUSH,
    pub panel: HBRUSH,
    pub compact: HBRUSH,
    pub sunk: HBRUSH,
    pub border: HBRUSH,
    pub accent: HBRUSH,
    pub accent_strong: HBRUSH,
    pub selected: HBRUSH,
    pub input: HBRUSH,
    pub input_border: HBRUSH,
    pub danger: HBRUSH,
}

impl Theme {
    pub fn new() -> Self {
        unsafe {
            Self {
                backdrop: CreateSolidBrush(BACKDROP),
                topbar: CreateSolidBrush(TOPBAR),
                panel: CreateSolidBrush(PANEL),
                compact: CreateSolidBrush(COMPACT),
                sunk: CreateSolidBrush(SUNK),
                border: CreateSolidBrush(BORDER),
                accent: CreateSolidBrush(ACCENT),
                accent_strong: CreateSolidBrush(ACCENT_STRONG),
                selected: CreateSolidBrush(SELECTED),
                input: CreateSolidBrush(INPUT),
                input_border: CreateSolidBrush(INPUT_BORDER),
                danger: CreateSolidBrush(DANGER),
            }
        }
    }

    pub fn section(
        &self,
        dc: windows_sys::Win32::Graphics::Gdi::HDC,
        rect: RECT,
        title: &str,
        font: windows_sys::Win32::Graphics::Gdi::HFONT,
        dpi: u32,
    ) {
        use windows_sys::Win32::Graphics::Gdi::{
            DT_LEFT, DT_SINGLELINE, DT_VCENTER, DrawTextW, FrameRect, SelectObject, SetBkMode,
            SetTextColor, TRANSPARENT,
        };

        unsafe {
            let px = |value| px(value, dpi);
            windows_sys::Win32::Graphics::Gdi::FillRect(dc, &rect, self.panel);
            FrameRect(dc, &rect, self.border);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, ACCENT_TEXT);
            let old = SelectObject(dc, font);
            let heading = super::wide(title);
            let mut title_rect = RECT {
                left: rect.left + px(8),
                top: rect.top + px(3),
                right: rect.right - px(8),
                bottom: rect.top + px(19),
            };
            DrawTextW(
                dc,
                heading.as_ptr(),
                (heading.len() - 1) as i32,
                &mut title_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
            let rule = RECT {
                left: rect.left + px(8),
                top: rect.top + px(22),
                right: rect.right - px(8),
                bottom: rect.top + px(23),
            };
            windows_sys::Win32::Graphics::Gdi::FillRect(dc, &rule, self.border);
            SelectObject(dc, old);
        }
    }
}

impl Drop for Theme {
    fn drop(&mut self) {
        unsafe {
            for brush in [
                self.backdrop,
                self.topbar,
                self.panel,
                self.compact,
                self.sunk,
                self.border,
                self.accent,
                self.accent_strong,
                self.selected,
                self.input,
                self.input_border,
                self.danger,
            ] {
                if !brush.is_null() {
                    DeleteObject(brush);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::wide;

    #[test]
    fn section_heading_encoding_is_terminated_at_boundary_lengths() {
        for title in [
            "a".repeat(99),
            "b".repeat(100),
            "c".repeat(101),
            "x😀y".to_owned(),
        ] {
            let encoded = wide(&title);
            assert_eq!(encoded.last(), Some(&0));
            assert_eq!(
                encoded[..encoded.len() - 1],
                title.encode_utf16().collect::<Vec<_>>()
            );
        }
    }
}
