#![cfg(windows)]

#[path = "../examples/support/mod.rs"]
mod support;

use std::{
    cell::Cell,
    path::PathBuf,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use switchmut::input::Device;
use switchmut::ui::settings::SettingsEvent;
use switchmut::{
    clients::{Client, ClientKey, is_live},
    config::{Config, RefreshMode},
    platform::{ActivationResult, activate},
    radial::Radial,
    switching::Rotation,
    ui::settings::Settings,
    ui::tray::{TrayCallback, decode_callback},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::*},
        UI::{
            HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow},
            Input::KeyboardAndMouse::{GetFocus, SetFocus, VK_ESCAPE, VK_RETURN, VK_TAB},
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

#[test]
fn legacy_tray_callback_decodes_shell_mouse_message_from_lparam() {
    assert_eq!(
        decode_callback(WM_LBUTTONDBLCLK as LPARAM),
        Some(TrayCallback::OpenSettings)
    );
    assert_eq!(
        decode_callback(WM_RBUTTONUP as LPARAM),
        Some(TrayCallback::OpenContextMenu)
    );
    assert_eq!(
        decode_callback(WM_CONTEXTMENU as LPARAM),
        Some(TrayCallback::OpenContextMenu)
    );
    assert_eq!(decode_callback(WM_LBUTTONUP as LPARAM), None);
}

unsafe extern "system" fn fixture_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                DestroyWindow(hwnd);
                0
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let brush = CreateSolidBrush(0x00339966);
                FillRect(dc, &paint.rcPaint, brush);
                DeleteObject(brush);
                EndPaint(hwnd, &paint);
                0
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
fn fixture_window() -> HWND {
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(fixture_proc),
            hInstance: GetModuleHandleW(null()),
            lpszClassName: w!("Switchmut.NativeFixture"),
            ..std::mem::zeroed()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            0,
            class.lpszClassName,
            w!("Switchmut native integration fixture"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            40,
            40,
            500,
            360,
            null_mut(),
            null_mut(),
            class.hInstance,
            null(),
        );
        assert!(!hwnd.is_null());
        UpdateWindow(hwnd);
        hwnd
    }
}
fn key(hwnd: HWND) -> ClientKey {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        assert!(!process.is_null());
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        assert_ne!(
            GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user),
            0
        );
        CloseHandle(process);
        ClientKey {
            hwnd: hwnd as isize,
            pid,
            created: ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64,
        }
    }
}
fn pump(duration: Duration) {
    let start = Instant::now();
    while start.elapsed() < duration {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message != WM_QUIT {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn text(hwnd: HWND) -> String {
    unsafe {
        let mut buffer = vec![0u16; GetWindowTextLengthW(hwnd).max(0) as usize + 1];
        let length = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..length.max(0) as usize])
    }
}
fn child_texts(hwnd: HWND) -> Vec<String> {
    unsafe {
        let mut child = GetWindow(hwnd, GW_CHILD);
        let mut texts = Vec::new();
        while !child.is_null() {
            texts.push(text(child));
            child = GetWindow(child, GW_HWNDNEXT);
        }
        texts
    }
}
fn list_item_text(hwnd: HWND, index: usize) -> String {
    unsafe {
        let length = SendMessageW(hwnd, LB_GETTEXTLEN, index, 0);
        assert!(length >= 0);
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = SendMessageW(hwnd, LB_GETTEXT, index, buffer.as_mut_ptr() as LPARAM);
        String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
    }
}
fn logical_region_has_different_pixel(hwnd: HWND, rect: [i32; 4], color: u32) -> bool {
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let px = |value: i32| value * dpi / 96;
        let dc = GetDC(hwnd);
        assert!(!dc.is_null());
        // Parent DCs clip child windows for GetPixel. A captured bitmap includes
        // those pixels, so an excluded pixel cannot masquerade as painted text.
        let memory = CreateCompatibleDC(dc);
        let bitmap = CreateCompatibleBitmap(dc, px(rect[2]), px(rect[3]));
        let previous = SelectObject(memory, bitmap);
        assert_ne!(
            BitBlt(memory, 0, 0, px(rect[2]), px(rect[3]), dc, 0, 0, SRCCOPY),
            0
        );
        let different = (px(rect[1])..px(rect[3]))
            .any(|y| (px(rect[0])..px(rect[2])).any(|x| GetPixel(memory, x, y) != color));
        SelectObject(memory, previous);
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(hwnd, dc);
        different
    }
}

struct StatusColors {
    hwnd: HWND,
    calls: Cell<usize>,
    unthemed: Cell<usize>,
}

impl Drop for StatusColors {
    fn drop(&mut self) {
        unsafe { RemoveWindowSubclass(self.hwnd, Some(status_color_probe), 1) };
    }
}

unsafe extern "system" fn status_color_probe(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        let result = DefSubclassProc(hwnd, message, wp, lp);
        if message == WM_CTLCOLORSTATIC && lp as HWND == GetDlgItem(hwnd, 90) {
            let probe = &*(data as *const StatusColors);
            probe.calls.set(probe.calls.get() + 1);
            let brush = result as HBRUSH;
            let mut description = LOGBRUSH::default();
            let color = if brush == GetStockObject(DC_BRUSH) {
                GetDCBrushColor(wp as HDC)
            } else if GetObjectW(
                brush,
                std::mem::size_of::<LOGBRUSH>() as i32,
                (&mut description as *mut LOGBRUSH).cast(),
            ) != 0
            {
                description.lbColor
            } else {
                CLR_INVALID
            };
            if color != 0x006e4e05 || GetTextColor(wp as HDC) != 0x00f7efd7 {
                probe.unthemed.set(probe.unthemed.get() + 1);
            }
        }
        result
    }
}

fn assert_general_status_absent(hwnd: HWND) {
    unsafe {
        InvalidateRect(hwnd, null(), 1);
        UpdateWindow(hwnd);
    }
    pump(Duration::from_millis(10));
    assert!(
        !logical_region_has_different_pixel(hwnd, [20, 46, 25, 50], 0x007a5a06)
            && !logical_region_has_different_pixel(hwnd, [150, 46, 155, 50], 0x00402b03),
        "General tab highlight follows the visible page"
    );
    assert_eq!(unsafe { IsWindowVisible(GetDlgItem(hwnd, 90)) }, 0);
    assert_eq!(text(unsafe { GetDlgItem(hwnd, 90) }), "");
    assert!(
        !logical_region_has_different_pixel(hwnd, [16, 380, 274, 398], 0x00af7f08),
        "General has only backdrop pixels below Refresh, never stale status text"
    );
    assert!(
        logical_region_has_different_pixel(hwnd, [16, 340, 274, 356], 0x006e4e05),
        "Refresh combo remains painted with no status overlap"
    );
}

