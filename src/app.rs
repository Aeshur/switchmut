use crate::{
    actions::Action,
    clients::{Client, ClientKey, Discovery},
    config::{self, Config, RefreshMode},
    diagnostics::Log,
    input::{ControllerState, Device, InputEvent, InputWorker},
    platform,
    radial::Radial,
    switching::Rotation,
    ui::{
        self,
        settings::{Settings, SettingsEvent},
        tray::{Tray, TrayCallback, TrayCommand, decode_callback},
    },
};
use std::{
    collections::VecDeque,
    mem::size_of,
    path::PathBuf,
    ptr::{null, null_mut},
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
        UI::{
            Controls::{ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx},
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

const SHOW_SETTINGS: u32 = WM_APP + 2;
const OWNER_DISPATCH: u32 = WM_APP + 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnerEvent {
    Tick,
    Close,
    ShowSettings,
    RestoreTray,
    Tray(TrayCallback),
}

fn should_end_popup(popup_active: bool, popup_ending: bool, message: u32) -> bool {
    popup_active && !popup_ending && matches!(message, WM_CLOSE | WM_CANCELMODE)
}

struct OwnerState {
    restart_message: u32,
    events: VecDeque<OwnerEvent>,
    wake_pending: bool,
    popup_active: bool,
    // EndMenu may re-enter the owner procedure with WM_CANCELMODE.
    popup_ending: bool,
    closing: bool,
}
impl OwnerState {
    fn new(restart_message: u32) -> Self {
        Self {
            restart_message,
            events: VecDeque::new(),
            wake_pending: false,
            popup_active: false,
            popup_ending: false,
            closing: false,
        }
    }

    fn enqueue(&mut self, event: OwnerEvent) -> bool {
        if self.closing && event != OwnerEvent::Close {
            return false;
        }
        if event == OwnerEvent::Close {
            self.closing = true;
            self.events.clear();
        }
        if self.events.contains(&event) {
            return false;
        }
        self.events.push_back(event);
        if self.wake_pending {
            false
        } else {
            self.wake_pending = true;
            true
        }
    }

    fn request_popup_end(&mut self, message: u32) -> bool {
        let end = should_end_popup(self.popup_active, self.popup_ending, message);
        self.popup_ending |= end;
        end
    }

    fn may_dispatch_menu_command(&self) -> bool {
        !self.closing
    }
}

struct Mutex(HANDLE);
impl Drop for Mutex {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Owner {
    hwnd: HWND,
    // WndProc retains this pointer until DestroyWindow returns in Owner::drop.
    state: Box<OwnerState>,
}
impl Owner {
    fn new(restart_message: u32) -> Result<Self, String> {
        let mut state = Box::new(OwnerState::new(restart_message));
        let hwnd = unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(owner_proc),
                hInstance: GetModuleHandleW(null()),
                lpszClassName: w!("Switchmut.Owner"),
                ..std::mem::zeroed()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.lpszClassName,
                w!("Switchmut"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                class.hInstance,
                state.as_mut() as *mut OwnerState as *const _,
            )
        };
        if hwnd.is_null() {
            return Err("Could not create the application message window".into());
        }
        Ok(Self { hwnd, state })
    }

    fn next_event(&mut self) -> Option<OwnerEvent> {
        let event = self.state.events.pop_front();
        if self.state.events.is_empty() {
            self.state.wake_pending = false;
        }
        event
    }

    fn set_popup_active(&mut self, active: bool) {
        self.state.popup_active = active;
        self.state.popup_ending = false;
    }

    fn begin_shutdown(&mut self) {
        self.state.closing = true;
        self.state.events.clear();
        self.state.wake_pending = false;
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        unsafe {
            KillTimer(self.hwnd, 1);
            DestroyWindow(self.hwnd);
        }
    }
}

struct App {
    config: Config,
    path: PathBuf,
    log: Log,
    discovery: Discovery,
    rotation: Rotation,
    input: InputWorker,
    input_events: Receiver<InputEvent>,
    activation: platform::ActivationWorker,
    devices: Vec<Device>,
    axes: ControllerState,
    settings: Option<Settings>,
    radial: Option<Radial>,
    last_refresh: Instant,
    last_source_check: Instant,
    menu_origin: Option<ClientKey>,
    exit: bool,
    suppress_until_release: u32,
    awaiting_selection: Option<u64>,
    pending_activation: Option<(ClientKey, Option<bool>, Instant)>,
}

pub fn run() -> Result<(), String> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        });
    }
    let mutex = unsafe { CreateMutexW(null(), 0, w!("Local\\Switchmut")) };
    if mutex.is_null() {
        return Err(format!("Could not create instance guard: {}", unsafe {
            GetLastError()
        }));
    }
    let existing = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let _mutex = Mutex(mutex);
    if existing {
        unsafe {
            let window = FindWindowW(w!("Switchmut.Owner"), null());
            if !window.is_null() {
                PostMessageW(window, SHOW_SETTINGS, 0, 0);
            }
        }
        return Ok(());
    }
    let previous_mutex = unsafe { CreateMutexW(null(), 0, w!("Local\\Switchmut.Rust.3")) };
    if previous_mutex.is_null() {
        return Err(format!(
            "Could not inspect the previous Switchmut instance: {}",
            unsafe { GetLastError() }
        ));
    }
    let previous_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let _previous_mutex = Mutex(previous_mutex);
    if previous_exists {
        unsafe {
            let window = FindWindowW(w!("Switchmut.Owner"), null());
            if !window.is_null() {
                PostMessageW(window, WM_CLOSE, 0, 0);
            }
        }
        return Err(
            "A previous Switchmut instance is still running; its normal shutdown was requested. Retry after it exits.".into(),
        );
    }
    let path = config::config_path()?;
    let directory = path.parent().ok_or("Configuration directory is missing")?;
    let log = Log::new(directory)?;
    log.write(format!(
        "startup version={} architecture=x86_64",
        env!("CARGO_PKG_VERSION")
    ));
    let loaded = config::load(&path)
        .inspect_err(|error| log.write(format!("startup configuration failed: {error}")))?;
    for warning in &loaded.warnings {
        log.write(format!("configuration: {warning}"));
    }
    let config = loaded.config;
    let restart_message = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if restart_message == 0 {
        return Err(format!("Could not register TaskbarCreated: {}", unsafe {
            GetLastError()
        }));
    }
    let mut owner = Owner::new(restart_message)?;
    let tray = Tray::new(owner.hwnd)?;
    let (input, input_events) = InputWorker::start()?;
    let activation = platform::ActivationWorker::start()?;
    let initial_selection = input.select(config.controller.clone());
    let mut app = App {
        config,
        path,
        log,
        discovery: Discovery::default(),
        rotation: Rotation::default(),
        input,
        input_events,
        activation,
        devices: Vec::new(),
        axes: ControllerState {
            buttons: 0,
            x: 32767,
            y: 32767,
        },
        settings: None,
        radial: None,
        last_refresh: Instant::now(),
        last_source_check: Instant::now(),
        menu_origin: None,
        exit: false,
        suppress_until_release: 0,
        awaiting_selection: Some(initial_selection),
        pending_activation: None,
    };
    app.refresh();
    app.open_settings();
    if !loaded.warnings.is_empty()
        && let Some(settings) = &app.settings
    {
        settings.status(&format!(
            "Configuration migration/recovery: {}.",
            loaded.warnings.join("; ")
        ));
    }
    unsafe {
        if SetTimer(owner.hwnd, 1, 10, None) == 0 {
            return Err("Could not start the application dispatch timer".into());
        }
    }
    app.log
        .write("message loop running; controller polling independent of settings window");
    unsafe {
        let mut message: MSG = std::mem::zeroed();
        while !app.exit {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result == 0 {
                break;
            }
            if result < 0 {
                return Err(format!("Message loop failed: {}", GetLastError()));
            }
            let consumed = app
                .settings
                .as_ref()
                .is_some_and(|s| s.dialog_message(&message));
            if !consumed {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            while let Some(event) = owner.next_event() {
                match event {
                    OwnerEvent::Tick => app.tick(),
                    OwnerEvent::Close => app.exit = true,
                    OwnerEvent::ShowSettings => app.open_settings(),
                    OwnerEvent::RestoreTray => match tray.restore() {
                        Ok(()) => app.log.write("TaskbarCreated received; tray icon restored"),
                        Err(error) => app.log.write(format!(
                            "TaskbarCreated received; tray icon restore failed (Win32 {error})"
                        )),
                    },
                    OwnerEvent::Tray(TrayCallback::OpenSettings) => app.open_settings(),
                    OwnerEvent::Tray(TrayCallback::OpenContextMenu) => {
                        owner.set_popup_active(true);
                        let command = tray.popup();
                        owner.set_popup_active(false);
                        // WM_CLOSE can arrive in TrackPopupMenu's nested loop; it wins over
                        // the menu result, even when that result was already selected.
                        if let Some(command) =
                            command.filter(|_| owner.state.may_dispatch_menu_command())
                        {
                            app.tray_command(command);
                        }
                    }
                }
                if app.exit {
                    owner.begin_shutdown();
                    break;
                }
            }
            if app.exit {
                break;
            }
            app.drain_settings();
        }
    }
    app.cancel_menu();
    app.settings = None;
    app.input.stop();
    app.log
        .write("shutdown: controller worker stopped; owned overlays and tray released");
    drop(app);
    drop(tray);
    drop(owner);
    Ok(())
}

