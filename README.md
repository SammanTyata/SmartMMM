# SmartMMM

A modular smart mirror: a Rust core that hosts pluggable modules, drives a
browser-based mirror display, and exposes a REST + WebSocket API so a phone can
show/hide modules, change their config, and call their tools.

Design lives in [`ARCHITECTURE.md`](./ARCHITECTURE.md).

## Status

Running end to end on a Raspberry Pi 4 (native build), with four modules:

- Rust core: module registry, state store, event bus, config, persistence, host runtime
- axum HTTP + WebSocket API
- Mirror UI (renders the view tree) and phone control UI
- Modules: `clock`, `weather` (Open-Meteo, no API key), `news` (RSS), `presence`
  (GPIO PIR that sleeps/wakes the display via `vcgencmd`)
- Resilience: network fetches retry transient failures — the weather module
  retries 3× with a 15 s request timeout, so one slow Open-Meteo response no
  longer blanks the module until its next poll
- Deployment: `deploy/smartmmm.service` (systemd) plus a `labwc` kiosk watchdog
  in `deploy/labwc/` that relaunches Chromium if it dies

Not yet done: auth/pairing (the hook exists but there is no QR flow), the
`calendar` and `sysinfo` modules, config hot-reload. See the roadmap in the
design doc.

## Quick start

```sh
cargo run -p smartmmm                 # core + server on 0.0.0.0:8080
cargo run -p smartmmm -- doctor       # host capabilities & Pi health checks
cargo run -p smartmmm -- module-list  # modules in this build
```

Then open:

- mirror — <http://localhost:8080/>
- phone — <http://localhost:8080/phone> (or the LAN URL printed at startup)

Useful flags: `--config <path>`, `--listen <addr>`, `--state <path>`,
`--no-state`.

## Layout

```
crates/protocol/         shared serde contract: Manifest, ViewNode, API, WS frames
crates/core/             module trait, registry, state store, config, host runtime
crates/server/           axum routes, WebSocket, embedded UI assets
crates/modules/clock/    reference module (no config dependencies)
crates/modules/weather/  reference module (async I/O, tools, degraded states)
crates/modules/news/     RSS headlines (multi-feed, lower-third strip)
crates/modules/presence/ PIR motion → display sleep/wake (headless)
ui/mirror/               mirror renderer (pure subscriber)
ui/phone/                phone control UI
ui/shared/               shared CSS + protocol client (view renderer, API, WS)
bin/smartmmm/            binary: wiring, CLI, doctor
deploy/                  systemd + launchd units, labwc kiosk watchdog
```

## Module contract

A module implements one trait and returns one manifest. The core drives it on a
timer, diffs the result, and broadcasts changes. Modules never touch HTTP, the
state store, or each other.

```rust
#[async_trait]
impl Module for MyModule {
    fn manifest(&self) -> Manifest { /* id, name, default region, config schema */ }
    fn update_interval(&self) -> Duration { Duration::from_secs(60) }
    async fn update(&mut self) -> anyhow::Result<()> { /* refresh; set self.view */ }
    fn visible(&self) -> bool { self.visible }
    fn set_visible(&mut self, v: bool) { self.visible = v; }
    fn view(&self) -> ViewNode { self.view.clone() }
    // optional: tools(), call_tool(), on_config(), on_visibility(), init(), status()
}
```

Rendering goes through a small closed set of primitives (`ViewNode`), which keeps
the renderer dumb and every module visually consistent. A module's `config_schema`
is a JSON Schema; the phone UI generates its settings form from it automatically.

A module's `tools()` is the same idea for *behaviour*. Each tool is declared once
with a title, a description written for a model to read, a JSON Schema for its
arguments, and a `kind` of `query` or `action`. The core namespaces them and
serves them all through `/tools`, so the phone, the HTTP call, and a future LLM
all drive the exact same code path. The phone renders every tool and derives its
shape from `params_schema` — no arguments means one tap, any arguments means a
generated form — so there is no separate "show this in the UI" flag to drift.
Show/hide is deliberately *not* a module tool: it is core state, so
`core__hide_module` works on every module, including ones that do not exist yet.

### Adding a module

1. `crates/modules/<name>/` with a `Module` impl.
2. Register it in `bin/smartmmm/src/main.rs`:
   ```rust
   registry.register(ModuleFactory::new(|| Box::new(MyModule::new())));
   ```
3. Add config under `[modules.<id>]` in your `config.toml` if you want overrides.

## API

