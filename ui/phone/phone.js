// Phone control UI.
//
// Two rules keep this usable with a thumb:
//
//   1. The screen is a short list. Each row is a status dot, a name, a visibility
//      toggle and a "⋯". No tools, no forms, no JSON. Nothing competes.
//
//   2. Everything a module can *do* lives in a bottom sheet, ordered by how
//      likely you are to want it:
//
//        ┌ Weather ──────────────────────────── ⓘ ✕ ┐
//        │ [ Refresh now ]            action        │   no arguments → one tap
//        │ [ Get conditions ]         query         │
//        │ [ Get forecast ]           query         │
//        │ ▸ More tools · 1                         │   arguments → behind this
//        │ ▸ Configure                              │   config → behind this
//        └──────────────────────────────────────────┘
//
//      and inside it, descriptions and API names stay hidden until ⓘ is tapped.
//
// Both the ordering and the split are derived from each tool's `params_schema`,
// so a tool added for an LLM still appears here with no second list to maintain.
// Accessibility note: every affordance is a real <button>, so focus order and
// screen readers work without extra effort.

import { api, connectEvents } from "/shared/protocol.js";

const CORE = "core";

const modules = new Map(); // module id -> ModuleState
const tools = new Map(); // namespaced tool name -> ToolInfo
const ui = new Map(); // owner key -> view state that must survive a rebuild
const cards = new Map(); // owner key -> the row/card element in the list

/// Which sheet is open: a module id, `core`, or null.
let sheetKey = null;

function uiState(key) {
  let st = ui.get(key);
  if (!st) {
    st = {
      results: new Map(), // tool name -> { ok, value, at, expanded }
      args: new Map(), // tool name -> the arguments last sent
      forms: new Set(), // tool names currently showing an argument form
      details: false, // show descriptions and API names
      more: false, // reveal tools that take arguments
      config: false, // reveal the config form
    };
    ui.set(key, st);
  }
  return st;
}

/// A half-filled form or an open config editor: rebuilding the sheet under one
/// would throw away what was typed, so patches leave it alone until it closes.
///
/// The exception is a config editor for a config the module has not reported
/// yet. Nothing can be lost there, and leaving it alone would strand the form on
/// its "waiting" note, so keep rebuilding until the values arrive.
function hasDraft(st, mod) {
  if (st.forms.size > 0) return true;
  if (!st.config) return false;
  return mod === undefined || (mod.config !== null && mod.config !== undefined);
}

function toolsFor(moduleId) {
  return [...tools.values()].filter((tool) => tool.module === moduleId);
}

function takesArgs(tool) {
  const properties = tool.params_schema?.properties;
  return Boolean(properties) && Object.keys(properties).length > 0;
}

/// Actions first — the phone's job is mostly to *do* things — then queries,
/// each alphabetical.
function sortTools(list) {
  return [...list].sort(
    (a, b) =>
      Number(a.kind !== "action") - Number(b.kind !== "action") ||
      a.title.localeCompare(b.title),
  );
}

function sortedModules() {
  return [...modules.values()].sort(
    (a, b) =>
      a.region.localeCompare(b.region) ||
      a.order - b.order ||
      a.id.localeCompare(b.id),
  );
}

function regionLabel(region) {
  return String(region).replace(/_/g, " ");
}

function timestamp() {
  return new Date().toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

const SVG_NS = "http://www.w3.org/2000/svg";

// Inline SVG rather than glyph characters: no dependence on which fonts the
// device has, and no emoji fallback surprises.
const ICONS = {
  more: "M6 10c-1.1 0-2 .9-2 2s.9 2 2 2 2-.9 2-2-.9-2-2-2zm12 0c-1.1 0-2 .9-2 2s.9 2 2 2 2-.9 2-2-.9-2-2-2zm-6 0c-1.1 0-2 .9-2 2s.9 2 2 2 2-.9 2-2-.9-2-2-2z",
  info: "M11 7h2v2h-2zm0 4h2v6h-2zm1-9C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm0 18c-4.41 0-8-3.59-8-8s3.59-8 8-8 8 3.59 8 8-3.59 8-8 8z",
  close:
    "M19 6.41 17.59 5 12 10.59 6.41 5 5 6.41 10.59 12 5 17.59 6.41 19 12 13.41 17.59 19 19 17.59 13.41 12z",
};

function iconButton(label, icon) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "icon-btn";
  button.title = label;
  button.setAttribute("aria-label", label);

  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", "18");
  svg.setAttribute("height", "18");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(SVG_NS, "path");
  path.setAttribute("d", icon);
  path.setAttribute("fill", "currentColor");
  svg.appendChild(path);
  button.appendChild(svg);

  return button;
}

