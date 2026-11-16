//! Runtime state that has to outlive the process.
//!
//! `config.toml` is *declared* intent: a person writes it, it is read once at
//! startup, and the process never writes back to it. That split is deliberate —
//! a daemon that rewrites the file you hand-edit destroys your comments and makes
//! `git diff` useless on the one file that should be reviewable.
//!
//! But the phone also makes *runtime* changes: hiding a module, moving it to
//! another region, editing its config. Those must survive a restart, or a mirror
//! that reboots nightly forgets its layout. They are kept here instead, in a
//! separate state file:
//!
//! ```text
//! manifest defaults  <  config.toml  <  state.json
//! ```
//!
//! Only fields that actually *differ* from what `config.toml` resolves to are
//! written. That is what keeps `config.toml` authoritative for everything the
//! phone has not touched: edit it, restart, and the change takes effect. Delete
//! `state.json` and every module returns to what `config.toml` says.
//!
//! Writes are debounced and atomic. The target is an SD card on a device that
//! will eventually lose power without warning.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smmm_protocol::{ModuleState, Region};
use tokio::sync::mpsc;
use tracing::warn;

use crate::state::StateStore;

/// Bumped if the shape of the file changes, so a later version can migrate
/// instead of guessing.
const VERSION: u32 = 1;

/// How long the writer waits for a burst of changes to finish before its
/// follow-up write.
///
/// The first change is written immediately, so this is not what makes a change
/// durable — it only keeps a run of taps from becoming a run of writes.
const DEBOUNCE: Duration = Duration::from_millis(750);

/// One module's runtime overrides.
///
/// Every field is optional, and `None` means "`config.toml` still decides this".
/// The struct mirrors `config::ModuleConfig` on purpose: the same shape is read
/// from the file and written to it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

impl Overrides {
    /// True when this module matches `config.toml` exactly, i.e. there is
    /// nothing to persist and any entry for it should be removed.
    pub fn is_empty(&self) -> bool {
        self.visible.is_none()
            && self.region.is_none()
            && self.order.is_none()
            && self.config.is_none()
    }
}

/// The state file as a whole.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StateDoc {
    pub version: u32,
    pub modules: BTreeMap<String, Overrides>,
}

impl Default for StateDoc {
    fn default() -> Self {
        Self {
            version: VERSION,
            modules: BTreeMap::new(),
        }
    }
}

impl StateDoc {
    /// Read the file. A missing file is not an error — that is simply the first
    /// boot — but an unreadable one is, so the caller can decide what to do.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("reading state {}", path.display()));
            }
        };
        serde_json::from_str(&text).with_context(|| format!("parsing state {}", path.display()))
    }

    /// Write atomically: a temp file in the same directory, flushed all the way
    /// to the platter, then renamed over the target. A power cut mid-write
    /// therefore leaves the previous state intact rather than a truncated file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating state directory {}", dir.display()))?;
        }

        let text = serde_json::to_string_pretty(self).context("serialising runtime state")?;

        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);

        {
            let mut file = std::fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            file.write_all(text.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()
                .with_context(|| format!("flushing {}", tmp.display()))?;
        }

        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn module(&self, id: &str) -> Option<&Overrides> {
        self.modules.get(id)
    }
}

/// What `config.toml` alone produces for a module.
///
/// Held in the host so that "has this changed?" is answered against the file,
/// not against the previous write. Without it, the first save would pin every
/// field of every module and `config.toml` would quietly stop mattering.
#[derive(Debug, Clone)]
pub struct Baseline {
    pub visible: bool,
    pub region: Region,
    pub order: i32,
    /// The config the module would run with no state file. `Null` for a module
    /// that has no config at all.
    pub config: Value,
}

/// The overrides worth writing for one module: only fields that differ from
/// `baseline`.
pub fn overrides_for(state: &ModuleState, baseline: &Baseline) -> Overrides {
    Overrides {
        visible: (state.visible != baseline.visible).then_some(state.visible),
        region: (state.region != baseline.region).then_some(state.region),
        order: (state.order != baseline.order).then_some(state.order),
        config: (state.config != baseline.config).then(|| state.config.clone()),
    }
}

/// How the host should treat runtime state.
#[derive(Debug, Clone, Default)]
pub struct StateOptions {
    /// Where to write. `None` disables persistence entirely, which is the old
    /// behaviour: every restart returns to `config.toml`.
    pub path: Option<PathBuf>,
    pub doc: StateDoc,
}

impl StateOptions {
    /// Load the state file at `path`, if there is one.
    ///
    /// A file we cannot parse is moved aside rather than ignored: the mirror
    /// must not become unbootable because of it, but silently overwriting
    /// something a person might want to look at is worse than renaming it.
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };

        match StateDoc::load(&path) {
            Ok(doc) => Ok(Self {
                path: Some(path),
                doc,
            }),
            Err(e) => {
                let mut backup = path.as_os_str().to_owned();
                backup.push(".bad");
                let backup = PathBuf::from(backup);
                match std::fs::rename(&path, &backup) {
                    Ok(()) => warn!(
                        "could not read {} ({e:#}); moved it to {} and starting with defaults",
                        path.display(),
                        backup.display()
                    ),
                    Err(rename_error) => warn!(
                        "could not read {} ({e:#}), and could not move it aside \
                         ({rename_error}); starting with defaults and it will be overwritten",
                        path.display()
                    ),
                }
                Ok(Self {
                    path: Some(path),
                    doc: StateDoc::default(),
                })
            }
        }
    }
}

