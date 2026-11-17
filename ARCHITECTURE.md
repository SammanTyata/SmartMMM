# SmartMMM — Architecture

A modular smart-mirror application with a Rust core and first-class smartphone control,
running identically on macOS and Raspberry Pi.

> Status: implemented through the appliance milestone on a live Raspberry Pi 4 —
> core, server, phone UI, and the `clock`, `weather`, `news` and `presence`
> modules are running. Not yet built: QR pairing/auth, `calendar`/`sysinfo`,
> config hot-reload, the WASM module host. Design details below that are not
> yet implemented are marked as such.

---

## 1. Goals & non-goals

### Goals
- **Modular.** Adding a feature means adding a module, not editing the core.
- **Remote-controllable.** From a phone on the LAN: list modules, show/hide them, change
  their config, and invoke named actions.
- **Cross-platform.** One codebase for macOS (development) and Raspberry Pi OS (deployment),
  including ARMv8 64-bit.
- **Appliance-grade.** Runs unattended for weeks. No GC pauses, no unbounded growth, no
  manual restarts.
- **Glanceable.** Dark background, large type, legible through a two-way mirror.

### Non-goals (v1)
- Cloud accounts, remote access from outside the LAN.
- A native App Store phone app (the PWA covers this; a native client can come later
  against the same API).
- Untrusted third-party module sandboxing (designed *for*, but not shipped in v1 — see §11).
- Multiple mirror displays per core.

### Explicitly chosen
- Web UI rendered in a webview/kiosk (not a native Rust UI toolkit).
- Modules are Rust, compiled into the binary behind a `Module` trait.
- The core serves both the mirror UI and the phone UI; the display process is a thin,
  swappable shell.

---

## 2. Hardware target & constraints

**Confirmed target: Raspberry Pi 4 Model B, 4 GB.**

| Target | CPU | RAM | GPU | Role |
|---|---|---|---|---|
| **Raspberry Pi 4 Model B** | 4× Cortex-A72 @ 1.5 GHz (1.8 GHz on later revs), 64-bit | **4 GB** | VideoCore VI, GLES 3.0, dual HDMI (4Kp60) | **Primary deployment** |
| Apple Silicon / Intel Mac | — | — | — | Development |
| Pi 3 / Zero 2 W | — | — | — | Not targeted. Would need the Slint fallback; the core is unchanged. |

The Pi 4 with 4 GB is comfortable for this design. A Chromium-based renderer is a solved problem
here, WebKitGTK is viable, and there is ample headroom for several modules plus a browser. The
remaining Pi-specific risks are **thermal and power**, not compute:

- **Thermal throttling.** Behind a two-way mirror, airflow is poor and the panel adds heat. A
  throttled Pi 4 (A72 drops toward 1.0 GHz) can visibly stutter a webview. Fit a heatsink or fan
  and keep the enclosure vented. Monitor with `vcgencmd measure_temp`.
- **Undervoltage.** Use the official 5 V/3 A USB-C supply. Underpowered supplies cause silent
  throttling (check `vcgencmd get_throttled`) that looks like a software performance bug.
- **SD card wear.** A mirror runs 24/7. Log to a tmpfs or set a log cap, and use a quality
  A1/A2 card (or boot from USB SSD) to avoid card death in a few months.