/// A CSS-drawn triangle, so a disclosure marker looks the same everywhere.
function caretElement(open) {
  const caret = document.createElement("span");
  caret.className = "caret";
  caret.dataset.open = String(open);
  return caret;
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

function render() {
  const root = document.getElementById("modules");
  root.replaceChildren();
  cards.clear();

  if (toolsFor(CORE).length) {
    const el = coreRow();
    cards.set(CORE, el);
    root.appendChild(el);
  }

  for (const mod of sortedModules()) {
    const el = moduleCard(mod);
    cards.set(mod.id, el);
    root.appendChild(el);
  }
}

/// Rebuild one entry in place, leaving the rest of the list — and the sheet,
/// which is a sibling — untouched.
function refreshCard(key) {
  const el =
    key === CORE
      ? coreRow()
      : modules.has(key)
        ? moduleCard(modules.get(key))
        : null;
  if (!el) return;

  const existing = cards.get(key);
  cards.set(key, el);
  if (existing?.parentElement) existing.replaceWith(el);
  else document.getElementById("modules").appendChild(el);
}

function coreRow() {
  const row = document.createElement("button");
  row.type = "button";
  row.className = "row-card";
  row.addEventListener("click", () => openSheet(CORE));

  const title = document.createElement("span");
  title.className = "row-title";
  title.textContent = "Mirror API";

  const count = toolsFor(CORE).length;
  const meta = document.createElement("span");
  meta.className = "row-meta";
  meta.textContent = `${count} tool${count === 1 ? "" : "s"}`;

  const caret = caretElement(false);

  row.append(title, meta, caret);
  return row;
}

function moduleCard(mod) {
  const el = document.createElement("article");
  el.className = "card";

  const head = document.createElement("div");
  head.className = "card-head";

  const left = document.createElement("div");
  const h2 = document.createElement("h2");
  const dot = document.createElement("span");
  dot.className = "status-dot";
  dot.dataset.state = mod.status?.state ?? "starting";
  h2.append(dot, document.createTextNode(mod.name));
  const desc = document.createElement("p");
  desc.className = "desc";
  desc.textContent = mod.description;
  left.append(h2, desc);

  const right = document.createElement("div");
  right.className = "card-tools";

  const toggle = document.createElement("label");
  toggle.className = "toggle";
  const checkbox = document.createElement("input");
  checkbox.type = "checkbox";
  checkbox.checked = mod.visible;
  checkbox.addEventListener("change", () => setVisible(mod.id, checkbox.checked));
  toggle.append(checkbox, document.createTextNode("Visible"));

  const more = iconButton(`${mod.name} tools and settings`, ICONS.more);
  more.addEventListener("click", () => openSheet(mod.id));

  right.append(toggle, more);
  head.append(left, right);
  el.appendChild(head);

  const foot = document.createElement("div");
  foot.className = "card-foot";
  const region = document.createElement("span");
  region.className = "region-tag";
  region.textContent = regionLabel(mod.region);
  foot.appendChild(region);
  el.appendChild(foot);

  if (mod.status?.state === "error" || mod.status?.state === "degraded") {
    const warning = document.createElement("p");
    warning.className = "desc warn";
    warning.textContent = mod.status.message;
    el.insertBefore(warning, foot);
  }

  return el;
}

// ---------------------------------------------------------------------------
// The sheet
// ---------------------------------------------------------------------------

function openSheet(key) {
  sheetKey = key;
  renderSheet();
}

function closeSheet() {
  sheetKey = null;
  renderSheet();
}

function renderSheet() {
  const sheet = document.getElementById("sheet");
  const scrim = document.getElementById("scrim");

  if (!sheetKey || (sheetKey !== CORE && !modules.has(sheetKey))) {
    sheetKey = null;
    sheet.hidden = true;
    scrim.hidden = true;
    sheet.replaceChildren();
    document.body.classList.remove("sheet-open");
    return;
  }

  const key = sheetKey;
  const st = uiState(key);
  const isCore = key === CORE;
  const mod = isCore ? null : modules.get(key);

  const body = document.createElement("div");
  body.className = "sheet-body";
  sheetBody(key, st, isCore, mod, body);

  sheet.replaceChildren(sheetHead(key, st, isCore, mod), body);
  sheet.hidden = false;
  scrim.hidden = false;
  document.body.classList.add("sheet-open");
}

function sheetHead(key, st, isCore, mod) {
  const head = document.createElement("header");
  head.className = "sheet-head";

  const left = document.createElement("div");
  const h2 = document.createElement("h2");
  if (!isCore) {
    const dot = document.createElement("span");
    dot.className = "status-dot";
    dot.dataset.state = mod.status?.state ?? "starting";
    h2.appendChild(dot);
  }
  h2.appendChild(document.createTextNode(isCore ? "Mirror API" : mod.name));

  const sub = document.createElement("p");
  sub.className = "desc";
  sub.textContent = isCore
    ? "Core tools that work on every module."
    : [mod.description, regionLabel(mod.region)].filter(Boolean).join(" · ");
  left.append(h2, sub);

  const actions = document.createElement("div");
  actions.className = "sheet-actions";

  const detail = iconButton("Show API details", ICONS.info);
  detail.classList.toggle("active", st.details);
  detail.setAttribute("aria-pressed", String(st.details));
  detail.addEventListener("click", () => {
    st.details = !st.details;
    renderSheet();
  });

  const close = iconButton("Close", ICONS.close);
  close.addEventListener("click", closeSheet);

  actions.append(detail, close);
  head.append(left, actions);
  return head;
}

function sheetBody(key, st, isCore, mod, body) {
  const list = toolsFor(isCore ? CORE : mod.id);
  const direct = sortTools(list.filter((tool) => !takesArgs(tool)));
  const advanced = sortTools(list.filter(takesArgs));

  // No arguments: tap and it happens. These come first, unconditionally.
  if (direct.length) {
    const row = document.createElement("div");
    row.className = "tools";
    for (const tool of direct) row.appendChild(toolRow(key, tool, st));
    body.appendChild(row);
  }

  if (advanced.length) {
    body.appendChild(
      disclosure(`More tools · ${advanced.length}`, st.more, () => {
        st.more = !st.more;
        renderSheet();
      }),
    );
    if (st.more) {
      const row = document.createElement("div");
      row.className = "tools";
      for (const tool of advanced) row.appendChild(toolRow(key, tool, st));
      body.appendChild(row);
    }
  }

  if (!isCore && hasConfig(mod)) {
    body.appendChild(
      disclosure("Configure", st.config, () => {
        st.config = !st.config;
        renderSheet();
      }),
    );
    if (st.config) body.appendChild(configForm(mod));
  }
}

function disclosure(label, expanded, onToggle) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "disclosure";
  button.setAttribute("aria-expanded", String(expanded));
  button.append(caretElement(expanded), document.createTextNode(label));
  button.addEventListener("click", onToggle);
  return button;
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

function toolRow(key, tool, st) {
  const row = document.createElement("div");
  row.className = "tool";

  const head = document.createElement("div");
  head.className = "tool-head";

  const action = document.createElement("button");
  action.type = "button";
  action.className = "tool-action";
  action.textContent = tool.title;
  if (tool.dangerous) action.classList.add("danger");
  action.addEventListener("click", () => {
    if (takesArgs(tool)) {
      if (st.forms.has(tool.name)) st.forms.delete(tool.name);
      else st.forms.add(tool.name);
      renderSheet();
    } else {
      callTool(key, tool, {}, action);
    }
  });

  const kind = document.createElement("span");
  kind.className = "tool-kind";
  kind.dataset.kind = tool.kind;
  kind.textContent = tool.kind;

  head.append(action, kind);
  row.appendChild(head);

  // The reference material waits to be asked for.
  if (st.details) {
    const name = document.createElement("code");
    name.className = "tool-name";
    name.textContent = tool.name;
    const hint = document.createElement("p");
    hint.className = "tool-desc";
    hint.textContent = tool.description;
    row.append(name, hint);
  }

  if (st.forms.has(tool.name)) row.appendChild(toolForm(key, tool));

  const entry = st.results.get(tool.name);
  if (entry) row.appendChild(resultStrip(key, entry));

  return row;
}

/// A result collapses to one line — `✓ ok · 19:31` — and opens when tapped. A
/// collapsed strip still tells you *that* something came back, which is the part
/// you need while glancing.
function resultStrip(key, entry) {
  const wrap = document.createElement("div");
  wrap.className = "result";
  wrap.dataset.ok = String(entry.ok);

  const strip = document.createElement("button");
  strip.type = "button";
  strip.className = "result-strip";
  strip.setAttribute("aria-expanded", String(entry.expanded));
  strip.title = "Show result";

  const mark = document.createElement("span");
  mark.className = "result-dot";

  const label = document.createElement("span");
  label.className = "result-label";
  label.textContent = `${entry.ok ? "ok" : "error"} · ${entry.at}`;

  const caret = caretElement(entry.expanded);

  strip.append(mark, label, caret);
  strip.addEventListener("click", () => {
    entry.expanded = !entry.expanded;
    renderSheet();
  });
  wrap.appendChild(strip);

  if (entry.expanded) {
    const body = document.createElement("pre");
    body.className = "result-body";
    const value = entry.value ?? null;
    body.textContent =
      typeof value === "string" ? value : JSON.stringify(value, null, 2);
    wrap.appendChild(body);
  }

  return wrap;
}

function toolForm(key, tool) {
  const schema = tool.params_schema ?? {};
  const properties = schema.properties ?? {};
  const required = schema.required ?? [];
  const st = uiState(key);
  const seeded = st.args.get(tool.name) ?? {};

  const form = document.createElement("form");
  form.className = "tool-form";
  const fields = [];

  for (const [fieldName, spec] of Object.entries(properties)) {
    const field = buildField(
      fieldName,
      spec,
      seeded[fieldName],
      required.includes(fieldName),
    );
    fields.push({ name: fieldName, field });
    form.appendChild(field.el);
  }

  const call = document.createElement("button");
  call.type = "submit";
  call.className = "primary";
  call.textContent = "Call";
  form.appendChild(call);

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const args = {};
    for (const { name, field } of fields) {
      const value = field.read();
      if (value !== undefined) args[name] = value;
    }
    // Remember the arguments so a repeat call does not mean retyping them.
    st.args.set(tool.name, args);
    callTool(key, tool, args, call);
  });

  return form;
}

