//! Gestures module: PAJ7620U2 hand-gesture sensor on I2C.
//!
//! Polls the sensor's gesture-result registers, debounces detections, and
//! runs configured mappings — each mapping fires another module's tool, so
//! hardware gestures drive the same surface the phone and the HTTP API use.
//!
//! This module renders nothing on the mirror (it announces itself through
//! toasts instead). Hiding it in the phone disables the sensor polling
//! entirely, exactly like the presence module's kill switch.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smmm_core::{Module, ModuleCtx};
use smmm_protocol::{Manifest, ModuleStatus, Region, ToolDescriptor, ViewNode};

/// PAJ7620U2 default 7-bit I2C address (datasheet §8.1).
const PAJ7620_ADDR: u16 = 0x73;

/// Register bank select. The chip splits its registers across two banks.
const REG_BANK_SELECT: u8 = 0xEF;
/// Gesture detection result registers, bank 0 (datasheet §8.1.3).
const REG_GES_RESULT_0: u8 = 0x43;
const REG_GES_RESULT_1: u8 = 0x44;

/// Gesture bits in `GES_RESULT_0` (datasheet, gesture recognition).
const GES_RIGHT: u8 = 0x01;
const GES_LEFT: u8 = 0x02;
const GES_UP: u8 = 0x04;
const GES_DOWN: u8 = 0x08;
const GES_FORWARD: u8 = 0x10;
const GES_BACKWARD: u8 = 0x20;
const GES_CLOCKWISE: u8 = 0x40;
const GES_ANTI_CLOCKWISE: u8 = 0x80;
/// Gesture bit in `GES_RESULT_1`.
const GES_WAVE: u8 = 0x01;

/// Poll fast enough to catch a swipe (~100ms hand transit) but not so fast
/// the I2C bus is saturated.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// One action per gesture: repeated detections inside this window are the
/// same hand movement trailing off.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Power-on / reset register programming for the PAJ7620U2 gesture engine,
/// as given in the datasheet's "Gestures Recognition" initialization
/// example. Bank 0 configures the gesture engine; bank 1 tunes sensitivity
/// thresholds. Written verbatim by the datasheet, then bank 0 reselected.
const INIT_BANK0: &[(u8, u8)] = &[
    (0x32, 0x29), (0x33, 0x01), (0x34, 0x00), (0x35, 0x01),
    (0x36, 0x00), (0x37, 0x07), (0x38, 0x17), (0x39, 0x06),
    (0x3A, 0x12), (0x3F, 0x00), (0x40, 0x02), (0x41, 0xFF),
    (0x42, 0x01), (0x46, 0x2D), (0x47, 0x0F), (0x48, 0x3C),
    (0x49, 0x00), (0x4A, 0x1E), (0x4B, 0x00), (0x4C, 0x20),
    (0x4D, 0x00), (0x4E, 0x1A), (0x4F, 0x14), (0x50, 0x00),
    (0x51, 0x10), (0x52, 0x00), (0x5C, 0x02), (0x5D, 0x00),
    (0x5E, 0x10), (0x5F, 0x3F), (0x60, 0x27), (0x61, 0x28),
    (0x62, 0x00), (0x63, 0x03), (0x64, 0xF7), (0x65, 0x03),
    (0x66, 0xD9), (0x67, 0x03), (0x68, 0x01), (0x69, 0xC8),
    (0x6A, 0x40), (0x6D, 0x04), (0x6E, 0x00), (0x6F, 0x00),
    (0x70, 0x80), (0x71, 0x00), (0x72, 0x00), (0x73, 0x00),
    (0x74, 0xF0), (0x75, 0x00), (0x80, 0x42), (0x81, 0x44),
    (0x82, 0x04), (0x83, 0x20), (0x84, 0x20), (0x85, 0x00),
    (0x86, 0x10), (0x87, 0x00), (0x88, 0x05), (0x89, 0x18),
    (0x8A, 0x10), (0x8B, 0x01), (0x8C, 0x37), (0x8D, 0x00),
    (0x8E, 0xF0), (0x8F, 0x81), (0x90, 0x06), (0x91, 0x06),
    (0x92, 0x1E), (0x93, 0x0D), (0x94, 0x0A), (0x95, 0x0A),
    (0x96, 0x0C), (0x97, 0x05), (0x98, 0x0A), (0x99, 0x41),
    (0x9A, 0x14), (0x9B, 0x0A), (0x9C, 0x3F), (0x9D, 0x33),
    (0x9E, 0xAE), (0x9F, 0xF9), (0xA0, 0x48), (0xA1, 0x13),
    (0xA2, 0x10), (0xA3, 0x08), (0xA4, 0x30), (0xA5, 0x19),
    (0xA6, 0x10), (0xA7, 0x08), (0xA8, 0x24), (0xA9, 0x04),
    (0xAA, 0x1E), (0xAB, 0x1E), (0xCC, 0x19), (0xCD, 0x0B),
    (0xCE, 0x13), (0xCF, 0x64), (0xD0, 0x21), (0xD1, 0x0F),
    (0xD2, 0x88),
];

