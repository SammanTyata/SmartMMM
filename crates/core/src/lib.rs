//! The SmartMMM core: module contract, registry, state store and host runtime.
//!
//! Deliberately headless — it knows nothing about HTTP or rendering. The
//! `smmm-server` crate exposes it; the renderer and phone are just clients.

pub mod config;
pub mod host;
pub mod persist;
pub mod registry;
pub mod state;
pub mod tools;
pub mod traits;

pub use config::Config;
pub use host::{Host, ModuleCommand};
pub use persist::{StateDoc, StateOptions};
pub use registry::{ModuleFactory, Registry};
pub use state::{Event, StateStore};
pub use tools::core_tools;
pub use traits::{Module, ModuleCtx, ModuleUpdate};

/// Current time as RFC 3339 with milliseconds, e.g. `2026-10-15T09:31:00.123Z`.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
