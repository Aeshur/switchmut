use super::{CURRENT_SCHEMA_VERSION, Config, LoadResult, normalize_config};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const CONFIG_FILE_NAME: &str = "config-switchmut.json";
const MAX_CONFIG_BYTES: usize = 1_048_576;

pub fn config_path() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not resolve running executable location: {error}"))?;
    config_path_for_executable(&executable)
}

fn config_path_for_executable(executable: &Path) -> Result<PathBuf, String> {
    let parent = executable
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "could not resolve running executable parent directory".to_owned())?;
    Ok(parent.join(CONFIG_FILE_NAME))
}

/// Loads the current schema, or returns portable defaults when no config exists.
pub fn load(path: &Path) -> Result<LoadResult, String> {
    let exists = path
        .try_exists()
        .map_err(|error| format!("could not inspect config at {}: {error}", path.display()))?;
    if !exists {
        return Ok(normalize_config(Config::default()));
    }

    match read_config(path) {
        Ok(config) => Ok(normalize_config(config)),
        Err(ConfigReadError::UnsupportedSchema(message)) => Err(message),
        Err(ConfigReadError::Malformed(primary_error)) => {
            let backup = backup_path(path);
            if backup.exists() {
                match read_config(&backup) {
                    Ok(config) => {
                        let mut loaded = normalize_config(config);
                        loaded.warnings.insert(
                            0,
                            format!(
                                "config-switchmut.json was malformed ({primary_error}); loaded the backup at {}",
                                backup.display()
                            ),
                        );
                        Ok(loaded)
                    }
                    Err(error) => Err(format!(
                        "config at {} is malformed ({primary_error}) and backup {} could not be used ({})",
                        path.display(),
                        backup.display(),
                        error.message()
                    )),
                }
            } else {
                Err(format!(
                    "config at {} is malformed ({primary_error}) and no usable backup exists",
                    path.display()
                ))
            }
        }
    }
}

/// Atomically replaces the config with validated JSON in the same directory.
/// Keeps the previous valid config in a `.bak` file.
pub fn save(path: &Path, config: &Config) -> Result<(), String> {
    config.validate()?;
    let bytes = serialize_config(config)?;
    let parent = parent_directory(path);
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create config directory {}: {error}",
            parent.display()
        )
    })?;

    let previous_is_valid = if path.exists() {
        match read_config(path) {
            Ok(_) => true,
            Err(ConfigReadError::UnsupportedSchema(message)) => return Err(message),
            Err(ConfigReadError::Malformed(_)) => {
                preserve_malformed_file(path)?;
                false
            }
        }
    } else {
        false
    };

    let temp_path = unique_sibling_path(path, "tmp");
    write_new_synced(&temp_path, &bytes).map_err(|error| {
        format!(
            "could not write temporary config {}: {error}",
            temp_path.display()
        )
    })?;

    if previous_is_valid {
        let backup = backup_path(path);
        let backup_temp = unique_sibling_path(&backup, "tmp");
        if let Err(error) = copy_synced(path, &backup_temp) {
            let _ = fs::remove_file(&temp_path);
            return Err(format!(
                "could not prepare config backup {}: {error}",
                backup.display()
            ));
        }
        if let Err(error) = fs::rename(&backup_temp, &backup) {
            let _ = fs::remove_file(&backup_temp);
            let _ = fs::remove_file(&temp_path);
            return Err(format!(
                "could not replace config backup {}: {error}",
                backup.display()
            ));
        }
    }

    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!(
            "could not atomically replace config {}: {error}",
            path.display()
        ));
    }
    Ok(())
}

fn serialize_config(config: &Config) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(config)
        .map_err(|error| format!("could not serialize config: {error}"))
}

#[derive(Debug)]
enum ConfigReadError {
    UnsupportedSchema(String),
    Malformed(String),
}

impl ConfigReadError {
    fn message(&self) -> &str {
        match self {
            Self::UnsupportedSchema(message) | Self::Malformed(message) => message,
        }
    }
}

fn read_config(path: &Path) -> Result<Config, ConfigReadError> {
    let bytes = read_bounded(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| ConfigReadError::Malformed(format!("invalid JSON: {error}")))?;
    let schema = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|schema| u32::try_from(schema).ok());
    if schema != Some(CURRENT_SCHEMA_VERSION) {
        return Err(ConfigReadError::UnsupportedSchema(format!(
            "unsupported or missing config schema version {:?}; expected {}",
            schema, CURRENT_SCHEMA_VERSION
        )));
    }
    serde_json::from_value(value)
        .map_err(|error| ConfigReadError::Malformed(format!("invalid config data: {error}")))
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn preserve_malformed_file(path: &Path) -> Result<(), String> {
    let temp = unique_sibling_path(path, "corrupt-tmp");
    let preserved = unique_sibling_path(path, "corrupt");
    copy_synced(path, &temp).map_err(|error| {
        format!(
            "could not preserve malformed config {}: {error}",
            path.display()
        )
    })?;
    if let Err(error) = fs::rename(&temp, &preserved) {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "could not retain malformed config at {}: {error}",
            preserved.display()
        ));
    }
    Ok(())
}

