use crate::actions::Action;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

mod storage;

pub use storage::{config_path, load, save};

pub const CURRENT_SCHEMA_VERSION: u32 = 2;
pub const PERIODIC_REFRESH_INTERVAL_MS: u32 = 1_000;
const MIN_REFRESH_INTERVAL_MS: u32 = 1;
const MAX_REFRESH_INTERVAL_MS: u32 = i32::MAX as u32;
const MAX_CONTROLLER_LENGTH: usize = 512;
const MAX_MENU_ITEMS: usize = 2;
const MAX_MENU_LABEL_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RefreshMode {
    Manual,
    Periodic,
    #[default]
    OnSwitch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub refresh_mode: RefreshMode,
    #[serde(default = "default_refresh_interval_ms")]
    pub refresh_interval_ms: u32,
    #[serde(default)]
    pub controller: Option<String>,
    #[serde(default)]
    pub bindings: Bindings,
    #[serde(default)]
    pub menu: MenuConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Bindings {
    #[serde(default)]
    pub next: Option<u8>,
    #[serde(default)]
    pub previous: Option<u8>,
    #[serde(default)]
    pub menu: Option<u8>,
}

impl Bindings {
    pub fn get(&self, action: Action) -> Option<u8> {
        match action {
            Action::Next => self.next,
            Action::Previous => self.previous,
            Action::Menu => self.menu,
        }
    }

    pub fn set(&mut self, action: Action, button: Option<u8>) {
        match action {
            Action::Next => self.next = button,
            Action::Previous => self.previous = button,
            Action::Menu => self.menu = button,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (Action, Option<u8>)> + '_ {
        Action::ALL
            .into_iter()
            .map(|action| (action, self.get(action)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuConfig {
    #[serde(default = "default_menu_items")]
    pub items: Vec<MenuItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuItem {
    pub action: Action,
    pub label: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadResult {
    pub config: Config,
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            refresh_mode: RefreshMode::OnSwitch,
            refresh_interval_ms: default_refresh_interval_ms(),
            controller: None,
            bindings: Bindings::default(),
            menu: MenuConfig::default(),
        }
    }
}

impl Default for MenuConfig {
    fn default() -> Self {
        Self {
            items: default_menu_items(),
        }
    }
}

impl Default for MenuItem {
    fn default() -> Self {
        Self {
            action: Action::Next,
            label: "Next".to_owned(),
            enabled: true,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported config schema version {}; expected {}",
                self.schema_version, CURRENT_SCHEMA_VERSION
            ));
        }
        if !(MIN_REFRESH_INTERVAL_MS..=MAX_REFRESH_INTERVAL_MS).contains(&self.refresh_interval_ms)
        {
            return Err(format!(
                "refresh_interval_ms must be between {} and {}",
                MIN_REFRESH_INTERVAL_MS, MAX_REFRESH_INTERVAL_MS
            ));
        }
        if self.controller.as_ref().is_some_and(|controller| {
            controller.len() > MAX_CONTROLLER_LENGTH || controller.contains('\0')
        }) {
            return Err("controller identifier is invalid or too long".to_owned());
        }
        validate_bindings(&self.bindings)?;
        self.menu.validate()?;
        Ok(())
    }
}

impl MenuConfig {
    fn validate(&self) -> Result<(), String> {
        if self.items.len() > MAX_MENU_ITEMS {
            return Err(format!(
                "menu.items must contain at most {MAX_MENU_ITEMS} entries"
            ));
        }
        let mut actions = HashSet::new();
        for item in &self.items {
            if item.action == Action::Menu {
                return Err("the Menu action cannot appear inside the radial menu".to_owned());
            }
            if !actions.insert(item.action as u8) {
                return Err(format!(
                    "menu action {:?} appears more than once",
                    item.action
                ));
            }
            if item.label.len() > MAX_MENU_LABEL_BYTES || item.label.contains('\0') {
                return Err(format!(
                    "menu label for {:?} is invalid or too long",
                    item.action
                ));
            }
        }
        Ok(())
    }
}

pub fn validate_bindings(bindings: &Bindings) -> Result<(), String> {
    let mut seen = HashSet::new();
    for (action, binding) in bindings.iter() {
        if let Some(button) = binding {
            if button > 31 {
                return Err(format!(
                    "button ID {button} is outside the supported range 0..=31"
                ));
            }
            if !seen.insert(button) {
                return Err(format!(
                    "button {button} is assigned more than once (action {:?})",
                    action
                ));
            }
        }
    }
    Ok(())
}

fn resolve_duplicate_bindings(bindings: &mut Bindings, warnings: &mut Vec<String>) {
    let mut seen = HashSet::new();
    for action in Action::ALL.into_iter().rev() {
        if let Some(button) = bindings.get(action) {
            if button > 31 {
                bindings.set(action, None);
                warnings.push(format!(
                    "button ID {button} was outside controller button range 0..=31 and was cleared"
                ));
                continue;
            }
            if !seen.insert(button) {
                bindings.set(action, None);
                warnings.push(format!(
                    "duplicate button {button} was assigned to multiple actions; kept the action with the highest stable ID"
                ));
            }
        }
    }
}

pub(super) fn normalize_config(config: Config) -> LoadResult {
    normalize_config_with_warnings(config, Vec::new())
}

pub(super) fn normalize_config_with_warnings(
    mut config: Config,
    mut warnings: Vec<String>,
) -> LoadResult {
    config.refresh_interval_ms = clamp_with_warning(
        config.refresh_interval_ms,
        MIN_REFRESH_INTERVAL_MS,
        MAX_REFRESH_INTERVAL_MS,
        "refresh_interval_ms",
        &mut warnings,
    );
    if config.controller.as_ref().is_some_and(|controller| {
        controller.len() > MAX_CONTROLLER_LENGTH || controller.contains('\0')
    }) {
        config.controller = None;
        warnings.push("invalid controller identifier was cleared".to_owned());
    }
    resolve_duplicate_bindings(&mut config.bindings, &mut warnings);

    let original_items = std::mem::take(&mut config.menu.items);
    let original_items_were_empty = original_items.is_empty();
    let mut seen_actions = HashSet::new();
    let valid_items: Vec<MenuItem> = original_items
        .into_iter()
        .filter_map(|mut item| {
            if item.action == Action::Menu {
                warnings.push("radial menu action Menu was skipped".to_owned());
                return None;
            }
            if !seen_actions.insert(item.action as u8) {
                warnings.push(format!(
                    "duplicate menu action {:?} was skipped",
                    item.action
                ));
                return None;
            }
            if item.label.contains('\0') {
                item.label = item.action.menu_label().to_owned();
                warnings.push(format!(
                    "invalid menu label for {:?} was replaced with its default",
                    item.action
                ));
            }
            if item.label.len() > MAX_MENU_LABEL_BYTES {
                item.label = truncate_utf8(&item.label, MAX_MENU_LABEL_BYTES);
                warnings.push(format!("menu label for {:?} was truncated", item.action));
            }
            Some(item)
        })
        .collect();
    config.menu.items = valid_items;
    if config.menu.items.is_empty() && !original_items_were_empty {
        warnings.push(
            "all radial menu actions were invalid; the built-in radial menu will be used"
                .to_owned(),
        );
    }
    debug_assert!(config.validate().is_ok());
    LoadResult { config, warnings }
}

fn clamp_with_warning(
    value: u32,
    min: u32,
    max: u32,
    label: &str,
    warnings: &mut Vec<String>,
) -> u32 {
    let bounded = value.clamp(min, max);
    if bounded != value {
        warnings.push(format!("{label} value {value} was clamped to {bounded}"));
    }
    bounded
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn default_refresh_interval_ms() -> u32 {
    PERIODIC_REFRESH_INTERVAL_MS
}

fn default_menu_items() -> Vec<MenuItem> {
    [Action::Next, Action::Previous]
        .into_iter()
        .map(|action| MenuItem {
            action,
            label: action.menu_label().to_owned(),
            enabled: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_schema_uses_named_bindings_and_stable_action_ids() {
        let config = Config::default();
        assert_eq!(config.schema_version, 2);
        assert_eq!(Action::Next as u8, 0);
        assert_eq!(Action::Previous as u8, 1);
        assert_eq!(Action::Menu as u8, 5);
        assert_eq!(config.menu.items.len(), 2);

        let json = serde_json::to_value(&config).expect("serialize schema 2 config");
        assert_eq!(json["bindings"]["next"], serde_json::Value::Null);
        assert_eq!(json["bindings"]["previous"], serde_json::Value::Null);
        assert_eq!(json["bindings"]["menu"], serde_json::Value::Null);
        assert!(json["bindings"].is_object());
        assert_eq!(json["bindings"].as_object().unwrap().len(), 3);
        assert!(json.get("diagnostics").is_none());
        assert_eq!(json["menu"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn validation_and_normalization_resolve_named_bindings() {
        let bindings = Bindings {
            next: Some(4),
            menu: Some(4),
            ..Bindings::default()
        };
        assert!(validate_bindings(&bindings).is_err());
        let config = Config {
            bindings: bindings.clone(),
            ..Config::default()
        };
        let normalized = normalize_config(config.clone());
        assert_eq!(normalized.config.bindings.next, None);
        assert_eq!(normalized.config.bindings.menu, Some(4));
        assert_eq!(normalized.warnings.len(), 1);
        assert!(normalized.config.validate().is_ok());

        let invalid = Config {
            bindings: Bindings {
                next: Some(32),
                ..Bindings::default()
            },
            ..Config::default()
        };
        let normalized = normalize_config(invalid);
        assert_eq!(normalized.config.bindings.next, None);
        assert!(normalized.config.validate().is_ok());
    }

    #[test]
    fn disabled_and_empty_menu_lists_remain_intact() {
        let mut config = Config::default();
        config.menu.items = vec![MenuItem {
            action: Action::Previous,
            label: "Back".to_owned(),
            enabled: false,
        }];
        let normalized = normalize_config(config);
        assert_eq!(normalized.config.menu.items.len(), 1);
        assert!(!normalized.config.menu.items[0].enabled);

        let mut empty = normalized.config;
        empty.menu.items.clear();
        assert!(normalize_config(empty).config.menu.items.is_empty());
    }

    #[test]
    fn normalization_clamps_retained_settings_and_labels() {
        let mut config = Config {
            refresh_interval_ms: 0,
            controller: Some("bad\0id".to_owned()),
            ..Config::default()
        };
        config.menu.items = vec![MenuItem {
            action: Action::Next,
            label: "é".repeat(200),
            enabled: true,
        }];

        let normalized = normalize_config(config);
        assert_eq!(normalized.config.refresh_interval_ms, 1);
        assert_eq!(normalized.config.controller, None);
        assert!(normalized.config.menu.items[0].label.len() <= MAX_MENU_LABEL_BYTES);
        assert!(
            normalized.config.menu.items[0]
                .label
                .is_char_boundary(normalized.config.menu.items[0].label.len())
        );
        assert!(normalized.config.validate().is_ok());
        assert!(!normalized.warnings.is_empty());
    }

    #[test]
    fn normalization_filters_invalid_items_before_limiting_valid_items() {
        let mut config = Config::default();
        config.menu.items = vec![
            MenuItem {
                action: Action::Menu,
                label: "invalid".to_owned(),
                enabled: true,
            },
            MenuItem {
                action: Action::Next,
                label: "Next".to_owned(),
                enabled: true,
            },
            MenuItem {
                action: Action::Previous,
                label: "Previous".to_owned(),
                enabled: true,
            },
        ];
        let normalized = normalize_config(config);
        assert_eq!(
            normalized
                .config
                .menu
                .items
                .iter()
                .map(|item| item.action)
                .collect::<Vec<_>>(),
            [Action::Next, Action::Previous]
        );
        assert!(
            normalized
                .warnings
                .iter()
                .any(|warning| warning.contains("Menu"))
        );
    }

    #[test]
    fn all_invalid_items_report_builtin_fallback() {
        let mut config = Config::default();
        config.menu.items = vec![MenuItem {
            action: Action::Menu,
            label: "invalid".to_owned(),
            enabled: true,
        }];
        let normalized = normalize_config(config);
        assert!(normalized.config.menu.items.is_empty());
        assert!(
            normalized
                .warnings
                .iter()
                .any(|warning| warning.contains("built-in radial menu"))
        );
    }
}
