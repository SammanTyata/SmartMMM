//! Shared contract between the core, the mirror renderer and the phone client.
//!
//! Everything in here is `serde`-serialisable and has no dependencies beyond
//! `serde`/`serde_json`, so it can be reused by any future client (native phone
//! app, CLI, tests) without pulling in the core.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Where a module draws on the mirror. Mirrors the familiar region model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    TopLeft,
    TopCenter,
    TopRight,
    UpperThird,
    #[default]
    MiddleCenter,
    LowerThird,
    BottomLeft,
    BottomCenter,
    BottomRight,
    Fullscreen,
}

impl Region {
    /// All regions, in the order the renderer lays them out.
    pub const ALL: [Region; 10] = [
        Region::TopLeft,
        Region::TopCenter,
        Region::TopRight,
        Region::UpperThird,
        Region::MiddleCenter,
        Region::LowerThird,
        Region::BottomLeft,
        Region::BottomCenter,
        Region::BottomRight,
        Region::Fullscreen,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Region::TopLeft => "top_left",
            Region::TopCenter => "top_center",
            Region::TopRight => "top_right",
            Region::UpperThird => "upper_third",
            Region::MiddleCenter => "middle_center",
            Region::LowerThird => "lower_third",
            Region::BottomLeft => "bottom_left",
            Region::BottomCenter => "bottom_center",
            Region::BottomRight => "bottom_right",
            Region::Fullscreen => "fullscreen",
        }
    }
}

// ---------------------------------------------------------------------------
// View tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TextSize {
    Sm,
    #[default]
    Md,
    Lg,
    Xl,
    Xxl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Weight {
    Light,
    #[default]
    Normal,
    Bold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    #[default]
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyValueItem {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The closed set of primitives a module may render.
///
/// Deliberately small: it keeps the renderer dumb, keeps every module visually
/// consistent, and makes the phone preview trivial. Modules that genuinely need
/// bespoke rendering opt into an asset bundle + sandboxed iframe (post-M0).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewNode {
    /// Renders nothing. Use for a hidden or not-yet-loaded module.
    #[default]
    Empty,
    Divider,
    Text {
        text: String,
        #[serde(default)]
        size: TextSize,
        #[serde(default)]
        weight: Weight,
        #[serde(default)]
        align: Align,
    },
    Stack {
        #[serde(default)]
        direction: Axis,
        #[serde(default)]
        gap: f32,
        children: Vec<ViewNode>,
    },
    KeyValue {
        items: Vec<KeyValueItem>,
    },
    List {
        items: Vec<ListItem>,
        #[serde(default)]
        numbered: bool,
    },
    /// `value` is clamped to `0.0..=1.0`.
    Progress {
        value: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// RFC 3339 timestamp to count down to.
    Countdown {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Image {
        src: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_height: Option<f32>,
    },
    Icon {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<f32>,
    },
}

impl ViewNode {
    pub fn text(text: impl Into<String>) -> Self {
        ViewNode::Text {
            text: text.into(),
            size: TextSize::default(),
            weight: Weight::default(),
            align: Align::default(),
        }
    }

    pub fn styled(text: impl Into<String>, size: TextSize, weight: Weight, align: Align) -> Self {
        ViewNode::Text {
            text: text.into(),
            size,
            weight,
            align,
        }
    }

    pub fn stack(direction: Axis, gap: f32, children: Vec<ViewNode>) -> Self {
        ViewNode::Stack {
            direction,
            gap,
            children,
        }
    }

    pub fn column(gap: f32, children: Vec<ViewNode>) -> Self {
        ViewNode::stack(Axis::Vertical, gap, children)
    }

    pub fn key_value(
        items: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        ViewNode::KeyValue {
            items: items
                .into_iter()
                .map(|(k, v)| KeyValueItem {
                    key: k.into(),
                    value: v.into(),
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Module metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Stable, unique, kebab-case identifier, e.g. `weather`.
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub default_region: Region,
    pub default_order: i32,
    pub default_visible: bool,
    /// JSON Schema for this module's `[modules.<id>.config]` table.
    #[serde(default)]
    pub config_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Whether a tool only reads state or changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads state. Safe to call repeatedly, no side effects.
    Query,
    /// Changes state.
    Action,
}

/// A capability a module declares.
///
/// This is the unit an LLM chooses between, the unit the HTTP tool API calls,
/// and the unit the phone renders. One definition, three consumers — so the
/// phone has no separate notion of what is "exposable": it renders everything,
/// and derives *how* from `params_schema`. No parameters means one tap; any
/// parameters means a generated form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Local name, unique within the module, e.g. `get_time`.
    pub name: String,
    /// Short human label, e.g. "Get time".
    pub title: String,
    /// LLM-facing prose: what it does and when a caller should use it.
    pub description: String,
    pub kind: ToolKind,
    /// JSON Schema for the arguments object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_schema: Option<serde_json::Value>,
    /// JSON Schema (informational) for the result, to help a model interpret it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns_schema: Option<serde_json::Value>,
    /// Callers should confirm before executing.
    #[serde(default)]
    pub dangerous: bool,
}

impl ToolDescriptor {
    pub fn query(
        name: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self::new(name, title, description, ToolKind::Query)
    }

    pub fn action(
        name: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self::new(name, title, description, ToolKind::Action)
    }

    fn new(
        name: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
        kind: ToolKind,
    ) -> Self {
        Self {
            name: name.into(),
            title: title.into(),
            description: description.into(),
            kind,
            params_schema: None,
            returns_schema: None,
            dangerous: false,
        }
    }

    pub fn params(mut self, schema: serde_json::Value) -> Self {
        self.params_schema = Some(schema);
        self
    }

    pub fn returns(mut self, schema: serde_json::Value) -> Self {
        self.returns_schema = Some(schema);
        self
    }

    pub fn dangerous(mut self) -> Self {
        self.dangerous = true;
        self
    }
}

/// The module id reserved for core-owned tools.
pub const CORE_MODULE: &str = "core";

/// A tool as exposed over the API, with its globally unique name.
///
/// Names are `<module>__<tool>` — a *double* underscore, because module ids and
/// local tool names may themselves contain single ones, and because
/// `[A-Za-z0-9_-]` is the only character class every mainstream tool-calling
/// API accepts (dots are rejected by OpenAI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Globally unique, e.g. `clock__get_time`.
    pub name: String,
    /// Owning module id, or `core` for built-ins.
    pub module: String,
    /// The module-local tool name.
    pub local_name: String,
    pub title: String,
    pub description: String,
    pub kind: ToolKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub dangerous: bool,
}

impl ToolInfo {
    pub fn namespaced(module: &str, local_name: &str) -> String {
        format!("{module}__{local_name}")
    }

    pub fn from_descriptor(module: &str, descriptor: &ToolDescriptor) -> Self {
        Self {
            name: Self::namespaced(module, &descriptor.name),
            module: module.to_string(),
            local_name: descriptor.name.clone(),
            title: descriptor.title.clone(),
            description: descriptor.description.clone(),
            kind: descriptor.kind,
            params_schema: descriptor.params_schema.clone(),
            returns_schema: descriptor.returns_schema.clone(),
            dangerous: descriptor.dangerous,
        }
    }
}

// ---------------------------------------------------------------------------
// Live module state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ModuleStatus {
    #[default]
    Starting,
    Ok,
    Degraded {
        message: String,
    },
    Error {
        message: String,
    },
}

impl ModuleStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, ModuleStatus::Ok)
    }
}

/// Full state of one module as held by the core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleState {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub visible: bool,
    pub region: Region,
    pub order: i32,
    pub status: ModuleStatus,
    pub view: ViewNode,
    pub config: serde_json::Value,
    pub config_schema: serde_json::Value,
    pub tools: Vec<ToolInfo>,
    /// Monotonic revision. Clients detect a gap and resync.
    pub revision: u64,
    /// RFC 3339.
    pub updated_at: String,
}

