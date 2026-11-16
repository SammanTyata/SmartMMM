use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use smmm_protocol::{ModulePatch, ModuleState, Region};
use tokio::sync::broadcast;

use crate::now_rfc3339;

/// Events fanned out to every connected client (mirror + phone).
#[derive(Debug, Clone)]
pub enum Event {
    Patch { id: String, patch: ModulePatch },
    Removed { id: String },
    Toast { kind: String, message: String },
}

/// The single source of truth for module state.
///
/// Uses a `std::sync::RwLock`: every access is short and never held across an
/// `await`, so a blocking lock is both correct and cheaper than the async one.
pub struct StateStore {
    modules: RwLock<HashMap<String, ModuleState>>,
    events: broadcast::Sender<Event>,
    revision: AtomicU64,
}

impl StateStore {
    pub fn new() -> Arc<Self> {
        // 512 is plenty of backlog for a LAN client; slow clients resync.
        let (events, _) = broadcast::channel(512);
        Arc::new(Self {
            modules: RwLock::new(HashMap::new()),
            events,
            revision: AtomicU64::new(0),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    fn next_revision(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// All modules, ordered the way the renderer should lay them out.
    pub fn snapshot(&self) -> Vec<ModuleState> {
        let guard = self.modules.read().expect("state lock poisoned");
        let mut all: Vec<ModuleState> = guard.values().cloned().collect();
        all.sort_by(|a, b| {
            region_index(a.region)
                .cmp(&region_index(b.region))
                .then(a.order.cmp(&b.order))
                .then(a.id.cmp(&b.id))
        });
        all
    }

    pub fn get(&self, id: &str) -> Option<ModuleState> {
        self.modules
            .read()
            .expect("state lock poisoned")
            .get(id)
            .cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .modules
            .read()
            .expect("state lock poisoned")
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    /// Insert (or replace) a module and broadcast its full state.
    pub fn insert(&self, state: ModuleState) -> ModulePatch {
        let rev = self.next_revision();
        let mut state = state;
        state.revision = rev;
        state.updated_at = now_rfc3339();
        let id = state.id.clone();
        let patch = state.to_patch();
        self.modules
            .write()
            .expect("state lock poisoned")
            .insert(id.clone(), state);
        let _ = self.events.send(Event::Patch {
            id,
            patch: patch.clone(),
        });
        patch
    }

    /// Apply a partial update.
    ///
    /// Only fields that actually changed are stored and broadcast — a module
    /// reporting the same status every tick costs nothing. Returns the patch
    /// that was broadcast, or `None` if the module is unknown or nothing changed.
    pub fn apply_patch(&self, id: &str, patch: ModulePatch) -> Option<ModulePatch> {
        let changed = {
            let mut guard = self.modules.write().expect("state lock poisoned");
            let state = guard.get_mut(id)?;

            let mut changed = ModulePatch::default();

            if let Some(value) = patch.visible {
                if state.visible != value {
                    state.visible = value;
                    changed.visible = Some(value);
                }
            }
            if let Some(value) = patch.region {
                if state.region != value {
                    state.region = value;
                    changed.region = Some(value);
                }
            }
            if let Some(value) = patch.order {
                if state.order != value {
                    state.order = value;
                    changed.order = Some(value);
                }
            }
            if let Some(value) = patch.status {
                if state.status != value {
                    state.status = value.clone();
                    changed.status = Some(value);
                }
            }
            if let Some(value) = patch.view {
                if state.view != value {
                    state.view = value.clone();
                    changed.view = Some(value);
                }
            }
            if let Some(value) = patch.config {
                if state.config != value {
                    state.config = value.clone();
                    changed.config = Some(value);
                }
            }

            if changed == ModulePatch::default() {
                return None;
            }

            let revision = self.next_revision();
            state.revision = revision;
            state.updated_at = now_rfc3339();
            changed.revision = revision;
            changed
        };

        self.broadcast_patch(&changed, id);
        Some(changed)
    }

    fn broadcast_patch(&self, patch: &ModulePatch, id: &str) {
        let _ = self.events.send(Event::Patch {
            id: id.to_string(),
            patch: patch.clone(),
        });
    }

    pub fn set_visible(&self, id: &str, visible: bool) -> Option<ModulePatch> {
        self.apply_patch(
            id,
            ModulePatch {
                visible: Some(visible),
                ..Default::default()
            },
        )
    }

    pub fn set_config(&self, id: &str, config: serde_json::Value) -> Option<ModulePatch> {
        self.apply_patch(
            id,
            ModulePatch {
                config: Some(config),
                ..Default::default()
            },
        )
    }

    pub fn set_region(&self, id: &str, region: Region) -> Option<ModulePatch> {
        self.apply_patch(
            id,
            ModulePatch {
                region: Some(region),
                ..Default::default()
            },
        )
    }

    pub fn set_order(&self, id: &str, order: i32) -> Option<ModulePatch> {
        self.apply_patch(
            id,
            ModulePatch {
                order: Some(order),
                ..Default::default()
            },
        )
    }

    pub fn remove(&self, id: &str) -> Option<ModuleState> {
        let removed = self
            .modules
            .write()
            .expect("state lock poisoned")
            .remove(id);
        if removed.is_some() {
            let _ = self.events.send(Event::Removed { id: id.to_string() });
        }
        removed
    }

    pub fn toast(&self, kind: impl Into<String>, message: impl Into<String>) {
        let _ = self.events.send(Event::Toast {
            kind: kind.into(),
            message: message.into(),
        });
    }
}

fn region_index(region: Region) -> usize {
    Region::ALL
        .iter()
        .position(|r| *r == region)
        .unwrap_or(usize::MAX)
}
