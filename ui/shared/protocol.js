// Shared client for SmartMMM. Used by both the mirror renderer and the phone UI.
//
// Three responsibilities: render a ViewNode tree, talk to the REST API, and
// keep a WebSocket in sync with the core.

export const REGIONS = [
  "top_left",
  "top_center",
  "top_right",
  "upper_third",
  "middle_center",
  "lower_third",
  "bottom_left",
  "bottom_center",
  "bottom_right",
  "fullscreen",
];

// --------------------------------------------------------------------------
// View rendering
// --------------------------------------------------------------------------

export function renderView(node) {
  if (!node || !node.type) return document.createComment("empty view");

  switch (node.type) {
    case "empty":
      return document.createComment("empty view");

    case "divider": {
      const el = document.createElement("div");
      el.className = "view-divider";
      return el;
    }

    case "text": {
      const el = document.createElement("div");
      el.className = `text size-${node.size || "md"} weight-${node.weight || "normal"} align-${node.align || "start"}`;
      el.textContent = node.text ?? "";
      return el;
    }

    case "stack": {
      const el = document.createElement("div");
      el.className = `stack dir-${node.direction || "vertical"}`;
      if (node.gap) el.style.gap = `${node.gap}px`;
      for (const child of node.children || []) el.appendChild(renderView(child));
      return el;
    }

    case "key_value": {
      const dl = document.createElement("dl");
      dl.className = "kv";
      for (const item of node.items || []) {
        const dt = document.createElement("dt");
        dt.textContent = item.key;
        const dd = document.createElement("dd");
        dd.textContent = item.value;
        dl.append(dt, dd);
      }
      return dl;
    }

    case "list": {
      const el = document.createElement(node.numbered ? "ol" : "ul");
      el.className = "list";
      for (const item of node.items || []) {
        const li = document.createElement("li");
        li.appendChild(document.createTextNode(item.text));
        if (item.detail) {
          const small = document.createElement("small");
          small.textContent = item.detail;
          li.appendChild(small);
        }
        el.appendChild(li);
      }
      return el;
    }

    case "progress": {
      const wrap = document.createElement("div");
      wrap.className = "progress";
      if (node.label) {
        const label = document.createElement("div");
        label.className = "progress-label";
        label.textContent = node.label;
        wrap.appendChild(label);
      }
      const bar = document.createElement("div");
      bar.className = "progress-bar";
      const fill = document.createElement("div");
      fill.className = "progress-fill";
      const value = Math.max(0, Math.min(1, Number(node.value) || 0));
      fill.style.width = `${value * 100}%`;
      bar.appendChild(fill);
      wrap.appendChild(bar);
      return wrap;
    }

    case "countdown": {
      const wrap = document.createElement("div");
      wrap.className = "countdown";
      if (node.label) {
        const label = document.createElement("div");
        label.className = "text size-md";
        label.textContent = node.label;
        wrap.appendChild(label);
      }
      const value = document.createElement("div");
      value.className = "text size-lg";
      value.textContent = formatRemaining(node.target);
      wrap.appendChild(value);
      return wrap;
    }

    case "image": {
      const img = document.createElement("img");
      img.className = "view-image";
      img.src = node.src;
      if (node.max_height) img.style.maxHeight = `${node.max_height}px`;
      img.alt = "";
      return img;
    }

    case "icon": {
      const span = document.createElement("span");
      span.className = "icon";
      span.textContent = node.name;
      if (node.size) span.style.fontSize = `${node.size}px`;
      return span;
    }

    default: {
      const el = document.createElement("div");
      el.textContent = "";
      return el;
    }
  }
}

function formatRemaining(target) {
  const ms = new Date(target).getTime() - Date.now();
  if (!Number.isFinite(ms)) return "—";
  if (ms <= 0) return "now";
  const totalMinutes = Math.floor(ms / 60000);
  const days = Math.floor(totalMinutes / 1440);
  const hours = Math.floor((totalMinutes % 1440) / 60);
  const minutes = totalMinutes % 60;
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

// --------------------------------------------------------------------------
// REST client
// --------------------------------------------------------------------------

const API = "/api/v1";

async function request(method, path, body) {
  const options = { method, headers: {} };
  if (body !== undefined) {
    options.headers["content-type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  const token = new URLSearchParams(location.search).get("token");
  if (token) options.headers["authorization"] = `Bearer ${token}`;

  const response = await fetch(`${API}${path}`, options);
  const text = await response.text();
  const data = text ? JSON.parse(text) : null;
  if (!response.ok) {
    throw new Error(data?.error || `${response.status} ${response.statusText}`);
  }
  return data;
}

export const api = {
  info: () => request("GET", "/info"),
  listModules: () => request("GET", "/modules"),
  getModule: (id) => request("GET", `/modules/${id}`),
  patchModule: (id, body) => request("PATCH", `/modules/${id}`, body),
  // Tools are the single callable surface. The phone renders every one of them;
  // an LLM would be handed the same list, unchanged.
  listTools: () => request("GET", "/tools"),
  callTool: (name, args = {}) => request("POST", `/tools/${name}`, args),
};

// --------------------------------------------------------------------------
// Live event stream
// --------------------------------------------------------------------------

export function connectEvents({ onMessage, onStatus } = {}) {
  let socket = null;
  let attempt = 0;
  let closed = false;

  const scheme = location.protocol === "https:" ? "wss" : "ws";

  function open() {
    socket = new WebSocket(`${scheme}://${location.host}${API}/events`);

    socket.onopen = () => {
      attempt = 0;
      onStatus?.("connected");
      const token = new URLSearchParams(location.search).get("token");
      socket.send(JSON.stringify({ type: "hello", token }));
    };

    socket.onmessage = (event) => {
      try {
        onMessage?.(JSON.parse(event.data));
      } catch {
        /* ignore malformed frames */
      }
    };

    socket.onclose = () => {
      onStatus?.("disconnected");
      if (closed) return;
      attempt += 1;
      setTimeout(open, Math.min(1000 * attempt, 10000));
    };

    socket.onerror = () => socket?.close();
  }

  open();

  return {
    send: (message) => {
      if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
    },
    close: () => {
      closed = true;
      socket?.close();
    },
  };
}
