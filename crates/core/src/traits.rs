use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use smmm_protocol::{Manifest, ModuleStatus, ToolDescriptor, ViewNode};
use tokio::sync::{mpsc, oneshot};

/// Something a module pushes out-of-band to the core.
#[derive(Debug, Clone)]
pub enum ModuleUpdate {
    View(ViewNode),
    Status(ModuleStatus),
    Toast { kind: String, message: String },
}

/// Command sent to a running module's task by the core (or, through
/// [`ModuleRouter`], by another module).
pub enum ModuleCommand {
    SetVisible(bool),
    SetConfig {
        config: Value,
        reply: oneshot::Sender<Result<()>>,
    },
    CallTool {
        tool: String,
        args: Value,
        reply: oneshot::Sender<Result<Value>>,
    },
    Stop,
}

/// Shared registry of every running module's command channel.
pub type CommandMap = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<ModuleCommand>>>>;

/// Lets one module call another module's tools.
///
/// Two guards, as designed in ARCHITECTURE.md §5.4:
/// - **Self-calls are rejected.** A module calling its own tool would await a
///   reply from the very task that is currently awaiting it — a deadlock.
/// - **Every call is bounded by a timeout**, so a wedged target module cannot
///   hang the caller forever.
#[derive(Clone)]
pub struct ModuleRouter {
    owner: Arc<str>,
    commands: CommandMap,
}

impl ModuleRouter {
    pub fn new(owner: impl Into<Arc<str>>, commands: CommandMap) -> Self {
        Self {
            owner: owner.into(),
            commands,
        }
    }

    /// Call `<module>__<tool>` with `args` and wait for the result.
    pub async fn call_tool(&self, module: &str, tool: &str, args: Value) -> Result<Value> {
        if module == self.owner.as_ref() {
            anyhow::bail!("a module cannot call its own tools (`{module}__{tool}`)");
        }
        let tx = self
            .commands
            .lock()
            .expect("command map lock poisoned")
            .get(module)
            .cloned()
            .ok_or_else(|| anyhow!("unknown module: {module}"))?;

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ModuleCommand::CallTool {
            tool: tool.to_string(),
            args,
            reply: reply_tx,
        })
        .map_err(|_| anyhow!("module task for `{module}` is gone"))?;

        match tokio::time::timeout(Duration::from_secs(2), reply_rx).await {
            Ok(reply) => reply.map_err(|_| anyhow!("module task for `{module}` dropped the reply"))?,
            Err(_) => anyhow::bail!("`{module}__{tool}` timed out after 2s"),
        }
    }
}

/// Handles a module receives in [`Module::init`] and may keep for its lifetime.
///
/// This is the *only* channel between a module and the core: modules never
/// touch the HTTP layer, the state store, or each other.
#[derive(Clone)]
pub struct ModuleCtx {
    id: Arc<str>,
    tx: mpsc::UnboundedSender<ModuleUpdate>,
    router: ModuleRouter,
}

impl ModuleCtx {
    pub fn new(id: impl Into<Arc<str>>, tx: mpsc::UnboundedSender<ModuleUpdate>, commands: CommandMap) -> Self {
        let id = id.into();
        let router = ModuleRouter::new(id.clone(), commands);
        Self { id, tx, router }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Call another module's tool (`<module>__<tool>` style, but by parts).
    /// Bounded by a 2s timeout and rejects self-calls — see [`ModuleRouter`].
    pub async fn call_module_tool(&self, module: &str, tool: &str, args: Value) -> Result<Value> {
        self.router.call_tool(module, tool, args).await
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