async function callTool(key, tool, args, button) {
  if (tool.dangerous && !window.confirm(`${tool.title}?`)) return;

  const st = uiState(key);
  if (button) button.disabled = true;

  try {
    const response = await api.callTool(tool.name, args);
    st.results.set(tool.name, {
      ok: Boolean(response.ok),
      value: response.ok ? response.result : response.error,
      at: timestamp(),
      expanded: false,
    });
  } catch (error) {
    // A transport failure (server down, unknown tool) never reaches the body.
    st.results.set(tool.name, {
      ok: false,
      value: error.message,
      at: timestamp(),
      expanded: false,
    });
  } finally {
    // Core tools can move modules around, so the list matters as much as the
    // sheet. Rebuilding the sheet keeps the result visible and the form open.
    if (key === CORE) render();
    else refreshCard(key);
    if (sheetKey) renderSheet();
  }
}

// ---------------------------------------------------------------------------
// Form fields, generated from JSON Schema
// ---------------------------------------------------------------------------

function hasConfig(mod) {
  const properties = mod.config_schema?.properties;
  return Boolean(properties) && Object.keys(properties).length > 0;
}

/// Dynamic choice lists. `enum` is static; `x-options: "modules"` asks for the
/// module ids the phone already knows (see `core::tools` for the convention).
function optionsFor(spec) {
  if (Array.isArray(spec.enum)) return spec.enum;
  if (spec["x-options"] === "modules") return [...modules.keys()].sort();
  return null;
}

