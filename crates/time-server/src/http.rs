//! HTTP routes: OTLP ingest, health, metrics, status.

use std::collections::HashMap;
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
        .route("/inbox", get(inbox_html))
        .route("/inbox/rule", post(inbox_rule))
        .route("/", get(today_html))
        .route("/day/{date}", get(day_html))
        .route("/api/day/{date}", get(day_json))
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

async fn load_day(
    state: Arc<AppState>,
    date: Option<String>,
) -> Result<(crate::day::Day, bool), (StatusCode, String)> {
    let today = crate::day::today(state.tz, state.clock.now_ms());
    let date = match date {
        None => today,
        Some(d) => d.parse().map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!("not a date: {d:?} (use YYYY-MM-DD)"),
            )
        })?,
    };
    let tz = state.tz;
    let day = tokio::task::spawn_blocking(move || crate::day::summarise(&state.store, tz, date))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok((day, date == today))
}

async fn today_html(State(state): State<Arc<AppState>>) -> Response {
    let tz = state.tz;
    match load_day(state, None).await {
        Ok((day, is_today)) => {
            axum::response::Html(crate::web::day_page(&day, tz, is_today)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn day_html(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
) -> Response {
    let tz = state.tz;
    match load_day(state, Some(date)).await {
        Ok((day, is_today)) => {
            axum::response::Html(crate::web::day_page(&day, tz, is_today)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn day_json(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
) -> Response {
    match load_day(state, Some(date)).await {
        Ok((day, _)) => Json(day).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn inbox_html(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let st = state.clone();
    let now = state.clock.now_ms();
    let items = tokio::task::spawn_blocking(move || {
        crate::inbox::items(&st.store, now - crate::inbox::WINDOW_MS, now)
    })
    .await;
    match items {
        Ok(Ok(items)) => axum::response::Html(crate::web::inbox_page(
            &items,
            state.tz,
            q.get("added").map(String::as_str),
        ))
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct NewRule {
    on: crate::inbox::MatchOn,
    value: String,
    #[serde(default)]
    class: String,
    activity: time_core::label::Activity,
}

/// Turn an inbox answer into a rule, then relabel history with it.
async fn inbox_rule(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Form(f): axum::Form<NewRule>,
) -> Response {
    // A page on another site must not be able to post rules here: if the
    // browser says where the form came from, it has to be this host.
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if let Some(o) = origin {
        if o.split("://").nth(1) != Some(host) {
            return (StatusCode::FORBIDDEN, "cross-site form post refused").into_response();
        }
    }
    let id = format!("local.{}", state.clock.now_ms());
    let toml = match crate::inbox::rule_toml(f.on, f.value.trim(), f.class.trim(), f.activity, &id)
    {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    };
    let st = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        st.classifier.add_local_rule(&toml)?;
        st.classifier.requeue_if_rules_changed(&st.store)?;
        st.classifier.run_once(&st.store, st.clock.now_ms())
    })
    .await;
    match result {
        Ok(Ok(_)) => axum::response::Redirect::to(&format!("/inbox?added={id}")).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