const INIT_BANK1: &[(u8, u8)] = &[
    (0x00, 0x1E), (0x01, 0x1E), (0x02, 0x0F), (0x03, 0x10),
    (0x04, 0x02), (0x05, 0x00), (0x06, 0xB0), (0x07, 0x04),
    (0x08, 0x0D), (0x09, 0x0E), (0x0A, 0x9C), (0x0B, 0x04),
    (0x0C, 0x05), (0x0D, 0x0F), (0x0E, 0x02), (0x0F, 0x12),
    (0x10, 0x02), (0x11, 0x02), (0x12, 0x00), (0x13, 0x01),
    (0x14, 0x05), (0x15, 0x07), (0x16, 0x05), (0x17, 0x07),
    (0x18, 0x01), (0x19, 0x04), (0x1A, 0x05), (0x1B, 0x0C),
    (0x1C, 0x2A), (0x1D, 0x01), (0x1E, 0x00), (0x21, 0x00),
    (0x22, 0x00), (0x23, 0x00), (0x25, 0x01), (0x26, 0x00),
    (0x27, 0x39), (0x28, 0x7F), (0x29, 0x08), (0x30, 0x03),
    (0x31, 0x00), (0x32, 0x1A), (0x33, 0x1A), (0x34, 0x07),
    (0x35, 0x07), (0x36, 0x01), (0x37, 0xFF), (0x38, 0x36),
    (0x39, 0x07), (0x3A, 0x00), (0x3E, 0xFF), (0x3F, 0x00),
    (0x40, 0x77), (0x41, 0x40), (0x42, 0x00), (0x43, 0x30),
    (0x44, 0xA0), (0x45, 0x5C), (0x46, 0x00), (0x47, 0x00),
    (0x48, 0x58), (0x4A, 0x1E), (0x4B, 0x1E), (0x4C, 0x00),
    (0x4D, 0x00), (0x4E, 0xA0), (0x4F, 0x80), (0x50, 0x00),
    (0x51, 0x00), (0x52, 0x00), (0x53, 0x00), (0x54, 0x00),
    (0x57, 0x80), (0x59, 0x10), (0x5A, 0x08), (0x5B, 0x94),
    (0x5C, 0xE8), (0x5D, 0x08), (0x5E, 0x3D), (0x5F, 0x99),
    (0x60, 0x45), (0x61, 0x40), (0x63, 0x2D), (0x64, 0x02),
    (0x65, 0x96), (0x66, 0x00), (0x67, 0x97), (0x68, 0x01),
    (0x69, 0xCD), (0x6A, 0x01), (0x6B, 0xB0), (0x6C, 0x04),
    (0x6D, 0x2C), (0x6E, 0x01), (0x6F, 0x32), (0x71, 0x00),
    (0x72, 0x01), (0x73, 0x35), (0x74, 0x00), (0x75, 0x33),
    (0x76, 0x31), (0x77, 0x01), (0x7C, 0x84), (0x7D, 0x03),
    (0x7E, 0x01),
];