function buildField(name, spec, value, isRequired) {
  const label = document.createElement("label");
  label.className = "field";
  if (isRequired) label.classList.add("required");

  const caption = document.createElement("span");
  caption.textContent = spec.title || name;
  label.appendChild(caption);

  const types = Array.isArray(spec.type) ? spec.type : [spec.type];
  const options = optionsFor(spec);
  const fallback = value ?? spec.default;
  let input;

  if (options) {
    input = document.createElement("select");
    const blank = document.createElement("option");
    blank.value = "";
    blank.textContent = isRequired ? "choose…" : "(default)";
    input.appendChild(blank);
    for (const option of options) {
      const el = document.createElement("option");
      el.value = String(option);
      el.textContent = String(option).replace(/_/g, " ");
      input.appendChild(el);
    }
    if (fallback != null) input.value = String(fallback);
  } else if (types.includes("boolean")) {
    input = document.createElement("input");
    input.type = "checkbox";
    input.checked = fallback ?? false;
  } else if (types.includes("number") || types.includes("integer")) {
    input = document.createElement("input");
    input.type = "number";
    input.step = types.includes("integer") ? "1" : "any";
    if (spec.minimum != null) input.min = spec.minimum;
    if (spec.maximum != null) input.max = spec.maximum;
    if (fallback != null) input.value = fallback;
  } else {
    input = document.createElement("input");
    input.type = "text";
    if (fallback != null) input.value = fallback;
  }

  // A checkbox cannot express "required" in HTML, but nothing asks for one.
  if (isRequired && input.type !== "checkbox") input.required = true;

  label.appendChild(input);

  return {
    el: label,
    read: () => {
      if (input.type === "checkbox") return input.checked;
      if (input.value === "") return undefined; // omit, so schema defaults apply
      if (types.includes("number") || types.includes("integer")) {
        const number = Number(input.value);
        return Number.isFinite(number) ? number : undefined;
      }
      return input.value;
    },
  };
}

