use crate::actions::Action;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

mod storage;

pub use storage::{config_path, load, save};

pub const CURRENT_SCHEMA_VERSION: u32 = 2;
pub const PERIODIC_REFRESH_INTERVAL_MS: u32 = 1_000;
const MAX_CONTROLLER_LENGTH: usize = 512;

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
    #[serde(default)]
    pub controller: Option<String>,
    #[serde(default)]
    pub bindings: Bindings,
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
            controller: None,
            bindings: Bindings::default(),
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
        if self.controller.as_ref().is_some_and(|controller| {
            controller.len() > MAX_CONTROLLER_LENGTH || controller.contains('\0')
        }) {
            return Err("controller identifier is invalid or too long".to_owned());
        }
        validate_bindings(&self.bindings)
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
                    "duplicate button {button} was cleared from {action:?}"
                ));
            }
        }
    }
}

pub(super) fn normalize_config(mut config: Config) -> LoadResult {
    let mut warnings = Vec::new();
    if config.controller.as_ref().is_some_and(|controller| {
        controller.len() > MAX_CONTROLLER_LENGTH || controller.contains('\0')
    }) {
        config.controller = None;
        warnings.push("invalid controller identifier was cleared".to_owned());
    }
    resolve_duplicate_bindings(&mut config.bindings, &mut warnings);

    debug_assert!(config.validate().is_ok());
    LoadResult { config, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_schema_uses_named_bindings() {
        let config = Config::default();
        assert_eq!(config.schema_version, 2);

        let json = serde_json::to_value(&config).expect("serialize schema 2 config");
        assert_eq!(json["bindings"]["next"], serde_json::Value::Null);
        assert_eq!(json["bindings"]["previous"], serde_json::Value::Null);
        assert_eq!(json["bindings"]["menu"], serde_json::Value::Null);
        assert!(json["bindings"].is_object());
        assert_eq!(json["bindings"].as_object().unwrap().len(), 3);
        assert!(json.get("menu").is_none());
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
    fn normalization_clears_invalid_controller() {
        let config = Config {
            controller: Some("bad\0id".to_owned()),
            ..Config::default()
        };
        let normalized = normalize_config(config);
        assert_eq!(normalized.config.controller, None);
        assert!(normalized.config.validate().is_ok());
        assert_eq!(normalized.warnings.len(), 1);
    }
}
