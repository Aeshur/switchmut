//! Discovery and stable identities for supported FFXIV 1.23b client windows.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A window identity includes the process creation time so reused HWND and PID
/// values cannot silently refer to a later client process.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ClientKey {
    pub hwnd: isize,
    pub pid: u32,
    pub created: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Client {
    pub key: ClientKey,
    pub title: String,
    pub allowed: bool,
}

pub const SUPPORTED_CLIENT_SIZE: u64 = 15_996_808;
pub const SUPPORTED_CLIENT_NORMALISED_SHA256: &str =
    "c8bd8e58bb48de41096e1f31b907e75ffe1ebc60e594bb0a897312ce7b99be65";
const SUPPORTED_CLIENT_NAMES: [&str; 2] = ["ffxivgame.exe", "ffxivgame.patched.exe"];
const PATCH_SLOTS: [(usize, usize); 5] = [
    (0x0049_2550, 29),
    (0x0049_4B70, 4),
    (0x0064_8BBF, 16),
    (0x009A_15E3, 5),
    (0x00B9_0110, 0x14),
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImageMetadata {
    size: u64,
    modified: Option<SystemTime>,
    created: u64,
    changed: Option<i64>,
    file_id: Option<(u64, [u8; 16])>,
}

impl ImageMetadata {
    fn read(path: &Path) -> std::io::Result<Self> {
        let metadata = fs::metadata(path)?;
        let native = native_file_metadata(path).ok();
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: native.map(|value| value.0).unwrap_or_default(),
            changed: native.map(|value| value.1),
            file_id: native.map(|value| value.2),
        })
    }
}

#[derive(Clone)]
struct CachedIdentity {
    metadata: ImageMetadata,
    supported: bool,
}

#[derive(Default)]
struct IdentityCache {
    entries: HashMap<PathBuf, CachedIdentity>,
}

impl IdentityCache {
    fn check(&mut self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            return false;
        };
        if !is_supported_client_name(name) {
            return false;
        }

        let cache_path = cache_path(path);
        let metadata = match ImageMetadata::read(path) {
            Ok(metadata) => metadata,
            Err(_) => {
                self.entries.remove(&cache_path);
                return false;
            }
        };
        cache_decision(&mut self.entries, cache_path, metadata.clone(), || {
            if metadata.size == SUPPORTED_CLIENT_SIZE {
                fs::read(path)
                    .ok()
                    .and_then(|bytes| {
                        let after_read = ImageMetadata::read(path).ok()?;
                        (after_read == metadata).then(|| normalized_sha256(&bytes))
                    })
                    .is_some_and(|digest| digest == SUPPORTED_CLIENT_NORMALISED_SHA256)
            } else {
                false
            }
        })
    }

    fn keep_only(&mut self, paths: &HashSet<PathBuf>) {
        self.entries.retain(|path, _| paths.contains(path));
    }
}

fn cache_decision(
    entries: &mut HashMap<PathBuf, CachedIdentity>,
    path: PathBuf,
    metadata: ImageMetadata,
    compute: impl FnOnce() -> bool,
) -> bool {
    let cacheable = metadata.file_id.is_some() && metadata.changed.is_some();
    if !cacheable {
        entries.remove(&path);
        return compute();
    }
    if let Some(entry) = entries.get(&path)
        && entry.metadata == metadata
    {
        return entry.supported;
    }
    let supported = compute();
    entries.insert(
        path,
        CachedIdentity {
            metadata,
            supported,
        },
    );
    supported
}

fn cache_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

fn is_supported_client_name(name: &str) -> bool {
    SUPPORTED_CLIENT_NAMES
        .iter()
        .any(|supported| supported.eq_ignore_ascii_case(name))
}

fn normalized_sha256(bytes: &[u8]) -> String {
    let mut normalized = bytes.to_vec();
    for (offset, length) in PATCH_SLOTS {
        if let Some(range) = normalized.get_mut(offset..offset + length) {
            range.fill(0);
        }
    }
    format!("{:x}", Sha256::digest(&normalized))
}

