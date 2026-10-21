//! HTTP routes: OTLP ingest, health, metrics, status.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use time_core::otlp::{self, ExportLogsRequest};

use crate::{status, AppState};

/// OTLP batches from a device catching up after days away can be large;
/// v1 capped a backlog post at 2000 minutes. 32 MiB is far beyond any real
/// batch and still bounded.
const MAX_BODY: usize = 32 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/logs", post(ingest))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/api/status", get(api_status))
        .route("/status", get(status_html))
        .route(
            "/",
            get(|| async { axum::response::Redirect::temporary("/status") }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

/// OTLP/HTTP logs, JSON encoding.
///
/// Per the OTLP spec a partly bad batch still answers 200, naming how many
/// records were rejected; only an unreadable body is a 400. The good records
/// in a batch are never held hostage by a bad one.
async fn ingest(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    state.counters.ingest_requests.fetch_add(1, Relaxed);
    if let Some(token) = &state.ingest_token {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| t == token);
        if !ok {
            return (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
        }
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("application/x-protobuf") {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "protobuf OTLP is not supported yet; send application/json",
        )
            .into_response();
    }
    let req: ExportLogsRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("not an OTLP logs request: {e}"),
            )
                .into_response()
        }
    };

    let mut good = Vec::new();
    let mut errors = Vec::new();
    for result in otlp::decode(&req) {
        match result {
            Ok(env) => good.push(env),
            Err(e) => errors.push(e.to_string()),
        }
    }

    let received = state.clock.now_ms();
    let st = state.clone();
    let inserted = tokio::task::spawn_blocking(move || st.store.insert(&good, received)).await;
    let inserted = match inserted {
        Ok(Ok(i)) => i,
        // Storage failing is the server's problem: 503 tells the sender to
        // keep the batch and retry, which is exactly right.
        Ok(Err(e)) => {
            return (StatusCode::SERVICE_UNAVAILABLE, format!("store: {e:#}")).into_response()
        }
        Err(e) => {
            return (StatusCode::SERVICE_UNAVAILABLE, format!("store task: {e}")).into_response()
        }
    };

    state
        .counters
        .records_accepted
        .fetch_add(inserted.new as u64, Relaxed);
    state
        .counters
        .records_duplicate
        .fetch_add(inserted.duplicate as u64, Relaxed);
    state
        .counters
        .records_rejected
        .fetch_add(errors.len() as u64, Relaxed);
    if !errors.is_empty() {
        eprintln!(
            "ingest: rejected {} record(s), first: {}",
            errors.len(),
            errors[0]
        );
    }

    let mut resp = json!({});
    if !errors.is_empty() {
        resp = json!({
            "partialSuccess": {
                "rejectedLogRecords": errors.len().to_string(),
                "errorMessage": errors.first().cloned().unwrap_or_default(),
            }
        });
    }
    (StatusCode::OK, Json(resp)).into_response()
}

/// Up and able to write. Nothing about freshness: a quiet night is normal,
/// and a failing check here starts a repair session.
async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let st = state.clone();
    match tokio::task::spawn_blocking(move || st.store.check_writable()).await {
        Ok(Ok(())) => (StatusCode::OK, "ok\n").into_response(),
        Ok(Err(e)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("database not writable: {e:#}\n"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("health task: {e}\n"),
        )
            .into_response(),
    }
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    match collect(state).await {
        Ok(s) => (
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            status::metrics(&s),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn api_status(State(state): State<Arc<AppState>>) -> Response {
    match collect(state).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn collect(state: Arc<AppState>) -> Result<status::Status, (StatusCode, String)> {
    tokio::task::spawn_blocking(move || status::collect(&state))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

async fn status_html(State(state): State<Arc<AppState>>) -> Response {
    let tz = state.tz;
    match collect(state).await {
        Ok(s) => axum::response::Html(crate::web::status_page(&s, tz)).into_response(),
        Err(e) => e.into_response(),
    }
}