/// A partial update to a [`ModuleState`]. Only `Some` fields are applied.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModulePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ModuleStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ViewNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Revision assigned by the core when the patch is applied.
    pub revision: u64,
}

impl ModuleState {
    /// Build a full-snapshot patch (used for the initial publish).
    pub fn to_patch(&self) -> ModulePatch {
        ModulePatch {
            visible: Some(self.visible),
            region: Some(self.region),
            order: Some(self.order),
            status: Some(self.status.clone()),
            view: Some(self.view.clone()),
            config: Some(self.config.clone()),
            revision: self.revision,
        }
    }
}

// ---------------------------------------------------------------------------
// REST payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoResponse {
    pub name: String,
    pub version: String,
    pub platform: String,
    pub hostname: String,
    pub uptime_secs: u64,
    pub module_count: usize,
    pub auth_required: bool,
}

/// Body of `PATCH /api/v1/modules/{id}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModuleUpdateRequest {
    pub visible: Option<bool>,
    pub region: Option<Region>,
    pub order: Option<i32>,
    pub config: Option<serde_json::Value>,
}

/// Response to a tool call. Tool *failures* are returned here as results (with
/// `ok: false`), not as HTTP errors, so a model can read the message and retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

// ---------------------------------------------------------------------------
// WebSocket frames  (`WS /api/v1/events`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        #[serde(default)]
        token: Option<String>,
        /// Last revision the client has seen, for gap detection.
        #[serde(default)]
        since: Option<u64>,
    },
    Subscribe {
        #[serde(default)]
        channels: Vec<String>,
    },
    CallTool {
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    SetVisible {
        id: String,
        value: bool,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        revision: u64,
        modules: Vec<ModuleState>,
    },
    Patch {
        id: String,
        patch: ModulePatch,
    },
    Removed {
        id: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Toast {
        kind: String,
        message: String,
    },
    Pong,
    Error {
        message: String,
    },
}
