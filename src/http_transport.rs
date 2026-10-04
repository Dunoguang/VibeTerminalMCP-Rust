use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use axum::{
    Router,
    extract::{State, Request},
    http::{HeaderMap, HeaderValue, StatusCode, header, Method},
    response::{IntoResponse, Response},
    routing::{get, post, any},
};
use serde_json::{Value, json};
use tracing::{info, warn};
use rand::Rng;

use crate::server::AppState;

#[derive(Clone, Default)]
struct SessionStore {
    sessions: Arc<Mutex<HashMap<String, i64>>>,
}

#[derive(Clone)]
struct ServerContext {
    app: AppState,
    sessions: SessionStore,
}

// ── Master handler ────────────────────────────────────────────────────────────

async fn handle_any(
    State(ctx): State<ServerContext>,
    method: Method,
    headers: HeaderMap,
    req: Request,
) -> Response {
    let uri = req.uri().to_string();
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let session_hdr = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()).unwrap_or("");
    info!("REQ {method} {uri} session={session_hdr} accept={accept}");

    // CORS preflight (OPTIONS)
    if method == Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS, DELETE")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type, Authorization, Mcp-Session-Id, MCP-Protocol-Version, Last-Event-ID")
            .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
            .body(axum::body::Body::empty())
            .unwrap();
    }

    // GET request -> SSE stream (Streamable HTTP GET / SSE)
    if method == Method::GET {
        let qsid = generate_session_id();
        let endpoint_data = format!("event: endpoint\ndata: /message?sessionId={qsid}\n\n");
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Accel-Buffering", "no")
            .body(axum::body::Body::from(endpoint_data))
            .unwrap();
    }

    if method != Method::POST {
        return json_error(StatusCode::METHOD_NOT_ALLOWED, -32601, "Method not allowed");
    }

    // Read body
    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, -32700, &format!("Body read error: {e}")),
    };

    let req_json: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            warn!("JSON parse error: {e}");
            return json_error(StatusCode::BAD_REQUEST, -32700, &format!("Parse error: {e}"));
        }
    };

    // Single or batch request
    let is_initialize = req_json.get("method").and_then(|v| v.as_str()) == Some("initialize");
    let resp = if req_json.is_array() {
        let responses: Vec<Value> = req_json.as_array().unwrap().iter()
            .filter_map(|r| ctx.app.handle_request(r))
            .collect();
        if responses.is_empty() {
            return Response::builder().status(StatusCode::NO_CONTENT).body(axum::body::Body::empty()).unwrap();
        }
        Value::Array(responses)
    } else {
        match ctx.app.handle_request(&req_json) {
            Some(r) => r,
            None => {
                // notification
                return Response::builder().status(StatusCode::NO_CONTENT).body(axum::body::Body::empty()).unwrap();
            }
        }
    };

    let mut res_builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, "Mcp-Session-Id");

    // On initialize, issue Mcp-Session-Id
    if is_initialize {
        let sid = generate_session_id();
        info!("session created: {sid}");
        res_builder = res_builder.header("Mcp-Session-Id", &sid);
    } else if !session_hdr.is_empty() {
        res_builder = res_builder.header("Mcp-Session-Id", session_hdr);
    }

    let body = serde_json::to_vec(&resp).unwrap_or_default();
    res_builder.body(axum::body::Body::from(body)).unwrap()
}

fn json_error(status: StatusCode, code: i32, msg: &str) -> Response {
    let err = json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": code, "message": msg }
    });
    let body = serde_json::to_vec(&err).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(axum::body::Body::from(body))
        .unwrap()
}

fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    (0..32).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect()
}

// ── Server startup ────────────────────────────────────────────────────────────

pub async fn run(state: AppState, host: &str, port: u16) {
    let ctx = ServerContext {
        app: state,
        sessions: SessionStore::default(),
    };

    // 匹配所有端点路径：/mcp, /, /sse, /message, /messages 以及 fallback
    let app = Router::new()
        .route("/mcp", any(handle_any))
        .route("/", any(handle_any))
        .route("/sse", any(handle_any))
        .route("/message", any(handle_any))
        .route("/messages", any(handle_any))
        .fallback(handle_any)
        .with_state(ctx);

    let addr = SocketAddr::from_str(&format!("{host}:{port}"))
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], port)));

    info!("vibeterminalmcp-server (Rust) listening on http://{addr} (all paths: /mcp, /sse, /message, /)");
    info!("protocol: MCP 2025-06-18 Streamable HTTP + SSE compatible");

    let listener = tokio::net::TcpListener::bind(addr).await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server error");
}
