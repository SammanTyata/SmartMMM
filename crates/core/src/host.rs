use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use smmm_protocol::{
    ModulePatch, ModuleState, ModuleStatus, Region, ToolInfo, ViewNode, CORE_MODULE,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use tracing::{error, warn};

use crate::config::Config;
use crate::now_rfc3339;
use crate::persist::{Baseline, Persist, StateOptions};
use crate::registry::Registry;
use crate::state::StateStore;
use crate::tools::{core_tools, optional_str, required, required_str};
use crate::traits::{Module, ModuleCtx, ModuleUpdate};

/// Command sent from the API surface to a module's task.
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

/// Owns every running module and mediates all access to them.
///
/// Each module runs in its own task (the actor model): the task holds the sole
/// `&mut` to the module and services a `select!` over its timer, its command
/// channel, and its out-of-band update channel. Callers never touch a module
/// directly, which is what makes `Module: Send + Sync` enough.
#[derive(Clone)]
pub struct Host {
    store: Arc<StateStore>,
    commands: Arc<HashMap<String, mpsc::UnboundedSender<ModuleCommand>>>,
    started: Instant,
    /// `None` when the caller asked for a stateless run.
    persist: Option<Persist>,
}

impl Host {
    pub async fn start(
        config: &Config,
        registry: &Registry,
        store: Arc<StateStore>,
        state: StateOptions,
    ) -> Result<Self> {
        let mut commands = HashMap::new();
        let mut baseline = HashMap::new();

        for factory in registry.factories() {
            let module = factory.build();
            let manifest = module.manifest();
            let id = manifest.id.clone();
            let tools: Vec<ToolInfo> = module
                .tools()
                .iter()
                .map(|descriptor| ToolInfo::from_descriptor(&id, descriptor))
                .collect();

            let declared = config.module(&id);
            let declared_config = declared
                .map(|m| m.config.clone())
                .filter(|config| !config.is_null());

            // What `config.toml` resolves to: the manifest defaults it does not
            // override, and its config table laid over the module's own defaults.
            // Comparing against the *resolved* value rather than the file's table
            // matters — a partial table would otherwise look like a change to
            // every field it never mentioned, and the first save would quietly
            // pin them.
            let base = Baseline {
                visible: declared
                    .and_then(|m| m.visible)
                    .unwrap_or(manifest.default_visible),
                region: declared
                    .and_then(|m| m.region)
                    .unwrap_or(manifest.default_region),
                order: declared
                    .and_then(|m| m.order)
                    .unwrap_or(manifest.default_order),
                config: match declared_config.clone() {
                    Some(declared) => merge(module.config().unwrap_or(Value::Null), declared),
                    None => module.config().unwrap_or(Value::Null),
                },
            };

            // A saved override wins over `config.toml`.
            let saved = state.doc.module(&id);

            // The config the module is *started* with: the saved override, else
            // the file's table. `Null` here means "use your own defaults", which
            // is not the same thing as the defaults themselves.
            let saved_config = saved.and_then(|o| o.config.clone());
            let initial_config = saved_config
                .clone()
                .or(declared_config)
                .unwrap_or(Value::Null);

            // What the store advertises from the very first request: the saved
            // override, else what `config.toml` resolves to.
            //
            // This must never be `null` for a module that has a config at all.
            // The phone builds its generated form from one snapshot, and a `null`
            // there leaves it showing the schema's `default` values — or blank
            // boxes, for a field like the clock's optional label that has no
            // default — until the form is closed and reopened. Measured: before
            // this, `weather.config` stayed `null` for over a second after boot,
            // because the module only reports its running config once its first
            // `on_config` returns, and that one does a network fetch.
            let advertised_config = saved_config.unwrap_or_else(|| base.config.clone());

            let initial_state = ModuleState {
                id: id.clone(),
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                description: manifest.description.clone(),
                visible: saved.and_then(|o| o.visible).unwrap_or(base.visible),
                region: saved.and_then(|o| o.region).unwrap_or(base.region),
                order: saved.and_then(|o| o.order).unwrap_or(base.order),
                status: ModuleStatus::Starting,
                view: ViewNode::Empty,
                config: advertised_config,
                config_schema: manifest.config_schema.clone(),
                tools,
                revision: 0,
                updated_at: now_rfc3339(),
            };

            baseline.insert(id.clone(), base);
            let initial_visible = initial_state.visible;
            store.insert(initial_state);

            let (ctx_tx, ctx_rx) = mpsc::unbounded_channel();
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            commands.insert(id.clone(), cmd_tx);

            let ctx = ModuleCtx::new(id.clone(), ctx_tx);
            tokio::spawn(run_module(
                id,
                module,
                ctx,
                ctx_rx,
                cmd_rx,
                Startup {
                    store: store.clone(),
                    config: initial_config,
                    visible: initial_visible,
                },
            ));
        }

        let persist = state.path.map(|path| {
            tracing::info!(path = %path.display(), "runtime state");
            Persist::start(path, store.clone(), baseline, state.doc)
        });

        Ok(Self {
            store,
            commands: Arc::new(commands),
            started: Instant::now(),
            persist,
        })
    }

    /// Record that module state changed in a way worth remembering.
    ///
    /// Called from every mutation entry point rather than from the state store
    /// itself, because only the caller knows whether a change was *intended*:
    /// the store also absorbs the initial sync of every module's own defaults,
    /// which must not be persisted.
    fn touch(&self) {
        if let Some(persist) = &self.persist {
            persist.touch();
        }
    }

    /// Write runtime state to disk now, without waiting for the debounce.
    pub fn flush_state(&self) -> Result<()> {
        match &self.persist {
            Some(persist) => persist.flush(),
            None => Ok(()),
        }
    }

    pub fn store(&self) -> &Arc<StateStore> {
        &self.store
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub fn module_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.commands.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn sender(&self, id: &str) -> Result<&mpsc::UnboundedSender<ModuleCommand>> {
        self.commands
            .get(id)
            .ok_or_else(|| anyhow!("unknown module: {id}"))
    }

    /// Best-effort: updates the store immediately and notifies the module.
    pub fn set_visible(&self, id: &str, visible: bool) -> Result<()> {
        let tx = self.sender(id)?;
        self.store.set_visible(id, visible);
        let _ = tx.send(ModuleCommand::SetVisible(visible));
        self.touch();
        Ok(())
    }

    /// Move a module to another region.
    ///
    /// Goes through the host rather than straight to the store so that placement
    /// changes are recorded for the next boot, exactly like visibility.
    pub fn set_region(&self, id: &str, region: Region) -> Result<()> {
        self.sender(id)?;
        self.store.set_region(id, region);
        self.touch();
        Ok(())
    }

    /// Change a module's sort order within its region.
    pub fn set_order(&self, id: &str, order: i32) -> Result<()> {
        self.sender(id)?;
        self.store.set_order(id, order);
        self.touch();
        Ok(())
    }

    /// Applies a config update. The module validates it first; the new config
    /// is only stored if the module accepts it.
    pub async fn set_config(&self, id: &str, config: Value) -> Result<()> {
        let tx = self.sender(id)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ModuleCommand::SetConfig {
            config,
            reply: reply_tx,
        })
        .map_err(|_| anyhow!("module task for `{id}` is gone"))?;

        let result = reply_rx
            .await
            .map_err(|_| anyhow!("module task for `{id}` dropped the reply"))?;
        if result.is_ok() {
            self.touch();
        }
        result
    }

    // --- Tools ---------------------------------------------------------------

    /// Every tool the mirror exposes: core-owned plus every module's.
    pub fn tools(&self) -> Vec<ToolInfo> {
        let mut tools: Vec<ToolInfo> = core_tools()
            .iter()
            .map(|descriptor| ToolInfo::from_descriptor(CORE_MODULE, descriptor))
            .collect();
        for state in self.store.snapshot() {
            tools.extend(state.tools.iter().cloned());
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Resolve a globally-unique tool name.
    ///
    /// Matching against the registry rather than parsing the name means there is
    /// no separator ambiguity to get wrong.
    pub fn find_tool(&self, name: &str) -> Option<ToolInfo> {
        if let Some(local) = name.strip_prefix(&format!("{CORE_MODULE}__")) {
            if let Some(descriptor) = core_tools().into_iter().find(|d| d.name == local) {
                return Some(ToolInfo::from_descriptor(CORE_MODULE, &descriptor));
            }
        }
        self.store
            .snapshot()
            .into_iter()
            .flat_map(|state| state.tools)
            .find(|tool| tool.name == name)
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.find_tool(name).is_some()
    }

    /// Call a tool by its globally-unique name.
    ///
    /// Tool *failures* are ordinary `Err`s, which callers turn into results
    /// (`ok: false`) instead of HTTP errors, so a caller can read the message
    /// and try something else.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        let info = self
            .find_tool(name)
            .ok_or_else(|| anyhow!("unknown tool `{name}`"))?;
        if info.module == CORE_MODULE {
            self.call_core_tool(&info.local_name, args).await
        } else {
            self.dispatch(&info.module, &info.local_name, args).await
        }
    }

    async fn dispatch(&self, module: &str, tool: &str, args: Value) -> Result<Value> {
        let tx = self.sender(module)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ModuleCommand::CallTool {
            tool: tool.to_string(),
            args,
            reply: reply_tx,
        })
        .map_err(|_| anyhow!("module task for `{module}` is gone"))?;

        let result = reply_rx
            .await
            .map_err(|_| anyhow!("module task for `{module}` dropped the reply"))?;
        if result.is_ok() {
            // A tool may have changed the module's config (or its placement, via
            // a core tool), so treat any successful call as worth a look.
            self.touch();
        }
        result
    }

    /// Core-owned tools.
    ///
    /// Note that "hide the clock" is not a clock capability — it is a core one,
    /// which means it is written once and works for every module, including
    /// ones that do not exist yet.
    async fn call_core_tool(&self, tool: &str, args: Value) -> Result<Value> {
        match tool {
            "list_modules" => Ok(Value::Array(
                self.store.snapshot().iter().map(module_summary).collect(),
            )),

            "get_status" => match optional_str(&args, "id") {
                Some(id) => {
                    let state = self
                        .store
                        .get(&id)
                        .ok_or_else(|| anyhow!("unknown module `{id}`"))?;
                    Ok(module_summary(&state))
                }
                None => Ok(json!({
                    "revision": self.store.revision(),
                    "modules": self
                        .store
                        .snapshot()
                        .iter()
                        .map(module_summary)
                        .collect::<Vec<_>>(),
                })),
            },

            "show_module" => {
                let id = required_str(&args, "id")?;
                self.set_visible(&id, true)?;
                Ok(json!({ "id": id, "visible": true }))
            }

            "hide_module" => {
                let id = required_str(&args, "id")?;
                self.set_visible(&id, false)?;
                Ok(json!({ "id": id, "visible": false }))
            }

            "set_region" => {
                let id = required_str(&args, "id")?;
                let region: Region = serde_json::from_value(required(&args, "region")?.clone())
                    .map_err(|e| anyhow!("invalid region: {e}"))?;
                self.set_region(&id, region)?;
                Ok(json!({ "id": id, "region": region }))
            }

            "set_order" => {
                let id = required_str(&args, "id")?;
                let order = required(&args, "order")?
                    .as_i64()
                    .ok_or_else(|| anyhow!("argument `order` must be an integer"))?
                    as i32;
                self.set_order(&id, order)?;
                Ok(json!({ "id": id, "order": order }))
            }

            other => anyhow::bail!("unknown core tool `{other}`"),
        }
    }
}

/// Compact, caller-friendly view of one module.
fn module_summary(state: &ModuleState) -> Value {
    json!({
        "id": state.id,
        "name": state.name,
        "visible": state.visible,
        "region": state.region,
        "order": state.order,
        "status": state.status,
    })
}

/// Lay `over` on top of `base`, one key deep.
///
/// Module configs are flat tables, so a shallow merge is the whole story: a key
/// the config file sets wins, and a key it omits keeps whatever the module
/// defaults to.
fn merge(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Object(mut base), Value::Object(over)) => {
            for (key, value) in over {
                base.insert(key, value);
            }
            Value::Object(base)
        }
        (_, over) => over,
    }
}