fn assert_gamepad_status(hwnd: HWND, expected: &str) {
    pump(Duration::from_millis(10));
    assert!(
        !logical_region_has_different_pixel(hwnd, [20, 46, 25, 50], 0x00402b03)
            && !logical_region_has_different_pixel(hwnd, [150, 46, 155, 50], 0x007a5a06),
        "Gamepad tab highlight follows the visible page"
    );
    assert_ne!(unsafe { IsWindowVisible(GetDlgItem(hwnd, 90)) }, 0);
    assert_eq!(text(unsafe { GetDlgItem(hwnd, 90) }), expected);
    assert!(
        logical_region_has_different_pixel(hwnd, [16, 338, 110, 356], 0x006e4e05),
        "Gamepad status text remains painted"
    );
    let themed = !logical_region_has_different_pixel(hwnd, [260, 338, 274, 356], 0x006e4e05);
    if !themed {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-artifacts")
            .join(format!("status-{}.bmp", std::process::id()));
        support::capture(hwnd, &path).unwrap();
        eprintln!(
            "Unexpected status background for {expected:?}: {}",
            path.display()
        );
    }
    assert!(
        themed,
        "Gamepad status background matches the Actions panel"
    );
}

fn gamepad_save(settings: &Settings) -> u64 {
    unsafe { SendMessageW(settings.hwnd(), WM_COMMAND, 214, 0) };
    assert!(matches!(
        settings.events.try_recv(),
        Ok(SettingsEvent::CaptureController(_))
    ));
    assert!(settings.capture_button(9));
    match settings.events.try_recv().unwrap() {
        SettingsEvent::Save { request_id, .. } => request_id,
        event => panic!("Unexpected save event: {event:?}"),
    }
}

fn status_lifecycle() {
    let settings = Settings::new(Config::default(), Vec::new(), None, Vec::new()).unwrap();
    let hwnd = settings.hwnd();
    assert_general_status_absent(hwnd);
    let probe = Box::new(StatusColors {
        hwnd,
        calls: Cell::new(0),
        unthemed: Cell::new(0),
    });
    unsafe {
        assert_ne!(
            SetWindowSubclass(
                hwnd,
                Some(status_color_probe),
                1,
                &*probe as *const _ as usize
            ),
            0
        );
        SendMessageW(hwnd, WM_COMMAND, 101, 0);
        SetFocus(GetDlgItem(hwnd, 213));
        SendMessageW(hwnd, WM_NEXTDLGCTL, 0, 0);
        assert_eq!(GetFocus(), GetDlgItem(hwnd, 214));
        SendMessageW(hwnd, WM_NEXTDLGCTL, 1, 0);
        assert_eq!(GetFocus(), GetDlgItem(hwnd, 213));
    }
    for _ in 0..3 {
        let request = gamepad_save(&settings);
        assert_gamepad_status(hwnd, "Saving button assignment...");
        settings.save_succeeded(request);
        assert_gamepad_status(hwnd, "Saved.");
        unsafe {
            SetWindowTextW(GetDlgItem(hwnd, 90), w!("Updated feedback."));
            InvalidateRect(hwnd, null(), 1);
            UpdateWindow(hwnd);
            SendMessageW(hwnd, WM_SIZE, SIZE_RESTORED as usize, 0);
        }
        assert_gamepad_status(hwnd, "Updated feedback.");
        unsafe { SendMessageW(hwnd, WM_COMMAND, 100, 0) };
        assert_general_status_absent(hwnd);
        unsafe { SendMessageW(hwnd, WM_COMMAND, 101, 0) };
        assert_eq!(unsafe { IsWindowVisible(GetDlgItem(hwnd, 90)) }, 0);
        let pending = gamepad_save(&settings);
        unsafe { SendMessageW(hwnd, WM_COMMAND, 100, 0) };
        settings.save_succeeded(pending);
        assert_general_status_absent(hwnd);
        unsafe { SendMessageW(hwnd, WM_COMMAND, 101, 0) };
        assert_eq!(text(unsafe { GetDlgItem(hwnd, 90) }), "");
        assert_eq!(unsafe { IsWindowVisible(GetDlgItem(hwnd, 90)) }, 0);
        let pending = gamepad_save(&settings);
        unsafe { SendMessageW(hwnd, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0) };
        settings.show();
        settings.save_succeeded(pending);
        assert_eq!(unsafe { IsWindowVisible(GetDlgItem(hwnd, 90)) }, 0);
        let request = gamepad_save(&settings);
        settings.save_succeeded(request);
        assert_gamepad_status(hwnd, "Saved.");
    }
    unsafe {
        SendMessageW(hwnd, WM_COMMAND, 100, 0);
        SendMessageW(hwnd, WM_COMMAND, 116 | ((CBN_SELCHANGE as usize) << 16), 0);
    }
    let SettingsEvent::Save { request_id, .. } = settings.events.try_recv().unwrap() else {
        panic!("Expected General save");
    };
    settings.save_succeeded(request_id);
    assert_general_status_absent(hwnd);
    unsafe {
        SendMessageW(hwnd, WM_COMMAND, 101, 0);
    }
    assert_eq!(text(unsafe { GetDlgItem(hwnd, 90) }), "");
    let request = gamepad_save(&settings);
    unsafe { SendMessageW(hwnd, WM_COMMAND, 100, 0) };
    settings.save_failed(request, "Disk full.");
    assert_ne!(unsafe { IsWindowVisible(GetDlgItem(hwnd, 90)) }, 0);
    assert!(text(unsafe { GetDlgItem(hwnd, 90) }).starts_with("Save failed: Disk full."));
    unsafe { SendMessageW(hwnd, WM_COMMAND, 100, 0) };
    assert_general_status_absent(hwnd);
    settings.status("Recovered configuration.");
    assert_gamepad_status(hwnd, "Recovered configuration.");
    assert!(
        probe.calls.get() > 0,
        "native static color callbacks were exercised"
    );
    assert_eq!(
        probe.unthemed.get(),
        0,
        "every status color callback, including synchronous reentry, is themed"
    );
    println!(
        "status lifecycle: {} themed native color callbacks",
        probe.calls.get()
    );
}
fn logical_top(parent: HWND, child: HWND) -> i32 {
    unsafe {
        let mut rect = RECT::default();
        GetWindowRect(child, &mut rect);
        let mut origin = POINT {
            x: rect.left,
            y: rect.top,
        };
        ScreenToClient(parent, &mut origin);
        let scale = GetDpiForWindow(parent).max(96) as i32;
        (origin.y * 96 + scale / 2) / scale
    }
}
fn logical_left(parent: HWND, child: HWND) -> i32 {
    unsafe {
        let mut rect = RECT::default();
        GetWindowRect(child, &mut rect);
        let mut origin = POINT {
            x: rect.left,
            y: rect.top,
        };
        ScreenToClient(parent, &mut origin);
        let scale = GetDpiForWindow(parent).max(96) as i32;
        (origin.x * 96 + scale / 2) / scale
    }
}
fn logical_size(parent: HWND, child: HWND) -> (i32, i32) {
    unsafe {
        let mut rect = RECT::default();
        GetWindowRect(child, &mut rect);
        let scale = GetDpiForWindow(parent).max(96) as i32;
        (
            ((rect.right - rect.left) * 96 + scale / 2) / scale,
            ((rect.bottom - rect.top) * 96 + scale / 2) / scale,
        )
    }
}
struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}