unsafe extern "system" fn owner_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lp as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = unsafe { (*create).lpCreateParams as *mut OwnerState };
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            }
        }
        return unsafe { DefWindowProcW(hwnd, message, wp, lp) };
    }
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OwnerState };
    if state.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wp, lp) };
    }
    if message == OWNER_DISPATCH {
        unsafe {
            (*state).wake_pending = false;
        }
        return 0;
    }
    if message == WM_CANCELMODE {
        let (popup_active, end_popup) = {
            let owner_state = unsafe { &mut *state };
            (
                owner_state.popup_active,
                owner_state.request_popup_end(message),
            )
        };
        if popup_active {
            if end_popup {
                unsafe {
                    EndMenu();
                }
            }
            return 0;
        }
    }
    let event = if message == WM_TIMER {
        Some(OwnerEvent::Tick)
    } else if message == WM_CLOSE {
        Some(OwnerEvent::Close)
    } else if message == SHOW_SETTINGS {
        Some(OwnerEvent::ShowSettings)
    } else if message == unsafe { (*state).restart_message } {
        Some(OwnerEvent::RestoreTray)
    } else if message == crate::ui::tray::TRAY_MESSAGE {
        let callback = decode_callback(lp);
        if let Some(TrayCallback::OpenContextMenu) = callback {
            let owner_state = unsafe { &mut *state };
            if owner_state.popup_active
                || owner_state
                    .events
                    .contains(&OwnerEvent::Tray(TrayCallback::OpenContextMenu))
            {
                return 0;
            }
        }
        callback.map(OwnerEvent::Tray)
    } else {
        None
    };
    if let Some(event) = event {
        // Only this UI-thread procedure mutates routing state. The wake message
        // lets the outer loop drain it without borrowing App inside nested menus.
        let (wake, end_popup) = {
            let owner_state = unsafe { &mut *state };
            (
                owner_state.enqueue(event),
                event == OwnerEvent::Close && owner_state.request_popup_end(message),
            )
        };
        if wake {
            unsafe {
                PostMessageW(hwnd, OWNER_DISPATCH, 0, 0);
            }
        }
        if end_popup {
            unsafe {
                EndMenu();
            }
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}

impl App {
    fn refresh(&mut self) {
        self.refresh_clients(false);
    }
    fn refresh_clients(&mut self, for_switch: bool) {
        let previous = self.rotation.current();
        let clients = self.discovery.refresh();
        self.log.write(format!(
            "client refresh: {} supported windows",
            clients.len()
        ));
        if for_switch {
            self.rotation.reconcile_for_switch(clients);
        } else {
            self.rotation.reconcile(clients);
            if previous != self.rotation.current()
                && let Some(key) = self.rotation.current()
            {
                self.request_activation(key, None);
            }
        }
        self.last_refresh = Instant::now();
        if let Some(s) = &self.settings {
            s.update_clients(self.rotation.clients(), self.rotation.current());
        }
    }
    fn open_settings(&mut self) {
        if let Some(s) = &self.settings {
            s.show();
            return;
        }
        match Settings::new(
            self.config.clone(),
            self.rotation.clients().to_vec(),
            self.rotation.current(),
            self.devices.clone(),
        ) {
            Ok(settings) => self.settings = Some(settings),
            Err(e) => {
                self.log.write(&e);
                ui::message(null_mut(), &e);
            }
        }
    }
    fn tick(&mut self) {
        self.drain_activation();
        if let Some(menu) = self.radial.as_mut() {
            for diagnostic in menu.take_diagnostics() {
                self.log.write(format!("menu: {diagnostic}"));
            }
        }
        for _ in 0..256 {
            let Ok(event) = self.input_events.try_recv() else {
                break;
            };
            self.input_event(event);
        }
        if self.config.refresh_mode == RefreshMode::Periodic
            && self.last_refresh.elapsed()
                >= Duration::from_millis(config::PERIODIC_REFRESH_INTERVAL_MS as u64)
        {
            self.refresh();
        }
        if self.last_source_check.elapsed() >= Duration::from_millis(100) {
            self.last_source_check = Instant::now();
            if self
                .menu_origin
                .is_some_and(|key| !crate::clients::is_live(key))
            {
                self.cancel_menu();
                self.log.write("menu cancelled: source client exited");
            }
        }
    }
    fn input_event(&mut self, event: InputEvent) {
        match event {
            InputEvent::Devices(devices) => {
                self.log.write(format!("controller discovery: {devices:?}"));
                self.devices = devices;
                if let Some(s) = &self.settings {
                    s.update_devices(self.devices.clone());
                }
            }
            InputEvent::Selected(id) => {
                self.log.write(format!("controller selected: {id:?}"));
                self.cancel_menu();
                self.suppress_until_release = 0;
            }
            InputEvent::SelectionApplied(token) => {
                if self.awaiting_selection == Some(token) {
                    self.awaiting_selection = None;
                }
            }
            InputEvent::Connected(connected) => {
                self.log.write(format!("controller connected={connected}"));
                if !connected {
                    self.cancel_menu();
                    self.suppress_until_release = 0;
                }
            }
            InputEvent::Diagnostic(text) => self.log.write(text),
            InputEvent::State(state) => {
                self.axes = state;
                if let Some(menu) = self.radial.as_mut() {
                    menu.update_axes(self.axes.x, self.axes.y);
                }
            }
            InputEvent::Button(edge) => {
                if edge.button >= 32 || self.awaiting_selection.is_some() {
                    return;
                }
                let mask = 1u32 << edge.button;
                if !edge.pressed {
                    self.suppress_until_release &= !mask;
                }
                if edge.pressed && self.settings.as_ref().is_some_and(|s| s.capturing()) {
                    self.cancel_menu();
                    if let Some(s) = &self.settings {
                        s.capture_button(edge.button);
                    }
                    self.awaiting_selection =
                        Some(self.input.select(self.config.controller.clone()));
                    self.suppress_until_release |= mask;
                    self.log.write(format!(
                        "assignment captured button={}; normal command suppressed",
                        edge.button
                    ));
                    return;
                }
                if self.settings.as_ref().is_some_and(|s| s.capturing())
                    || (self.suppress_until_release & mask) != 0
                {
                    return;
                }
                match crate::actions::route_button(
                    &self.config.bindings,
                    edge.button,
                    edge.pressed,
                    false,
                ) {
                    crate::actions::Routing::MenuRelease => self.release_menu(),
                    crate::actions::Routing::Command(action) => self.execute(action),
                    _ => {}
                }
            }
        }
    }
    fn execute(&mut self, action: Action) {
        self.log.write(format!("command {}", action.label()));
        match action {
            Action::Next | Action::Previous => {
                if self.config.refresh_mode == RefreshMode::OnSwitch
                    || self.rotation.clients().is_empty()
                {
                    self.refresh_clients(true);
                }
                if let Some(key) = self.rotation.next(action == Action::Previous) {
                    self.request_activation(key, Some(action == Action::Previous));
                } else {
                    self.log
                        .write("no eligible client: no activation attempted");
                }
                if let Some(s) = &self.settings {
                    s.update_clients(self.rotation.clients(), self.rotation.current());
                }
            }
            Action::Menu => self.open_menu(),
        }
    }
    fn request_activation(&mut self, key: ClientKey, retry_direction: Option<bool>) {
        if self.activation.request(key) {
            self.pending_activation = Some((key, retry_direction, Instant::now()));
        } else {
            self.log
                .write(format!("activation target={key:?} was already pending"));
        }
    }
    fn drain_activation(&mut self) {
        while let Some((key, result)) = self.activation.try_result() {
            let Some((pending_key, retry_direction, _started)) = self.pending_activation else {
                continue;
            };
            if pending_key != key {
                continue;
            }
            self.pending_activation = None;
            self.log.write(format!(
                "activation target={key:?} result={result:?} observed_foreground={:?}",
                unsafe { GetForegroundWindow() }
            ));
            if result == platform::ActivationResult::Stale
                && let Some(previous) = retry_direction
            {
                self.refresh_clients(true);
                if let Some(retry) = self.rotation.next(previous) {
                    self.request_activation(retry, None);
                }
            }
        }
        if let Some((key, _, started)) = self.pending_activation
            && started.elapsed() >= Duration::from_millis(500)
        {
            self.pending_activation = None;
            self.log.write(format!(
                "activation target={key:?} timed out waiting for worker"
            ));
        }
    }
    fn open_menu(&mut self) {
        if self.radial.as_ref().is_some_and(Radial::is_open) {
            return;
        }
        self.menu_origin = self.rotation.current();
        let mut menu = Radial::new(self.config.menu.clone(), self.menu_origin);
        if !menu.open() {
            self.log.write("native radial menu creation failed");
        }
        menu.update_axes(self.axes.x, self.axes.y);
        self.radial = Some(menu);
    }
    fn cancel_menu(&mut self) {
        if let Some(mut menu) = self.radial.take() {
            menu.cancel();
        }
        self.menu_origin = None;
    }
    fn release_menu(&mut self) {
        if let Some(mut menu) = self.radial.take() {
            let action = menu.release();
            self.menu_origin = None;
            if let Some(action) = action {
                self.execute(action);
            } else {
                self.log.write("menu dismissed without selection");
            }
        }
    }
    fn drain_settings(&mut self) {
        loop {
            let event = self
                .settings
                .as_ref()
                .and_then(|s| s.events.try_recv().ok());
            let Some(event) = event else {
                break;
            };
            match event {
                SettingsEvent::Cancel => {
                    self.settings = None;
                    self.awaiting_selection =
                        Some(self.input.select(self.config.controller.clone()));
                    self.suppress_until_release = self.axes.buttons;
                }
                SettingsEvent::Refresh => self.refresh(),
                SettingsEvent::CaptureController(id) => {
                    self.cancel_menu();
                    self.awaiting_selection = Some(self.input.select(id));
                    self.suppress_until_release = self.axes.buttons;
                }
                SettingsEvent::EndCapture => {
                    self.awaiting_selection =
                        Some(self.input.select(self.config.controller.clone()));
                    self.suppress_until_release = self.axes.buttons;
                }
                SettingsEvent::ClientEdits(clients) => {
                    self.apply_client_edits(clients);
                    if let Some(s) = &self.settings {
                        s.update_clients(self.rotation.clients(), self.rotation.current());
                    }
                    self.log.write("session client edits applied");
                }
                SettingsEvent::Save {
                    request_id,
                    config,
                    clients: _,
                } => {
                    let controller_changed = self.config.controller != config.controller;
                    let bindings_changed = self.config.bindings != config.bindings;
                    match config::save(&self.path, &config) {
                        Err(error) => {
                            self.log.write(&error);
                            if let Some(s) = &self.settings {
                                s.save_failed(request_id, &error);
                            }
                        }
                        Ok(()) => {
                            self.config = *config;
                            if controller_changed {
                                self.cancel_menu();
                                self.awaiting_selection =
                                    Some(self.input.select(self.config.controller.clone()));
                                self.suppress_until_release = self.axes.buttons;
                            } else if bindings_changed {
                                self.cancel_menu();
                                self.suppress_until_release = self.axes.buttons;
                            }
                            if let Some(s) = &self.settings {
                                s.save_succeeded(request_id);
                            }
                            self.log.write("settings applied and saved");
                        }
                    }
                }
            }
        }
    }
    fn apply_client_edits(&mut self, clients: Vec<Client>) {
        for (index, client) in clients.iter().enumerate() {
            if let Some(old) = self
                .rotation
                .clients()
                .iter()
                .position(|c| c.key == client.key)
            {
                self.rotation.reorder(
                    old,
                    index.min(self.rotation.clients().len().saturating_sub(1)),
                );
                self.rotation.allow(client.key, client.allowed);
            }
        }
    }
    fn tray_command(&mut self, command: TrayCommand) {
        match command {
            TrayCommand::Settings => self.open_settings(),
            TrayCommand::Refresh => self.refresh(),
            TrayCommand::Exit => {
                self.log.write("tray command selected: Exit");
                self.exit = true;
            }
            TrayCommand::Action(action) => self.execute(action),
        }
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;

    fn owner() -> Owner {
        let restart_message = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        assert_ne!(restart_message, 0);
        Owner::new(restart_message).unwrap()
    }

    fn dispatch_posted(hwnd: HWND, message_id: u32, wparam: WPARAM, lparam: LPARAM) {
        unsafe {
            assert_ne!(PostMessageW(hwnd, message_id, wparam, lparam), 0);
            let mut message = MSG::default();
            assert_ne!(
                PeekMessageW(&mut message, hwnd, message_id, message_id, PM_REMOVE),
                0
            );
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    fn dispatch_wakeup(hwnd: HWND) {
        unsafe {
            let mut message = MSG::default();
            assert_ne!(
                PeekMessageW(
                    &mut message,
                    hwnd,
                    OWNER_DISPATCH,
                    OWNER_DISPATCH,
                    PM_REMOVE
                ),
                0
            );
            DispatchMessageW(&message);
        }
    }

    #[test]
    fn owner_routes_sent_and_posted_notifications_without_popup_reentry() {
        let mut owner = owner();
        unsafe {
            SendMessageW(
                owner.hwnd,
                crate::ui::tray::TRAY_MESSAGE,
                1,
                WM_LBUTTONDBLCLK as LPARAM,
            );
        }
        dispatch_wakeup(owner.hwnd);
        assert_eq!(
            owner.next_event(),
            Some(OwnerEvent::Tray(TrayCallback::OpenSettings))
        );

        dispatch_posted(
            owner.hwnd,
            crate::ui::tray::TRAY_MESSAGE,
            1,
            WM_RBUTTONUP as LPARAM,
        );
        dispatch_wakeup(owner.hwnd);
        assert_eq!(
            owner.next_event(),
            Some(OwnerEvent::Tray(TrayCallback::OpenContextMenu))
        );

        owner.set_popup_active(true);
        unsafe {
            SendMessageW(
                owner.hwnd,
                crate::ui::tray::TRAY_MESSAGE,
                1,
                WM_RBUTTONUP as LPARAM,
            );
            SendMessageW(
                owner.hwnd,
                crate::ui::tray::TRAY_MESSAGE,
                1,
                WM_RBUTTONUP as LPARAM,
            );
        }
        owner.set_popup_active(false);
        assert_eq!(owner.next_event(), None);

        dispatch_posted(owner.hwnd, owner.state.restart_message, 0, 0);
        dispatch_wakeup(owner.hwnd);
        assert_eq!(owner.next_event(), Some(OwnerEvent::RestoreTray));

        unsafe {
            SendMessageW(owner.hwnd, SHOW_SETTINGS, 0, 0);
        }
        assert_eq!(owner.next_event(), Some(OwnerEvent::ShowSettings));
        unsafe {
            SendMessageW(owner.hwnd, owner.state.restart_message, 0, 0);
            SendMessageW(owner.hwnd, WM_CLOSE, 0, 0);
        }
        dispatch_wakeup(owner.hwnd);
        assert_eq!(owner.next_event(), Some(OwnerEvent::Close));
    }

    #[test]
    fn close_discards_queued_and_subsequent_non_close_owner_events() {
        let mut state = OwnerState::new(WM_APP + 4);
        state.enqueue(OwnerEvent::RestoreTray);
        state.enqueue(OwnerEvent::Tray(TrayCallback::OpenSettings));
        assert!(!state.enqueue(OwnerEvent::Close));
        assert_eq!(state.events, VecDeque::from([OwnerEvent::Close]));
        assert!(!state.enqueue(OwnerEvent::Tick));
        assert_eq!(state.events, VecDeque::from([OwnerEvent::Close]));
        assert!(!state.may_dispatch_menu_command());
    }

    #[test]
    fn close_and_cancelmode_end_only_an_active_popup() {
        assert!(should_end_popup(true, false, WM_CLOSE));
        assert!(should_end_popup(true, false, WM_CANCELMODE));
        assert!(!should_end_popup(true, true, WM_CANCELMODE));
        assert!(!should_end_popup(false, false, WM_CLOSE));
        assert!(!should_end_popup(false, false, WM_CANCELMODE));

        let mut state = OwnerState::new(WM_APP + 4);
        state.popup_active = true;
        assert!(state.request_popup_end(WM_CANCELMODE));
        assert!(!state.request_popup_end(WM_CLOSE));
    }
}