/// The long-lived handle the host uses to record changes.
///
/// Cloning is cheap; `touch` never blocks and never does IO, because it is
/// called from the middle of request handling.
#[derive(Clone)]
pub struct Persist {
    path: PathBuf,
    store: Arc<StateStore>,
    baseline: Arc<HashMap<String, Baseline>>,
    state: Arc<Mutex<Written>>,
    tx: mpsc::UnboundedSender<()>,
}

/// The in-memory copy of the file, plus the exact bytes last written so that a
/// no-op save never reaches the disk.
struct Written {
    doc: StateDoc,
    last: Option<String>,
}

impl Persist {
    /// Start the writer task and return a handle to it.
    pub fn start(
        path: PathBuf,
        store: Arc<StateStore>,
        baseline: HashMap<String, Baseline>,
        doc: StateDoc,
    ) -> Persist {
        let (tx, rx) = mpsc::unbounded_channel();

        // Treat the loaded document as already being what is on disk. Without
        // this, the first save would rewrite an identical file, and a boot in
        // which nothing changed would still touch the card on shutdown.
        let last = serde_json::to_string_pretty(&doc).ok();

        let persist = Persist {
            path,
            store,
            baseline: Arc::new(baseline),
            state: Arc::new(Mutex::new(Written { doc, last })),
            tx,
        };

        let writer = persist.clone();
        tokio::spawn(async move { writer.run(rx).await });

        persist
    }

    /// Note that something worth remembering changed.
    ///
    /// Deliberately argument-less: the store is the source of truth, so the
    /// writer simply re-reads it. That also means a caller cannot forget to pass
    /// which module changed.
    pub fn touch(&self) {
        let _ = self.tx.send(());
    }

    /// Write now, without waiting for the debounce. Called on shutdown, and by
    /// tests that cannot wait for a background task.
    pub fn flush(&self) -> Result<()> {
        self.sync_all()
    }

    /// Rewrite the document from the store and save it if anything changed.
    fn sync_all(&self) -> Result<()> {
        let ids = self.store.ids();
        let known: HashSet<&str> = ids.iter().map(String::as_str).collect();

        let mut guard = self.state.lock().expect("state lock poisoned");

        // A module that is no longer part of this build should not keep an entry
        // around forever.
        guard
            .doc
            .modules
            .retain(|id, _| known.contains(id.as_str()));

        for id in &ids {
            let (Some(state), Some(baseline)) = (self.store.get(id), self.baseline.get(id)) else {
                continue;
            };
            let overrides = overrides_for(&state, baseline);
            if overrides.is_empty() {
                guard.doc.modules.remove(id);
            } else {
                guard.doc.modules.insert(id.clone(), overrides);
            }
        }
        guard.doc.version = VERSION;

        let text = serde_json::to_string_pretty(&guard.doc).context("serialising runtime state")?;
        if guard.last.as_deref() == Some(text.as_str()) {
            // Identical to what is already on disk: leave the card alone.
            return Ok(());
        }

        // Release the lock before the write so a concurrent `touch` (and the
        // request handling behind it) never waits on the filesystem.
        let doc = guard.doc.clone();
        drop(guard);

        doc.save(&self.path)?;
        self.state.lock().expect("state lock poisoned").last = Some(text);
        Ok(())
    }

    async fn run(self, mut rx: mpsc::UnboundedReceiver<()>) {
        // Ends when the host is dropped, which also gives a last write.
        while rx.recv().await.is_some() {
            // Write straight away rather than after a quiet period. A change is
            // a few hundred bytes, and waiting means a change made in the moment
            // before a crash — or before someone closes the terminal, or pulls
            // the power, which a mirror on a Pi will do — is simply gone. The
            // debounce below is only there to fold a *burst* into one more
            // write; it must never be what makes the first change durable.
            if let Err(e) = self.sync_all() {
                warn!(
                    "could not save runtime state to {}: {e:#}",
                    self.path.display()
                );
            }

            // Keep draining until things go quiet, then write once more.
            while let Ok(Some(())) = tokio::time::timeout(DEBOUNCE, rx.recv()).await {}

            // A no-op when nothing changed after the first write, which is the
            // common case: one tap, one write.
            if let Err(e) = self.sync_all() {
                warn!(
                    "could not save runtime state to {}: {e:#}",
                    self.path.display()
                );
            }
        }
    }
}

/// Where runtime state lives when the caller does not say.
///
/// `STATE_DIRECTORY` is set by systemd from `StateDirectory=`, which is the
/// right answer on the Pi: the directory is created with the right owner and
/// survives redeploys. Otherwise follow the XDG base directory spec.
pub fn default_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("STATE_DIRECTORY") {
        return PathBuf::from(dir).join("state.json");
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("smartmmm/state.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/smartmmm/state.json");
    }
    PathBuf::from("state.json")
}
