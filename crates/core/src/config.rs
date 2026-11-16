use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use smmm_protocol::Region;

/// Whole-file configuration. Resolution order (later wins):
/// `--config` → `/etc/smartmmm/config.toml` → `$XDG_CONFIG_HOME/smartmmm/config.toml`
/// → `./config.toml` → built-in defaults.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub core: CoreConfig,
    pub display: DisplayConfig,
    pub presence: PresenceConfig,
    /// Keyed by module id, e.g. `[modules.weather]`.
    pub modules: HashMap<String, ModuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    pub listen: String,
    pub name: String,
    pub log_level: String,
    /// If set, mutating requests must carry `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_string(),
            name: "SmartMMM".to_string(),
            log_level: "info".to_string(),
            auth_token: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct DisplayConfig {
    /// `auto` | `pi_vcgencmd` | `pi_cec` | `macos` | `noop`
    pub driver: Option<String>,
    pub brightness: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct PresenceConfig {
    /// `noop` | `gpio_pir` | `camera`
    pub driver: Option<String>,
    pub wake_on_motion: Option<bool>,
}

/// Per-module overrides. Anything left out falls back to the module's manifest.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ModuleConfig {
    pub visible: Option<bool>,
    pub region: Option<Region>,
    pub order: Option<i32>,
    pub config: serde_json::Value,
}

impl Config {
    /// Parse from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).context("parsing config TOML")
    }

    /// Load from an explicit path, or search the default locations.
    ///
    /// Returns the config and the path it was read from (`None` for defaults).
    pub fn load(explicit: Option<&Path>) -> Result<(Self, Option<PathBuf>)> {
        if let Some(path) = explicit {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?;
            let cfg = Self::from_toml_str(&text)?;
            return Ok((cfg, Some(path.to_path_buf())));
        }

        for candidate in default_paths() {
            if candidate.is_file() {
                let text = std::fs::read_to_string(&candidate)
                    .with_context(|| format!("reading config {}", candidate.display()))?;
                let cfg = Self::from_toml_str(&text)?;
                return Ok((cfg, Some(candidate)));
            }
        }

        Ok((Self::default(), None))
    }

    pub fn module(&self, id: &str) -> Option<&ModuleConfig> {
        self.modules.get(id)
    }
}

fn default_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/etc/smartmmm/config.toml")];
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(dir).join("smartmmm/config.toml"));
    } else if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".config/smartmmm/config.toml"));
    }
    paths.push(PathBuf::from("config.toml"));
    paths
}
