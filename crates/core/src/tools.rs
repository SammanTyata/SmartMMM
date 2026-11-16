//! Core-owned tools and argument helpers.
//!
//! These are the capabilities the core provides *for every module*, so no module
//! has to reimplement them. Visibility in particular is core state: a module
//! cannot meaningfully implement "hide me" better than the core can.
//!
//! # `x-options`
//!
//! A module id is a string whose valid values the core knows and the schema
//! cannot enumerate, because modules are registered at runtime. Schemas here
//! therefore carry a small JSON Schema extension:
//!
//! ```json
//! { "type": "string", "x-options": "modules" }
//! ```
//!
//! `x-options: "modules"` tells a UI to offer *the current module ids* instead of
//! a free-text box. `x-` prefixed keywords are the conventional extension point,
//! so this stays valid JSON Schema and is ignored by anything that does not know
//! it. The core still validates the value that arrives.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use smmm_protocol::{Region, ToolDescriptor};

/// Every core-owned tool, in the order they are presented to callers.
pub fn core_tools() -> Vec<ToolDescriptor> {
    let regions: Vec<&str> = Region::ALL.iter().map(|region| region.as_str()).collect();

    vec![
        ToolDescriptor::query(
            "list_modules",
            "List modules",
            "List every module on the mirror with its id, name, visibility, region and health. \
             Call this first when you need a module id.",
        )
        .returns(json!({ "type": "array", "items": { "type": "object" } })),
        ToolDescriptor::query(
            "get_status",
            "Get status",
            "Report the health of one module, or of the whole mirror when no id is given.",
        )
        .params(json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "x-options": "modules",
                    "description": "Module id. Omit for the whole mirror."
                }
            }
        })),
        ToolDescriptor::action(
            "show_module",
            "Show a module",
            "Make a module visible on the mirror. It resumes refreshing.",
        )
        .params(json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "x-options": "modules",
                    "description": "Module id, e.g. \"clock\"."
                }
            },
            "required": ["id"]
        })),
        ToolDescriptor::action(
            "hide_module",
            "Hide a module",
            "Hide a module so it stops rendering and stops refreshing. Its configuration is kept, \
             so it can be shown again later.",
        )
        .params(json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "x-options": "modules",
                    "description": "Module id, e.g. \"clock\"."
                }
            },
            "required": ["id"]
        })),
        ToolDescriptor::action(
            "set_region",
            "Move a module",
            "Move a module to a different region of the mirror.",
        )
        .params(json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "x-options": "modules" },
                "region": { "type": "string", "enum": regions }
            },
            "required": ["id", "region"]
        })),
        ToolDescriptor::action(
            "set_order",
            "Reorder a module",
            "Set a module's sort order within its region. Lower values come first.",
        )
        .params(json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "x-options": "modules" },
                "order": { "type": "integer", "description": "Sort order; lower comes first." }
            },
            "required": ["id", "order"]
        })),
    ]
}

// ---------------------------------------------------------------------------
// Argument helpers
//
// Tools receive untyped JSON, so every core tool validates its own arguments and
// refuses with a message that names the offending key. That message is what a
// model gets back, so it is worth writing carefully.
// ---------------------------------------------------------------------------

pub(crate) fn required<'a>(args: &'a Value, key: &str) -> Result<&'a Value> {
    args.get(key)
        .ok_or_else(|| anyhow!("missing required argument `{key}`"))
}

pub(crate) fn required_str(args: &Value, key: &str) -> Result<String> {
    required(args, key)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("argument `{key}` must be a string"))
}

pub(crate) fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}
