// Mirror renderer: a pure subscriber. It renders the ViewNode tree the core
// pushes over the WebSocket and never computes anything of its own.

import { REGIONS, api, connectEvents, renderView } from "/shared/protocol.js";

const state = new Map(); // id -> ModuleState
const nodes = new Map(); // id -> HTMLElement

function buildShell() {
  const mirror = document.getElementById("mirror");
  for (const region of REGIONS) {
    const el = document.createElement("div");
    el.className = "region";
    el.dataset.region = region;
    mirror.appendChild(el);
  }
}

function regionElement(region) {
  return document.querySelector(`.region[data-region="${region}"]`);
}

function renderModule(mod) {
  let el = nodes.get(mod.id);
  if (!el) {
    el = document.createElement("div");
    el.className = "module";
    el.dataset.module = mod.id;
    nodes.set(mod.id, el);
  }

  el.dataset.order = String(mod.order ?? 0);
  el.hidden = !mod.visible;
  el.classList.toggle("status-error", mod.status?.state === "error");
  el.classList.toggle("status-degraded", mod.status?.state === "degraded");
  el.replaceChildren(renderView(mod.view));

  const target = regionElement(mod.region) || regionElement("middle_center");
  if (el.parentElement !== target) target.appendChild(el);
  sortRegion(target);
}

function sortRegion(region) {
  const ordered = [...region.children].sort(
    (a, b) => Number(a.dataset.order ?? 0) - Number(b.dataset.order ?? 0),
  );
  for (const child of ordered) region.appendChild(child);
}

function applyModules(list) {
  const seen = new Set();
  for (const mod of list) {
    seen.add(mod.id);
    state.set(mod.id, mod);
    renderModule(mod);
  }
  for (const id of [...state.keys()]) {
    if (!seen.has(id)) removeModule(id);
  }
}

function applyPatch(id, patch) {
  const current = state.get(id);
  if (!current) return;
  const next = { ...current, ...patch };
  state.set(id, next);
  renderModule(next);
}

function removeModule(id) {
  state.delete(id);
  nodes.get(id)?.remove();
  nodes.delete(id);
}

// Transient toast, shown whenever any module announces something through the
// event bus (e.g. a detected gesture). One at a time; later ones replace it.
let toastTimer = null;
function showToast(message, kind = "info") {
  let el = document.getElementById("toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "toast";
    document.body.appendChild(el);
  }
  el.textContent = message;
  el.dataset.kind = kind;
  el.dataset.show = "true";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    el.dataset.show = "false";
  }, 2200);
}

async function main() {
  buildShell();

  try {
    const info = await api.info();
    document.title = info.name;
  } catch {
    /* info is cosmetic */
  }

  applyModules(await api.listModules());

  connectEvents({
    onMessage: (msg) => {
      switch (msg.type) {
        case "welcome":
          applyModules(msg.modules);
          break;
        case "patch":
          applyPatch(msg.id, msg.patch);
          break;
        case "removed":
          removeModule(msg.id);
          break;
        case "toast":
          showToast(msg.message, msg.kind);
          break;
      }
    },
    onStatus: (status) => {
      document.body.dataset.connection = status;
    },
  });
}

main();