/// What a module task needs to bring itself up.
struct Startup {
    store: Arc<StateStore>,
    /// The config it should start with: a saved override, else `config.toml`'s
    /// table, else `Null` (meaning "use your defaults").
    config: Value,
    /// The visibility it should start with, which may not be the module's own
    /// default.
    visible: bool,
}

async fn run_module(
    id: String,
    mut module: Box<dyn Module>,
    ctx: ModuleCtx,
    mut updates: mpsc::UnboundedReceiver<ModuleUpdate>,
    mut commands: mpsc::UnboundedReceiver<ModuleCommand>,
    startup: Startup,
) {
    let Startup {
        store,
        config: initial_config,
        visible: initial_visible,
    } = startup;
    if let Err(e) = module.init(ctx.clone()).await {
        error!(module = %id, "init failed: {e:#}");
        set_status(
            &store,
            &id,
            ModuleStatus::Error {
                message: format!("init: {e}"),
            },
        );
    }

    if let Err(e) = module.on_config(initial_config).await {
        warn!(module = %id, "initial config rejected: {e:#}");
    }

    // A module is constructed with its own default visibility, which may well
    // disagree with `config.toml` (or the state file). Tell it before the first
    // sync below, or that sync pushes the constructor's default straight back
    // over the value we just restored.
    module.set_visible(initial_visible);
    if let Err(e) = module.on_visibility(initial_visible).await {
        warn!(module = %id, "on_visibility failed: {e:#}");
    }

    // A module omitted from `config.toml` is stored as `config: null` while it
    // runs its own Rust-side defaults, so the phone's generated config form would
    // show the schema's `default` values instead of what is actually running.
    // Ask the module what it is using and make the store say the same thing.
    if let Some(config) = module.config() {
        store.set_config(&id, config);
    }

    if let Err(e) = module.start().await {
        warn!(module = %id, "start failed: {e:#}");
    }

    sync_state(&store, &id, module.as_ref());

    let mut ticker =
        tokio::time::interval(module.update_interval().max(Duration::from_millis(100)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if module.visible() {
                    if let Err(e) = module.update().await {
                        warn!(module = %id, "update failed: {e:#}");
                        set_status(&store, &id, ModuleStatus::Error { message: e.to_string() });
                    }
                }
                sync_state(&store, &id, module.as_ref());
            }

            cmd = commands.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    ModuleCommand::SetVisible(visible) => {
                        module.set_visible(visible);
                        if let Err(e) = module.on_visibility(visible).await {
                            warn!(module = %id, "on_visibility failed: {e:#}");
                        }
                        store.set_visible(&id, visible);
                        sync_state(&store, &id, module.as_ref());
                    }
                    ModuleCommand::SetConfig { config, reply } => {
                        let result = module.on_config(config.clone()).await;
                        if result.is_ok() {
                            // A module that can describe its own config wins: the
                            // store should hold what is running, not the fragment
                            // that was sent (which may omit fields, or be null to
                            // mean "reset to defaults").
                            store.set_config(&id, module.config().unwrap_or(config));
                            sync_state(&store, &id, module.as_ref());
                        }
                        let _ = reply.send(result);
                    }
                    ModuleCommand::CallTool { tool, args, reply } => {
                        let result = module.call_tool(&tool, args).await;
                        if result.is_ok() {
                            // A tool may have mutated the module's config; keep
                            // the state store in sync if it says so.
                            if let Some(config) = module.config() {
                                store.set_config(&id, config);
                            }
                        }
                        sync_state(&store, &id, module.as_ref());
                        let _ = reply.send(result);
                    }
                    ModuleCommand::Stop => {
                        if let Err(e) = module.stop().await {
                            warn!(module = %id, "stop failed: {e:#}");
                        }
                        break;
                    }
                }
            }

            update = updates.recv() => {
                if let Some(update) = update {
                    apply_update(&store, &id, update);
                }
            }
        }
    }
}

/// Push the module's current view/visibility/status into the store.
fn sync_state(store: &StateStore, id: &str, module: &dyn Module) {
    store.apply_patch(
        id,
        ModulePatch {
            visible: Some(module.visible()),
            status: Some(module.status()),
            view: Some(module.view()),
            ..Default::default()
        },
    );
}

fn set_status(store: &StateStore, id: &str, status: ModuleStatus) {
    store.apply_patch(
        id,
        ModulePatch {
            status: Some(status),
            ..Default::default()
        },
    );
}

fn apply_update(store: &StateStore, id: &str, update: ModuleUpdate) {
    match update {
        ModuleUpdate::View(view) => {
            store.apply_patch(
                id,
                ModulePatch {
                    view: Some(view),
                    ..Default::default()
                },
            );
        }
        ModuleUpdate::Status(status) => set_status(store, id, status),
        ModuleUpdate::Toast { kind, message } => store.toast(kind, message),
    }
}