function configForm(mod) {
  const schema = mod.config_schema ?? {};
  const properties = schema.properties ?? {};
  const required = schema.required ?? [];

  const form = document.createElement("form");
  form.className = "tool-form config-form";

  // Config values the phone has not been told about yet are *unknown*, not
  // empty. Drawing blank boxes — or, worse, the schema's `default` values —
  // would read as "your saved settings are gone", so wait instead. The form is
  // rebuilt as soon as the values turn up; see `hasDraft`.
  if (mod.config === null || mod.config === undefined) {
    const note = document.createElement("p");
    note.className = "config-waiting";
    note.textContent = "Waiting for this module to report its config…";
    form.appendChild(note);
    return form;
  }

  const values = mod.config && typeof mod.config === "object" ? mod.config : {};
  const fields = [];

  for (const [name, spec] of Object.entries(properties)) {
    const field = buildField(name, spec, values[name], required.includes(name));
    fields.push({ name, field });
    form.appendChild(field.el);
  }

  const save = document.createElement("button");
  save.type = "submit";
  save.className = "primary";
  save.textContent = "Save";
  form.appendChild(save);

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const config = {};
    for (const { name, field } of fields) {
      const value = field.read();
      if (value !== undefined) config[name] = value;
    }

    save.disabled = true;
    try {
      // Take the value from the response: waiting for the WebSocket echo would
      // make reopening the form show stale values.
      const updated = await api.patchModule(mod.id, { config });
      modules.set(updated.id, updated);
      uiState(mod.id).config = false;
      toast("info", `${mod.name} updated`);
      refreshCard(mod.id);
      renderSheet();
    } catch (error) {
      toast("error", error.message);
      save.disabled = false;
    }
  });

  return form;
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

async function setVisible(id, visible) {
  try {
    const mod = await api.patchModule(id, { visible });
    modules.set(mod.id, mod);
    toast("info", `${mod.name} ${visible ? "shown" : "hidden"}`);
  } catch (error) {
    toast("error", error.message);
  }
  refreshCard(id);
  if (sheetKey === id) renderSheet();
}

// ---------------------------------------------------------------------------
// Live updates
// ---------------------------------------------------------------------------

function applyTools(list) {
  tools.clear();
  for (const tool of list) tools.set(tool.name, tool);
}

function applyModules(list) {
  modules.clear();
  for (const mod of list) modules.set(mod.id, mod);
  render();
  if (sheetKey) renderSheet();
}

function applyPatch(id, patch) {
  const current = modules.get(id);
  if (!current) return;
  const next = { ...current, ...patch };
  modules.set(id, next);
  refreshCard(id);
  // Judge the draft against `current` — the state the open form was built from,
  // not the state this patch is bringing. A form waiting on a config it has
  // never seen has nothing to lose, and this is the patch that fills it in.
  if (sheetKey === id && !hasDraft(uiState(id), current)) renderSheet();
}

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

function toast(kind, message) {
  const host = document.getElementById("toasts");
  const el = document.createElement("div");
  el.className = "toast";
  el.dataset.kind = kind;
  el.textContent = message;
  host.appendChild(el);
  setTimeout(() => el.remove(), 3200);
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

function wireSheet() {
  document.getElementById("scrim").addEventListener("click", closeSheet);
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && sheetKey) closeSheet();
  });
}

async function main() {
  try {
    const info = await api.info();
    document.getElementById("name").textContent = info.name;
    document.getElementById("meta").textContent =
      `${info.hostname} · ${info.platform} · v${info.version}`;
  } catch {
    /* cosmetic */
  }

  // The tool list is the API surface. Modules are registered at startup, so one
  // fetch is enough; a hot-reloading core would refetch on `welcome`.
  try {
    applyTools(await api.listTools());
  } catch (error) {
    toast("error", `could not load tools: ${error.message}`);
  }

  applyModules(await api.listModules());
  wireSheet();

  connectEvents({
    onMessage: (msg) => {
      switch (msg.type) {
        case "welcome":
          applyModules(msg.modules);
          break;
        case "patch":
          applyPatch(msg.id, msg.patch);
          break;
        case "toast":
          toast(msg.kind, msg.message);
          break;
      }
    },
    onStatus: (status) => {
      const conn = document.getElementById("conn");
      conn.dataset.state = status;
      conn.textContent = status === "connected" ? "live" : "reconnecting";
    },
  });
}

main();
