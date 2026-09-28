use super::{
    Bindings, CURRENT_SCHEMA_VERSION, Config, LoadResult, MenuConfig, MenuItem, RefreshMode,
    normalize_config, normalize_config_with_warnings,
};
use crate::actions::Action;
use serde::Deserialize;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const SCHEMA_1_VERSION: u32 = 1;
const SCHEMA_1_REMOVED_LABEL_TOKENS: [&str; 3] = ["%magstate%", "%curset%", "%nextset%"];
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

/// Loads schema 2, migrates supported schema 1 JSON, or returns portable defaults.
pub fn load(path: &Path) -> Result<LoadResult, String> {
    let exists = path
        .try_exists()
        .map_err(|error| format!("could not inspect config at {}: {error}", path.display()))?;
    if !exists {
        return Ok(normalize_config(Config::default()));
    }

    match read_config(path) {
        Ok(config) => {
            let ParsedConfig {
                config,
                warnings,
                schema_1_source,
            } = config;
            let loaded = normalize_config_with_warnings(config, warnings);
            if let Some(source) = schema_1_source.as_deref() {
                preserve_schema_1_source(path, source)?;
                let bytes = serialize_config(&loaded.config)?;
                atomic_replace(path, &bytes)?;
            }
            Ok(loaded)
        }
        Err(ConfigReadError::UnsupportedSchema(message)) => Err(message),
        Err(ConfigReadError::Malformed(primary_error)) => {
            let backup = backup_path(path);
            if backup.exists() {
                match read_config(&backup) {
                    Ok(config) => {
                        let mut loaded =
                            normalize_config_with_warnings(config.config, config.warnings);
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

/// Saves validated JSON with a same-directory atomic replacement and a `.bak` copy.
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
            Ok(config) if config.schema_1_source.is_none() => true,
            Ok(_) => {
                return Err(format!(
                    "schema 1 config at {} must be loaded and migrated before saving",
                    path.display()
                ));
            }
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
    sync_directory(parent);
    Ok(())
}

fn serialize_config(config: &Config) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(config)
        .map_err(|error| format!("could not serialize config: {error}"))
}

#[derive(Debug)]
struct ParsedConfig {
    config: Config,
    warnings: Vec<String>,
    schema_1_source: Option<Vec<u8>>,
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

fn read_config(path: &Path) -> Result<ParsedConfig, ConfigReadError> {
    let bytes = read_bounded(path)?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| ConfigReadError::Malformed(format!("invalid JSON: {error}")))?;
    let schema = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|schema| u32::try_from(schema).ok());
    match schema {
        Some(CURRENT_SCHEMA_VERSION) => {
            let warnings = if remove_schema_2_maximize_items(&mut value) {
                vec!["schema 2 legacy Maximize data was discarded".to_owned()]
            } else {
                Vec::new()
            };
            let config = serde_json::from_value(value).map_err(|error| {
                ConfigReadError::Malformed(format!("invalid config data: {error}"))
            })?;
            Ok(ParsedConfig {
                config,
                warnings,
                schema_1_source: None,
            })
        }
        Some(SCHEMA_1_VERSION) => {
            let legacy: Schema1Config = serde_json::from_value(value).map_err(|error| {
                ConfigReadError::Malformed(format!("invalid schema 1 config data: {error}"))
            })?;
            let (config, warnings) = legacy.into_config();
            Ok(ParsedConfig {
                config,
                warnings,
                schema_1_source: Some(bytes),
            })
        }
        _ => Err(ConfigReadError::UnsupportedSchema(format!(
            "unsupported or missing config schema version {:?}; expected {}",
            schema, CURRENT_SCHEMA_VERSION
        ))),
    }
}

fn remove_schema_2_maximize_items(value: &mut serde_json::Value) -> bool {
    let removed_binding = value
        .pointer("/bindings/maximize")
        .is_some_and(|binding| !binding.is_null());
    let Some(items) = value
        .pointer_mut("/menu/items")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return removed_binding;
    };
    let original_len = items.len();
    items.retain(|item| item.get("action").and_then(serde_json::Value::as_str) != Some("Maximize"));
    removed_binding || items.len() != original_len
}

#[derive(Deserialize)]
#[serde(default)]
struct Schema1Config {
    refresh_mode: RefreshMode,
    refresh_interval_ms: u32,
    controller: Option<String>,
    bindings: [Option<u8>; 6],
    menu: Schema1MenuConfig,
}

impl Default for Schema1Config {
    fn default() -> Self {
        Self {
            refresh_mode: RefreshMode::default(),
            refresh_interval_ms: 1_000,
            controller: None,
            bindings: [None; 6],
            menu: Schema1MenuConfig::default(),
        }
    }
}

impl Schema1Config {
    fn into_config(self) -> (Config, Vec<String>) {
        let Schema1Config {
            refresh_mode,
            refresh_interval_ms,
            controller,
            bindings,
            menu,
        } = self;
        let [
            next,
            previous,
            removed_maximize_binding,
            _removed_magnifiers,
            _removed_cycle_set,
            menu_binding,
        ] = bindings;
        let Schema1MenuConfig {
            items: legacy_items,
        } = menu;
        let mut removed_actions = false;
        let mut replaced_tokens = false;
        let items = legacy_items
            .into_iter()
            .filter_map(|item| {
                let action = match item.action {
                    Schema1Action::Next => Action::Next,
                    Schema1Action::Previous => Action::Previous,
                    Schema1Action::Maximize => {
                        removed_actions = true;
                        return None;
                    }
                    Schema1Action::Menu => Action::Menu,
                    Schema1Action::Magnifiers | Schema1Action::CycleSet => {
                        removed_actions = true;
                        return None;
                    }
                };
                let has_removed_token = SCHEMA_1_REMOVED_LABEL_TOKENS
                    .iter()
                    .any(|token| item.label.contains(token));
                let label = if has_removed_token {
                    replaced_tokens = true;
                    action.menu_label().to_owned()
                } else {
                    item.label
                };
                Some(MenuItem {
                    action,
                    label,
                    enabled: item.enabled,
                })
            })
            .collect();
        let mut warnings = Vec::new();
        if removed_maximize_binding.is_some() {
            warnings.push("schema 1 Maximize binding at slot 2 was skipped".to_owned());
        }
        match (removed_actions, replaced_tokens) {
            (true, true) => warnings.push(
                "schema 1 removed menu actions were skipped and magnifier-only label tokens were replaced with their action defaults".to_owned(),
            ),
            (true, false) => warnings.push("schema 1 removed menu actions were skipped".to_owned()),
            (false, true) => warnings.push(
                "schema 1 magnifier-only label tokens were replaced with their action defaults".to_owned(),
            ),
            (false, false) => {}
        }
        let config = Config {
            schema_version: CURRENT_SCHEMA_VERSION,
            refresh_mode,
            refresh_interval_ms,
            controller,
            bindings: Bindings {
                next,
                previous,
                menu: menu_binding,
            },
            menu: MenuConfig { items },
        };
        (config, warnings)
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct Schema1MenuConfig {
    #[serde(default = "schema1_default_menu_items")]
    items: Vec<Schema1MenuItem>,
}

impl Default for Schema1MenuConfig {
    fn default() -> Self {
        Self {
            items: schema1_default_menu_items(),
        }
    }
}

#[derive(Deserialize)]
enum Schema1Action {
    Next,
    Previous,
    Maximize,
    Magnifiers,
    CycleSet,
    Menu,
}

#[derive(Deserialize)]
struct Schema1MenuItem {
    action: Schema1Action,
    label: String,
    enabled: bool,
}

fn schema1_default_menu_items() -> Vec<Schema1MenuItem> {
    vec![
        Schema1MenuItem {
            action: Schema1Action::Next,
            label: "Next".to_owned(),
            enabled: true,
        },
        Schema1MenuItem {
            action: Schema1Action::Previous,
            label: "Previous".to_owned(),
            enabled: true,
        },
        Schema1MenuItem {
            action: Schema1Action::Maximize,
            label: "Zoom".to_owned(),
            enabled: true,
        },
        Schema1MenuItem {
            action: Schema1Action::Magnifiers,
            label: "%magstate%\n%curset%".to_owned(),
            enabled: true,
        },
        Schema1MenuItem {
            action: Schema1Action::CycleSet,
            label: "Next:\n%nextset%".to_owned(),
            enabled: true,
        },
    ]
}

fn preserve_schema_1_source(path: &Path, source: &[u8]) -> Result<(), String> {
    let backup = migration_backup_path(path);
    if backup.exists() {
        let saved = fs::read(&backup).map_err(|error| {
            format!(
                "could not read schema 1 migration backup {}: {error}",
                backup.display()
            )
        })?;
        if saved == source {
            return Ok(());
        }
        return Err(format!(
            "schema 1 migration backup {} already contains different data; original config was preserved at {}",
            backup.display(),
            path.display()
        ));
    }
    let parent = parent_directory(path);
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create config directory {}: {error}",
            parent.display()
        )
    })?;
    let temp = unique_sibling_path(&backup, "tmp");
    write_new_synced(&temp, source).map_err(|error| {
        format!(
            "could not write schema 1 migration backup {}: {error}",
            backup.display()
        )
    })?;
    if let Err(error) = fs::rename(&temp, &backup) {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "could not create schema 1 migration backup {}: {error}",
            backup.display()
        ));
    }
    sync_directory(parent);
    Ok(())
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = parent_directory(path);
    let temp = unique_sibling_path(path, "tmp");
    write_new_synced(&temp, bytes).map_err(|error| {
        format!(
            "could not write temporary migrated config {}: {error}",
            temp.display()
        )
    })?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "could not atomically replace migrated config {}: {error}",
            path.display()
        ));
    }
    sync_directory(parent);
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

