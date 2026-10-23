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
use serde_json::{json, Value};
use time_core::otlp::{self, ExportLogsRequest};

use crate::{status, AppState};

/// OTLP batches from a device catching up after days away can be large;
/// v1 capped a backlog post at 2000 minutes. 32 MiB is far beyond any real
/// batch and still bounded.
const MAX_BODY: usize = 32 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/logs", post(ingest))
        .route("/v1/frame", post(legacy_frame))
        .route("/v1/frames", post(legacy_frames))
        .route("/healthz", get(healthz))
        .route(
            "/v1/focus",
            get(focus_get).post(focus_set).delete(focus_clear),
        )
        .route("/metrics", get(metrics))
        .route("/api/status", get(api_status))
        .route("/status", get(status_html))
        .route("/app", get(app_apk))
        .route("/app/", get(app_page))
        .route("/app/version", get(app_version))
        .route("/inbox", get(inbox_html))
        .route("/inbox/rule", post(inbox_rule))
        .route("/", get(today_html))
        .route("/day/{date}", get(day_html))
        .route("/week", get(this_week_html))
        .route("/week/{date}", get(week_html))
        .route("/month", get(this_month_html))
        .route("/month/{date}", get(month_html))
        .route("/api/day/{date}", get(day_json))
        .route("/api/now", get(api_now))
        .route("/api/summary", get(api_summary))
        .route("/api/summary/line", get(api_summary_line))
        .route("/api/timeline", get(api_timeline))
        .route(
            "/mcp",
            post(crate::mcp::post)
                .get(crate::mcp::not_allowed)
                .delete(crate::mcp::not_allowed),
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

/// `/`, `/day/<date>`, `/week[/<date>]`, `/month[/<YYYY-MM>]`: one period,
/// with the one before it for comparison.
async fn period_html(
    state: Arc<AppState>,
    kind: crate::views::Kind,
    date: Option<String>,
) -> Response {
    use crate::views::{Kind, Period};
    let tz = state.tz;
    let today = crate::day::today(tz, state.clock.now_ms());
    let date = match date.as_deref() {
        None => today,
        Some(d) => match d
            .parse::<chrono::NaiveDate>()
            .or_else(|_| format!("{d}-01").parse::<chrono::NaiveDate>())
        {
            Ok(d) => d,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("not a date: {d:?} (use YYYY-MM-DD, or YYYY-MM for a month)"),
                )
                    .into_response()
            }
        },
    };
    let period = Period::containing(kind, date);
    let phones = phones(&state);
    let goal = state.config.step_goal;
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let cur = crate::views::summarise(&state.store, tz, period, &phones, today)?;
        let prev = crate::views::summarise(&state.store, tz, period.prev(), &phones, today)?;
        Ok(match kind {
            Kind::Day => {
                let day = crate::day::summarise(&state.store, tz, date)?;
                crate::web::day_page(&day, tz, today, &cur, &prev, &phones, goal)
            }
            _ => crate::views::period_page(&cur, &prev, goal, today),
        })
    })
    .await;
    match r {
        Ok(Ok(html)) => axum::response::Html(html).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn today_html(State(state): State<Arc<AppState>>) -> Response {
    period_html(state, crate::views::Kind::Day, None).await
}

async fn day_html(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
) -> Response {
    period_html(state, crate::views::Kind::Day, Some(date)).await
}

async fn this_week_html(State(state): State<Arc<AppState>>) -> Response {
    period_html(state, crate::views::Kind::Week, None).await
}

async fn week_html(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
) -> Response {
    period_html(state, crate::views::Kind::Week, Some(date)).await
}

async fn this_month_html(State(state): State<Arc<AppState>>) -> Response {
    period_html(state, crate::views::Kind::Month, None).await
}

async fn month_html(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
) -> Response {
    period_html(state, crate::views::Kind::Month, Some(date)).await
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

/// What the phone's updater reads. Field names and the relative `url` are
/// the contract the installed app already parses (Update.kt).
async fn app_version(State(state): State<Arc<AppState>>) -> Response {
    let s = state.apk.snapshot();
    match s.meta {
        Some(m) => Json(json!({
            "version": m.version,
            "versionCode": m.version_code,
            "sha256": m.sha256,
            "url": "/app",
            "published": m.published,
            "size": m.size,
            "signing": m.signing,
            "stale": !s.error.is_empty(),
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no APK published" })),
        )
            .into_response(),
    }
}

async fn app_apk(State(state): State<Arc<AppState>>) -> Response {
    let path = state.apk.path.clone();
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (
                    header::CONTENT_TYPE,
                    "application/vnd.android.package-archive",
                ),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"time.apk\"",
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "no APK published yet\n").into_response(),
    }
}

async fn app_page(State(state): State<Arc<AppState>>) -> Response {
    axum::response::Html(crate::web::app_page(
        &state.apk.snapshot(),
        &state.apk.repo,
        state.tz,
    ))
    .into_response()
}

/// The personal agent marks a focus day: nudges at any hour, optionally a
/// lower threshold, and the reason in the nudge text. Guarded by the ingest
/// token when one is set.
fn agent_authorised(state: &AppState, headers: &HeaderMap) -> bool {
    match &state.ingest_token {
        None => true,
        Some(token) => headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| t == token),
    }
}

async fn focus_get(State(state): State<Arc<AppState>>) -> Response {
    match crate::nudge::focus(&state.store, state.clock.now_ms()) {
        Ok(f) => Json(json!({ "focus": f })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn focus_set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(f): Json<time_core::nudge::Focus>,
) -> Response {
    if !agent_authorised(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
    }
    if f.until_ms <= state.clock.now_ms() {
        return (StatusCode::BAD_REQUEST, "until_ms is in the past").into_response();
    }
    match serde_json::to_string(&f)
        .map_err(anyhow::Error::from)
        .and_then(|s| state.store.set_meta("focus", &s))
    {
        Ok(()) => Json(json!({ "focus": f })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn focus_clear(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !agent_authorised(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
    }
    match state.store.set_meta("focus", "null") {
        Ok(()) => Json(json!({ "focus": null })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

fn phones(state: &AppState) -> Vec<String> {
    state
        .config
        .devices
        .iter()
        .filter(|(_, d)| d.kind == crate::config::DeviceKind::Phone)
        .map(|(n, _)| n.clone())
        .collect()
}

async fn store_legacy(
    state: Arc<AppState>,
    frames: Vec<crate::legacy::Frame>,
) -> Result<Vec<crate::legacy::Ack>, (StatusCode, String)> {
    let now = state.clock.now_ms();
    let phones = phones(&state);
    let events: Vec<_> = frames
        .iter()
        .flat_map(|f| crate::legacy::convert(f, &phones, now))
        .collect();
    let acks = frames.iter().map(crate::legacy::ack).collect();
    let st = state.clone();
    match tokio::task::spawn_blocking(move || st.store.insert(&events, now)).await {
        Ok(Ok(i)) => {
            state
                .counters
                .records_accepted
                .fetch_add(i.new as u64, Relaxed);
            state
                .counters
                .records_duplicate
                .fetch_add(i.duplicate as u64, Relaxed);
            Ok(acks)
        }
        Ok(Err(e)) => Err((StatusCode::SERVICE_UNAVAILABLE, format!("store: {e:#}"))),
        Err(e) => Err((StatusCode::SERVICE_UNAVAILABLE, e.to_string())),
    }
}

/// v1 desktop agent: one frame.
async fn legacy_frame(
    State(state): State<Arc<AppState>>,
    Json(frame): Json<crate::legacy::Frame>,
) -> Response {
    match store_legacy(state, vec![frame]).await {
        Ok(mut acks) => Json(acks.remove(0)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// v1 phone and backlogs: an array of frames.
async fn legacy_frames(
    State(state): State<Arc<AppState>>,
    Json(frames): Json<Vec<crate::legacy::Frame>>,
) -> Response {
    match store_legacy(state, frames).await {
        Ok(acks) => Json(acks).into_response(),
        Err(e) => e.into_response(),
    }
}

/// The MCP tools as plain JSON for scripts: `/api/now`, `/api/summary?window=3h`,
/// `/api/timeline?window=3h`. Open like `/api/day`; the route is lan-only.
async fn digest_json(
    state: Arc<AppState>,
    tool: &'static str,
    q: HashMap<String, String>,
) -> Response {
    let mut args = serde_json::Map::new();
    for (k, v) in &q {
        // `window=3` is a length, not a number.
        let value = if k == "window" {
            json!(v)
        } else {
            v.parse::<u64>().map_or_else(|_| json!(v), |n| json!(n))
        };
        args.insert(k.clone(), value);
    }
    let args = Value::Object(args);
    let done =
        tokio::task::spawn_blocking(move || crate::mcp::call_tool(&state, tool, &args)).await;
    match done {
        Ok(Ok(d)) => Json(json!({"summary": d.text, "data": d.json})).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, format!("{e:#}\n")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn api_now(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    digest_json(state, "time_now", q).await
}

async fn api_summary(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    digest_json(state, "time_summary", q).await
}

async fn api_timeline(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    digest_json(state, "time_timeline", q).await
}

/// The last hour in one plain-text line, for the hourly review.
async fn api_summary_line(State(state): State<Arc<AppState>>) -> Response {
    let done = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let now = state.clock.now_ms();
        let from = now - crate::digest::NOW_LOOKBACK_MIN * time_core::clock::MINUTE_MS;
        let (labels, events) = crate::digest::load(&state.store, from, now)?;
        Ok(crate::digest::line(
            &state.config.mcp,
            state.tz,
            now,
            &labels,
            &events,
        ))
    })
    .await;
    match done {
        Ok(Ok(l)) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("{l}\n"),
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
