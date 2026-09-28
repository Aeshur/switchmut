//! Launch a packaged application with isolated settings and close it normally.
mod support;
use std::{
    ffi::OsStr,
    mem::size_of,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use switchmut::{
    config::{self, Config},
    platform::wide,
    ui::tray::TRAY_MESSAGE,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM},
    UI::Input::KeyboardAndMouse::VK_RETURN,
    UI::{
        Shell::{ExtractIconExW, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW},
        WindowsAndMessaging::*,
    },
};

struct ExtractedIcons {
    large: HICON,
    small: HICON,
}
impl Drop for ExtractedIcons {
    fn drop(&mut self) {
        unsafe {
            if !self.large.is_null() {
                DestroyIcon(self.large);
            }
            if !self.small.is_null() {
                DestroyIcon(self.small);
            }
        }
    }
}

fn assert_embedded_icons(executable: &OsStr) {
    let path = wide(&executable.to_string_lossy());
    let mut icons = ExtractedIcons {
        large: null_mut(),
        small: null_mut(),
    };
    let count = unsafe { ExtractIconExW(path.as_ptr(), 0, &mut icons.large, &mut icons.small, 1) };
    assert!(count > 0, "Packaged executable has no extractable icon");
    assert!(
        !icons.large.is_null(),
        "Packaged executable has no large icon"
    );
    assert!(
        !icons.small.is_null(),
        "Packaged executable has no small icon"
    );
}