fn migration_backup_path(path: &Path) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(".schema1.bak");
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
    let mut cleanup = TempFileGuard::new(path);
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if result.is_ok() {
        cleanup.disarm();
    }
    result
}

fn copy_synced(source: &Path, destination: &Path) -> io::Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut cleanup = TempFileGuard::new(destination);
    let result = (|| {
        io::copy(&mut input, &mut output)?;
        output.sync_all()
    })();
    if result.is_ok() {
        cleanup.disarm();
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

struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn sync_directory(_path: &Path) {
    // Windows does not expose directory fsync through std. File contents have
    // already been flushed, and rename remains an atomic same-volume operation.
    #[cfg(unix)]
    if let Ok(directory) = File::open(_path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Routing, route_button};
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
    fn schema_1_migration_maps_bindings_and_preserves_supported_settings() {
        let directory = test_directory("migration");
        let path = directory.join(CONFIG_FILE_NAME);
        let source = br#"{
  "schema_version": 1,
  "refresh_mode": "periodic",
  "refresh_interval_ms": 2345,
  "magnifier_interval_ms": 30,
  "controller": "controller-id",
  "bindings": [10, 11, 12, 13, 14, 15],
  "diagnostics": true,
  "magnifiers": {"active_set": 4},
  "unknown_old_field": {"retain": true},
  "menu": {
    "items": [
      {"action": "CycleSet", "label": "Next: %nextset%", "enabled": true},
      {"action": "Previous", "label": "Back", "enabled": false},
      {"action": "Maximize", "label": "%magstate% %curset%", "enabled": true},
      {"action": "Next", "label": "Forward %curset%", "enabled": true}
    ],
    "x": 123,
    "y": 456,
    "opacity": 65,
    "sensitivity": 9000,
    "draw_interval_ms": 17,
    "sound": false,
    "sound_path": "sounds\\click.wav",
    "background": 2130778740,
    "base": 4278324566,
    "text": 4278389111,
    "selector": 4278453656
  }
}"#;
        fs::create_dir_all(path.parent().unwrap()).expect("create portable app directory");
        fs::write(&path, source).expect("write synthetic schema 1 fixture");

        let loaded = load(&path).expect("migrate schema 1 JSON");
        assert_eq!(loaded.config.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(loaded.config.refresh_mode, RefreshMode::Periodic);
        assert_eq!(loaded.config.refresh_interval_ms, 2345);
        assert_eq!(loaded.config.controller.as_deref(), Some("controller-id"));
        assert_eq!(
            loaded.config.bindings,
            Bindings {
                next: Some(10),
                previous: Some(11),
                menu: Some(15),
            }
        );
        assert_eq!(
            route_button(&loaded.config.bindings, 15, true, false),
            Routing::Command(Action::Menu)
        );
        assert_eq!(
            route_button(&loaded.config.bindings, 15, false, false),
            Routing::MenuRelease
        );
        assert_eq!(
            loaded
                .config
                .menu
                .items
                .iter()
                .map(|item| item.action)
                .collect::<Vec<_>>(),
            [Action::Previous, Action::Next]
        );
        assert_eq!(loaded.config.menu.items[0].label, "Back");
        assert!(!loaded.config.menu.items[0].enabled);
        assert_eq!(loaded.config.menu.items[1].label, "Next");
        assert_eq!(loaded.warnings.len(), 2);
        assert!(loaded.warnings[0].contains("Maximize binding at slot 2 was skipped"));
        assert!(loaded.warnings[1].contains("removed menu actions were skipped"));
        assert!(loaded.warnings[1].contains("tokens were replaced"));

        let rewritten = fs::read(&path).expect("read converted schema 2 config");
        let converted: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(converted["schema_version"], 2);
        assert_eq!(converted["bindings"]["menu"], 15);
        assert!(converted["bindings"].get("maximize").is_none());
        assert!(converted.get("diagnostics").is_none());
        assert!(converted["menu"].get("opacity").is_none());
        assert!(converted["menu"].get("sound").is_none());
        assert!(converted.get("unknown_old_field").is_none());
        assert!(converted["bindings"].is_object());
        assert_eq!(fs::read(migration_backup_path(&path)).unwrap(), source);
        assert!(load(&path).unwrap().warnings.is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn schema_2_drops_removed_features_and_backs_up_unknown_fields_before_save() {
        let directory = test_directory("schema2-removed-features");
        let path = directory.join(CONFIG_FILE_NAME);
        let source = br#"{"schema_version":2,"unknown_old_field":{"retain":true},"diagnostics":true,"bindings":{"next":3,"previous":4,"maximize":5,"menu":6,"unknown_binding":7},"menu":{"items":[{"action":"Maximize","label":"Zoom","enabled":true},{"action":"Next","label":"Forward","enabled":true}],"x":123,"opacity":30,"sensitivity":9000,"draw_interval_ms":17,"sound":true,"sound_path":"old.wav","background":1,"base":2,"text":3,"selector":4,"unknown_menu_field":true}}"#;
        fs::write(&path, source).expect("write old schema 2 fixture");

        let loaded = load(&path).expect("load old schema 2 config");
        assert_eq!(loaded.config.bindings.next, Some(3));
        assert_eq!(loaded.config.bindings.previous, Some(4));
        assert_eq!(loaded.config.bindings.menu, Some(6));
        assert_eq!(loaded.config.menu.items.len(), 1);
        assert_eq!(loaded.config.menu.items[0].action, Action::Next);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("legacy Maximize data was discarded"));
        assert_eq!(
            fs::read(&path).expect("source remains untouched on load"),
            source
        );

        save(&path, &loaded.config).expect("save reduced current config");
        assert_eq!(fs::read(backup_path(&path)).unwrap(), source);
        let current: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(current.get("unknown_old_field").is_none());
        assert!(current.get("diagnostics").is_none());
        assert!(current["bindings"].get("maximize").is_none());
        assert!(current["menu"].get("opacity").is_none());
        assert!(current["menu"].get("sound_path").is_none());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn migration_preserves_an_all_disabled_menu() {
        let directory = test_directory("disabled-menu");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            br#"{"schema_version":1,"menu":{"items":[{"action":"Next","label":"Next","enabled":false}]}}"#,
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.config.menu.items.len(), 1);
        assert!(!loaded.config.menu.items[0].enabled);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_primary_recovers_from_backup_without_rewriting_primary() {
        let directory = test_directory("recover");
        let path = directory.join(CONFIG_FILE_NAME);
        let original = Config::default();
        save(&path, &original).expect("save initial config");
        let mut newer = original.clone();
        newer.refresh_interval_ms = 2_000;
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
        changed.refresh_interval_ms = 3_000;
        save(&path, &changed).expect("save recovered configuration");
        let backup: Config =
            serde_json::from_slice(&fs::read(backup_path(&path)).unwrap()).unwrap();
        let current: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(backup.refresh_interval_ms, 1_000);
        assert_eq!(current.refresh_interval_ms, 3_000);
        assert!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt."))
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_schema_1_and_future_schema_are_preserved() {
        let directory = test_directory("schema-errors");
        let malformed_path = directory.join("malformed.json");
        let malformed = br#"{"schema_version":1,"bindings":[1,2]}"#;
        fs::write(&malformed_path, malformed).unwrap();
        let error = load(&malformed_path).expect_err("short schema 1 binding list is malformed");
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
    fn save_is_atomic_and_keeps_the_previous_schema_2_version() {
        let directory = test_directory("save");
        let path = directory.join(CONFIG_FILE_NAME);
        let first = Config::default();
        save(&path, &first).expect("save first config");
        let mut second = first.clone();
        second.refresh_interval_ms = 2_000;
        save(&path, &second).expect("replace config");
        let current: Config = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let backup: Config =
            serde_json::from_slice(&fs::read(backup_path(&path)).unwrap()).unwrap();
        assert_eq!(current.refresh_interval_ms, 2_000);
        assert_eq!(backup.refresh_interval_ms, 1_000);
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

    #[test]
    fn temporary_file_guard_removes_only_owned_partial_files() {
        let directory = test_directory("temp-guard");
        let path = directory.join("owned.tmp");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create temp fixture");
        drop(file);
        {
            let _guard = TempFileGuard::new(&path);
        }
        assert!(!path.exists());

        let preserved = directory.join("preserved.tmp");
        fs::write(&preserved, b"keep").expect("create preserved fixture");
        {
            let mut guard = TempFileGuard::new(&preserved);
            guard.disarm();
        }
        assert_eq!(fs::read(&preserved).unwrap(), b"keep");
        let _ = fs::remove_dir_all(directory);
    }
}
