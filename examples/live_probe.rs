//! Explicit interactive test: validates an installed game through production discovery.
use std::{
    fs,
    path::PathBuf,
    ptr::{null, null_mut},
    time::Duration,
};
use switchmut::{
    clients::{ClientKey, Discovery},
    platform::{self, ActivationResult},
};
mod support;
use support::capture;
use windows_sys::{
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::*},
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

unsafe extern "system" fn procedure(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}
fn pump(ms: u64) {
    let until = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < until {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn window_key(hwnd: HWND) -> ClientKey {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        let mut c = FILETIME::default();
        let mut e = c;
        let mut k = c;
        let mut u = c;
        GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u);
        CloseHandle(process);
        ClientKey {
            hwnd: hwnd as isize,
            pid,
            created: ((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64,
        }
    }
}
fn main() -> Result<(), String> {
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts/live");
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let clients = Discovery::default().refresh();
    println!(
        "Production identity validation found {} clients: {clients:?}",
        clients.len()
    );
    let Some(client) = clients.first() else {
        return Err("No supported game window is available".into());
    };
    let initial = unsafe { GetForegroundWindow() };
    let initial_key = if initial.is_null() {
        None
    } else {
        Some(window_key(initial))
    };
    struct Restore(Option<ClientKey>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(key) = self.0 {
                platform::activate(key);
            }
        }
    }
    let _restore = Restore(initial_key);
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: GetModuleHandleW(null()),
            lpszClassName: w!("Switchmut.LiveProbe"),
            ..std::mem::zeroed()
        };
        RegisterClassW(&class);
        let other = CreateWindowExW(
            0,
            class.lpszClassName,
            w!("Switchmut foreground probe"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            40,
            40,
            420,
            260,
            null_mut(),
            null_mut(),
            class.hInstance,
            null(),
        );
        struct Close(HWND);
        impl Drop for Close {
            fn drop(&mut self) {
                unsafe {
                    DestroyWindow(self.0);
                }
            }
        }
        let _other = Close(other);
        let foreign = platform::activate(window_key(other));
        let game = platform::activate(client.key);
        pump(200);
        if foreign != ActivationResult::Activated || game != ActivationResult::Activated {
            return Err(format!(
                "Foreground switching failed: fixture={foreign:?}, game={game:?}"
            ));
        }
        let foreground = GetForegroundWindow() as isize;
        let pixels = capture(client.key.hwnd as HWND, &output.join("game-gdi.bmp"))?;
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, client.key.pid);
        let mut wow = 0;
        IsWow64Process(process, &mut wow);
        CloseHandle(process);
        let mut placement = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..std::mem::zeroed()
        };
        GetWindowPlacement(client.key.hwnd as HWND, &mut placement);
        ShowWindow(client.key.hwnd as HWND, SW_MINIMIZE);
        pump(100);
        let restore = platform::activate(client.key);
        pump(200);
        let minimized = IsIconic(client.key.hwnd as HWND);
        SetWindowPlacement(client.key.hwnd as HWND, &placement);
        let report = format!(
            "client={client:?}\nx86_game_process={}\nfixture_foreground={foreign:?}\ngame_foreground={game:?}\nobserved_foreground={foreground}\nminimize_restore={restore:?}\nstill_minimized={minimized}\ngdi_capture={}x{} unique_rgb_colors={}\n",
            wow != 0,
            pixels.0,
            pixels.1,
            pixels.2
        );
        println!("{report}");
        fs::write(output.join("report.txt"), report).map_err(|e| e.to_string())?;
        if restore != ActivationResult::Activated || minimized != 0 {
            return Err("Minimize/restore did not complete".into());
        }
    }
    Ok(())
}
