use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use smmm_protocol::{Manifest, ModuleStatus, ToolDescriptor, ViewNode};
use tokio::sync::mpsc;

/// Something a module pushes out-of-band to the core.
#[derive(Debug, Clone)]
pub enum ModuleUpdate {
    View(ViewNode),
    Status(ModuleStatus),
    Toast { kind: String, message: String },
}

/// Handles a module receives in [`Module::init`] and may keep for its lifetime.
///
/// This is the *only* channel between a module and the core: modules never
/// touch the HTTP layer, the state store, or each other.
#[derive(Clone)]
pub struct ModuleCtx {
    id: Arc<str>,
    tx: mpsc::UnboundedSender<ModuleUpdate>,
}

impl ModuleCtx {
    pub fn new(id: impl Into<Arc<str>>, tx: mpsc::UnboundedSender<ModuleUpdate>) -> Self {
        Self { id: id.into(), tx }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn publish(&self, update: ModuleUpdate) {
        let _ = self.tx.send(update);
    }

    pub fn set_view(&self, view: ViewNode) {
        self.publish(ModuleUpdate::View(view));
    }

    pub fn set_status(&self, status: ModuleStatus) {
        self.publish(ModuleUpdate::Status(status));
    }

    pub fn toast(&self, kind: impl Into<String>, message: impl Into<String>) {
        self.publish(ModuleUpdate::Toast {
            kind: kind.into(),
            message: message.into(),
        });
    }

    pub fn info(&self, msg: impl std::fmt::Display) {
        tracing::info!(module = %self.id, "{msg}");
    }

    pub fn warn(&self, msg: impl std::fmt::Display) {
        tracing::warn!(module = %self.id, "{msg}");
    }

    pub fn error(&self, msg: impl std::fmt::Display) {
        tracing::error!(module = %self.id, "{msg}");
    }
}

/// The contract every module implements.
///
/// The core drives modules on a timer: it calls [`Module::update`] at
/// [`Module::update_interval`] whenever the module is visible, then reads back
/// [`Module::view`] / [`Module::status`] and broadcasts any change. Commands
/// (actions, visibility, config) are delivered between ticks.
#[async_trait]
pub trait Module: Send + Sync + 'static {
    /// Static description. Must not depend on runtime state.
    fn manifest(&self) -> Manifest;

    /// How often the core should call [`Module::update`] while visible.
    fn update_interval(&self) -> Duration;

    /// Capabilities this module exposes.
    ///
    /// The core namespaces each one as `<module>__<name>` and serves it through
    /// the tool API. The phone renders all of them; `params_schema` decides
    /// whether a tool is one tap or a generated form.
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }

    /// Called once before `start`. Store `ctx` if you need to publish later.
    async fn init(&mut self, ctx: ModuleCtx) -> anyhow::Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Called once after `init` and after the initial config is applied.
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Refresh data. Only called while the module is visible.
    async fn update(&mut self) -> anyhow::Result<()>;

    /// Graceful shutdown; must not block for long.
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Apply a config update. The core only stores the new config if this
    /// returns `Ok`.
    async fn on_config(&mut self, config: Value) -> anyhow::Result<()> {
        let _ = config;
        Ok(())
    }

    /// Visibility changed. Use to pause work and resume cheaply.
    async fn on_visibility(&mut self, visible: bool) -> anyhow::Result<()> {
        let _ = visible;
        Ok(())
    }

    /// Invoke one of [`Module::tools`] by its local name.
    ///
    /// Errors are surfaced to the caller as tool results, not failures, so an
    /// LLM can read the message and try something else.
    async fn call_tool(&mut self, tool: &str, args: Value) -> anyhow::Result<Value> {
        let _ = (tool, args);
        anyhow::bail!("unknown tool")
    }

    /// The module's config as JSON, if it has one.
    ///
    /// The core uses this to keep the state store honest: after the initial
    /// `on_config`, after a config update, and after any successful tool call, a
    /// module that returns `Some` here has that value written to the store. That
    /// is what lets the phone's generated config form show what is *running*
    /// rather than what was last sent.
    ///
    /// The default `None` means "this module has no config to report" (or that a
    /// tool did not change it), in which case the store is left alone.
    fn config(&self) -> Option<Value> {
        None
    }

    fn visible(&self) -> bool;
    fn set_visible(&mut self, visible: bool);
    fn view(&self) -> ViewNode;

    /// Current health. Default: healthy.
    fn status(&self) -> ModuleStatus {
        ModuleStatus::Ok
    }
}
