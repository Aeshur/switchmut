//! Single-owner XInput and WinMM controller polling.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(10);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(1);
const EVENT_CAPACITY: usize = 128;
const XINPUT_TRIGGER_THRESHOLD: u8 = 30;
const AXIS_CENTER: i32 = 32_767;
const JOY_RETURN_ALL: u32 = 0x0000_00ff;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControllerState {
    pub buttons: u32,
    pub x: i32,
    pub y: i32,
}

impl Default for ControllerState {
    fn default() -> Self {
        Self {
            buttons: 0,
            x: AXIS_CENTER,
            y: AXIS_CENTER,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Edge {
    pub button: u8,
    pub pressed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Devices(Vec<Device>),
    Selected(Option<String>),
    SelectionApplied(u64),
    Connected(bool),
    State(ControllerState),
    Button(Edge),
    Diagnostic(String),
}

#[derive(Default)]
struct WorkerControl {
    selection_request: Option<(u64, Option<String>)>,
    last_selection_token: u64,
}

pub struct InputWorker {
    control: Arc<Mutex<WorkerControl>>,
    stop_requested: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl InputWorker {
    pub fn start() -> Result<(Self, Receiver<InputEvent>), String> {
        let (sender, receiver) = mpsc::sync_channel(EVENT_CAPACITY);
        let control = Arc::new(Mutex::new(WorkerControl::default()));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_control = Arc::clone(&control);
        let worker_stop = Arc::clone(&stop_requested);
        let worker = thread::Builder::new()
            .name("switchmut-input".to_owned())
            .spawn(move || run_worker(sender, worker_control, worker_stop))
            .map_err(|error| format!("could not start the controller input worker: {error}"))?;
        Ok((
            Self {
                control,
                stop_requested,
                worker: Some(worker),
            },
            receiver,
        ))
    }

    /// Select a stable device ID or restore automatic selection.
    /// The returned token is acknowledged by `InputEvent::SelectionApplied`.
    pub fn select(&self, id: Option<String>) -> u64 {
        let mut control = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        control.last_selection_token = control.last_selection_token.wrapping_add(1);
        if control.last_selection_token == 0 {
            control.last_selection_token = 1;
        }
        let token = control.last_selection_token;
        control.selection_request = Some((token, id));
        token
    }

    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for InputWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    XInput,
    WinMm,
}

#[derive(Clone, Debug)]
struct BackendDevice {
    device: Device,
    backend: Backend,
    index: u32,
    button_count: u8,
}

#[derive(Default)]
struct InputTracker {
    connected: bool,
    state: ControllerState,
}

impl InputTracker {
    fn reset_selection(&mut self) -> Vec<InputEvent> {
        let releases = self.release_edges();
        self.connected = false;
        self.state = ControllerState::default();
        let mut events = vec![InputEvent::Connected(false), InputEvent::State(self.state)];
        events.extend(releases);
        events
    }

    fn update(&mut self, next: Option<ControllerState>) -> Vec<InputEvent> {
        let Some(next) = next else {
            if !self.connected {
                return Vec::new();
            }
            let releases = self.release_edges();
            self.connected = false;
            self.state = ControllerState::default();
            let mut events = vec![InputEvent::Connected(false), InputEvent::State(self.state)];
            events.extend(releases);
            events.push(InputEvent::Diagnostic(
                "selected controller disconnected".to_owned(),
            ));
            return events;
        };

        if !self.connected {
            self.connected = true;
            self.state = next;
            return vec![InputEvent::Connected(true), InputEvent::State(next)];
        }

        let changed = self.state.buttons ^ next.buttons;
        let mut events = Vec::new();
        if self.state != next {
            events.push(InputEvent::State(next));
        }
        events.extend(
            (0..32)
                .filter(|index| changed & (1u32 << index) != 0)
                .map(|index| {
                    InputEvent::Button(Edge {
                        button: index as u8,
                        pressed: next.buttons & (1u32 << index) != 0,
                    })
                }),
        );
        self.state = next;
        events
    }

    fn release_edges(&self) -> Vec<InputEvent> {
        (0..32)
            .filter(|index| self.state.buttons & (1u32 << index) != 0)
            .map(|index| {
                InputEvent::Button(Edge {
                    button: index as u8,
                    pressed: false,
                })
            })
            .collect()
    }

    fn force_reset(&mut self) {
        self.connected = false;
        self.state = ControllerState::default();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResyncStage {
    Cancel,
    Center,
    Selection,
    Devices,
    Applied,
}

fn run_worker(
    sender: SyncSender<InputEvent>,
    control: Arc<Mutex<WorkerControl>>,
    stop_requested: Arc<AtomicBool>,
) {
    let mut selected: Option<String> = None;
    let mut automatic_selection: Option<String> = None;
    let mut fixed_selection = false;
    let mut tracker = InputTracker::default();
    let mut last_devices: Option<Vec<Device>> = None;
    let mut resync_stage: Option<ResyncStage> = None;
    let mut pending_ack: Option<u64> = None;
    let mut devices = Vec::new();
    let mut public_devices = Vec::new();
    let mut next_discovery = std::time::Instant::now();

    while !stop_requested.load(Ordering::Acquire) {
        let discover = std::time::Instant::now() >= next_discovery;
        if discover {
            devices = enumerate_devices();
            public_devices = devices
                .iter()
                .map(|device| device.device.clone())
                .collect::<Vec<_>>();
            next_discovery = std::time::Instant::now() + DISCOVERY_INTERVAL;
        }

        // A full queue can leave ordinary device updates pending. Recover
        // input state first so a changed device list never overtakes cancel.
        if let Some(stage) = resync_stage {
            let event = resync_event(stage, selected.clone(), public_devices.clone(), pending_ack);
            match emit(&sender, event) {
                Ok(()) => {
                    resync_stage = next_resync_stage(stage, pending_ack);
                    if resync_stage.is_none() {
                        last_devices = Some(public_devices.clone());
                        tracker.force_reset();
                        pending_ack = None;
                    }
                }
                Err(EmitError::Full) => {
                    thread::sleep(SAMPLE_INTERVAL);
                    continue;
                }
                Err(EmitError::Disconnected) => return,
            }
            if resync_stage.is_some() {
                thread::sleep(SAMPLE_INTERVAL);
                continue;
            }
        }

        if discover {
            match emit_devices_update(
                &sender,
                &public_devices,
                &mut last_devices,
                &mut tracker,
                &mut resync_stage,
            ) {
                Ok(()) => {}
                Err(EmitError::Full) => {
                    thread::sleep(SAMPLE_INTERVAL);
                    continue;
                }
                Err(EmitError::Disconnected) => return,
            }
        }

        let mut requested = None;
        if let Ok(mut shared) = control.lock()
            && let Some(request) = shared.selection_request.take()
        {
            requested = Some(request);
        }
        let (selection_changed, ack_token) = resolve_selection(
            &mut selected,
            &mut automatic_selection,
            &mut fixed_selection,
            requested,
            &public_devices,
        );
        pending_ack = ack_token;

        if let Some(token) = pending_ack {
            if !send_batch(
                &sender,
                selection_events(&mut tracker, selected.clone(), Some(token)),
                &mut tracker,
                &mut resync_stage,
            ) {
                return;
            }
            if resync_stage.is_some() {
                continue;
            }
            pending_ack = None;
        } else if selection_changed {
            if !send_batch(
                &sender,
                selection_events(&mut tracker, selected.clone(), None),
                &mut tracker,
                &mut resync_stage,
            ) {
                return;
            }
            if resync_stage.is_some() {
                continue;
            }
        }

        let current = selected
            .as_deref()
            .and_then(|id| devices.iter().find(|device| device.device.id == id));
        let state = current.and_then(poll_device);
        if !send_batch(
            &sender,
            tracker.update(state),
            &mut tracker,
            &mut resync_stage,
        ) {
            return;
        }
        if resync_stage.is_some() {
            continue;
        }
        thread::sleep(SAMPLE_INTERVAL);
    }
}

fn auto_selection(current: Option<&str>, devices: &[Device]) -> Option<String> {
    current
        .filter(|id| devices.iter().any(|device| device.id == *id))
        .map(str::to_owned)
        .or_else(|| devices.first().map(|device| device.id.clone()))
}

fn resolve_selection(
    selected: &mut Option<String>,
    automatic: &mut Option<String>,
    fixed: &mut bool,
    request: Option<(u64, Option<String>)>,
    devices: &[Device],
) -> (bool, Option<u64>) {
    let previous = selected.clone();
    let ack_token = request.as_ref().map(|(token, _)| *token);
    let use_automatic = match request {
        Some((_, Some(id))) => {
            *fixed = true;
            *selected = Some(id);
            false
        }
        Some((_, None)) => {
            *fixed = false;
            true
        }
        None => !*fixed,
    };

    if use_automatic {
        *automatic = auto_selection(automatic.as_deref(), devices);
        *selected = automatic.clone();
    }
    (*selected != previous, ack_token)
}

fn selection_events(
    tracker: &mut InputTracker,
    selected: Option<String>,
    token: Option<u64>,
) -> Vec<InputEvent> {
    let mut events = tracker.reset_selection();
    events.push(InputEvent::Selected(selected));
    if let Some(token) = token {
        events.push(InputEvent::SelectionApplied(token));
    }
    events
}

fn resync_event(
    stage: ResyncStage,
    selected: Option<String>,
    devices: Vec<Device>,
    pending_ack: Option<u64>,
) -> InputEvent {
    match stage {
        ResyncStage::Cancel => InputEvent::Connected(false),
        ResyncStage::Center => InputEvent::State(ControllerState::default()),
        ResyncStage::Selection => InputEvent::Selected(selected),
        ResyncStage::Devices => InputEvent::Devices(devices),
        ResyncStage::Applied => InputEvent::SelectionApplied(
            pending_ack.expect("applied resync stage requires a pending selection request"),
        ),
    }
}

fn next_resync_stage(stage: ResyncStage, pending_ack: Option<u64>) -> Option<ResyncStage> {
    match stage {
        ResyncStage::Cancel => Some(ResyncStage::Center),
        ResyncStage::Center => Some(ResyncStage::Selection),
        ResyncStage::Selection => Some(ResyncStage::Devices),
        ResyncStage::Devices => pending_ack.map(|_| ResyncStage::Applied),
        ResyncStage::Applied => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmitError {
    Full,
    Disconnected,
}

fn emit(sender: &SyncSender<InputEvent>, event: InputEvent) -> Result<(), EmitError> {
    match sender.try_send(event) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(EmitError::Full),
        Err(TrySendError::Disconnected(_)) => Err(EmitError::Disconnected),
    }
}

fn emit_devices_update(
    sender: &SyncSender<InputEvent>,
    devices: &[Device],
    last_devices: &mut Option<Vec<Device>>,
    tracker: &mut InputTracker,
    resync_stage: &mut Option<ResyncStage>,
) -> Result<(), EmitError> {
    if last_devices.as_deref() == Some(devices) {
        return Ok(());
    }
    match emit(sender, InputEvent::Devices(devices.to_vec())) {
        Ok(()) => {
            *last_devices = Some(devices.to_vec());
            Ok(())
        }
        Err(EmitError::Full) => {
            schedule_resync(tracker, resync_stage);
            Err(EmitError::Full)
        }
        Err(EmitError::Disconnected) => Err(EmitError::Disconnected),
    }
}

fn send_batch(
    sender: &SyncSender<InputEvent>,
    events: Vec<InputEvent>,
    tracker: &mut InputTracker,
    resync_stage: &mut Option<ResyncStage>,
) -> bool {
    for event in events {
        match emit(sender, event) {
            Ok(()) => {}
            Err(EmitError::Full) => {
                schedule_resync(tracker, resync_stage);
                return true;
            }
            Err(EmitError::Disconnected) => return false,
        }
    }
    true
}

fn schedule_resync(tracker: &mut InputTracker, resync_stage: &mut Option<ResyncStage>) {
    tracker.force_reset();
    *resync_stage = Some(ResyncStage::Cancel);
}

fn legacy_device_id(backend: Backend, index: u32) -> String {
    let group = if backend == Backend::XInput { 1 } else { 0 };
    format!("00000000-0000-{group:04x}-0000-{:012x}", index + 1)
}

fn xinput_buttons(buttons: u16, left_trigger: u8, right_trigger: u8) -> u32 {
    const BUTTONS: [u16; 14] = [
        0x0001, 0x0002, 0x0004, 0x0008, 0x0010, 0x0020, 0x0040, 0x0080, 0x0100, 0x0200, 0x1000,
        0x2000, 0x4000, 0x8000,
    ];
    let mut mapped = 0u32;
    for (index, mask) in BUTTONS.into_iter().enumerate() {
        if buttons & mask != 0 {
            mapped |= 1u32 << index;
        }
    }
    if left_trigger >= XINPUT_TRIGGER_THRESHOLD {
        mapped |= 1 << 14;
    }
    if right_trigger >= XINPUT_TRIGGER_THRESHOLD {
        mapped |= 1 << 15;
    }
    mapped
}

fn xinput_axis(value: i16, invert: bool) -> i32 {
    let value = i32::from(value) / 2;
    if invert {
        AXIS_CENTER - value
    } else {
        AXIS_CENTER + value
    }
}

fn winmm_axis(value: u32) -> i32 {
    value.min(65_535) as i32
}

fn button_mask(count: u8) -> u32 {
    match count {
        0 => 0,
        32.. => u32::MAX,
        count => (1u32 << count) - 1,
    }
}

#[cfg(windows)]
fn enumerate_devices() -> Vec<BackendDevice> {
    use windows_sys::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

    let mut xinput = Vec::new();
    for index in 0..4u32 {
        let mut state: XINPUT_STATE = unsafe { std::mem::zeroed() };
        if unsafe { XInputGetState(index, &mut state) } == 0 {
            xinput.push(BackendDevice {
                device: Device {
                    id: legacy_device_id(Backend::XInput, index),
                    name: format!("XInput Controller #{}", index + 1),
                },
                backend: Backend::XInput,
                index,
                button_count: 16,
            });
        }
    }
    xinput.extend(enumerate_winmm_devices());
    xinput
}

#[cfg(not(windows))]
fn enumerate_devices() -> Vec<BackendDevice> {
    Vec::new()
}

#[cfg(windows)]
fn enumerate_winmm_devices() -> Vec<BackendDevice> {
    use windows_sys::Win32::Media::Multimedia::{JOYCAPSW, joyGetDevCapsW, joyGetNumDevs};

    let mut devices = Vec::new();
    let count = unsafe { joyGetNumDevs() };
    for index in 0..count {
        let mut caps: JOYCAPSW = unsafe { std::mem::zeroed() };
        if unsafe {
            joyGetDevCapsW(
                index as usize,
                &mut caps,
                std::mem::size_of::<JOYCAPSW>() as u32,
            )
        } != 0
        {
            continue;
        }
        let name_units = unsafe { std::ptr::addr_of!(caps.szPname).read_unaligned() };
        let button_count = unsafe { std::ptr::addr_of!(caps.wNumButtons).read_unaligned() };
        let end = name_units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(name_units.len());
        let name = String::from_utf16_lossy(&name_units[..end])
            .trim()
            .to_owned();
        let base_name = if name.is_empty() { "Joystick" } else { &name };
        let device = BackendDevice {
            device: Device {
                id: legacy_device_id(Backend::WinMm, index),
                name: format!("{base_name} #{} (WinMM)", index + 1),
            },
            backend: Backend::WinMm,
            index,
            button_count: button_count.min(32) as u8,
        };
        if poll_winmm(device.index, device.button_count).is_some() {
            devices.push(device);
        }
    }
    devices
}

#[cfg(windows)]
fn poll_device(device: &BackendDevice) -> Option<ControllerState> {
    match device.backend {
        Backend::XInput => poll_xinput(device.index),
        Backend::WinMm => poll_winmm(device.index, device.button_count),
    }
}

#[cfg(not(windows))]
fn poll_device(_device: &BackendDevice) -> Option<ControllerState> {
    None
}

#[cfg(windows)]
fn poll_xinput(index: u32) -> Option<ControllerState> {
    use windows_sys::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

    let mut raw: XINPUT_STATE = unsafe { std::mem::zeroed() };
    if unsafe { XInputGetState(index, &mut raw) } != 0 {
        return None;
    }
    let gamepad = raw.Gamepad;
    Some(ControllerState {
        buttons: xinput_buttons(
            gamepad.wButtons,
            gamepad.bLeftTrigger,
            gamepad.bRightTrigger,
        ),
        x: xinput_axis(gamepad.sThumbLX, false),
        y: xinput_axis(gamepad.sThumbLY, true),
    })
}

#[cfg(windows)]
fn poll_winmm(index: u32, button_count: u8) -> Option<ControllerState> {
    use windows_sys::Win32::Media::Multimedia::{JOYINFOEX, joyGetPosEx};

    let mut raw: JOYINFOEX = unsafe { std::mem::zeroed() };
    raw.dwSize = std::mem::size_of::<JOYINFOEX>() as u32;
    raw.dwFlags = JOY_RETURN_ALL;
    if unsafe { joyGetPosEx(index, &mut raw) } != 0 {
        return None;
    }
    Some(ControllerState {
        buttons: raw.dwButtons & button_mask(button_count),
        x: winmm_axis(raw.dwXpos),
        y: winmm_axis(raw.dwYpos),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take_buttons(events: Vec<InputEvent>) -> Vec<Edge> {
        events
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Button(edge) => Some(edge),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn legacy_ids_and_axis_centers_are_stable() {
        assert_eq!(
            legacy_device_id(Backend::XInput, 0),
            "00000000-0000-0001-0000-000000000001"
        );
        assert_eq!(
            legacy_device_id(Backend::WinMm, 3),
            "00000000-0000-0000-0000-000000000004"
        );
        assert_eq!(ControllerState::default().x, 32_767);
        assert_eq!(ControllerState::default().y, 32_767);
    }

    #[test]
    fn automatic_selection_reconsiders_devices_after_disconnect_and_reconnect() {
        let first = Device {
            id: "first".to_owned(),
            name: "First".to_owned(),
        };
        let second = Device {
            id: "second".to_owned(),
            name: "Second".to_owned(),
        };
        assert_eq!(auto_selection(None, &[]), None);
        assert_eq!(
            auto_selection(None, std::slice::from_ref(&first)),
            Some("first".to_owned())
        );
        assert_eq!(
            auto_selection(Some("second"), &[first.clone(), second]),
            Some("second".to_owned())
        );
        assert_eq!(
            auto_selection(Some("second"), std::slice::from_ref(&first)),
            Some("first".to_owned())
        );
        assert_eq!(auto_selection(None, &[first]), Some("first".to_owned()));
    }

    #[test]
    fn automatic_device_survives_explicit_preview_and_is_restored_by_none() {
        let first = Device {
            id: "auto-a".to_owned(),
            name: "Automatic A".to_owned(),
        };
        let second = Device {
            id: "draft-b".to_owned(),
            name: "Draft B".to_owned(),
        };
        let both = [first.clone(), second.clone()];
        let mut selected = None;
        let mut automatic = None;
        let mut fixed = false;

        let (changed, ack) =
            resolve_selection(&mut selected, &mut automatic, &mut fixed, None, &both);
        assert!(changed);
        assert_eq!(ack, None);
        assert_eq!(selected.as_deref(), Some("auto-a"));
        assert_eq!(automatic.as_deref(), Some("auto-a"));

        let (changed, ack) = resolve_selection(
            &mut selected,
            &mut automatic,
            &mut fixed,
            Some((1, Some("draft-b".to_owned()))),
            &both,
        );
        assert!(changed);
        assert_eq!(ack, Some(1));
        assert_eq!(selected.as_deref(), Some("draft-b"));
        assert_eq!(automatic.as_deref(), Some("auto-a"));

        let (changed, ack) = resolve_selection(
            &mut selected,
            &mut automatic,
            &mut fixed,
            Some((2, None)),
            &both,
        );
        assert!(changed);
        assert_eq!(ack, Some(2));
        assert_eq!(selected.as_deref(), Some("auto-a"));
        assert_eq!(automatic.as_deref(), Some("auto-a"));

        resolve_selection(
            &mut selected,
            &mut automatic,
            &mut fixed,
            None,
            std::slice::from_ref(&second),
        );
        assert_eq!(selected.as_deref(), Some("draft-b"));
        assert_eq!(automatic.as_deref(), Some("draft-b"));

        resolve_selection(&mut selected, &mut automatic, &mut fixed, None, &both);
        assert_eq!(selected.as_deref(), Some("draft-b"));
        assert_eq!(automatic.as_deref(), Some("draft-b"));
    }

    #[test]
    fn xinput_masks_follow_legacy_button_order_and_trigger_threshold() {
        assert_eq!(
            xinput_buttons(0x1001, 29, 30),
            (1 << 0) | (1 << 10) | (1 << 15)
        );
        assert_eq!(xinput_buttons(0, 30, 0), 1 << 14);
        assert_eq!(xinput_axis(0, false), 32_767);
        assert_eq!(xinput_axis(0, true), 32_767);
        assert!((0..=65_535).contains(&xinput_axis(i16::MIN, false)));
        assert_eq!(winmm_axis(u32::MAX), 65_535);
    }

    #[test]
    fn startup_held_buttons_do_not_create_press_edges() {
        let mut tracker = InputTracker::default();
        let connected = tracker.update(Some(ControllerState {
            buttons: 0b101,
            x: 12_000,
            y: 51_000,
        }));
        assert!(connected.contains(&InputEvent::Connected(true)));
        assert!(take_buttons(connected).is_empty());
    }

    #[test]
    fn button_changes_emit_edges_and_disconnect_releases_held_buttons_once() {
        let mut tracker = InputTracker::default();
        tracker.update(Some(ControllerState {
            buttons: 1,
            ..ControllerState::default()
        }));
        let changed = tracker.update(Some(ControllerState {
            buttons: 3,
            ..ControllerState::default()
        }));
        assert_eq!(
            take_buttons(changed),
            vec![Edge {
                button: 1,
                pressed: true
            }]
        );

        let disconnected = tracker.update(None);
        assert_eq!(disconnected[0], InputEvent::Connected(false));
        assert_eq!(
            disconnected[1],
            InputEvent::State(ControllerState::default())
        );
        assert_eq!(
            take_buttons(disconnected.clone()),
            vec![
                Edge {
                    button: 0,
                    pressed: false
                },
                Edge {
                    button: 1,
                    pressed: false
                }
            ]
        );
        assert!(disconnected.contains(&InputEvent::Connected(false)));
        assert!(tracker.update(None).is_empty());
    }

    #[test]
    fn reconnecting_with_a_held_button_suppresses_a_startup_press() {
        let mut tracker = InputTracker::default();
        tracker.update(Some(ControllerState {
            buttons: 1,
            ..ControllerState::default()
        }));
        tracker.update(None);
        let reconnected = tracker.update(Some(ControllerState {
            buttons: 1,
            ..ControllerState::default()
        }));
        assert!(reconnected.contains(&InputEvent::Connected(true)));
        assert!(take_buttons(reconnected).is_empty());
    }

    #[test]
    fn selection_change_releases_buttons_and_centers_state() {
        let mut tracker = InputTracker::default();
        tracker.update(Some(ControllerState {
            buttons: 0b11,
            x: 5,
            y: 9,
        }));
        let events = tracker.reset_selection();
        assert_eq!(events[0], InputEvent::Connected(false));
        assert_eq!(events[1], InputEvent::State(ControllerState::default()));
        assert!(matches!(
            events[2],
            InputEvent::Button(Edge { pressed: false, .. })
        ));
        assert_eq!(
            take_buttons(events.clone()),
            vec![
                Edge {
                    button: 0,
                    pressed: false
                },
                Edge {
                    button: 1,
                    pressed: false
                }
            ]
        );
        assert!(events.contains(&InputEvent::Connected(false)));
        assert!(events.contains(&InputEvent::State(ControllerState::default())));
    }

    #[test]
    fn explicit_same_selection_emits_ack_after_reset_and_selected_events() {
        let mut tracker = InputTracker::default();
        tracker.update(Some(ControllerState {
            buttons: 1,
            x: 5,
            y: 9,
        }));

        let events = selection_events(&mut tracker, Some("device".to_owned()), Some(41));

        assert_eq!(events[0], InputEvent::Connected(false));
        assert_eq!(events[1], InputEvent::State(ControllerState::default()));
        assert_eq!(
            events[2],
            InputEvent::Button(Edge {
                button: 0,
                pressed: false,
            })
        );
        assert_eq!(events[3], InputEvent::Selected(Some("device".to_owned())));
        assert_eq!(events[4], InputEvent::SelectionApplied(41));
    }

    #[test]
    fn explicit_automatic_selection_still_emits_ack_for_none() {
        let mut tracker = InputTracker::default();
        let events = selection_events(&mut tracker, None, Some(42));
        assert_eq!(
            events,
            vec![
                InputEvent::Connected(false),
                InputEvent::State(ControllerState::default()),
                InputEvent::Selected(None),
                InputEvent::SelectionApplied(42),
            ]
        );
    }

    #[test]
    fn newest_state_is_sent_before_button_edges() {
        let mut tracker = InputTracker::default();
        tracker.update(Some(ControllerState::default()));
        let next = ControllerState {
            buttons: 1,
            x: 12_000,
            y: 54_000,
        };
        let events = tracker.update(Some(next));
        assert_eq!(events[0], InputEvent::State(next));
        assert_eq!(
            events[1],
            InputEvent::Button(Edge {
                button: 0,
                pressed: true
            })
        );
    }

    #[test]
    fn full_event_channel_schedules_bounded_cancel_and_state_resync() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .try_send(InputEvent::Button(Edge {
                button: 0,
                pressed: true,
            }))
            .expect("test event fills bounded channel");
        let mut tracker = InputTracker::default();
        tracker.update(Some(ControllerState {
            buttons: 1,
            x: 1,
            y: 2,
        }));
        let mut stage = None;
        assert!(send_batch(
            &sender,
            vec![InputEvent::State(ControllerState::default())],
            &mut tracker,
            &mut stage,
        ));
        assert_eq!(stage, Some(ResyncStage::Cancel));
        assert!(!tracker.connected);
        assert_eq!(tracker.state, ControllerState::default());

        // The already queued event stays bounded; after the receiver drains it,
        // the worker sends cancellation before its centered state and snapshot.
        let _ = receiver.try_recv().expect("queued event remains bounded");
        let selected = Some("first".to_owned());
        let devices = vec![Device {
            id: "first".to_owned(),
            name: "First".to_owned(),
        }];
        let mut recovered = Vec::new();
        let pending_ack = None;
        while let Some(current) = stage {
            let event = resync_event(current, selected.clone(), devices.clone(), pending_ack);
            sender
                .try_send(event)
                .expect("resync uses available bounded slot");
            recovered.push(receiver.try_recv().expect("resync event delivered"));
            stage = next_resync_stage(current, pending_ack);
        }
        assert_eq!(recovered[0], InputEvent::Connected(false));
        assert_eq!(recovered[1], InputEvent::State(ControllerState::default()));
        assert_eq!(recovered[2], InputEvent::Selected(selected));
        assert_eq!(recovered[3], InputEvent::Devices(devices));
        assert!(
            !recovered
                .iter()
                .any(|event| matches!(event, InputEvent::Button(_)))
        );
    }

    #[test]
    fn overflow_resync_replays_latest_selection_ack_after_selected() {
        let selected = Some("first".to_owned());
        let devices = vec![Device {
            id: "first".to_owned(),
            name: "First".to_owned(),
        }];
        let mut stage = Some(ResyncStage::Cancel);
        let token = Some(77);
        let mut recovered = Vec::new();
        while let Some(current) = stage {
            recovered.push(resync_event(
                current,
                selected.clone(),
                devices.clone(),
                token,
            ));
            stage = next_resync_stage(current, token);
        }

        assert_eq!(recovered[0], InputEvent::Connected(false));
        assert_eq!(recovered[1], InputEvent::State(ControllerState::default()));
        assert_eq!(recovered[2], InputEvent::Selected(selected));
        assert_eq!(recovered[3], InputEvent::Devices(devices));
        assert_eq!(recovered[4], InputEvent::SelectionApplied(77));
    }

    #[test]
    fn full_device_update_channel_schedules_resync_without_advancing_snapshot() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        sender
            .try_send(InputEvent::Button(Edge {
                button: 0,
                pressed: true,
            }))
            .expect("test event fills bounded channel");
        let mut tracker = InputTracker::default();
        tracker.update(Some(ControllerState {
            buttons: 1,
            x: 1,
            y: 2,
        }));
        let mut stage = None;
        let old_snapshot = vec![Device {
            id: "old".to_owned(),
            name: "Old".to_owned(),
        }];
        let new_snapshot = vec![Device {
            id: "new".to_owned(),
            name: "New".to_owned(),
        }];
        let mut last_devices = Some(old_snapshot.clone());

        assert_eq!(
            emit_devices_update(
                &sender,
                &new_snapshot,
                &mut last_devices,
                &mut tracker,
                &mut stage,
            ),
            Err(EmitError::Full)
        );

        assert_eq!(stage, Some(ResyncStage::Cancel));
        assert!(!tracker.connected);
        assert_eq!(tracker.state, ControllerState::default());
        assert_eq!(last_devices, Some(old_snapshot));
    }

    #[test]
    fn worker_stop_and_drop_join_cleanly() {
        let (mut worker, receiver) = InputWorker::start().expect("start input worker");
        let token = worker.select(None);
        let mut observed = Vec::new();
        loop {
            let event = receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("worker applies controller selection");
            observed.push(event.clone());
            if event == InputEvent::SelectionApplied(token) {
                break;
            }
        }
        let applied = observed
            .iter()
            .position(|event| *event == InputEvent::SelectionApplied(token))
            .expect("matching selection acknowledgement was observed");
        assert!(applied >= 3);
        assert!(matches!(observed[applied - 1], InputEvent::Selected(_)));
        assert!(observed[..applied].contains(&InputEvent::Connected(false)));
        assert!(observed[..applied].contains(&InputEvent::State(ControllerState::default())));
        worker.stop();
        drop(receiver);
    }
}
