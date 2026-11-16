//! Clock module: current time and date.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smmm_core::Module;
use smmm_protocol::{Align, Manifest, Region, TextSize, ToolDescriptor, ViewNode, Weight};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ClockConfig {
    /// Optional caption above the time.
    pub label: Option<String>,
    pub format_24h: bool,
    pub show_seconds: bool,
    pub show_date: bool,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            label: None,
            format_24h: true,
            show_seconds: false,
            show_date: true,
        }
    }
}

pub struct ClockModule {
    config: ClockConfig,
    visible: bool,
    view: ViewNode,
}

impl ClockModule {
    pub fn new() -> Self {
        Self {
            config: ClockConfig::default(),
            visible: true,
            view: ViewNode::Empty,
        }
    }

    fn time_format(&self) -> &'static str {
        match (self.config.format_24h, self.config.show_seconds) {
            (true, true) => "%H:%M:%S",
            (true, false) => "%H:%M",
            (false, true) => "%I:%M:%S %p",
            (false, false) => "%I:%M %p",
        }
    }

    fn time_string(&self) -> String {
        Local::now().format(self.time_format()).to_string()
    }

    fn date_string(&self) -> String {
        Local::now().format("%A, %d %B").to_string()
    }

    /// Machine-readable form of what the module is currently showing. This is
    /// what `clock__get_time` returns, so it must not depend on the view tree.
    fn snapshot(&self) -> Value {
        json!({
            "time": self.time_string(),
            "date": self.date_string(),
            "label": self.config.label,
            "format_24h": self.config.format_24h,
            "show_seconds": self.config.show_seconds,
            "show_date": self.config.show_date,
        })
    }

    fn render(&self) -> ViewNode {
        let mut children = Vec::new();

        if let Some(label) = self.config.label.as_ref().filter(|label| !label.is_empty()) {
            children.push(ViewNode::styled(
                label.clone(),
                TextSize::Sm,
                Weight::Light,
                Align::Center,
            ));
        }

        children.push(ViewNode::styled(
            self.time_string(),
            TextSize::Xxl,
            Weight::Light,
            Align::Center,
        ));

        if self.config.show_date {
            children.push(ViewNode::styled(
                self.date_string(),
                TextSize::Md,
                Weight::Normal,
                Align::Center,
            ));
        }

        ViewNode::column(4.0, children)
    }
}

impl Default for ClockModule {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Module for ClockModule {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "clock".to_string(),
            name: "Clock".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Current time and date.".to_string(),
            icon: Some("clock".to_string()),
            default_region: Region::TopLeft,
            default_order: 0,
            default_visible: true,
            config_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "label": {
                        "type": ["string", "null"],
                        "title": "Label",
                        "description": "Optional caption shown above the time."
                    },
                    "format_24h": { "type": "boolean", "title": "24-hour clock", "default": true },
                    "show_seconds": { "type": "boolean", "title": "Show seconds", "default": false },
                    "show_date": { "type": "boolean", "title": "Show date", "default": true }
                }
            }),
        }
    }

    fn update_interval(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::query(
                "get_time",
                "Get time",
                "Read the current local time and date that the mirror is showing.",
            )
            .returns(json!({
                "type": "object",
                "properties": {
                    "time": { "type": "string" },
                    "date": { "type": "string" }
                }
            })),
            ToolDescriptor::action(
                "set_format",
                "Set clock format",
                "Change how the clock is formatted. Any omitted field is left unchanged.",
            )
            .params(json!({
                "type": "object",
                "properties": {
                    "format_24h": { "type": "boolean", "description": "Use 24-hour time." },
                    "show_seconds": { "type": "boolean", "description": "Include seconds." },
                    "show_date": { "type": "boolean", "description": "Show the date line." }
                }
            })),
        ]
    }

    async fn call_tool(&mut self, tool: &str, args: Value) -> anyhow::Result<Value> {
        match tool {
            "get_time" => Ok(self.snapshot()),

            "set_format" => {
                if let Some(value) = args.get("format_24h").and_then(Value::as_bool) {
                    self.config.format_24h = value;
                }
                if let Some(value) = args.get("show_seconds").and_then(Value::as_bool) {
                    self.config.show_seconds = value;
                }
                if let Some(value) = args.get("show_date").and_then(Value::as_bool) {
                    self.config.show_date = value;
                }
                self.view = self.render();
                Ok(self.snapshot())
            }

            other => anyhow::bail!("unknown tool `{other}`"),
        }
    }

    fn config(&self) -> Option<Value> {
        // Report the running config: `set_format` mutates it from a tool, and at
        // startup it may hold Rust-side defaults the config file never mentioned.
        serde_json::to_value(&self.config).ok()
    }

    async fn update(&mut self) -> anyhow::Result<()> {
        self.view = self.render();
        Ok(())
    }

    async fn on_config(&mut self, config: Value) -> anyhow::Result<()> {
        self.config = if config.is_null() {
            ClockConfig::default()
        } else {
            serde_json::from_value(config)?
        };
        self.view = self.render();
        Ok(())
    }

    fn visible(&self) -> bool {
        self.visible
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    fn view(&self) -> ViewNode {
        self.view.clone()
    }
}
