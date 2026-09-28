use std::fs;
use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*};
pub fn capture(hwnd: HWND, path: &std::path::Path) -> Result<(i32, i32, usize), String> {
    unsafe {
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect);
        let width = rect.right;
        let height = rect.bottom;
        if width <= 0 || height <= 0 || width > 8192 || height > 8192 {
            return Err("Invalid capture dimensions".into());
        }
        let source = GetDC(hwnd);
        let memory = CreateCompatibleDC(source);
        let bitmap = CreateCompatibleBitmap(source, width, height);
        let previous = SelectObject(memory, bitmap);
        let ok = BitBlt(memory, 0, 0, width, height, source, 0, 0, SRCCOPY);
        SelectObject(memory, previous);
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let lines = GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            pixels.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        );
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(hwnd, source);
        if ok == 0 || lines == 0 {
            return Err("GDI capture failed".into());
        }
        let colors: std::collections::HashSet<[u8; 3]> = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| [p[0], p[1], p[2]])
            .collect();
        let mut bytes = Vec::with_capacity(54 + pixels.len());
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&(54u32 + pixels.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(&54u32.to_le_bytes());
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&(-height).to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&32u16.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 24]);
        bytes.extend_from_slice(&pixels);
        fs::write(path, bytes).map_err(|e| e.to_string())?;
        Ok((width, height, colors.len()))
    }
}
