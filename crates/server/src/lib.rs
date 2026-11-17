//! HTTP/WS surface for the core. Also serves the two embedded UIs (mirror + phone).

use std::sync::Arc;

use anyhow::anyhow;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Request, State,
    },
    http::{header, header::AUTHORIZATION, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use smmm_core::{Config, Event, Host};
use smmm_protocol::{
    ClientMsg, ErrorBody, InfoResponse, ModuleState, ModuleUpdateRequest, ServerMsg,
    ToolCallResponse, ToolInfo,
};
use tokio::sync::broadcast;
use tracing::warn;

// --- Embedded UI assets -----------------------------------------------------
// Embedded rather than served from disk so the Pi deployment is a single binary
// with no runtime path assumptions.

const MIRROR_HTML: &str = include_str!("../../../ui/mirror/index.html");
const MIRROR_JS: &str = include_str!("../../../ui/mirror/mirror.js");
const PHONE_HTML: &str = include_str!("../../../ui/phone/index.html");
const PHONE_JS: &str = include_str!("../../../ui/phone/phone.js");
const SHARED_JS: &str = include_str!("../../../ui/shared/protocol.js");
const SHARED_CSS: &str = include_str!("../../../ui/shared/style.css");

/// Weather icon SVGs, keyed by WMO-code-derived name (see the weather module).
/// Embedded like the UI assets so the deployment stays a single binary.
const WEATHER_ICONS: &[(&str, &str)] = &[
    (
        "clear-day",
        include_str!("../../../ui/icons/weather/clear-day.svg"),
    ),
    (
        "clear-night",
        include_str!("../../../ui/icons/weather/clear-night.svg"),
    ),
    (
        "cloudy",
        include_str!("../../../ui/icons/weather/cloudy.svg"),
    ),
    ("mist", include_str!("../../../ui/icons/weather/mist.svg")),
    (
        "partly-cloudy-day",
        include_str!("../../../ui/icons/weather/partly-cloudy-day.svg"),
    ),
    (
        "partly-cloudy-night",
        include_str!("../../../ui/icons/weather/partly-cloudy-night.svg"),
    ),
    (
        "rain-day",
        include_str!("../../../ui/icons/weather/rain-day.svg"),
    ),
    (
        "rain-night",
        include_str!("../../../ui/icons/weather/rain-night.svg"),
    ),
    ("rain", include_str!("../../../ui/icons/weather/rain.svg")),
    ("snow", include_str!("../../../ui/icons/weather/snow.svg")),
    (
        "thunderstorm",
        include_str!("../../../ui/icons/weather/thunderstorm.svg"),
    ),
];

#[derive(Clone)]
pub struct AppState {
    host: Host,
    config: Arc<Config>,
    auth_token: Option<String>,
}

/// Build the application router around a started [`Host`].
pub fn router(host: Host, config: Config) -> Router {
    let auth_token = config.core.auth_token.clone();
    let state = AppState {
        host,
        config: Arc::new(config),
        auth_token,
    };

    Router::new()
        // --- UI ---
        .route("/", get(mirror_index))
        .route("/mirror.js", get(mirror_script))
        .route("/phone", get(phone_index))
        .route("/phone.js", get(phone_script))
        .route("/shared/protocol.js", get(shared_script))
        .route("/shared/style.css", get(shared_style))
        .route("/icons/weather/{name}", get(weather_icon))
        // --- API ---
        .route("/api/v1/info", get(info))
        .route("/api/v1/modules", get(list_modules))
        .route("/api/v1/modules/{id}", get(get_module).patch(patch_module))
        .route("/api/v1/tools", get(list_tools))
        .route("/api/v1/tools/{name}", post(call_tool))
        .route("/api/v1/events", get(ws_handler))
        .fallback(get(not_found))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

// --- Static assets ----------------------------------------------------------

fn asset(body: &'static str, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn mirror_index() -> Response {
    asset(MIRROR_HTML, "text/html; charset=utf-8")
}

async fn mirror_script() -> Response {
    asset(MIRROR_JS, "text/javascript; charset=utf-8")
}

async fn phone_index() -> Response {
    asset(PHONE_HTML, "text/html; charset=utf-8")
}

async fn phone_script() -> Response {
    asset(PHONE_JS, "text/javascript; charset=utf-8")
}

async fn shared_script() -> Response {
    asset(SHARED_JS, "text/javascript; charset=utf-8")
}

async fn shared_style() -> Response {
    asset(SHARED_CSS, "text/css; charset=utf-8")
}

async fn weather_icon(Path(name): Path<String>) -> Response {
    // The path captures the filename with its extension (`clear-day.svg`);
    // accept both with and without.
    let key = name.strip_suffix(".svg").unwrap_or(&name);
    match WEATHER_ICONS.iter().find(|(n, _)| *n == key) {
        Some((_, svg)) => (
            [(header::CONTENT_TYPE, "image/svg+xml")],
            (*svg).to_string(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

// --- API handlers -----------------------------------------------------------

async fn info(State(st): State<AppState>) -> Json<InfoResponse> {
    Json(InfoResponse {
        name: st.config.core.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        hostname: hostname(),
        uptime_secs: st.host.uptime_secs(),
        module_count: st.host.store().snapshot().len(),
        auth_required: st.auth_token.is_some(),
    })
}

async fn list_modules(State(st): State<AppState>) -> Json<Vec<ModuleState>> {
    Json(st.host.store().snapshot())
}

async fn get_module(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ModuleState>, ApiError> {
    st.host
        .store()
        .get(&id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(&id))
}

async fn patch_module(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ModuleUpdateRequest>,
) -> Result<Json<ModuleState>, ApiError> {
    let store = st.host.store();
    if store.get(&id).is_none() {
        return Err(ApiError::not_found(&id));
    }

    if let Some(visible) = body.visible {
        st.host
            .set_visible(&id, visible)
            .map_err(ApiError::internal)?;
    }
    if let Some(region) = body.region {
        st.host
            .set_region(&id, region)
            .map_err(ApiError::internal)?;
    }
    if let Some(order) = body.order {
        st.host.set_order(&id, order).map_err(ApiError::internal)?;
    }
    if let Some(config) = body.config {
        st.host
            .set_config(&id, config)
            .await
            .map_err(ApiError::bad_request)?;
    }

    store
        .get(&id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(&id))
}

/// Every tool the mirror exposes, namespaced and LLM-ready.
///
/// This maps almost directly onto an Anthropic/OpenAI tool array: rename
/// `params_schema` to `parameters` (OpenAI) or `input_schema` (Anthropic).
async fn list_tools(State(st): State<AppState>) -> Json<Vec<ToolInfo>> {
    Json(st.host.tools())
}

/// Call a tool by its namespaced name. The body *is* the arguments object.
///
/// A tool that fails returns `200` with `{"ok": false, "error": "..."}` rather
/// than an HTTP error, so a model can read the message and retry. Only an
/// unknown tool name is a real `404`.
async fn call_tool(
    State(st): State<AppState>,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<ToolCallResponse>, ApiError> {
    if !st.host.has_tool(&name) {
        return Err(ApiError::not_found_tool(&name));
    }

    let args = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice::<Value>(&body)
            .map_err(|e| ApiError::bad_request(anyhow!("invalid JSON body: {e}")))?
    };

    let response = match st.host.call_tool(&name, args).await {
        Ok(result) => ToolCallResponse {
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(e) => ToolCallResponse {
            ok: false,
            result: None,
            error: Some(e.to_string()),
        },
    };
    Ok(Json(response))
}

// --- WebSocket --------------------------------------------------------------

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, st))
}

async fn handle_socket(socket: WebSocket, st: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = st.host.store().subscribe();

    let welcome = || ServerMsg::Welcome {
        revision: st.host.store().revision(),
        modules: st.host.store().snapshot(),
    };

    // Nothing is sent until the client says `hello`: that frame carries the
    // auth token and the client's last-seen revision, so the reply can be a
    // welcome or a gap-detection resync rather than a blind snapshot.
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::Patch { id, patch }) => {
                    if send_msg(&mut sender, &ServerMsg::Patch { id, patch }).await.is_err() {
                        break;
                    }
                }
                Ok(Event::Removed { id }) => {
                    if send_msg(&mut sender, &ServerMsg::Removed { id }).await.is_err() {
                        break;
                    }
                }
                Ok(Event::Toast { kind, message }) => {
                    if send_msg(&mut sender, &ServerMsg::Toast { kind, message }).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // The client missed events; a full snapshot is cheaper and
                    // safer than trying to replay a diff.
                    warn!("client lagged, {skipped} events dropped; resending snapshot");
                    if send_msg(&mut sender, &welcome()).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },

            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ClientMsg>(&text) {
                        Ok(msg) => {
                            if let Some(reply) = handle_client_msg(&st, msg).await {
                                if send_msg(&mut sender, &reply).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            let reply = ServerMsg::Error { message: format!("bad message: {e}") };
                            if send_msg(&mut sender, &reply).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
}

async fn handle_client_msg(st: &AppState, msg: ClientMsg) -> Option<ServerMsg> {
    match msg {
        ClientMsg::Ping => Some(ServerMsg::Pong),

        ClientMsg::Hello { .. } | ClientMsg::Subscribe { .. } => Some(ServerMsg::Welcome {
            revision: st.host.store().revision(),
            modules: st.host.store().snapshot(),
        }),

        ClientMsg::SetVisible { id, value } => match st.host.set_visible(&id, value) {
            Ok(()) => None,
            Err(e) => Some(ServerMsg::Error {
                message: e.to_string(),
            }),
        },

        ClientMsg::CallTool { name, args } => match st.host.call_tool(&name, args).await {
            Ok(result) => Some(ServerMsg::ToolResult {
                name,
                ok: true,
                result: Some(result),
                error: None,
            }),
            Err(e) => Some(ServerMsg::ToolResult {
                name,
                ok: false,
                result: None,
                error: Some(e.to_string()),
            }),
        },
    }
}

async fn send_msg<S>(sender: &mut S, msg: &ServerMsg) -> Result<(), axum::Error>
where
    S: futures::Sink<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(msg).expect("ServerMsg is always serialisable");
    sender.send(Message::Text(text.into())).await
}

// --- Auth -------------------------------------------------------------------

/// Requires `Authorization: Bearer <token>` on mutating `/api/` requests when a
/// token is configured. Read-only GETs and the UI assets stay open so the mirror
/// display can load before pairing. No token configured = open (dev mode).
async fn auth_middleware(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(expected) = &st.auth_token {
        let is_mutation = req.method() != Method::GET;
        let is_api = req.uri().path().starts_with("/api/");
        if is_mutation && is_api {
            let provided = req
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::trim);
            if provided != Some(expected.as_str()) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorBody {
                        error: "missing or invalid token".to_string(),
                    }),
                )
                    .into_response();
            }
        }
    }
    next.run(req).await
}

// --- Helpers ----------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn not_found(id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("unknown module: {id}"),
        }
    }

    fn not_found_tool(name: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("unknown tool: {name}"),
        }
    }

    fn bad_request(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: err.to_string(),
        }
    }

    fn internal(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "smartmmm".to_string())
}
