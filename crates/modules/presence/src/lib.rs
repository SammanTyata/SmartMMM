//! Presence module: turns the display off after a period of inactivity and
//! wakes it again on motion, using a PIR sensor on a GPIO input.
//!
//! This module renders nothing. It runs its check every second and drives the
//! display through a driver command (`vcgencmd`, CEC, or `pmset` on macOS).
//!
//! The host only ticks visible modules, so this module stays `visible` — which
//! also means the phone's visibility toggle doubles as a kill switch: hiding
//! the module pauses auto-sleep entirely.

use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use gpio_cdev::{Chip, LineRequestFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smmm_core::{Module, ModuleCtx};
use smmm_protocol::{Manifest, ModuleStatus, Region, ToolDescriptor, ViewNode};

const UPDATE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PresenceConfig {
    /// GPIO character device (`/dev/gpiochip0` covers the main header on every
    /// Pi, including the Pi 5's new GPIO driver).
    pub gpio_chip: String,
    /// BCM pin number the PIR output is wired to.
    pub gpio_pin: u32,
    /// Whether the sensor drives HIGH on motion (HC-SR501 and friends do).
    pub active_high: bool,
    /// Sleep the display after this much time without motion.
    pub timeout_secs: u32,
    /// `auto` | `pi_vcgencmd` | `pi_cec` | `macos` | `noop`
    pub display_driver: String,
}

impl Default for PresenceConfig {
    fn default() -> Self {
        Self {
            gpio_chip: "/dev/gpiochip0".to_string(),
            gpio_pin: 21,
            active_high: true,
            timeout_secs: 30,
            display_driver: "auto".to_string(),
        }
    }
}

pub struct PresenceModule {
    ctx: Option<ModuleCtx>,
    config: PresenceConfig,
    visible: bool,
    status: ModuleStatus,
    /// Last sensor reading, exposed through `status`.
    motion: Option<bool>,
    /// Time of the last observed motion.
    last_motion: Option<Instant>,
    /// Assumed/observed display state; commands are only sent on change.
    display_on: bool,
}

impl PresenceModule {
    pub fn new() -> Self {
        Self {
            ctx: None,
            config: PresenceConfig::default(),
            visible: true,
            status: ModuleStatus::Starting,
            motion: None,
            // Assume the mirror is on and somebody may still be standing in
            // front of it: start the grace period at boot, not in the past.
            last_motion: Some(Instant::now()),
            display_on: true,
        }
    }

    /// Read the sensor once. The chip is opened per read: the handle lives for
    /// microseconds and this keeps ownership simple in the async task.
    fn read_motion(&self) -> anyhow::Result<bool> {
        let mut chip =
            Chip::new(&self.config.gpio_chip).context("opening GPIO chip (on macOS this is expected to fail)")?;
        let line = chip
            .get_line(self.config.gpio_pin)
            .context("getting GPIO line")?;
        let handle = line.request(LineRequestFlags::INPUT, 0, "smartmmm-presence")?;
        let value = handle.get_value()?;
        Ok(self.config.active_high == (value == 1))
    }

    /// Which display driver to use: an explicit config choice, or detect from
    /// what is installed and what OS this is.
    fn resolve_driver(&self) -> &'static str {
        match self.config.display_driver.as_str() {
            "pi_vcgencmd" => "pi_vcgencmd",
            "pi_cec" => "pi_cec",
            "macos" => "macos",
            "noop" => "noop",
            _ => {
                if which("vcgencmd") {
                    "pi_vcgencmd"
                } else if which("cec-client") {
                    "pi_cec"
                } else if cfg!(target_os = "macos") {
                    "macos"
                } else {
                    "noop"
                }
            }
        }
    }

    /// Flip the display. Sends the command only when the state would change,
    /// so a still-wakeful mirror never shells out at all.
    fn set_display_power(&mut self, on: bool) {
        if on == self.display_on {
            return;
        }
        let driver = self.resolve_driver();
        let command: Option<(&str, Vec<String>)> = match driver {
            "pi_vcgencmd" => Some((
                "vcgencmd",
                vec!["display_power".into(), if on { "1" } else { "0" }.into()],
            )),
            "pi_cec" => Some((
                "sh",
                vec!["-c".into(), format!("echo '{}' | cec-client -s -d 1", if on { "on 0" } else { "standby 0" })],
            )),
            // `caffeinate -u` asserts user activity, which wakes the display.
            "macos" if on => Some(("caffeinate", vec!["-u".into(), "-t".into(), "2".into()])),
            "macos" => Some(("pmset", vec!["displaysleepnow".into()])),
            // `noop` only logs: useful for testing the wiring without a display.
            _ => None,
        };

        if let Some((program, args)) = command {
            match Command::new(program).args(&args).output() {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    self.warn(format!(
                        "{driver} said no ({}): {}",
                        output.status,
                        stderr.trim()
                    ));
                    return;
                }
                Err(e) => {
                    self.warn(format!("could not run {program}: {e}"));
                    return;
                }
            }
        }

        self.display_on = on;
        let state = if on { "on" } else { "off" };
        self.info(format!("display {state} ({driver})"));
    }

    fn status_json(&self) -> Value {
        let idle_secs = self
            .last_motion
            .map(|last| last.elapsed().as_secs())
            .unwrap_or(u64::MAX);
        json!({
            "motion": self.motion,
            "idle_secs": idle_secs,
            "display": if self.display_on { "on" } else { "off" },
            "timeout_secs": self.config.timeout_secs,
            "display_driver": self.resolve_driver(),
            "gpio": format!("{} pin {}", self.config.gpio_chip, self.config.gpio_pin),
        })
    }

    fn info(&self, message: impl std::fmt::Display) {
        if let Some(ctx) = &self.ctx {
            ctx.info(message);
        }
    }

    fn warn(&self, message: impl std::fmt::Display) {
        if let Some(ctx) = &self.ctx {
            ctx.warn(message);
        }
    }
}