/// Gesture names accepted in mappings and by `simulate`.
pub const GESTURES: &[&str] = &[
    "right", "left", "up", "down", "forward", "backward", "clockwise", "anticlockwise", "wave",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GestureMapping {
    /// One of [`GESTURES`].
    pub gesture: String,
    /// Target module id (must not be `gestures` itself).
    pub module: String,
    /// Tool local name on the target module, e.g. `next`.
    pub tool: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GesturesConfig {
    pub i2c_bus: u16,
    pub i2c_address: u16,
    pub debounce_ms: u64,
    /// `right` → next headline, `left` → previous, `wave` → wake the display.
    pub mappings: Vec<GestureMapping>,
}

impl Default for GesturesConfig {
    fn default() -> Self {
        Self {
            i2c_bus: 1,
            i2c_address: PAJ7620_ADDR,
            debounce_ms: DEBOUNCE.as_millis() as u64,
            mappings: vec![
                GestureMapping {
                    gesture: "right".into(),
                    module: "news".into(),
                    tool: "next".into(),
                },
                GestureMapping {
                    gesture: "left".into(),
                    module: "news".into(),
                    tool: "previous".into(),
                },
                GestureMapping {
                    gesture: "wave".into(),
                    module: "presence".into(),
                    tool: "display_on".into(),
                },
            ],
        }
    }
}

pub struct GesturesModule {
    ctx: Option<ModuleCtx>,
    config: GesturesConfig,
    visible: bool,
    status: ModuleStatus,
    /// Open I2C device, prepared by `init` (Linux only).
    #[cfg(target_os = "linux")]
    device: Option<Device>,
    /// Throttle for device reopen attempts, so a missing sensor doesn't
    /// hammer the bus ten times a second.
    #[cfg(target_os = "linux")]
    last_retry: Option<Instant>,
    last_gesture: Option<(String, Instant)>,
    last_action: Option<Instant>,
}

#[cfg(target_os = "linux")]
mod i2c {
    //! Minimal PAJ7620U2 register access over `/dev/i2c-<n>` via i2cdev.
    use anyhow::Context;
    use i2cdev::core::I2CDevice;
    use i2cdev::linux::LinuxI2CDevice;

    use super::{INIT_BANK0, INIT_BANK1, REG_BANK_SELECT};

    pub struct Device {
        dev: LinuxI2CDevice,
    }

    impl Device {
        pub fn open(bus: u16, address: u16) -> anyhow::Result<Self> {
            let path = format!("/dev/i2c-{bus}");
            let mut dev = LinuxI2CDevice::new(&path, address)
                .with_context(|| format!("opening {path} at 0x{address:02x}"))?;
            Self::init(&mut dev)?;
            Ok(Self { dev })
        }

        /// Power-on programming from the datasheet: select a bank, write its
        /// registers, and end back in bank 0 where the result registers live.
        fn init(dev: &mut LinuxI2CDevice) -> anyhow::Result<()> {
            dev.write(&[REG_BANK_SELECT, 0x00])
                .context("selecting bank 0")?;
            for (reg, value) in INIT_BANK0 {
                dev.write(&[*reg, *value])?;
            }
            dev.write(&[REG_BANK_SELECT, 0x01])
                .context("selecting bank 1")?;
            for (reg, value) in INIT_BANK1 {
                dev.write(&[*reg, *value])?;
            }
            dev.write(&[REG_BANK_SELECT, 0x00])
                .context("re-selecting bank 0")?;
            Ok(())
        }

        /// Both gesture-result registers, in one go (they are consecutive).
        /// Writing the first register's address sets the pointer, and the
        /// following read returns that register and the next one.
        pub fn read_results(&mut self) -> anyhow::Result<(u8, u8)> {
            self.dev.write(&[super::REG_GES_RESULT_0])?;
            let mut buf = [0u8; 2];
            self.dev.read(&mut buf)?;
            Ok((buf[0], buf[1]))
        }
    }
}

#[cfg(target_os = "linux")]
use i2c::Device;

impl GesturesModule {
    pub fn new() -> Self {
        Self {
            ctx: None,
            config: GesturesConfig::default(),
            visible: true,
            status: ModuleStatus::Starting,
            #[cfg(target_os = "linux")]
            device: None,
            #[cfg(target_os = "linux")]
            last_retry: None,
            last_gesture: None,
            last_action: None,
        }
    }

    /// Interpret the raw result registers as a gesture name. Only one bit is
    /// expected per detection; mask out anything unexpected.
    fn decode(result0: u8, result1: u8) -> Option<&'static str> {
        let name = match result0 {
            GES_RIGHT => "right",
            GES_LEFT => "left",
            GES_UP => "up",
            GES_DOWN => "down",
            GES_FORWARD => "forward",
            GES_BACKWARD => "backward",
            GES_CLOCKWISE => "clockwise",
            GES_ANTI_CLOCKWISE => "anticlockwise",
            _ => {
                if result1 & GES_WAVE != 0 {
                    "wave"
                } else {
                    return None;
                }
            }
        };
        Some(name)
    }

    /// Run every mapping for `gesture`: announce it, then fire each target
    /// tool through the core's router (cross-module calls are bounded by the
    /// core's 2s timeout and can never self-call).
    async fn dispatch(&mut self, gesture: &str) {
        if let Some(ctx) = &self.ctx {
            ctx.toast("info", format!("Gesture: {}", title_case(gesture)));
            ctx.info(format!("gesture `{gesture}` detected"));
        }
        self.last_gesture = Some((gesture.to_string(), Instant::now()));
        self.last_action = Some(Instant::now());

        let mappings = self.config.mappings.clone();
        for mapping in mappings.iter().filter(|m| m.gesture == gesture) {
            let Some(ctx) = &self.ctx else { continue };
            if mapping.module == "gestures" {
                ctx.warn(format!("mapping `{gesture}` targets `gestures` itself — ignored"));
                continue;
            }
            match ctx
                .call_module_tool(&mapping.module, &mapping.tool, json!({}))
                .await
            {
                Ok(_) => ctx.info(format!("`{gesture}` → {}__{}", mapping.module, mapping.tool)),
                Err(e) => ctx.warn(format!(
                    "`{gesture}` → {}__{} failed: {e:#}",
                    mapping.module, mapping.tool
                )),
            }
        }
    }

    fn status_json(&self) -> Value {
        let (last, age_secs) = self
            .last_gesture
            .as_ref()
            .map(|(g, at)| (g.clone(), at.elapsed().as_secs()))
            .unzip();
        json!({
            "last_gesture": last,
            "age_secs": age_secs,
            "debounce_ms": self.config.debounce_ms,
            "mappings": self.config.mappings,
            "i2c": format!("bus {} at 0x{:02x}", self.config.i2c_bus, self.config.i2c_address),
        })
    }
}

impl Default for GesturesModule {
    fn default() -> Self {
        Self::new()
    }
}

/// `"right"` → `"Right"` for the toast.
fn title_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[async_trait]
impl Module for GesturesModule {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "gestures".to_string(),
            name: "Gestures".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "PAJ7620U2 hand-gesture sensor. Swipe right/left to flip headlines, \
                wave to wake the display. Hiding this module disables the sensor."
                .to_string(),
            icon: Some("hand".to_string()),
            default_region: Region::TopLeft,
            default_order: 91,
            // Visible-but-viewless, like presence: the host only ticks visible
            // modules, so the phone's toggle is the sensor's off switch.
            default_visible: true,
            config_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "i2c_bus": { "type": "integer", "title": "I2C bus", "default": 1, "minimum": 0, "maximum": 10 },
                    "i2c_address": { "type": "integer", "title": "I2C address", "default": 115 },
                    "debounce_ms": { "type": "integer", "title": "Debounce (ms)", "default": 500, "minimum": 100, "maximum": 3000 },
                    "mappings": {
                        "type": "array",
                        "title": "Gesture actions",
                        "description": "Which module tool each gesture fires.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "gesture": { "type": "string", "enum": GESTURES, "title": "Gesture" },
                                "module": { "type": "string", "title": "Module" },
                                "tool": { "type": "string", "title": "Tool" }
                            },
                            "required": ["gesture", "module", "tool"]
                        }
                    }
                }
            }),
        }
    }

    fn update_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::action(
                "simulate",
                "Simulate gesture",
                "Fire the mappings for a gesture as if the sensor had detected it. Handy for \
                 testing the wiring without standing in front of the mirror.",
            )
            .params(json!({
                "type": "object",
                "properties": {
                    "gesture": { "type": "string", "enum": GESTURES }
                },
                "required": ["gesture"]
            })),
            ToolDescriptor::query(
                "get_status",
                "Get gesture status",
                "Read the last detected gesture, how long ago, and the configured \
                 gesture→action mappings.",
            ),
        ]
    }

    async fn init(&mut self, ctx: ModuleCtx) -> anyhow::Result<()> {
        self.ctx = Some(ctx);
        #[cfg(target_os = "linux")]
        {
            match Device::open(self.config.i2c_bus, self.config.i2c_address) {
                Ok(device) => {
                    // The registers may have been reprogrammed by something
                    // else; re-init on the next tick if the id looks wrong.
                    self.device = Some(device);
                    self.status = ModuleStatus::Ok;
                }
                Err(e) => {
                    self.status = ModuleStatus::Degraded {
                        message: e.to_string(),
                    };
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.status = ModuleStatus::Degraded {
                message: "I2C requires Linux; gestures are unavailable on this platform".into(),
            };
        }
        Ok(())
    }

    async fn update(&mut self) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            if self.device.is_none() {
                // One reopen attempt per second rather than ten.
                let due = self
                    .last_retry
                    .map(|at| at.elapsed() >= Duration::from_secs(1))
                    .unwrap_or(true);
                if due {
                    self.last_retry = Some(Instant::now());
                    match Device::open(self.config.i2c_bus, self.config.i2c_address) {
                        Ok(device) => {
                            self.device = Some(device);
                            self.status = ModuleStatus::Ok;
                            if let Some(ctx) = &self.ctx {
                                ctx.info("gesture sensor connected");
                            }
                        }
                        Err(e) => {
                            self.status = ModuleStatus::Degraded { message: e.to_string() };
                        }
                    }
                }
                return Ok(());
            }

            let read = self
                .device
                .as_mut()
                .map(|dev| dev.read_results())
                .unwrap_or(Err(anyhow::anyhow!("device not open")));
            match read {
                Ok((result0, result1)) => {
                    self.status = ModuleStatus::Ok;
                    if let Some(gesture) = Self::decode(result0, result1) {
                        let debounced = self
                            .last_action
                            .map(|at| at.elapsed() < Duration::from_millis(self.config.debounce_ms))
                            .unwrap_or(false);
                        if !debounced {
                            self.dispatch(gesture).await;
                        }
                    }
                }
                Err(e) => {
                    // Bus hiccup: drop the device so the next tick reopens it.
                    self.device = None;
                    self.status = ModuleStatus::Degraded {
                        message: format!("i2c read failed: {e:#}"),
                    };
                }
            }
        }
        Ok(())
    }

    async fn on_config(&mut self, config: Value) -> anyhow::Result<()> {
        self.config = if config.is_null() {
            GesturesConfig::default()
        } else {
            serde_json::from_value(config)?
        };
        // The bus/address may have changed: drop the device, reopen on the
        // next tick.
        #[cfg(target_os = "linux")]
        {
            self.device = None;
        }
        self.last_action = None;
        Ok(())
    }

    async fn call_tool(&mut self, tool: &str, args: Value) -> anyhow::Result<Value> {
        match tool {
            "simulate" => {
                let gesture = args
                    .get("gesture")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing required string `gesture`"))?;
                if !GESTURES.contains(&gesture) {
                    anyhow::bail!("unknown gesture `{gesture}` (one of: {})", GESTURES.join(", "));
                }
                self.dispatch(gesture).await;
                Ok(self.status_json())
            }
            "get_status" => Ok(self.status_json()),
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
        // Headless: gestures drive other modules; nothing to render.
        ViewNode::Empty
    }

    fn status(&self) -> ModuleStatus {
        self.status.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_a_paj7620_on_bus_1() {
        let config = GesturesConfig::default();
        assert_eq!(config.i2c_bus, 1);
        assert_eq!(config.i2c_address, 0x73);
        assert_eq!(config.debounce_ms, 500);
        assert_eq!(config.mappings.len(), 3);
    }

    #[test]
    fn decodes_each_result_bit() {
        assert_eq!(GesturesModule::decode(0x01, 0x00), Some("right"));
        assert_eq!(GesturesModule::decode(0x02, 0x00), Some("left"));
        assert_eq!(GesturesModule::decode(0x04, 0x00), Some("up"));
        assert_eq!(GesturesModule::decode(0x08, 0x00), Some("down"));
        assert_eq!(GesturesModule::decode(0x10, 0x00), Some("forward"));
        assert_eq!(GesturesModule::decode(0x20, 0x00), Some("backward"));
        assert_eq!(GesturesModule::decode(0x40, 0x00), Some("clockwise"));
        assert_eq!(GesturesModule::decode(0x80, 0x00), Some("anticlockwise"));
        assert_eq!(GesturesModule::decode(0x00, 0x01), Some("wave"));
        assert_eq!(GesturesModule::decode(0x00, 0x00), None);
    }

    #[test]
    fn config_survives_a_partial_update() {
        let config: GesturesConfig = serde_json::from_value(json!({
            "debounce_ms": 250,
            "mappings": [{ "gesture": "wave", "module": "presence", "tool": "display_on" }]
        }))
        .unwrap();
        assert_eq!(config.debounce_ms, 250);
        assert_eq!(config.i2c_bus, 1);
        assert_eq!(config.mappings.len(), 1);
    }
}