/// Invoked only as an isolated child of native_lifecycle; never an ordinary test.
#[test]
#[ignore]
fn fixture_server() {
    let Some(path) = std::env::var_os("SWITCHMUT_NATIVE_FIXTURE") else {
        return;
    };
    let hwnd = fixture_window();
    unsafe {
        SetTimer(hwnd, 1, 60_000, None);
    }
    let identity = key(hwnd);
    std::fs::write(
        path,
        format!("{} {} {}", identity.hwnd, identity.pid, identity.created),
    )
    .unwrap();
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[test]
#[ignore = "Creates real foreground windows; run explicitly on an interactive Windows desktop"]
fn native_lifecycle() {
    let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts/native");
    std::fs::create_dir_all(&scratch).unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let rendezvous = scratch.join(format!("fixture-{}-{nonce}.txt", std::process::id()));
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "fixture_server", "--nocapture"])
        .env("SWITCHMUT_NATIVE_FIXTURE", &rendezvous)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rendezvous.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let content = std::fs::read_to_string(&rendezvous).expect("native fixture rendezvous");
    let values: Vec<&str> = content.split_whitespace().collect();
    let other = ClientKey {
        hwnd: values[0].parse().unwrap(),
        pid: values[1].parse().unwrap(),
        created: values[2].parse().unwrap(),
    };
    // The child always receives WM_CLOSE, including when an assertion unwinds.
    struct ChildClose(ClientKey);
    impl Drop for ChildClose {
        fn drop(&mut self) {
            unsafe {
                PostMessageW(self.0.hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
    }
    let close = ChildClose(other);
    let target = Window(fixture_window());
    let target_key = key(target.0);
    assert!(is_live(target_key));
    let mut rotation = Rotation::default();
    rotation.reconcile(vec![Client {
        key: target_key,
        class: "fixture".into(),
        title: "fixture".into(),
        allowed: true,
    }]);
    assert_eq!(
        activate(other),
        ActivationResult::Activated,
        "separate process takes foreground"
    );
    assert_eq!(unsafe { GetForegroundWindow() }, other.hwnd as HWND);
    assert_eq!(rotation.next(false), Some(target_key));
    assert_eq!(
        activate(target_key),
        ActivationResult::Activated,
        "sole eligible window activates from background"
    );
    assert_eq!(unsafe { GetForegroundWindow() }, target.0);
    assert_eq!(
        activate(target_key),
        ActivationResult::Activated,
        "sole focused window stays active"
    );
    unsafe {
        ShowWindow(target.0, SW_MINIMIZE);
    }
    pump(Duration::from_millis(50));
    assert_eq!(activate(target_key), ActivationResult::Activated);
    assert_eq!(unsafe { IsIconic(target.0) }, 0);
    let stale = ClientKey {
        created: target_key.created + 1,
        ..target_key
    };
    assert!(!is_live(stale));
    assert_eq!(activate(stale), ActivationResult::Stale);
    // Foreground acknowledgement precedes the desktop restore animation.
    pump(Duration::from_millis(300));
    let dc = unsafe { GetDC(target.0) };
    assert!(!dc.is_null());
    assert_eq!(unsafe { GetPixel(dc, 20, 20) }, 0x00339966);
    unsafe {
        ReleaseDC(target.0, dc);
    }
    let config = Config::default();
    let mut settings_config = config.clone();
    settings_config.bindings.next = Some(9);
    let devices = vec![
        Device {
            id: "first".into(),
            name: "First".into(),
        },
        Device {
            id: "second".into(),
            name: "Second".into(),
        },
    ];
    {
        let settings = Settings::new(
            settings_config,
            vec![
                Client {
                    key: target_key,
                    class: "fixture-class".into(),
                    title: "A long fixture title for the compact client list".into(),
                    allowed: true,
                },
                Client {
                    key: other,
                    class: "fixture-class".into(),
                    title: "Other fixture client".into(),
                    allowed: true,
                },
            ],
            Some(target_key),
            devices.clone(),
        )
        .unwrap();
        let settings_hwnd = settings.hwnd();
        assert_eq!(text(settings_hwnd), "Switchmut");
        let dpi = unsafe { GetDpiForWindow(settings_hwnd).max(96) };
        let mut initial_client = RECT::default();
        unsafe {
            GetClientRect(settings_hwnd, &mut initial_client);
        }
        assert_eq!(
            initial_client.right,
            (290 * dpi as i32 + 95) / 96,
            "default logical client width is 290px"
        );
        assert_eq!(
            initial_client.bottom,
            (420 * dpi as i32 + 95) / 96,
            "default logical client height is content-fit"
        );
        for id in [
            3, 100, 101, 111, 112, 113, 114, 116, 211, 213, 214, 215, 216,
        ] {
            assert!(
                !unsafe { GetDlgItem(settings_hwnd, id) }.is_null(),
                "surviving control {id} exists"
            );
        }
        for id in [1, 2] {
            assert!(
                unsafe { GetDlgItem(settings_hwnd, id) }.is_null(),
                "footer control {id} is not created"
            );
        }
        assert!(
            !unsafe { GetDlgItem(settings_hwnd, 90) }.is_null(),
            "save and assignment status has an accessible native child control"
        );
        assert_eq!(
            unsafe { IsWindowVisible(GetDlgItem(settings_hwnd, 90)) },
            0,
            "empty status stays hidden on the General page"
        );
        for id in [
            4, 102, 110, 117, 118, 119, 120, 121, 212, 217, 310, 311, 312, 313, 314, 315, 316, 317,
            318, 321, 323, 325, 327, 329, 330, 332, 341, 343, 345, 347,
        ] {
            assert!(
                unsafe { GetDlgItem(settings_hwnd, id) }.is_null(),
                "removed control {id} is not created"
            );
        }
        assert!(
            child_texts(settings_hwnd)
                .iter()
                .all(|label| !label.to_ascii_lowercase().contains("magnifier")),
            "settings has no magnifier controls or labels"
        );
        assert_ne!(
            unsafe { GetClassLongPtrW(settings_hwnd, GCLP_HICON) },
            0,
            "Settings class has the shared application icon"
        );
        let style = unsafe { GetWindowLongPtrW(settings_hwnd, GWL_STYLE) as u32 };
        assert_eq!(
            style & WS_CAPTION,
            WS_BORDER,
            "Settings uses its painted title area with only a thin border"
        );
        assert_eq!(style & WS_THICKFRAME, 0, "Settings has no resize frame");
        assert_eq!(style & WS_MAXIMIZEBOX, 0, "Settings cannot be maximized");
        let mut fixed_outer = RECT {
            left: 0,
            top: 0,
            right: (290 * dpi as i32 + 95) / 96,
            bottom: (420 * dpi as i32 + 95) / 96,
        };
        let extended_style = unsafe { GetWindowLongPtrW(settings_hwnd, GWL_EXSTYLE) as u32 };
        unsafe {
            AdjustWindowRectExForDpi(&mut fixed_outer, style, 0, extended_style, dpi);
        }
        let mut actual_outer = RECT::default();
        unsafe {
            GetWindowRect(settings_hwnd, &mut actual_outer);
        }
        assert_eq!(
            actual_outer.right - actual_outer.left,
            fixed_outer.right - fixed_outer.left,
            "fixed window width includes the native frame"
        );
        assert_eq!(
            actual_outer.bottom - actual_outer.top,
            fixed_outer.bottom - fixed_outer.top,
            "fixed window height includes the native frame"
        );
        let client_list = unsafe { GetDlgItem(settings_hwnd, 111) };
        let client_row = list_item_text(client_list, 0);
        assert!(
            client_row.starts_with(&format!("* Allow | PID {} |", target_key.pid)),
            "client row keeps current/allow markers and includes its process ID"
        );
        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 114, 0);
        }
        match settings.events.try_recv().unwrap() {
            SettingsEvent::ClientEdits(clients) => {
                assert!(!clients[0].allowed, "allow change is saved automatically");
            }
            other => panic!("Unexpected allow-change event: {other:?}"),
        }
        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 113, 0);
        }
        match settings.events.try_recv().unwrap() {
            SettingsEvent::ClientEdits(clients) => {
                assert_eq!(
                    clients[0].key, other,
                    "client reorder is saved automatically"
                );
                assert_eq!(clients[1].key, target_key);
            }
            other => panic!("Unexpected reorder event: {other:?}"),
        }
        unsafe {
            SendMessageW(GetDlgItem(settings_hwnd, 116), CB_SETCURSEL, 1, 0);
            SendMessageW(
                settings_hwnd,
                WM_COMMAND,
                116 | ((CBN_SELCHANGE as usize) << 16),
                0,
            );
        }
        match settings.events.try_recv().unwrap() {
            SettingsEvent::Save { config, .. } => {
                assert_eq!(config.refresh_mode, RefreshMode::Periodic);
            }
            other => panic!("Unexpected refresh-mode event: {other:?}"),
        }
        assert!(settings.events.try_recv().is_err());
        assert_eq!(
            unsafe { GetWindowLongPtrW(client_list, GWL_STYLE) as u32 } & WS_HSCROLL,
            0,
            "client selection has no wide horizontal table"
        );
        let minimize = unsafe { GetDlgItem(settings_hwnd, 5) };
        for id in [5, 6] {
            assert_eq!(
                unsafe { GetWindowLongPtrW(GetDlgItem(settings_hwnd, id), GWL_STYLE) as u32 }
                    & WS_TABSTOP,
                0,
                "title button {id} is outside keyboard Tab traversal"
            );
        }
        let mut minimize_rect = RECT::default();
        unsafe {
            GetWindowRect(minimize, &mut minimize_rect);
        }
        let diameter = 24 * dpi as i32 / 96;
        assert_eq!(minimize_rect.right - minimize_rect.left, diameter);
        assert_eq!(minimize_rect.bottom - minimize_rect.top, diameter);
        let close = unsafe { GetDlgItem(settings_hwnd, 6) };
        let mut close_rect = RECT::default();
        unsafe {
            GetWindowRect(close, &mut close_rect);
        }
        assert_eq!(close_rect.right - close_rect.left, diameter);
        assert_eq!(close_rect.bottom - close_rect.top, diameter);
        assert_eq!(logical_left(settings_hwnd, minimize), 233);
        assert_eq!(logical_left(settings_hwnd, close), 261);
        assert_eq!(
            logical_left(settings_hwnd, close) - logical_left(settings_hwnd, minimize) - 24,
            4,
            "title buttons have a 4px gap"
        );
        assert_eq!(
            290 - logical_left(settings_hwnd, close) - 24,
            5,
            "title buttons have a 5px right inset"
        );
        for id in [100, 101] {
            let tab = unsafe { GetDlgItem(settings_hwnd, id) };
            assert_eq!(logical_top(settings_hwnd, tab), 42);
            assert_eq!(logical_size(settings_hwnd, tab), (132, 34));
        }
        let general_tab = unsafe { GetDlgItem(settings_hwnd, 100) };
        let gamepad_tab = unsafe { GetDlgItem(settings_hwnd, 101) };
        assert_eq!(logical_left(settings_hwnd, general_tab), 13);
        assert_eq!(logical_left(settings_hwnd, gamepad_tab), 145);
        assert_eq!(
            logical_left(settings_hwnd, gamepad_tab)
                - logical_left(settings_hwnd, general_tab)
                - logical_size(settings_hwnd, general_tab).0,
            0,
            "General and Gamepad tabs share the Navmut tab grid"
        );
        assert_eq!(
            290 - logical_left(settings_hwnd, gamepad_tab)
                - logical_size(settings_hwnd, gamepad_tab).0,
            13,
            "tabs retain the outer border and padding inset"
        );
        let screen_x = (minimize_rect.left + minimize_rect.right) / 2;
        let screen_y = (minimize_rect.top + minimize_rect.bottom) / 2;
        let hit_point =
            (((screen_y as i16 as u16 as u32) << 16) | (screen_x as i16 as u16 as u32)) as LPARAM;
        assert_eq!(
            unsafe { SendMessageW(settings_hwnd, WM_NCHITTEST, 0, hit_point) },
            HTCLIENT as isize,
            "title buttons remain clickable in the draggable header"
        );
        let mut drag_point = POINT { x: 120, y: 15 };
        unsafe {
            ClientToScreen(settings_hwnd, &mut drag_point);
        }
        let drag_lparam = (((drag_point.y as i16 as u16 as u32) << 16)
            | (drag_point.x as i16 as u16 as u32)) as LPARAM;
        assert_eq!(
            unsafe { SendMessageW(settings_hwnd, WM_NCHITTEST, 0, drag_lparam) },
            HTCAPTION as isize,
            "empty title area remains draggable"
        );
        unsafe {
            UpdateWindow(settings_hwnd);
        }
        let frame_dc = unsafe { GetWindowDC(settings_hwnd) };
        assert!(!frame_dc.is_null());
        assert_eq!(
            unsafe { GetPixel(frame_dc, 0, 0) },
            0x008e712a,
            "custom non-client frame uses the theme border color"
        );
        unsafe {
            ReleaseDC(settings_hwnd, frame_dc);
        }
        unsafe {
            SendMessageW(settings_hwnd, WM_NCACTIVATE, 0, 0);
            UpdateWindow(settings_hwnd);
        }
        let inactive_frame_dc = unsafe { GetWindowDC(settings_hwnd) };
        assert!(!inactive_frame_dc.is_null());
        assert_eq!(
            unsafe { GetPixel(inactive_frame_dc, 0, 0) },
            0x008e712a,
            "inactive non-client frame keeps the theme border color"
        );
        unsafe {
            ReleaseDC(settings_hwnd, inactive_frame_dc);
            SendMessageW(settings_hwnd, WM_NCACTIVATE, 1, 0);
            UpdateWindow(settings_hwnd);
        }
        pump(Duration::from_millis(20));
        let settings_dc = unsafe { GetDC(settings_hwnd) };
        assert!(!settings_dc.is_null());
        assert_eq!(
            unsafe { GetPixel(settings_dc, 210, 15) },
            0x00593b05,
            "native title area paints the Bahamut topbar color"
        );
        assert_eq!(
            unsafe { GetPixel(settings_dc, 280, 90) },
            0x006e4e05,
            "General page paints its blue client section"
        );
        unsafe {
            ReleaseDC(settings_hwnd, settings_dc);
        }
        let refresh_combo = unsafe { GetDlgItem(settings_hwnd, 116) };
        assert_eq!(
            text(refresh_combo),
            "Periodic",
            "Refresh mode keeps its selected value"
        );
        for _ in 0..3 {
            unsafe {
                InvalidateRect(settings_hwnd, null(), 1);
                UpdateWindow(settings_hwnd);
            }
            pump(Duration::from_millis(10));
            assert!(
                logical_region_has_different_pixel(settings_hwnd, [16, 340, 274, 368], 0x006e4e05),
                "Refresh mode combo remains painted after a parent repaint"
            );
            assert!(
                logical_region_has_different_pixel(settings_hwnd, [16, 340, 274, 356], 0x006e4e05),
                "Refresh mode combo's upper field remains painted"
            );
        }
        assert!(
            logical_top(settings_hwnd, unsafe { GetDlgItem(settings_hwnd, 111) }) > 99,
            "General client controls begin below the section rule"
        );
        assert!(
            logical_top(settings_hwnd, unsafe { GetDlgItem(settings_hwnd, 115) }) > 300,
            "General refresh controls begin below the section rule"
        );
        assert_eq!(
            logical_top(settings_hwnd, unsafe { GetDlgItem(settings_hwnd, 113) }),
            239
        );
        assert_eq!(
            logical_top(settings_hwnd, unsafe { GetDlgItem(settings_hwnd, 115) }),
            316
        );
        let gamepad_status = unsafe { GetDlgItem(settings_hwnd, 90) };
        assert_general_status_absent(settings_hwnd);
        assert_eq!(
            logical_left(settings_hwnd, unsafe { GetDlgItem(settings_hwnd, 111) }),
            16,
            "General content is inset inside its section"
        );
        {
            let (tab, x, y, page) = (101, 280, 90, "Gamepad");
            unsafe {
                SendMessageW(GetDlgItem(settings_hwnd, tab), BM_CLICK, 0, 0);
                UpdateWindow(settings_hwnd);
            }
            pump(Duration::from_millis(10));
            let dc = unsafe { GetDC(settings_hwnd) };
            assert_eq!(
                unsafe { GetPixel(dc, x, y) },
                0x006e4e05,
                "{page} page paints its blue section"
            );
            unsafe {
                ReleaseDC(settings_hwnd, dc);
            }
            assert!(
                logical_top(settings_hwnd, unsafe { GetDlgItem(settings_hwnd, 210) }) > 99,
                "Gamepad controller controls begin below the section rule"
            );
            assert!(
                logical_top(settings_hwnd, unsafe { GetDlgItem(settings_hwnd, 213) }) > 199,
                "Gamepad binding controls begin below the section rule"
            );
            assert_eq!(
                logical_left(settings_hwnd, unsafe { GetDlgItem(settings_hwnd, 213) }),
                16,
                "Gamepad content is inset inside its section"
            );
            assert_eq!(
                unsafe { SendMessageW(GetDlgItem(settings_hwnd, 213), LB_GETCOUNT, 0, 0) },
                3,
                "Gamepad keeps the three surviving action bindings"
            );
            assert_eq!(
                logical_top(settings_hwnd, gamepad_status),
                338,
                "Gamepad owns its status inside the Actions panel"
            );
            let status_rect = [16, 338, 274, 356];
            let cancel_button = unsafe { GetDlgItem(settings_hwnd, 216) };
            assert!(
                status_rect[1]
                    >= logical_top(settings_hwnd, cancel_button)
                        + logical_size(settings_hwnd, cancel_button).1,
                "painted status line sits below Assign, Unassign and Cancel"
            );
            assert!(
                status_rect[3] <= 372,
                "status line stays inside the Actions panel"
            );
            assert!(
                !logical_region_has_different_pixel(settings_hwnd, [16, 372, 274, 380], 0x00af7f08),
                "Actions panel ends at logical y=372"
            );
            assert_eq!(
                text(unsafe { GetDlgItem(settings_hwnd, 90) }),
                "",
                "General saves do not create Gamepad feedback"
            );
            for (current, next) in [(213, 214), (214, 215), (215, 216), (216, 100)] {
                unsafe {
                    SetFocus(GetDlgItem(settings_hwnd, current));
                }
                let tab = MSG {
                    hwnd: unsafe { GetDlgItem(settings_hwnd, current) },
                    message: WM_KEYDOWN,
                    wParam: VK_TAB as usize,
                    ..MSG::default()
                };
                assert!(settings.dialog_message(&tab));
                assert_eq!(
                    unsafe { GetFocus() },
                    unsafe { GetDlgItem(settings_hwnd, next) },
                    "Gamepad Tab traversal moves from {current} to {next}"
                );
            }
        }
        unsafe {
            SendMessageW(GetDlgItem(settings_hwnd, 100), BM_CLICK, 0, 0);
        }

        let mut stable_window = RECT::default();
        unsafe {
            GetWindowRect(settings_hwnd, &mut stable_window);
        }
        let dpi_list = unsafe { GetDlgItem(settings_hwnd, 111) };
        for dpi in [120u32, 144, 192] {
            let mut bounds = RECT {
                left: 0,
                top: 0,
                right: (290 * dpi as i32 + 95) / 96,
                bottom: (420 * dpi as i32 + 95) / 96,
            };
            let style = unsafe { GetWindowLongPtrW(settings_hwnd, GWL_STYLE) as u32 };
            let extended_style = unsafe { GetWindowLongPtrW(settings_hwnd, GWL_EXSTYLE) as u32 };
            unsafe {
                AdjustWindowRectExForDpi(&mut bounds, style, 0, extended_style, dpi);
            }
            let suggested = RECT {
                left: stable_window.left,
                top: stable_window.top,
                right: stable_window.left + bounds.right - bounds.left,
                bottom: stable_window.top + bounds.bottom - bounds.top,
            };
            unsafe {
                SendMessageW(
                    settings_hwnd,
                    WM_DPICHANGED,
                    ((dpi as usize) << 16) | dpi as usize,
                    (&suggested as *const RECT) as LPARAM,
                );
            }
            pump(Duration::from_millis(10));
            // A sent DPI message scales controls, but the OS keeps real desktop frame metrics.
            assert_eq!(
                unsafe { SendMessageW(dpi_list, LB_GETITEMHEIGHT, 0, 0) },
                (22 * dpi / 96) as isize,
                "owner-drawn list rows scale at {dpi} DPI"
            );
            let mut list_rect = RECT::default();
            unsafe {
                GetWindowRect(dpi_list, &mut list_rect);
            }
            let mut list_origin = POINT {
                x: list_rect.left,
                y: list_rect.top,
            };
            unsafe {
                ScreenToClient(settings_hwnd, &mut list_origin);
            }
            assert_eq!(
                (list_origin.x, list_origin.y),
                (16 * dpi as i32 / 96, 108 * dpi as i32 / 96),
                "client list placement scales at {dpi} DPI"
            );
            unsafe {
                UpdateWindow(settings_hwnd);
            }
            let dc = unsafe { GetDC(settings_hwnd) };
            let header_bottom = 30 * dpi as i32 / 96;
            let header_inset = 2 * dpi as i32 / 96;
            assert_eq!(
                unsafe { GetPixel(dc, 210 * dpi as i32 / 96, header_bottom - header_inset,) },
                0x00593b05,
                "painted title height scales at {dpi} DPI"
            );
            assert_eq!(
                unsafe { GetPixel(dc, 210 * dpi as i32 / 96, header_bottom + 1) },
                0x00402b03,
                "painted tab area follows the scaled title at {dpi} DPI"
            );
            unsafe {
                ReleaseDC(settings_hwnd, dc);
            }
        }
        let actual_dpi = unsafe { GetDpiForWindow(settings_hwnd).max(96) };
        unsafe {
            SendMessageW(
                settings_hwnd,
                WM_DPICHANGED,
                ((actual_dpi as usize) << 16) | actual_dpi as usize,
                (&stable_window as *const RECT) as LPARAM,
            );
        }
        pump(Duration::from_millis(20));

        let mut fixed_window = RECT::default();
        unsafe {
            GetWindowRect(settings_hwnd, &mut fixed_window);
        }
        let mut expected_fixed = RECT {
            left: 0,
            top: 0,
            right: (290 * actual_dpi as i32 + 95) / 96,
            bottom: (420 * actual_dpi as i32 + 95) / 96,
        };
        let style = unsafe { GetWindowLongPtrW(settings_hwnd, GWL_STYLE) as u32 };
        let extended_style = unsafe { GetWindowLongPtrW(settings_hwnd, GWL_EXSTYLE) as u32 };
        unsafe {
            AdjustWindowRectExForDpi(&mut expected_fixed, style, 0, extended_style, actual_dpi);
        }
        assert_eq!(
            fixed_window.right - fixed_window.left,
            expected_fixed.right - expected_fixed.left,
            "DPI reset keeps the fixed window width"
        );
        assert_eq!(
            fixed_window.bottom - fixed_window.top,
            expected_fixed.bottom - expected_fixed.top,
            "DPI reset keeps the fixed window height"
        );
        unsafe {
            SetFocus(GetDlgItem(settings_hwnd, 100));
            let tab = MSG {
                hwnd: GetDlgItem(settings_hwnd, 100),
                message: WM_KEYDOWN,
                wParam: VK_TAB as usize,
                ..MSG::default()
            };
            assert!(
                settings.dialog_message(&tab),
                "native dialog keyboard handling consumes Tab"
            );
            assert_eq!(
                GetFocus(),
                GetDlgItem(settings_hwnd, 101),
                "Tab advances through the themed controls"
            );
            SetFocus(GetDlgItem(settings_hwnd, 111));
            let general_list_tab = MSG {
                hwnd: GetDlgItem(settings_hwnd, 111),
                message: WM_KEYDOWN,
                wParam: VK_TAB as usize,
                ..MSG::default()
            };
            assert!(
                settings.dialog_message(&general_list_tab),
                "General Tab traversal continues from the client list"
            );
            assert_eq!(
                GetFocus(),
                GetDlgItem(settings_hwnd, 3),
                "General Tab traversal skips removed client details"
            );
            SetFocus(GetDlgItem(settings_hwnd, 116));
            let final_page_tab = MSG {
                hwnd: GetDlgItem(settings_hwnd, 116),
                message: WM_KEYDOWN,
                wParam: VK_TAB as usize,
                ..MSG::default()
            };
            assert!(
                settings.dialog_message(&final_page_tab),
                "native dialog keyboard handling advances past the final page control"
            );
            assert_eq!(
                GetFocus(),
                GetDlgItem(settings_hwnd, 100),
                "General page traversal wraps to the page tabs"
            );
            SendMessageW(GetDlgItem(settings_hwnd, 101), BM_CLICK, 0, 0);
        }
        pump(Duration::from_millis(30));
        unsafe {
            SendMessageW(GetDlgItem(settings_hwnd, 211), CB_SETCURSEL, 2, 0);
            SendMessageW(
                settings_hwnd,
                WM_COMMAND,
                211 | ((CBN_SELCHANGE as usize) << 16),
                0,
            );
        }
        match settings.events.try_recv().unwrap() {
            SettingsEvent::Save { config, .. } => {
                assert_eq!(config.controller.as_deref(), Some("second"));
            }
            other => panic!("Unexpected controller-selection event: {other:?}"),
        }
        assert!(
            settings.events.try_recv().is_err(),
            "controller selection does not reroute live commands"
        );
        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 214, 0);
        }
        assert!(
            matches!(settings.events.try_recv(),Ok(SettingsEvent::CaptureController(Some(id))) if id=="second")
        );
        assert!(settings.capturing());
        unsafe {
            UpdateWindow(settings_hwnd);
        }
        assert_eq!(
            text(unsafe { GetDlgItem(settings_hwnd, 90) }),
            "Press a button to assign it.",
            "Assign exposes one status line"
        );
        settings.update_devices(vec![devices[0].clone()]);
        settings.update_devices(devices.clone());
        unsafe {
            UpdateWindow(settings_hwnd);
        }
        assert!(
            settings.capturing(),
            "device disconnect and reconnect retain capture"
        );
        assert_eq!(
            text(unsafe { GetDlgItem(settings_hwnd, 90) }),
            "Press a button to assign it.",
            "device disconnect and reconnect retain the assignment prompt"
        );
        for (selection, expected) in [(1usize, "first"), (2usize, "second")] {
            unsafe {
                SendMessageW(GetDlgItem(settings_hwnd, 211), CB_SETCURSEL, selection, 0);
                SendMessageW(
                    settings_hwnd,
                    WM_COMMAND,
                    211 | ((CBN_SELCHANGE as usize) << 16),
                    0,
                );
            }
            assert!(matches!(
                settings.events.try_recv(),
                Ok(SettingsEvent::CaptureController(Some(id))) if id == expected
            ));
            match settings.events.try_recv().unwrap() {
                SettingsEvent::Save { config, .. } => {
                    assert_eq!(config.controller.as_deref(), Some(expected));
                }
                other => panic!("Unexpected controller-selection event: {other:?}"),
            }
            assert!(
                settings.capturing(),
                "controller reselection retains capture"
            );
            unsafe {
                UpdateWindow(settings_hwnd);
            }
            assert_eq!(
                text(unsafe { GetDlgItem(settings_hwnd, 90) }),
                "Saving changes...",
                "controller reselection retains pending save feedback"
            );
        }
        unsafe {
            SendMessageW(GetDlgItem(settings_hwnd, 5), BM_CLICK, 0, 0);
        }
        pump(Duration::from_millis(20));
        assert_eq!(unsafe { IsWindowVisible(settings_hwnd) }, 0);
        assert_eq!(unsafe { IsIconic(settings_hwnd) }, 0);
        assert!(!settings.capturing(), "minimize cancels active assignment");
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::EndCapture)
        ));
        assert!(
            settings.events.try_recv().is_err(),
            "minimize emits neither Cancel nor Save"
        );
        settings.show();
        assert_eq!(settings.hwnd(), settings_hwnd, "restore reuses Settings");
        assert_ne!(unsafe { IsWindowVisible(settings_hwnd) }, 0);
        unsafe {
            UpdateWindow(settings_hwnd);
        }
        assert_eq!(
            text(unsafe { GetDlgItem(settings_hwnd, 90) }),
            "",
            "minimize cancellation clears assignment feedback"
        );
        assert_eq!(
            unsafe { IsWindowVisible(GetDlgItem(settings_hwnd, 90)) },
            0,
            "cleared status stays hidden after restore"
        );

        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 214, 0);
        }
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::CaptureController(Some(id))) if id == "second"
        ));
        assert!(settings.capturing());
        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 215, 0);
            UpdateWindow(settings_hwnd);
        }
        assert!(!settings.capturing(), "Unassign ends active assignment");
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::EndCapture)
        ));
        match settings.events.try_recv().unwrap() {
            SettingsEvent::Save { config, .. } => {
                assert_eq!(config.bindings.next, None, "Unassign saves automatically");
            }
            other => panic!("Unexpected Unassign event: {other:?}"),
        }
        assert_eq!(
            text(unsafe { GetDlgItem(settings_hwnd, 90) }),
            "Saving changes...",
            "Unassign replaces assignment feedback with save feedback"
        );

        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 214, 0);
        }
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::CaptureController(Some(id))) if id == "second"
        ));
        assert!(settings.capturing());
        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 216, 0);
        }
        assert!(!settings.capturing(), "assignment Cancel clears capture");
        assert_ne!(
            unsafe { IsWindowVisible(settings_hwnd) },
            0,
            "assignment Cancel keeps the Settings HWND visible"
        );
        assert!(
            !settings.capture_button(10),
            "late input is suppressed after assignment Cancel"
        );
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::EndCapture)
        ));
        assert!(settings.events.try_recv().is_err());
        unsafe {
            UpdateWindow(settings_hwnd);
        }
        assert_eq!(
            text(unsafe { GetDlgItem(settings_hwnd, 90) }),
            "",
            "assignment Cancel clears assignment feedback"
        );
        assert!(settings.events.try_recv().is_err());

        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 214, 0);
        }
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::CaptureController(Some(id))) if id == "second"
        ));
        assert!(settings.capturing());
        unsafe {
            SendMessageW(settings_hwnd, WM_CLOSE, 0, 0);
        }
        assert!(!settings.capturing(), "WM_CLOSE clears active assignment");
        assert!(
            !settings.capture_button(10),
            "late input is suppressed after WM_CLOSE"
        );
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::EndCapture)
        ));
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::Cancel)
        ));
        unsafe {
            UpdateWindow(settings_hwnd);
        }
        assert_eq!(
            text(unsafe { GetDlgItem(settings_hwnd, 90) }),
            "",
            "window close clears assignment feedback"
        );
        assert!(settings.events.try_recv().is_err());

        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 214, 0);
        }
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::CaptureController(Some(id))) if id == "second"
        ));
        assert!(settings.capturing());
        unsafe {
            ShowWindow(settings_hwnd, SW_MINIMIZE);
        }
        assert_eq!(unsafe { IsWindowVisible(settings_hwnd) }, 0);
        assert!(
            !settings.capturing(),
            "native minimize cancels active assignment"
        );
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::EndCapture)
        ));
        assert!(
            settings.events.try_recv().is_err(),
            "native minimize emits neither Cancel nor Save"
        );
        settings.show();
        assert_eq!(
            settings.hwnd(),
            settings_hwnd,
            "native restore reuses Settings"
        );
        assert_ne!(unsafe { IsWindowVisible(settings_hwnd) }, 0);
        unsafe {
            UpdateWindow(settings_hwnd);
        }
        assert_eq!(
            text(unsafe { GetDlgItem(settings_hwnd, 90) }),
            "",
            "native minimize cancellation clears assignment feedback"
        );

        unsafe {
            SendMessageW(settings_hwnd, WM_COMMAND, 214, 0);
        }
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::CaptureController(Some(id))) if id == "second"
        ));
        assert!(settings.capturing());
        assert!(settings.capture_button(10));
        assert!(!settings.capturing());
        match settings.events.try_recv().unwrap() {
            SettingsEvent::Save { config, .. } => {
                assert_eq!(config.bindings.next, Some(10));
                assert_eq!(config.controller.as_deref(), Some("second"));
            }
            other => panic!("Unexpected assignment event: {other:?}"),
        }
        unsafe {
            UpdateWindow(settings_hwnd);
        }
        assert_eq!(
            text(unsafe { GetDlgItem(settings_hwnd, 90) }),
            "Saving button assignment...",
            "assignment result remains visible in the Gamepad status"
        );
        unsafe {
            SetFocus(GetDlgItem(settings_hwnd, 116));
            let enter = MSG {
                hwnd: GetDlgItem(settings_hwnd, 116),
                message: WM_KEYDOWN,
                wParam: VK_RETURN as usize,
                ..MSG::default()
            };
            settings.dialog_message(&enter);
        }
        assert!(
            settings.events.try_recv().is_err(),
            "Enter does not require a footer Save control"
        );
        assert_eq!(
            config.bindings,
            Default::default(),
            "draft did not mutate committed config"
        );
        unsafe {
            SetFocus(GetDlgItem(settings_hwnd, 116));
            let escape = MSG {
                hwnd: GetDlgItem(settings_hwnd, 116),
                message: WM_KEYDOWN,
                wParam: VK_ESCAPE as usize,
                ..MSG::default()
            };
            assert!(
                settings.dialog_message(&escape),
                "Escape invokes Cancel without saving"
            );
        }
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::Cancel)
        ));
        unsafe {
            SendMessageW(GetDlgItem(settings_hwnd, 6), BM_CLICK, 0, 0);
        }
        pump(Duration::from_millis(20));
        assert!(matches!(
            settings.events.try_recv(),
            Ok(SettingsEvent::Cancel)
        ));
    }
    status_lifecycle();
    let process = unsafe { GetCurrentProcess() };
    // Warm up Windows' lazy control/GDI allocations before comparing resource counts.
    for iteration in 0..25 {
        let settings = Settings::new(config.clone(), Vec::new(), None, Vec::new()).unwrap();
        let mut menu = Radial::new(config.menu.clone(), Some(target_key));
        menu.open();
        menu.update_axes(32767, 0);
        pump(Duration::from_millis(25));
        menu.cancel();
        drop(menu);
        drop(settings);
        pump(Duration::from_millis(10));
        let gdi = unsafe { GetGuiResources(process, GR_GDIOBJECTS) };
        let user = unsafe { GetGuiResources(process, GR_USEROBJECTS) };
        let mut handles = 0;
        unsafe {
            GetProcessHandleCount(process, &mut handles);
        }
        if iteration == 2 {
            std::fs::write(
                scratch.join("warm-counts.txt"),
                format!("{gdi} {user} {handles}"),
            )
            .unwrap();
        }
        if iteration == 24 {
            let before = std::fs::read_to_string(scratch.join("warm-counts.txt")).unwrap();
            let counts: Vec<u32> = before
                .split_whitespace()
                .map(|x| x.parse().unwrap())
                .collect();
            assert!(
                gdi <= counts[0] + 2,
                "GDI resources accumulated: {} -> {gdi}",
                counts[0]
            );
            assert!(
                user <= counts[1] + 2,
                "USER resources accumulated: {} -> {user}",
                counts[1]
            );
            assert!(
                handles <= counts[2] + 2,
                "process handles accumulated: {} -> {handles}",
                counts[2]
            );
            println!(
                "native lifecycle: GDI {} -> {gdi}, USER {} -> {user}, handles {} -> {handles}; 25 settings/radial cycles",
                counts[0], counts[1], counts[2]
            );
        }
    }
    drop(close);
    assert!(child.wait().unwrap().success());
    std::fs::write(scratch.join("native-result.txt"),"PASS: separate-process foreground, sole client, minimized restore, stale identity, GDI source pixels, 25 overlay/settings lifecycle cycles\n").unwrap();
}