fn native_file_metadata(path: &Path) -> std::io::Result<(u64, i64, (u64, [u8; 16]))> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_ID_INFO, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
    };

    let file = fs::File::open(path)?;
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let mut basic = FILE_BASIC_INFO::default();
    let basic_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            &mut basic as *mut FILE_BASIC_INFO as *mut std::ffi::c_void,
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } != 0;
    let mut file_id = FILE_ID_INFO::default();
    let file_id_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut file_id as *mut FILE_ID_INFO as *mut std::ffi::c_void,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0;
    if !basic_ok || !file_id_ok {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        basic.CreationTime as u64,
        basic.ChangeTime,
        (file_id.VolumeSerialNumber, file_id.FileId.Identifier),
    ))
}

#[derive(Default)]
pub struct Discovery {
    identities: IdentityCache,
}

impl Discovery {
    /// Enumerate visible top-level windows belonging to an exact supported image.
    /// Existing allow/deny decisions are managed by `switching::Rotation`.
    pub fn refresh(&mut self) -> Vec<Client> {
        use windows_sys::Win32::Foundation::{HWND, LPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;
        use windows_sys::core::BOOL;

        unsafe extern "system" fn visit(hwnd: HWND, context: LPARAM) -> BOOL {
            let context = unsafe { &mut *(context as *mut EnumerationContext<'_>) };
            unsafe { visit_window(hwnd as isize, context) };
            1
        }

        let mut context = EnumerationContext {
            clients: Vec::new(),
            identities: &mut self.identities,
            processes: HashMap::new(),
            verified: HashMap::new(),
            seen_paths: HashSet::new(),
        };
        unsafe {
            EnumWindows(
                Some(visit),
                &mut context as *mut EnumerationContext<'_> as LPARAM,
            );
        }
        context.identities.keep_only(&context.seen_paths);
        context.clients
    }
}

unsafe fn visit_window(hwnd: isize, context: &mut EnumerationContext<'_>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetParent, GetWindowThreadProcessId, IsWindowVisible,
    };

    let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
    if !unsafe { GetParent(hwnd) }.is_null() || unsafe { IsWindowVisible(hwnd) } == 0 {
        return;
    }

    let mut pid = 0u32;
    if unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } == 0 || pid == 0 {
        return;
    }

    let Some(process) = context.process(pid) else {
        return;
    };
    let path_key = cache_path(&process.path);
    context.seen_paths.insert(path_key.clone());
    // IdentityCache cannot store files without stable IDs; cache each path's verdict for this pass.
    let supported = match context.verified.get(&path_key) {
        Some(&supported) => supported,
        None => {
            let supported = context.identities.check(&process.path);
            context.verified.insert(path_key, supported);
            supported
        }
    };
    if !supported {
        return;
    }

    context.clients.push(Client {
        key: ClientKey {
            hwnd: hwnd as isize,
            pid,
            created: process.created,
        },
        title: window_title(hwnd as isize),
        allowed: true,
    });
}

struct ProcessInfo {
    path: PathBuf,
    created: u64,
}

struct EnumerationContext<'a> {
    clients: Vec<Client>,
    identities: &'a mut IdentityCache,
    processes: HashMap<u32, Option<ProcessInfo>>,
    verified: HashMap<PathBuf, bool>,
    seen_paths: HashSet<PathBuf>,
}

impl EnumerationContext<'_> {
    fn process(&mut self, pid: u32) -> Option<ProcessInfo> {
        self.processes
            .entry(pid)
            .or_insert_with(|| query_process(pid))
            .as_ref()
            .map(|info| ProcessInfo {
                path: info.path.clone(),
                created: info.created,
            })
    }
}

fn window_title(hwnd: isize) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};
    let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
    let length = unsafe { GetWindowTextLengthW(hwnd) }.clamp(0, 4096) as usize;
    let mut buffer = vec![0u16; length + 1];
    let count = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..count.max(0) as usize])
}

struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);

impl ProcessHandle {
    fn open(pid: u32) -> Option<Self> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }

    /// Creation time in FILETIME ticks; zero is rejected.
    fn creation_time(&self) -> Option<u64> {
        use windows_sys::Win32::System::Threading::GetProcessTimes;
        let mut created = unsafe { std::mem::zeroed() };
        let mut exited = unsafe { std::mem::zeroed() };
        let mut kernel = unsafe { std::mem::zeroed() };
        let mut user = unsafe { std::mem::zeroed() };
        if unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return None;
        }
        let created = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        (created != 0).then_some(created)
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