- **Output mode.** Pin the mirror panel to 1080p rather than 4K. Under the default KMS driver
  (`vc4-kms-v3d`) you set this with a kernel `video=HDMI-A-1:1920x1080@60` parameter (or via the
  desktop's display settings), not the legacy `hdmi_mode`. 4K roughly quadruples the compositor's
  work for no legibility benefit through a mirror.
- **Build on the Mac and cross-compile** to `aarch64-unknown-linux-gnu`. Native `cargo build`
  on the Pi works but is slow. Use a 64-bit Pi OS to match the cross-compile target.

**Design properties that keep the renderer swappable anyway** (cheap insurance, already in the plan):
1. The renderer is a *separate, swappable process* pointing at a URL, so it can be replaced
   (Chromium kiosk ↔ Tauri/WebKitGTK ↔ Slint) without touching the core.
2. The core is headless and cheap; only the renderer is heavy.
3. UI is declarative and patch-driven, so a module can update one field without a full repaint.

---

## 3. System overview

```
                          ┌──────────────────────────────────────────────┐
                          │            smartmmm (Rust core)              │
                          │                                              │
   ┌───────────┐          │  ┌────────────────────────────────────────┐  │
   │  Module   │  trait   │  │            Module Registry             │  │
   │ clock     │◄────────►│  │  clock · weather · calendar · sysinfo  │  │
   │ weather   │  Module  │  └───────────────┬────────────────────────┘  │
   │ calendar  │          │                  │ state + patches           │
   │ sysinfo   │          │  ┌───────────────▼────────────────────────┐  │
   └───────────┘          │  │  State Store + Event Bus (broadcast)   │  │
                          │  └───────────────┬────────────────────────┘  │
                          │  ┌───────────────▼────────────────────────┐  │
                          │  │  axum HTTP/WS API   ·  static assets   │  │
                          │  └───┬───────────────────────────────┬────┘  │
                          └──────┼───────────────────────────────┼───────┘
                                 │ GET /            WS /api/v1/events
                                 │ GET /phone       POST /api/v1/...
                          ┌──────▼──────────┐            ┌───────▼────────────┐
                          │ Mirror renderer │            │  Phone PWA         │
                          │ webview / kiosk │            │  show/hide/action  │
                          │ (swappable)     │            │  (browser / PWA)   │
                          └─────────────────┘            └────────────────────┘
```

Three processes at most:
1. **`smartmmm`** — the core. Owns modules, state, config, and the API. Headless.
2. **Mirror renderer** — a webview shell (or Chromium kiosk) pointed at `http://localhost:PORT/`.
   Dumb on purpose: it renders what the core tells it.
3. **Phone** — a browser/PWA pointed at `http://<mirror-host>:PORT/phone`.

Because both clients speak the same protocol over the same origin, the Mirror and the Phone
share one frontend component library and one state model.

---

## 4. Technology choices

| Concern | Choice | Rationale |
|---|---|---|
| Language (core) | **Rust** (edition 2021) | Memory safety + no GC for a months-long appliance; trivial cross-compile to `aarch64`; strong module contract. |
| Async runtime | **tokio** | De-facto standard; needed by axum and most module SDKs. |
| HTTP/WS | **axum** + **tower-http** | Type-safe extractors, first-class WS, static file serving, small. |
| Serialization | **serde** / **serde_json** | Universal; used for API, protocol, and module IPC. |
| Config | **toml** + **schemars** | Human-editable TOML; per-module **JSON Schema** enables auto-generated phone settings forms. |
| Errors | **thiserror** (libs) + **anyhow** (app) | Idiomatic layering. |
| Logging | **tracing** + **tracing-subscriber** | Structured, async-aware; can stream to the phone UI. |
| Service discovery | **mdns-sd** | Advertise `_smartmmm._tcp` for tooling; the phone pairs via QR (see §7.3). |
| Config hot-reload | **notify** | Re-read and re-validate `config.toml` on change. |
| CLI | **clap** | `smartmmm run`, `smartmmm doctor`, `smartmmm pair`, `smartmmm module list`. |
| Renderer — macOS | **Tauri 2** (or `wry` directly) | Uses WKWebView: a real window, native feel, far lighter than Electron. |
| Renderer — Raspberry Pi 4 | **Chromium `--kiosk`** | More predictable than WebKitGTK on ARM and easier to pin/tune via flags. Comfortable at 4 GB. |
| Renderer (escape hatch) | **Slint** | If a webview ever proves undesirable, the core is unchanged — only the shell is rewritten. |
| Phone client | **PWA** (same web stack as the mirror UI) | No app store, works on iOS + Android, shares components with the mirror. |
| Deployment | **systemd** (Pi) / **launchd** (macOS) | Native service management per platform. |

### Why not the alternatives
- **Go:** smaller ecosystem for typed plugin contracts; no credible native UI; would still be
  web-based. Good second choice if dev speed outweighs the module type-safety story.
- **Node/TS:** the MagicMirror² path — huge module ecosystem, but an Electron/Chromium tax and
  GC drift over weeks of uptime. Choose it only to *avoid building this*.
- **Python:** fine for prototyping; higher idle footprint and weaker long-run guarantees.
- **C++:** no safety or velocity advantage here.
- **Electron:** bundles Chromium + Node; the renderer table above uses the *system* webview
  instead, so there is no reason to pay for Electron even with 4 GB available.

---

## 5. Module system

### 5.1 The contract

Every module implements one trait and returns one manifest.

```rust
/// Static description of a module. Cheap to clone; no runtime state.
pub struct Manifest {
    pub id: &'static str,               // "weather" — stable, unique, kebab-case
    pub name: &'static str,             // "Weather"
    pub version: &'static str,          // semver
    pub description: &'static str,
    pub icon: Option<&'static str>,      // asset name for the phone UI
    pub default_region: Region,          // where it renders by default
    pub default_order: i32,              // sort within region (lower = higher up)
    pub default_visible: bool,
    pub capabilities: Capabilities,      // what the module may do (network, gpio, ...)
    pub config_schema: fn() -> schemars::schema::RootSchema, // JSON Schema for config
}

/// Where a module draws on the mirror. Mirrors the familiar MagicMirror region model.
pub enum Region { TopLeft, TopCenter, TopRight, UpperThird, MiddleCenter, LowerThird,
                  BottomLeft, BottomCenter, BottomRight, Fullscreen }

#[async_trait]
pub trait Module: Send + Sync + 'static {
    /// Called once at startup, before `start`. Receives handles the module may keep.
    async fn init(&mut self, ctx: ModuleCtx) -> anyhow::Result<()>;

    /// Begin producing state (spawn timers, subscribe to feeds).
    async fn start(&mut self) -> anyhow::Result<()>;

    /// Graceful shutdown; must not block longer than ~2 s.
    async fn stop(&mut self) -> anyhow::Result<()>;

    /// Show/hide is core-owned state; modules are notified via `on_visibility`.
    fn visible(&self) -> bool;
    fn set_visible(&self, visible: bool);
    async fn on_visibility(&mut self, visible: bool) -> anyhow::Result<()> { let _ = visible; Ok(()) }

    /// Capabilities the module exposes. Default: none.
    ///
    /// The core namespaces each as `<module>__<name>` and serves them through
    /// `GET /api/v1/tools`. This one list is the phone's buttons, the HTTP tool
    /// API, and — later — the LLM's function definitions.
    fn tools(&self) -> Vec<ToolDescriptor> { Vec::new() }

    /// Call one of `tools()` by its local name. Default: "unknown tool".
    async fn call_tool(&mut self, tool: &str, args: serde_json::Value)
        -> anyhow::Result<serde_json::Value>;

    /// Current renderable view for the mirror.
    fn view(&self) -> ViewNode;

    /// Apply a validated config update (schema-checked by the core first).
    async fn on_config(&mut self, config: serde_json::Value) -> anyhow::Result<()>;
}

pub enum ToolKind { Query, Action }

pub struct ToolDescriptor {
    pub name: String,                   // "refresh"      (local to the module)
    pub title: String,                  // "Refresh now"  (button label)
    pub description: String,            // written for a model to read
    pub kind: ToolKind,                 // Query is side-effect free
    pub params_schema: Option<serde_json::Value>,   // JSON Schema
    pub returns_schema: Option<serde_json::Value>,  // informational
    pub dangerous: bool,                // phone asks for confirmation
}
```

Modules **never** touch the HTTP layer, the state store, or each other. They talk to the core
via `ModuleCtx`:

```rust
pub struct ModuleCtx {
    /// Publish a new view / status; the core diffs and broadcasts it.
    pub publish: mpsc::Sender<ModuleUpdate>,
    /// Read the module's own current config.
    pub config: ConfigHandle,
    /// Structured logging, surfaced to the phone.
    pub log: tracing::Span,
    /// Read-only access to another module's *published* status, for composition
    /// (e.g. a "system" module showing the weather module's last-update age).
    pub peek: PeekHandle,
}
```

Modules own their resilience. A failing fetch must degrade the module's
`status` (see `ModuleStatus::Degraded` in §6.1) rather than panic or take the
host down, and transient network errors are retried in-process. The weather
module, for example, gives Open-Meteo a 15 s timeout and retries the fetch up
to three times with a 2 s backoff: the API occasionally stalls long enough to
trip a single attempt, and without retries one slow response would blank the
module until its next 15-minute poll.

### 5.2 Registration

v1 uses an **explicit registry** in the core (`registry::builtin()`), which is simple, greppable,
and has zero ABI risk. The `inventory` crate (link-time self-registration) is a drop-in upgrade
later if the explicit list becomes tedious.

```rust
pub fn builtin() -> Vec<ModuleFactory> {
    vec![
        clock::factory(),
        weather::factory(),
        news::factory(),
        presence::factory(),
    ]
}
```

### 5.3 The view protocol

The core → renderer contract is a small, closed set of primitives. This keeps the renderer dumb,
keeps modules theme-consistent, and makes the phone preview trivial.

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewNode {
    Text    { text: String, size: TextSize, weight: Weight, align: Align },
    Stack   { direction: Axis, gap: f32, children: Vec<ViewNode> },
    Row     { columns: Vec<Column>, children: Vec<ViewNode> },     // aligned columns
    KeyValue{ items: Vec<(String, String)> },
    List    { items: Vec<ListItem>, numbered: bool },
    Image   { src: String, max_height: f32 },
    Icon    { name: String, size: f32 },
    Progress{ value: f32, label: Option<String> },                 // 0.0..=1.0
    Countdown{ target: String, label: Option<String> },            // RFC 3339
    Divider,
    Empty,                                                         // render nothing
}
```

Example — a weather module's view:

```json
{
  "type": "stack", "direction": "vertical", "gap": 8,
  "children": [
    { "type": "text", "text": "72°", "size": "xl", "weight": "light", "align": "center" },
    { "type": "text", "text": "Partly cloudy", "size": "md", "align": "center" },
    { "type": "keyvalue", "items": [["Humidity", "54%"], ["Wind", "8 mph"]] }
  ]
}
```

A module that genuinely needs bespoke rendering can opt into a **custom asset bundle**: static
files served from `/modules/<id>/assets/` and rendered in a **sandboxed `<iframe>`** that talks
over `postMessage`. This is the escape hatch, deliberately not the default.

### 5.4 Tools — one surface, three consumers

`tools()` is the module's *behaviour* the way `view()` is its *appearance*. A tool is declared
once, and the same definition serves three consumers:

| Consumer | Uses |
|---|---|
| Phone UI | renders *every* tool, in a per-module sheet; `params_schema` decides one tap vs. a form |
| HTTP | `GET /api/v1/tools`, `POST /api/v1/tools/{name}` |
| LLM | `params_schema` → `parameters` (OpenAI) or `input_schema` (Anthropic) |

**Naming.** Tools are exposed as `<module>__<tool>`, e.g. `weather__refresh`. The separator is a
double underscore because OpenAI rejects `.` in function names, and a single `_` would be
ambiguous against module ids and tool names that already contain one. Clients never parse the
name: the core resolves it by looking it up in the registry, so the separator is a labelling
convention, not a parsing contract.

**Core-owned tools.** Show/hide, region and order are core state, not module state. The core
therefore publishes `core__show_module`, `core__hide_module`, `core__list_modules`,
`core__get_status`, `core__set_region` and `core__set_order` alongside each module's tools. This is
what makes "hide the clock" work without the clock implementing anything — and it will work for
modules that do not exist yet.

**Presentation is derived, not declared.** There is deliberately no `ui` flag saying "show this in
the phone". The phone renders everything and infers shape from the schema: no `properties` means
one tap, any `properties` means a form. That same rule sets the hierarchy — argument-free tools
stay in front, argument-taking ones fold into a `More tools` row — so no one has to curate what to
surface. A declaration would be a second place to keep in sync with `params_schema`, and it would
be the place that drifts: a tool added for an LLM would silently not appear for the person holding
the phone. The phone's default screen therefore shows no tools at all. Each module is a status dot,
a name, a visibility toggle and a `⋯` that opens a sheet holding its tools, config and results,
which keeps the list scannable no matter how many modules are added.

The schemas also carry one small extension so a generated form can offer the right control:

```json
{ "type": "string", "x-options": "modules" }   // render the current module ids
```

Module ids are known to the core at runtime and cannot be enumerated in a static schema, so
`x-options` is how a schema asks for them. `x-` keywords are the conventional JSON Schema
extension point, so this stays valid and is ignored by clients that do not know it.

**Errors are results.** A tool that fails returns `200` with `{"ok": false, "error": "..."}`. A
model can read that message and try something else; an HTTP 500 tells it nothing. Only an unknown
tool name is a `404`.

**Deliberately not built yet.** A `ToolHandle` in `ModuleCtx` would let one module call another's
tools. That needs two guards first, because a module's task owns its state exclusively: the router
must **reject self-calls** (a module calling its own tool would await a reply from the task that is
currently awaiting it — a deadlock), and every cross-module call needs a timeout. Until then,
modules compose by reading each other's published status, not by calling each other.

---

## 6. Control plane

### 6.1 State store

The core is the single source of truth. For each module it holds:

```rust
pub struct ModuleState {
    pub id: String,
    pub visible: bool,
    pub region: Region,
    pub order: i32,
    pub status: ModuleStatus,       // Ok | Degraded(String) | Error(String) | Starting
    pub view: ViewNode,             // last published
    pub config: serde_json::Value,  // validated
    pub revision: u64,              // monotonic; clients resync on gaps
    pub last_updated: SystemTime,
}
```

Any mutation goes through the store, bumps `revision`, and emits an event on the broadcast
channel. The renderer and phone are both pure subscribers — they never mutate state directly.

Four fields outlive the process — `visible`, `region`, `order`, `config` — and those are the ones
recorded to disk (§8.1). `status` and `view` are derived and are recomputed on every boot.

`config` is the **running** config: not the fragment written into `config.toml`, and never `null`
for a module that has a config at all. `Host` seeds it at startup with what the module is about to
use — the module's own defaults with the file's table laid over them — so the very first `/modules`
response already carries every module's real settings. The module corrects it moments later if it
disagrees, but nothing ever *drops* to `null`: a client that builds a generated form from that
snapshot would otherwise fall back to the schema's `default` values, or to blank boxes for a field
like the clock's optional label that has no default, and that reads as "my settings are gone". The
phone treats an unknown config as unknown rather than empty, and says so.

The store deliberately does *not* know about persistence. Marking a value as "worth keeping" is a
judgement only the caller can make: the store also absorbs each module's own startup defaults,
which must not be recorded. So `Host` owns that decision, and every mutating entry point goes
through `Host` rather than straight to the store.

### 6.2 Event bus

`tokio::sync::broadcast` for fan-out. Event kinds:

```rust
pub enum Event {
    ModuleUpdated { id: String, patch: ModulePatch, revision: u64 },
    ModuleRemoved { id: String },
    DisplayChanged { screen_on: bool, brightness: Option<f32> },
    Log { level: Level, target: String, message: String },
    Toast { kind: ToastKind, message: String },   // transient phone notification
}
```

`ModulePatch` is a partial update so a clock ticking every second sends one small field, not the
whole view. Clients detect a missed revision and request a full snapshot (see §7.2).

---

## 7. API

Base path `/api/v1`. JSON everywhere. All mutating endpoints require the pairing token (§7.3).

### 7.1 Endpoints

```
# Discovery / identity
GET    /api/v1/info                  → { name, version, host, platform, uptime }

# Modules
GET    /api/v1/modules               → [ ModuleState ]          (full snapshot)
GET    /api/v1/modules/{id}          → ModuleState
PATCH  /api/v1/modules/{id}          → { visible?, region?, order?, config? }   → ModuleState

# Tools — one callable surface for the phone, HTTP clients, and the LLM
GET    /api/v1/tools                 → [ ToolInfo ]             (namespaced, LLM-ready)
POST   /api/v1/tools/{name}          → body *is* the args object → { ok, result?, error? }

# Global
GET    /api/v1/display               → { screen_on, brightness, schedule }
POST   /api/v1/display               → set power / brightness (platform-abstracted)
POST   /api/v1/toast                 → push a transient message to the mirror
GET    /api/v1/logs?level=info       → recent ring buffer

# Live stream
WS     /api/v1/events                → see §7.2
```

### 7.2 WebSocket protocol

```
Client → Server
  { "type": "hello",     "token": "...", "since": 417 }        // since = last revision, optional
  { "type": "subscribe", "channels": ["modules", "logs", "display"] }
  { "type": "call_tool", "name": "weather__refresh", "args": {} }
  { "type": "set_visible", "id": "clock", "value": false }
  { "type": "ping" }

Server → Client
  { "type": "welcome",     "modules": [ ModuleState ], "revision": 418 }
  { "type": "patch",       "id": "weather", "patch": {...} }
  { "type": "removed",     "id": "calendar" }
  { "type": "tool_result", "name": "weather__refresh", "ok": true, "result": {...} }
  { "type": "display",     "screen_on": true, "brightness": 0.8 }
  { "type": "log",         "level": "warn", "target": "weather", "message": "..." }
  { "type": "toast",       "kind": "info", "message": "Weather updated" }
  { "type": "pong" }
```

The `since` field lets a reconnecting client either receive a diff or be told to resync. Revision
gaps are never silently tolerated.

### 7.3 Pairing & auth

1. On first run the core generates a random token and stores it in the platform config dir
   (mode `0600`).
2. The mirror displays a **QR code** encoding `http://<host>:<port>/phone#<pairing-code>`.
3. The phone scans it, exchanges the short-lived code for the long-lived token, and stores it
   (localStorage / cookie).
4. All `/api/v1` requests carry `Authorization: Bearer <token>`. WS sends it in the `hello` frame.
5. Token can be rotated with `smartmmm pair --rotate`, invalidating all phones.

**Honest note on discovery:** browsers *cannot* use mDNS/Bonjour from a PWA. So QR is the primary
pairing path; `http://smartmmm.local` is a convenience that works on macOS/iOS and is unreliable
on Android. mDNS is still advertised for CLI tooling and for a future native client.

### 7.4 Security posture

- Listens on all interfaces but requires the token for mutation; read-only `/info` is allowed only
  within a configured CIDR.
- TLS is optional and off by default (LAN, self-signed is worse than plain HTTP without a
  trusted CA). If exposed beyond the LAN, terminate TLS at a reverse proxy.
- Rate-limit action invocations per module to prevent a runaway client from spamming a module.

---

## 8. Configuration

`config.toml`, resolved in this order (later wins):
`/etc/smartmmm/config.toml` → `$XDG_CONFIG_HOME/smartmmm/config.toml` → `./config.toml` → `SMMMM__*` env vars.

```toml
[core]
listen = "0.0.0.0:8080"
name = "Hallway Mirror"
log_level = "info"

[display]
driver = "auto"            # auto | pi_vcgencmd | pi_cec | macos | noop
brightness = 0.9
schedule = [
  { at = "22:30", screen_on = false },
  { at = "06:30", screen_on = true },
]

[presence]
driver = "noop"            # noop | gpio_pir | camera
wake_on_motion = true

[modules.clock]
visible = true
region = "top_left"
order = 0
[modules.clock.config]
format_24h = true
show_seconds = false

[modules.weather]
visible = true
region = "top_right"
order = 10
[modules.weather.config]
api_key = "…"
location = "52.37,4.90"
units = "metric"
poll_minutes = 20
```

**Hot reload:** `notify` watches the file; on change the core re-parses, validates each
`[modules.<id>.config]` against that module's `config_schema`, and applies atomically. Invalid
config is rejected with a `Toast` to the phone and a log entry — the running config is untouched.

### 8.1 Declared config vs. runtime state

Two files, two owners. `config.toml` is written by a person, read once at startup, and never
rewritten by the process: a daemon that reformats the file you hand-edit destroys your comments and
takes away the one file worth reviewing in a diff. Everything the phone changes at runtime —
visibility, region, order, per-module config — goes to a separate state file instead.

```text
manifest defaults  <  config.toml  <  state.json
```

```json
{
  "version": 1,
  "modules": {
    "clock":   { "visible": false },
    "weather": { "region": "bottom_right", "config": { "forecast_days": 5 } }
  }
}
```

Only fields that *differ* from what `config.toml` resolves to are written, so the file stays
authoritative for anything the phone has not touched. The comparison is against the **resolved**
value — the module's own defaults with the file's table laid over them — because a partial
`[modules.x.config]` table would otherwise look like a change to every field it never mentioned,
and the first save would quietly pin them all. Deleting the file returns every module to
`config.toml`; that is the reset button, and the only one needed.

Once a module's config *is* overridden from a phone, the whole object is recorded, so later edits
to that module's `[modules.x.config]` table are shadowed until the state file goes away. That is
the deliberate side of the trade: an explicit tap beats a file edit, and it is discoverable in the
state file itself. Field-level config diffs would remove even that, but they buy little for the
machinery.

Writes are atomic (temp file, `fsync`, `rename`) and skipped when the resulting bytes are unchanged.
The **first** change goes to disk immediately: a mirror loses power without warning, and a change
that only survives a clean shutdown is not really saved. A *burst* of taps is then coalesced, so
toggling three modules is still one or two writes rather than three. The flush on SIGTERM/Ctrl-C is
a belt-and-braces last write, not the mechanism that makes anything durable.

Location: `$STATE_DIRECTORY/state.json` (systemd `StateDirectory=`), else
`$XDG_STATE_HOME/smartmmm/state.json`, else `~/.local/state/smartmmm/state.json`. `--state <path>`
overrides it, `--no-state` runs stateless, and `smartmmm doctor` prints the path in use. A state
file that cannot be parsed is renamed `*.bad` and the run continues on defaults — the mirror must
never be unbootable because of it, and silently overwriting something a person might want to read
would be worse than keeping it.

---

## 9. Cross-platform abstractions

Everything OS-specific hides behind a trait with a platform impl selected at startup. `main.rs`
stays free of `#[cfg]` branches beyond wiring.

```rust
#[async_trait]
pub trait DisplayControl: Send + Sync {
    async fn set_power(&self, on: bool) -> anyhow::Result<()>;
    async fn set_brightness(&self, level: f32) -> anyhow::Result<()>;
    fn capabilities(&self) -> DisplayCaps;
}

#[async_trait]
pub trait PresenceSensor: Send + Sync {
    /// Resolves when motion is detected (or immediately if configured to always-on).
    async fn wait_for_motion(&self) -> anyhow::Result<()>;
}

pub trait Paths: Send + Sync {
    fn config_dir(&self) -> PathBuf;
    fn data_dir(&self) -> PathBuf;
    fn cache_dir(&self) -> PathBuf;
}
```

| Concern | Raspberry Pi | macOS | Fallback |
|---|---|---|---|
| Display power | `vcgencmd display_power 0/1`; HDMI-CEC for TVs | `pmset displaysleepnow` | `NoopDisplayControl` |
| Brightness | `ddcutil` / CEC / backlight sysfs | `brightness` CLI / AppleScript | `NoopDisplayControl` |
| Motion | GPIO PIR (`gpio`/`rppal`) or camera | not supported | `NoopPresenceSensor` |
| Temp/health | `vcgencmd measure_temp`, `/sys/class/thermal` | `powermetrics` / SMC | return `Unavailable` |
| Service | systemd unit | launchd plist | run in foreground |
| Config dir | `/etc/smartmmm` | `~/Library/Application Support/smartmmm` | `$XDG_CONFIG_HOME` |

The `smartmmm doctor` subcommand probes each capability and prints what is available on the
current host — the single fastest way to diagnose Pi-specific problems.

---

## 10. Deployment

### Raspberry Pi 4 Model B (Raspberry Pi OS Bookworm, 64-bit)
- Use the **64-bit** image and the official 5 V/3 A USB-C supply. Confirm no undervoltage with
  `vcgencmd get_throttled` (`0x0` is healthy).
- Force a 1080p output for the mirror panel; leave headroom rather than driving 4K.
- Build natively on the Pi (`cargo build --release` — slow, but frictionless) or cross-compile
  from the Mac with `cross` or `cargo-zigbuild` targeting `aarch64-unknown-linux-gnu`.
  Produce one stripped binary.
- `deploy/smartmmm.service` — the core, `Restart=on-failure`, `After=network-online.target`,
  `StateDirectory=smartmmm`, supplementary `gpio`/`video` groups for the PIR and
  `vcgencmd` display control. *(Implemented.)*
- `deploy/labwc/mirror-kiosk.sh` + `deploy/labwc/autostart` — the renderer. A small watchdog
  launched by the labwc session: it polls the core until it answers, then runs
  `chromium --kiosk --ozone-platform=wayland …`, and relaunches Chromium whenever it exits.
  This replaced the originally planned `smartmmm-display.service`: a crashed Chromium
  network service used to leave a permanent white screen, and the watchdog turns that into
  a few seconds of blank. Kiosk flags are pinned in one file so they are easy to tune.
  *(Implemented.)*
- Force GPU acceleration on: the Pi 4 uses `dtoverlay=vc4-kms-v3d` (default on Bookworm) and
  gives Chromium working GLES 3.0. If you see software rendering, that overlay is the culprit.
- Bookworm uses Wayland/`labwc`. X11-era kiosk recipes (`.config/lxsession/LXDE-pi/autostart`)
  are **stale** — do not copy them. Use a `labwc` autostart entry (as here) or a systemd
  user unit.
- Disable blanking at the compositor level; control the panel through `DisplayControl`, not `xset`.
- Send logs to a tmpfs with a size cap to protect the SD card (§2).
- Boot order: core ready → display launches → display retries until the core answers —
  implemented literally in `deploy/labwc/mirror-kiosk.sh`.

### macOS
- `deploy/launchd/com.smartmmm.plist` — `RunAtLoad`, `KeepAlive`.
- `smartmmm run --foreground` for development; the Tauri shell provides a real window.

### Startup sequence
```
systemd/launchd
   └─ smartmmm (core)  ── loads config ── registers modules ── binds :8080 ── announces "ready"
        └─ renderer  ── polls /api/v1/info until 200 ── opens http://localhost:8080/
             └─ phone ── pairs via QR ── connects WS ── receives snapshot
```

---

## 11. Roadmap

| Milestone | Deliverable | Notes |
|---|---|---|
| **M0 — Skeleton** | Workspace, `protocol`, core registry, axum server, `clock` module, mirror web UI | Done: clock renders in a browser on Mac. |
| **M1 — Control** | Phone PWA: list, show/hide, call tools. WS patches. QR pairing. Runtime state survives restart (§8.1). | Done except QR pairing (bearer-token hook exists, no pairing flow). |
| **M2 — Modules** | `weather`, `calendar`, `sysinfo`. Auto-generated config forms from JSON Schema. | Done: `weather` (Open-Meteo, with fetch retries) and `news` (RSS). `calendar`/`sysinfo` remain. |
| **M3 — Appliance** | `DisplayControl`, `PresenceSensor`, night scheduling, `smartmmm doctor`. | Done on the live Pi: PIR presence (GPIO 21), display power via `vcgencmd`, `doctor`. Night scheduling not yet. |
| **M4 — Hardening** | Config hot-reload, revision-gap resync, tool rate limiting, log ring buffer, CI cross-compile. | |
| **M5 — Extensibility** | WASM module host behind the same contract; custom asset bundle + iframe escape hatch. | Optional; unlocks third-party modules. |
| **M6 — Assistant** | LLM layer over `/api/v1/tools`; MCP server exposing the same registry. | The tool registry (§5.4) is the whole interface. |

### Risk register

| Risk | Impact | Mitigation | Validate by |
|---|---|---|---|
| Thermal throttling in a mirror enclosure | Medium | Heatsink/fan, vented case, `vcgencmd measure_temp` in `doctor` | M3 |
| Undervoltage causing phantom slowness | Medium | Official 5 V/3 A PSU; surface `vcgencmd get_throttled` in `doctor` | M3 |
| WebKitGTK flakiness on ARM | Low | Use Chromium kiosk on Pi 4 (already the default) | M3 |
| SD card wear from 24/7 logging | Medium | Log to tmpfs + size cap; recommend A1/A2 card or USB boot | M3 |
| Rust build times on Pi | Low | Cross-compile from Mac | M0 |
| Module contract too narrow | Medium | `ViewNode` + iframe escape hatch covers custom UI | M2 |
| Config drift across machines | Low | One `config.toml` schema, validated, version-controlled per deployment | M4 |
| Stale `state.json` silently shadowing `config.toml` | Low | Only fields that actually differ are written; `doctor` prints the path; deleting the file is the documented reset (§8.1) | M1 |

---

## 12. Repository layout

```
SmartMMM/
├── ARCHITECTURE.md              # this document
├── Cargo.toml                   # workspace
├── crates/
│   ├── protocol/                # shared serde types: Manifest, ViewNode, API, WS frames
│   ├── core/                    # registry, state store, event bus, config, persistence, ModuleCtx
│   ├── server/                  # axum routes, WS handler, static assets, auth, mDNS
│   └── modules/
│       ├── clock/               # implemented
│       ├── weather/             # implemented (Open-Meteo, fetch retries)
│       ├── news/                # implemented (RSS)
│       └── presence/            # implemented (GPIO PIR → display power)
├── ui/
│   ├── mirror/                  # mirror web frontend (consumes ViewNode)
│   ├── phone/                   # control PWA
│   └── shared/                  # components + protocol client shared by both
├── bin/smartmmm/                # the core binary (clap: run | doctor | pair | module)
└── deploy/
    ├── systemd/                 # design draft for a second display unit
    ├── smartmmm.service         # the core as deployed (User=Samman, StateDirectory)
    ├── labwc/                   # kiosk watchdog + labwc autostart (the Pi renderer)
    └── launchd/                 # macOS units
```

---

## 13. Open questions

1. ~~**Weather provider**~~ — **Resolved: Open-Meteo** (no key, JSON, current + daily
   forecast in one call). Configured by label + coordinates in the module config.
2. **Calendar source** — local `.ics` URL, CalDAV, or Google/iCloud. Each is a different amount
   of OAuth work.
3. **Is Home Assistant in the picture?** If yes, an HA/MQTT bridge module is likely the single
   highest-value module to build early.
4. **Mirror hardware** — panel size/resolution and whether a two-way mirror is already in place.
   Drives typography and region layout.
6. **Motion sensing** — PIR on GPIO, camera, or none.