fn wait_until(mut test: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if test() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}
fn owner(pid: u32) -> Option<HWND> {
    unsafe {
        let hwnd = FindWindowW(wide("Switchmut.Owner").as_ptr(), null());
        let mut found = 0;
        if !hwnd.is_null() {
            GetWindowThreadProcessId(hwnd, &mut found);
        }
        (found == pid).then_some(hwnd)
    }
}
fn switchmut_owner_exists() -> bool {
    unsafe { !FindWindowW(wide("Switchmut.Owner").as_ptr(), null()).is_null() }
}
fn popup(pid: u32) -> Option<HWND> {
    unsafe {
        let hwnd = FindWindowW(wide("#32768").as_ptr(), null());
        let mut found = 0;
        if !hwnd.is_null() {
            GetWindowThreadProcessId(hwnd, &mut found);
        }
        (found == pid).then_some(hwnd)
    }
}
fn callback(hwnd: HWND, notification: u32, sent: bool) {
    unsafe {
        if sent {
            SendMessageW(hwnd, TRAY_MESSAGE, 1, notification as LPARAM);
        } else {
            PostMessageW(hwnd, TRAY_MESSAGE, 1, notification as LPARAM);
        }
    }
}
fn remove_tray_icon(hwnd: HWND) -> bool {
    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = 1;
        Shell_NotifyIconW(NIM_DELETE, &data) != 0
    }
}
fn log_count(path: &std::path::Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .matches(needle)
        .count()
}
fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    for entry in std::fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else {
            std::fs::copy(&source_path, &destination_path).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
fn main() -> Result<(), String> {
    let executable = std::env::args_os()
        .nth(1)
        .ok_or("Pass the extracted Switchmut.exe path")?;
    assert_embedded_icons(&executable);
    if switchmut_owner_exists() {
        return Err("Close the running Switchmut instance before release_smoke".into());
    }
    let output_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-artifacts/release-smoke")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&output_directory).map_err(|error| error.to_string())?;
    let executable = std::fs::canonicalize(&executable).map_err(|error| error.to_string())?;
    let application_directory = executable
        .parent()
        .ok_or("Could not resolve the extracted executable directory")?;
    let path = application_directory.join("config-switchmut.json");
    let log_path = application_directory.join("log-switchmut.log");
    for entry in std::fs::read_dir(application_directory).map_err(|error| error.to_string())? {
        let name = entry.map_err(|error| error.to_string())?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("config-switchmut.json") || name.starts_with("log-switchmut") {
            return Err("Use a freshly extracted package; smoke checks must not overwrite existing settings or logs".into());
        }
    }
    let configuration = Config::default();
    config::save(&path, &configuration)?;
    let mut child = std::process::Command::new(&executable)
        .spawn()
        .map_err(|e| e.to_string())?;
    let pid = child.id();
    if !wait_until(|| owner(pid).is_some()) {
        return Err("Packaged app did not create its native owner window".into());
    }
    let hwnd = owner(pid).unwrap();
    struct Close(HWND, u32);
    impl Drop for Close {
        fn drop(&mut self) {
            unsafe {
                let mut pid = 0;
                GetWindowThreadProcessId(self.0, &mut pid);
                if pid == self.1 {
                    PostMessageW(self.0, WM_CLOSE, 0, 0);
                }
            }
        }
    }
    let _close = Close(hwnd, pid);
    if !wait_until(|| log_count(&log_path, "message loop running") != 0) {
        return Err("Packaged app did not finish tray initialization".into());
    }
    let mut duplicate = std::process::Command::new(&executable)
        .spawn()
        .map_err(|e| e.to_string())?;
    if !wait_until(|| duplicate.try_wait().ok().flatten().is_some()) {
        return Err("Duplicate instance did not exit".into());
    }
    assert!(owner(pid).is_some());
    for iteration in 0..8 {
        callback(hwnd, WM_LBUTTONDBLCLK, false);
        let mut settings = std::ptr::null_mut();
        if !wait_until(|| {
            settings = unsafe { FindWindowW(wide("Switchmut.Settings").as_ptr(), null()) };
            !settings.is_null()
        }) {
            return Err("Settings did not reopen".into());
        }
        // These are the actual General/Gamepad controls, not a mock.
        for tab in [101usize, 100] {
            unsafe {
                SendMessageW(settings, WM_COMMAND, tab, 0);
            }
            if iteration == 0 {
                std::thread::sleep(Duration::from_millis(150));
                support::capture(
                    settings,
                    &output_directory.join(format!("settings-{tab}.bmp")),
                )?;
            }
        }
        if iteration == 0 {
            assert_ne!(
                unsafe { GetClassLongPtrW(settings, GCLP_HICON) },
                0,
                "Settings class has the packaged application icon"
            );
            unsafe {
                SendMessageW(settings, WM_SYSCOMMAND, SC_MINIMIZE as usize, 0);
            }
            if !wait_until(|| unsafe { IsWindowVisible(settings) == 0 }) {
                return Err("Minimizing Settings did not hide it from the taskbar".into());
            }
            callback(hwnd, WM_LBUTTONDBLCLK, true);
            if !wait_until(|| unsafe { IsWindowVisible(settings) != 0 }) {
                return Err("Opening Settings from the tray did not restore its window".into());
            }
            assert_eq!(
                unsafe { FindWindowW(wide("Switchmut.Settings").as_ptr(), null()) },
                settings,
                "restore reuses the minimized Settings window"
            );
            let saves = log_count(&log_path, "settings applied and saved");
            unsafe {
                SendMessageW(GetDlgItem(settings, 116), CB_SETCURSEL, 1, 0);
                SendMessageW(
                    settings,
                    WM_COMMAND,
                    116 | ((CBN_SELCHANGE as usize) << 16),
                    0,
                );
            }
            if !wait_until(|| log_count(&log_path, "settings applied and saved") > saves) {
                return Err("Changing Settings did not persist the draft".into());
            }
            if !unsafe { IsWindowVisible(settings) != 0 } {
                return Err("Changing Settings unexpectedly returned to the tray".into());
            }
            assert_eq!(unsafe { IsWindowVisible(GetDlgItem(settings, 90)) }, 0);
            support::capture(
                settings,
                &output_directory.join("settings-general-saved.bmp"),
            )?;
            unsafe {
                SendMessageW(settings, WM_COMMAND, 101, 0);
                SendMessageW(settings, WM_COMMAND, 214, 0);
            }
            if !wait_until(|| unsafe { IsWindowVisible(GetDlgItem(settings, 90)) != 0 }) {
                return Err("Gamepad assignment feedback did not appear".into());
            }
            std::thread::sleep(Duration::from_millis(150));
            support::capture(
                settings,
                &output_directory.join("settings-gamepad-status.bmp"),
            )?;
            unsafe {
                SendMessageW(settings, WM_COMMAND, 100, 0);
            }
            assert_eq!(unsafe { IsWindowVisible(GetDlgItem(settings, 90)) }, 0);
            std::thread::sleep(Duration::from_millis(150));
            support::capture(
                settings,
                &output_directory.join("settings-general-after-gamepad.bmp"),
            )?;
        }
        unsafe {
            PostMessageW(settings, WM_CLOSE, 0, 0);
        }
        if !wait_until(|| unsafe { IsWindow(settings) == 0 }) {
            return Err("Closing Settings did not leave a responsive tray owner".into());
        }
        assert!(owner(pid).is_some());
    }

    let refreshes = log_count(&log_path, "client refresh:");
    callback(hwnd, WM_RBUTTONUP, false);
    let mut menu = std::ptr::null_mut();
    if !wait_until(|| {
        menu = popup(pid).unwrap_or(null_mut());
        !menu.is_null()
    }) {
        return Err("Posted tray right-click did not open the native context menu".into());
    }
    unsafe {
        SendMessageW(menu, WM_CHAR, 'r' as usize, 0);
        SendMessageW(menu, WM_KEYDOWN, VK_RETURN as usize, 0);
    }
    if !wait_until(|| log_count(&log_path, "client refresh:") > refreshes) {
        return Err("Selecting Refresh from the tray context menu did not run".into());
    }

    callback(hwnd, WM_RBUTTONUP, true);
    if !wait_until(|| {
        menu = popup(pid).unwrap_or(null_mut());
        !menu.is_null()
    }) {
        return Err("Sent tray right-click did not open a menu for cancellation".into());
    }
    unsafe {
        SendMessageW(hwnd, WM_CANCELMODE, 0, 0);
    }
    if !wait_until(|| popup(pid).is_none()) {
        return Err("WM_CANCELMODE did not dismiss the active tray menu".into());
    }

    let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    if taskbar_created == 0 {
        return Err("Could not register TaskbarCreated in the smoke harness".into());
    }
    let restores = log_count(&log_path, "TaskbarCreated received; tray icon restored");
    if !remove_tray_icon(hwnd) {
        return Err("Could not remove the test icon before simulating TaskbarCreated".into());
    }
    unsafe {
        SendMessageW(hwnd, taskbar_created, 0, 0);
    }
    if !wait_until(|| {
        log_count(&log_path, "TaskbarCreated received; tray icon restored") > restores
    }) {
        return Err("Sent TaskbarCreated did not restore the tray icon".into());
    }
    if !remove_tray_icon(hwnd) {
        return Err("Could not remove the test icon before posted TaskbarCreated".into());
    }
    unsafe {
        PostMessageW(hwnd, taskbar_created, 0, 0);
    }
    if !wait_until(|| {
        log_count(&log_path, "TaskbarCreated received; tray icon restored") > restores + 1
    }) {
        return Err("Posted TaskbarCreated did not restore the tray icon".into());
    }

    callback(hwnd, WM_RBUTTONUP, true);
    if !wait_until(|| {
        menu = popup(pid).unwrap_or(null_mut());
        !menu.is_null()
    }) {
        return Err("Sent tray right-click did not open the native context menu".into());
    }
    let exit_selections = log_count(&log_path, "tray command selected: Exit");
    // This is the path under test; use the menu's native accelerator rather
    // than synthesizing WM_COMMAND or WM_CLOSE to pass it.
    unsafe {
        SendMessageW(menu, WM_CHAR, 'x' as usize, 0);
        SendMessageW(menu, WM_KEYDOWN, VK_RETURN as usize, 0);
    }
    if !wait_until(|| log_count(&log_path, "tray command selected: Exit") > exit_selections) {
        return Err("The native tray menu did not select Exit".into());
    }
    if !wait_until(|| child.try_wait().ok().flatten().is_some()) {
        return Err("Selecting Exit from the tray context menu did not stop the app".into());
    }
    let loaded = config::load(&path)?;
    assert_eq!(loaded.config.bindings, Default::default());
    let log = std::fs::read_to_string(&log_path).map_err(|e| e.to_string())?;
    if !log.contains("shutdown:") {
        return Err("Packaged app did not record clean worker shutdown".into());
    }

    let mut retained = configuration.clone();
    retained.refresh_interval_ms = 2345;
    config::save(&path, &retained)?;
    // A backup is the previous valid file, so save the retained settings once
    // more before corrupting the primary file used for recovery.
    config::save(&path, &retained)?;
    let backup_path = application_directory.join("config-switchmut.json.bak");
    assert!(
        backup_path.exists(),
        "Config backup is beside the executable"
    );
    let malformed = b"{ not valid JSON";
    std::fs::write(&path, malformed).map_err(|error| error.to_string())?;
    let moved_directory = output_directory.join("moved-app");
    copy_tree(application_directory, &moved_directory)?;
    let moved_executable = moved_directory.join("Switchmut.exe");
    assert!(
        !moved_directory.join("data").exists(),
        "Moved app has no data directory"
    );
    let moved_path = moved_directory.join("config-switchmut.json");
    let moved_backup_path = moved_directory.join("config-switchmut.json.bak");
    let moved_log_path = moved_directory.join("log-switchmut.log");
    assert!(moved_path.exists(), "Config is beside the moved executable");
    assert!(
        moved_backup_path.exists(),
        "Config backup is beside the moved executable"
    );
    let mut moved_child = std::process::Command::new(&moved_executable)
        .current_dir(&output_directory)
        .spawn()
        .map_err(|e| e.to_string())?;
    let moved_pid = moved_child.id();
    if !wait_until(|| owner(moved_pid).is_some()) {
        return Err("Moved packaged app did not create its native owner window".into());
    }
    let _close_moved = Close(owner(moved_pid).unwrap(), moved_pid);
    if !wait_until(|| log_count(&moved_log_path, "loaded the backup") != 0) {
        return Err(
            "Moved packaged app did not recover its malformed config from the backup".into(),
        );
    }
    let moved_loaded = config::load(&moved_path)?;
    assert_eq!(moved_loaded.config.refresh_interval_ms, 2345);
    assert_eq!(
        std::fs::read(&moved_path).map_err(|error| error.to_string())?,
        malformed
    );
    unsafe {
        PostMessageW(owner(moved_pid).unwrap(), WM_CLOSE, 0, 0);
    }
    if !wait_until(|| moved_child.try_wait().ok().flatten().is_some()) {
        return Err("Moved packaged app did not shut down normally".into());
    }
    std::fs::write(&moved_log_path, vec![b'x'; 1_048_576]).map_err(|error| error.to_string())?;
    let mut rotated_child = std::process::Command::new(&moved_executable)
        .current_dir(&output_directory)
        .spawn()
        .map_err(|e| e.to_string())?;
    let rotated_pid = rotated_child.id();
    if !wait_until(|| owner(rotated_pid).is_some()) {
        return Err("Moved app did not restart after log rotation setup".into());
    }
    let _close_rotated = Close(owner(rotated_pid).unwrap(), rotated_pid);
    if !wait_until(|| moved_directory.join("log-switchmut.previous.log").exists()) {
        return Err("Portable log did not rotate beside the executable".into());
    }
    unsafe {
        PostMessageW(owner(rotated_pid).unwrap(), WM_CLOSE, 0, 0);
    }
    if !wait_until(|| rotated_child.try_wait().ok().flatten().is_some()) {
        return Err("Rotated moved app did not shut down normally".into());
    }

    let shutdowns = log_count(&log_path, "shutdown: controller worker stopped");
    let mut close_child = std::process::Command::new(&executable)
        .spawn()
        .map_err(|e| e.to_string())?;
    let close_pid = close_child.id();
    if !wait_until(|| owner(close_pid).is_some()) {
        return Err("Second packaged app did not create its native owner window".into());
    }
    let close_hwnd = owner(close_pid).unwrap();
    let _close_active_menu = Close(close_hwnd, close_pid);
    if !wait_until(|| log_count(&log_path, "message loop running") > 1) {
        return Err("Second packaged app did not finish tray initialization".into());
    }
    callback(close_hwnd, WM_RBUTTONUP, true);
    if !wait_until(|| popup(close_pid).is_some()) {
        return Err("Second packaged app did not open its tray context menu".into());
    }
    unsafe {
        SendMessageW(close_hwnd, WM_CLOSE, 0, 0);
    }
    if !wait_until(|| close_child.try_wait().ok().flatten().is_some()) {
        return Err("WM_CLOSE did not exit the app while its tray menu was active".into());
    }
    if log_count(&log_path, "shutdown: controller worker stopped") <= shutdowns {
        return Err("WM_CLOSE did not record clean worker shutdown".into());
    }
    let log = std::fs::read_to_string(&log_path).map_err(|e| e.to_string())?;
    println!(
        "PASS: extracted native executable, duplicate-instance guard, settings class icon, sent and posted tray callbacks, context-menu Refresh and Exit, TaskbarCreated restoration, WM_CANCELMODE and WM_CLOSE menu dismissal, minimize/restore, 8 settings open/tab/close cycles, normal exit, configuration reload, moved-folder backup recovery and log rotation\n{log}"
    );
    Ok(())
}