fn query_process(pid: u32) -> Option<ProcessInfo> {
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;

    let process = ProcessHandle::open(pid)?;
    let mut path = vec![0u16; 32_768];
    let mut path_length = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &mut path_length) } == 0
    {
        return None;
    }
    path.truncate(path_length as usize);
    let created = process.creation_time()?;
    if path.is_empty() {
        return None;
    }
    Some(ProcessInfo {
        path: PathBuf::from(String::from_utf16_lossy(&path)),
        created,
    })
}

/// Check the window's PID and process creation time to reject reused identities.
pub fn is_live(key: ClientKey) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow};

    let hwnd = key.hwnd as windows_sys::Win32::Foundation::HWND;
    if hwnd.is_null() || key.pid == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    let mut pid = 0u32;
    if unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } == 0 || pid != key.pid {
        return false;
    }
    ProcessHandle::open(key.pid)
        .and_then(|process| process.creation_time())
        .is_some_and(|created| created == key.created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_two_supported_names_case_insensitively() {
        assert!(is_supported_client_name("ffxivgame.exe"));
        assert!(is_supported_client_name("FFXIVGAME.PATCHED.EXE"));
        assert!(!is_supported_client_name("ffxivgame.exe.bak"));
        assert!(!is_supported_client_name("other.exe"));
    }

    #[test]
    fn normalized_hash_ignores_only_the_documented_patch_slots() {
        let mut original = vec![0xA5; SUPPORTED_CLIENT_SIZE as usize];
        let mut patched = original.clone();
        for (offset, length) in PATCH_SLOTS {
            patched[offset..offset + length].fill(0x5A);
        }
        assert_eq!(normalized_sha256(&original), normalized_sha256(&patched));
        original[0x100] ^= 1;
        assert_ne!(normalized_sha256(&original), normalized_sha256(&patched));
    }

    #[test]
    fn cache_decision_rechecks_when_executable_metadata_changes() {
        let mut entries = HashMap::new();
        let path = PathBuf::from("sample.exe");
        let first = ImageMetadata {
            size: 12,
            modified: Some(SystemTime::UNIX_EPOCH),
            created: 1,
            changed: Some(1),
            file_id: Some((1, [10; 16])),
        };
        let changed = ImageMetadata {
            size: 12,
            modified: Some(SystemTime::UNIX_EPOCH),
            created: 1,
            changed: Some(1),
            file_id: Some((1, [11; 16])),
        };
        let mut computations = 0;
        assert!(cache_decision(
            &mut entries,
            path.clone(),
            first.clone(),
            || {
                computations += 1;
                true
            }
        ));
        assert!(cache_decision(&mut entries, path.clone(), first, || {
            computations += 1;
            false
        }));
        assert!(!cache_decision(&mut entries, path, changed, || {
            computations += 1;
            false
        }));
        assert_eq!(computations, 2);
    }

    #[test]
    fn production_cache_rejects_supported_name_and_size_with_wrong_digest() {
        let scratch =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts/platform");
        fs::create_dir_all(&scratch).expect("create the assigned platform scratch directory");
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos();
        let case_dir = scratch.join(format!("identity-{}-{unique}", std::process::id()));
        fs::create_dir(&case_dir).expect("create unique identity test directory");
        let path = case_dir.join("ffxivgame.exe");
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create unique synthetic executable");
        file.set_len(SUPPORTED_CLIENT_SIZE)
            .expect("create the supported image length");
        drop(file);
        struct RemoveCase {
            file: PathBuf,
            directory: PathBuf,
        }
        impl Drop for RemoveCase {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.file);
                let _ = fs::remove_dir(&self.directory);
            }
        }
        let _remove = RemoveCase {
            file: path.clone(),
            directory: case_dir,
        };

        let mut cache = IdentityCache::default();
        assert!(!cache.check(&path));
    }

    #[test]
    fn client_keys_distinguish_reused_process_ids() {
        let first = ClientKey {
            hwnd: 9,
            pid: 42,
            created: 10,
        };
        let restarted = ClientKey {
            created: 11,
            ..first
        };
        assert_ne!(first, restarted);
    }
}