fn unique_sibling_path(path: &Path, suffix: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut name: OsString = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("config"));
    name.push(format!(".{}.{}.{}", suffix, std::process::id(), serial));
    path.with_file_name(name)
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn copy_synced(source: &Path, destination: &Path) -> io::Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = (|| {
        io::copy(&mut input, &mut output)?;
        output.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, ConfigReadError> {
    let file = File::open(path).map_err(|error| {
        ConfigReadError::Malformed(format!("could not read {}: {error}", path.display()))
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ConfigReadError::Malformed(format!("could not read {}: {error}", path.display()))
        })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigReadError::Malformed(format!(
            "config at {} exceeds the {} byte limit",
            path.display(),
            MAX_CONFIG_BYTES
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RefreshMode;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_directory(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts/config-storage");
        fs::create_dir_all(&root).expect("create test artifact directory");
        let directory = root.join(format!("{label}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&directory).expect("create isolated test directory");
        directory
    }

    #[test]
    fn executable_relative_path_uses_the_executable_parent() {
        let executable = Path::new(r"C:\portable\Switchmut.exe");
        assert_eq!(
            config_path_for_executable(executable).unwrap(),
            PathBuf::from(r"C:\portable\config-switchmut.json")
        );
        assert!(config_path_for_executable(Path::new("Switchmut.exe")).is_err());
        let running_executable = std::env::current_exe().expect("resolve test executable");
        let path = config_path().expect("resolve portable config path");
        assert_eq!(
            path,
            config_path_for_executable(&running_executable).unwrap()
        );
    }

    #[test]
    fn missing_config_returns_defaults_without_writing_a_file() {
        let directory = test_directory("missing");
        let path = directory.join(CONFIG_FILE_NAME);
        let loaded = load(&path).expect("load local defaults");
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn version_1_0_0_config_with_removed_refresh_interval_loads() {
        let directory = test_directory("removed-field");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            br#"{"schema_version":2,"refresh_mode":"periodic","refresh_interval_ms":1000}"#,
        )
        .unwrap();
        let loaded = load(&path).expect("ignore the removed refresh interval");
        assert_eq!(loaded.config.refresh_mode, RefreshMode::Periodic);
        assert!(loaded.warnings.is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_primary_recovers_from_backup_without_rewriting_primary() {
        let directory = test_directory("recover");
        let path = directory.join(CONFIG_FILE_NAME);
        let original = Config::default();
        save(&path, &original).expect("save initial config");
        let mut newer = original.clone();
        newer.refresh_mode = RefreshMode::Periodic;
        save(&path, &newer).expect("create recovery backup");
        let malformed = b"{broken";
        fs::write(&path, malformed).expect("corrupt primary fixture");

        let loaded = load(&path).expect("recover from backup");
        assert_eq!(loaded.config, original);
        assert!(loaded.warnings[0].contains("was malformed"));
        assert_eq!(
            fs::read(&path).expect("primary remains unchanged"),
            malformed
        );

        let mut changed = loaded.config;
        changed.refresh_mode = RefreshMode::Manual;
        save(&path, &changed).expect("save recovered configuration");
        let backup: Config =
            serde_json::from_slice(&fs::read(backup_path(&path)).unwrap()).unwrap();
        let current: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(backup.refresh_mode, RefreshMode::OnSwitch);
        assert_eq!(current.refresh_mode, RefreshMode::Manual);
        assert!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt."))
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_and_future_schema_configs_are_preserved() {
        let directory = test_directory("schema-errors");
        let malformed_path = directory.join("malformed.json");
        let malformed = br#"{"schema_version":2,"bindings":{"next":"A"}}"#;
        fs::write(&malformed_path, malformed).unwrap();
        let error = load(&malformed_path).expect_err("non-numeric binding is malformed");
        assert!(error.contains("malformed"));
        assert_eq!(fs::read(&malformed_path).unwrap(), malformed);

        let future_path = directory.join("future.json");
        let future = br#"{"schema_version":99,"private":"keep me"}"#;
        fs::write(&future_path, future).unwrap();
        let error = load(&future_path).expect_err("future schema must be rejected");
        assert!(error.contains("unsupported or missing config schema version"));
        assert_eq!(fs::read(&future_path).unwrap(), future);
        assert!(save(&future_path, &Config::default()).is_err());
        assert_eq!(fs::read(&future_path).unwrap(), future);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn save_is_atomic_and_keeps_the_previous_version() {
        let directory = test_directory("save");
        let path = directory.join(CONFIG_FILE_NAME);
        let first = Config::default();
        save(&path, &first).expect("save first config");
        let mut second = first.clone();
        second.refresh_mode = RefreshMode::Periodic;
        save(&path, &second).expect("replace config");
        let current: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let backup: Config =
            serde_json::from_slice(&fs::read(backup_path(&path)).unwrap()).unwrap();
        assert_eq!(current.refresh_mode, RefreshMode::Periodic);
        assert_eq!(backup.refresh_mode, RefreshMode::OnSwitch);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn config_reads_are_bounded_and_oversized_primary_is_preserved() {
        let directory = test_directory("bounded-read");
        let path = directory.join(CONFIG_FILE_NAME);
        let oversized = vec![b' '; MAX_CONFIG_BYTES + 1];
        fs::write(&path, &oversized).expect("write oversized config fixture");

        let error = load(&path).expect_err("oversized config must be rejected");
        assert!(error.contains("exceeds the 1048576 byte limit"));
        assert_eq!(fs::read(&path).expect("read preserved config"), oversized);
        let _ = fs::remove_dir_all(directory);
    }
}