impl Default for PresenceModule {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Module for PresenceModule {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "presence".to_string(),
            name: "Presence".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Sleeps the display after inactivity; wakes it on motion. Hiding this \
                module in the phone pauses auto-sleep."
                .to_string(),
            icon: Some("motion".to_string()),
            default_region: Region::TopLeft,
            default_order: 90,
            // Visible-but-viewless: the host only ticks visible modules, and the
            // phone toggle doubles as the auto-sleep kill switch.
            default_visible: true,
            config_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "gpio_chip": { "type": "string", "title": "GPIO chip", "default": "/dev/gpiochip0" },
                    "gpio_pin": {
                        "type": "integer", "title": "GPIO pin (BCM)",
                        "default": 21, "minimum": 0, "maximum": 53
                    },
                    "active_high": {
                        "type": "boolean", "title": "Motion is HIGH", "default": true,
                        "description": "Sensors like the HC-SR501 drive HIGH on motion."
                    },
                    "timeout_secs": {
                        "type": "integer", "title": "Sleep after (seconds)",
                        "default": 30, "minimum": 5, "maximum": 3600
                    },
                    "display_driver": {
                        "type": "string", "title": "Display driver",
                        "enum": ["auto", "pi_vcgencmd", "pi_cec", "macos", "noop"],
                        "default": "auto"
                    }
                }
            }),
        }
    }

    fn update_interval(&self) -> Duration {
        UPDATE_INTERVAL
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::action(
                "display_on",
                "Display on",
                "Wake the display now. Motion keeps it awake as usual.",
            ),
            ToolDescriptor::action(
                "display_off",
                "Display off",
                "Sleep the display now. The next motion wakes it again.",
            ),
            ToolDescriptor::query(
                "status",
                "Get presence status",
                "Read the sensor state: whether motion is detected, how long since the last \
                 motion, and whether the display is on.",
            ),
            ToolDescriptor::action(
                "set_display",
                "Set display power",
                "Force the display on or off. Auto-sleep still applies: the next motion wakes \
                 the display, and quiet periods put it back to sleep.",
            )
            .params(json!({
                "type": "object",
                "properties": {
                    "on": { "type": "boolean", "description": "Turn the display on (true) or off (false)." }
                },
                "required": ["on"]
            })),
        ]
    }

    async fn init(&mut self, ctx: ModuleCtx) -> anyhow::Result<()> {
        self.ctx = Some(ctx);
        Ok(())
    }

    /// Runs every second: read the sensor, extend the wake window on motion,
    /// and flip the display only when the state actually changes.
    async fn update(&mut self) -> anyhow::Result<()> {
        let motion = match self.read_motion() {
            Ok(motion) => {
                self.status = ModuleStatus::Ok;
                motion
            }
            Err(e) => {
                // No sensor on this machine (e.g. developing on macOS): stay
                // degraded and make no decisions.
                self.status = ModuleStatus::Degraded {
                    message: e.to_string(),
                };
                return Ok(());
            }
        };

        self.motion = Some(motion);
        if motion {
            self.last_motion = Some(Instant::now());
            self.set_display_power(true);
        } else if self.display_on {
            let asleep_for = self
                .last_motion
                .map(|last| last.elapsed())
                .unwrap_or(Duration::MAX);
            if asleep_for >= Duration::from_secs(u64::from(self.config.timeout_secs)) {
                self.set_display_power(false);
            }
        }

        Ok(())
    }

    async fn on_config(&mut self, config: Value) -> anyhow::Result<()> {
        self.config = if config.is_null() {
            PresenceConfig::default()
        } else {
            serde_json::from_value(config)?
        };
        // Fresh grace period: don't sleep immediately after a config change.
        self.last_motion = Some(Instant::now());
        Ok(())
    }

    async fn call_tool(&mut self, tool: &str, args: Value) -> anyhow::Result<Value> {
        match tool {
            "status" => Ok(self.status_json()),
            "display_on" => {
                self.set_display_power(true);
                Ok(self.status_json())
            }
            "display_off" => {
                self.set_display_power(false);
                Ok(self.status_json())
            }
            "set_display" => {
                let on = args
                    .get("on")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| anyhow::anyhow!("missing required boolean `on`"))?;
                self.set_display_power(on);
                Ok(self.status_json())
            }
            other => anyhow::bail!("unknown tool `{other}`"),
        }
    }

    fn config(&self) -> Option<Value> {
        serde_json::to_value(&self.config).ok()
    }

    fn visible(&self) -> bool {
        self.visible
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    fn view(&self) -> ViewNode {
        // Headless module: it controls the display, it doesn't render on it.
        ViewNode::Empty
    }

    fn status(&self) -> ModuleStatus {
        self.status.clone()
    }
}

/// Whether `program` exists on `PATH`.
fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_a_pir_on_gpio21() {
        let config = PresenceConfig::default();
        assert_eq!(config.gpio_pin, 21);
        assert!(config.active_high);
        assert_eq!(config.timeout_secs, 30);
        assert_eq!(config.display_driver, "auto");
    }

    #[test]
    fn explicit_driver_beats_detection() {
        let mut module = PresenceModule::new();
        module.config.display_driver = "pi_cec".to_string();
        assert_eq!(module.resolve_driver(), "pi_cec");
        module.config.display_driver = "noop".to_string();
        assert_eq!(module.resolve_driver(), "noop");
    }

    #[test]
    fn config_survives_a_partial_update() {
        let config: PresenceConfig =
            serde_json::from_value(json!({ "gpio_pin": 17, "timeout_secs": 60 })).unwrap();
        // Fields not sent fall back to defaults, so a phone form can send one
        // field without erasing the rest.
        assert_eq!(config.gpio_pin, 17);
        assert_eq!(config.timeout_secs, 60);
        assert_eq!(config.gpio_chip, "/dev/gpiochip0");
    }
}