Base path `/api/v1`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/info` | name, version, platform, uptime |
| GET | `/modules` | full state snapshot |
| GET | `/modules/{id}` | one module |
| PATCH | `/modules/{id}` | `{ visible?, region?, order?, config? }` |
| GET | `/tools` | every callable tool, LLM-ready |
| POST | `/tools/{name}` | call a tool; body *is* the arguments |
| WS | `/events` | live patches, tool results, toasts |

Tool names are namespaced `<module>__<tool>` (`weather__refresh`,
`core__hide_module`) — a double underscore, because that is the only separator
every tool-calling API accepts while still allowing single ones inside ids.

```sh
curl -s localhost:8080/api/v1/modules | jq
curl -s localhost:8080/api/v1/tools | jq -r '.[].name'          # the tool list
curl -s -X POST localhost:8080/api/v1/tools/core__hide_module \
     -H 'content-type: application/json' -d '{"id":"clock"}'
curl -s -X POST localhost:8080/api/v1/tools/clock__get_time -d '{}'
curl -s -X POST localhost:8080/api/v1/tools/weather__refresh -d '{}'
curl -s -X POST localhost:8080/api/v1/tools/weather__get_forecast -d '{}' | jq
```

A tool that fails answers `200` with `{"ok": false, "error": "..."}` rather than
an HTTP error, so a model can read the message and correct itself. Only an
unknown tool name is a `404`.

The phone at `/phone` is a front end for this whole surface, without becoming a
developer console. The screen itself lists modules only — a status dot, a name, a
visibility toggle and a `⋯`. Everything a module can do opens in a sheet from that
`⋯`: tools that take no arguments are a single tap, tools that do take arguments
fold into a `More tools` row, config sits behind `Configure`, and a result
collapses to one line you can expand. So the API is exercised from the device you
actually carry, not only from `curl`.

## Configuration

Copy [`config.example.toml`](./config.example.toml) to `config.toml`. Search order
is `--config`, `/etc/smartmmm/config.toml`, `$XDG_CONFIG_HOME/smartmmm/config.toml`,
`./config.toml`, then built-in defaults.

### Runtime state

`config.toml` is declared intent, and the process never writes to it — a file you
hand-edit should not be rewritten by a daemon. Changes made from the phone
(visibility, region, order, per-module config) accumulate in a separate state file
instead, so both survive a restart:

```text
manifest defaults  <  config.toml  <  state.json
```

It lives at `$STATE_DIRECTORY/state.json` (set by the systemd unit), else
`$XDG_STATE_HOME/smartmmm/state.json`, else `~/.local/state/smartmmm/state.json`.
`--state <path>` overrides it; `--no-state` disables persistence and returns every
restart to `config.toml`. `smartmmm doctor` prints the path in use.

Only fields that actually *differ* from what `config.toml` resolves to are
written, so editing `config.toml` keeps working for everything you have not
overridden — including the fields you never mentioned in a partial
`[modules.x.config]` table. Delete the state file to hand every module back to
`config.toml`. Writes are atomic, and skipped when nothing changed. The first
change goes straight to disk, because a device that loses power without warning
should not lose the tap you just made; a burst of taps then coalesces so it stays
one write rather than ten, and the shutdown flush is only a last belt-and-braces
write.

The store also advertises each module's **running** config from the first request,
so a phone that loads during or right after a restart shows the settings it
actually has rather than a blank or defaulted form.

## Deploying to a Raspberry Pi 4

The current deployment builds natively on the Pi (slow but frictionless) and
drives the display with a `labwc` autostart watchdog rather than a second
systemd unit.

```sh
# On the Pi (Raspberry Pi OS Bookworm, 64-bit), as the user who owns the session:
git clone <repo> ~/smartmmm && cd ~/smartmmm
~/.cargo/bin/cargo build --release

sudo cp deploy/smartmmm.service /etc/systemd/system/   # check User=/ExecStart= paths
sudo cp config.example.toml /etc/smartmmm/config.toml  # then edit it
sudo systemctl daemon-reload && sudo systemctl enable --now smartmmm

# Renderer: kiosk watchdog launched by the labwc session
mkdir -p ~/.config/labwc
cp deploy/labwc/mirror-kiosk.sh ~/.config/labwc/
cp deploy/labwc/autostart ~/.config/labwc/autostart
chmod +x ~/.config/labwc/mirror-kiosk.sh
```

Cross-compiling from the Mac also works (`cross build --release --target
aarch64-unknown-linux-gnu`); adjust the unit's `ExecStart=` to wherever the
binary lands.

Run `smartmmm doctor` on the Pi afterwards: it reports temperature, throttle
state, and which display-control tools are present.

The kiosk watchdog waits until the server answers on `localhost:8080` before
launching Chromium, and relaunches it if it ever exits — so a crashed browser
recovers in seconds instead of leaving a blank (white) screen until the next
reboot.

The unit sets `StateDirectory=smartmmm`, so runtime state lives in
`/var/lib/smartmmm/state.json`, owned by the service user and untouched by a
redeploy of the binary.

## Development

```sh
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```
