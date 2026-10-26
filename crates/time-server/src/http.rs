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
        .route("/assets/echarts-5.6.0.min.js", get(echarts_js))
        .route("/assets/time.js", get(time_js))
        .route("/month", get(this_month_html))
        .route("/month/{date}", get(month_html))
        .route("/api/day/{date}", get(day_json))
        .route("/api/interactivity/{date}", get(interactivity_json))
        .route("/api/now", get(api_now))
        .route("/api/summary", get(api_summary))
        .route("/api/summary/line", get(api_summary_line))
        .route("/api/timeline", get(api_timeline))
        .route("/api/listening", get(api_listening))
        .route("/api/sleep", get(api_sleep))
        .route("/api/work", get(api_work))
        .route("/api/spans/{date}", get(api_spans))
        .route("/api/decisions", get(api_decisions))
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
    raw: bool,
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
        let mut cur = crate::views::summarise(&state.store, tz, period, &phones, today, raw)?;
        if period.contains(today) {
            use chrono::{TimeZone, Timelike};
            cur.now_min = tz
                .timestamp_millis_opt(state.clock.now_ms())
                .single()
                .map(|t| t.hour() * 60 + t.minute());
        }
        let prev = crate::views::summarise(&state.store, tz, period.prev(), &phones, today, raw)?;
        Ok(match kind {
            Kind::Day => {
                let day = crate::day::summarise_with(&state.store, tz, date, raw)?;
                let fills = state.store.fills_between(day.start_ms, day.end_ms)?;
                crate::web::day_page(&day, tz, today, &cur, &prev, &phones, goal, &fills)
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

/// `?raw=1`: the minutes as the rules made them, no gaps filled, no sleep.
type Q = axum::extract::Query<HashMap<String, String>>;

fn raw(q: &Q) -> bool {
    q.get("raw").is_some_and(|v| v != "0")
}

async fn today_html(State(state): State<Arc<AppState>>, q: Q) -> Response {
    period_html(state, crate::views::Kind::Day, None, raw(&q)).await
}

async fn day_html(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
    q: Q,
) -> Response {
    period_html(state, crate::views::Kind::Day, Some(date), raw(&q)).await
}

async fn this_week_html(State(state): State<Arc<AppState>>, q: Q) -> Response {
    period_html(state, crate::views::Kind::Week, None, raw(&q)).await
}

async fn week_html(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
    q: Q,
) -> Response {
    period_html(state, crate::views::Kind::Week, Some(date), raw(&q)).await
}

async fn this_month_html(State(state): State<Arc<AppState>>, q: Q) -> Response {
    period_html(state, crate::views::Kind::Month, None, raw(&q)).await
}

async fn month_html(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
    q: Q,
) -> Response {
    period_html(state, crate::views::Kind::Month, Some(date), raw(&q)).await
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

/// Keyboard and mouse in ten-minute chunks: counts only, never which keys.
async fn interactivity_json(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
) -> Response {
    match load_day(state, Some(date)).await {
        Ok((day, _)) => {
            let active: u32 = day.interactivity.iter().map(|c| c.active_seconds).sum();
            Json(serde_json::json!({
                "date": day.date,
                "chunk_minutes": crate::tracks::SLOT_MIN,
                "active_seconds": active,
                "key_presses": day.interactivity.iter().map(|c| c.keys).sum::<u32>(),
                "clicks": day.interactivity.iter().map(|c| c.clicks).sum::<u32>(),
                "chunks": day.interactivity,
            }))
            .into_response()
        }
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

async fn api_listening(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    digest_json(state, "time_listening", q).await
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

/// Never changes under this name: cache for a year.
async fn echarts_js() -> Response {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            ),
            (
                axum::http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        crate::web::ECHARTS_JS,
    )
        .into_response()
}

/// Versioned by `?v=` in the page, so it can be cached too.
async fn time_js() -> Response {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        crate::web::TIME_JS,
    )
        .into_response()
}

fn date_param(q: &HashMap<String, String>, k: &str) -> Result<Option<chrono::NaiveDate>, String> {
    match q.get(k) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| format!("{k}: not a date: {v:?} (use YYYY-MM-DD)\n")),
    }
}

/// Nights `?from=YYYY-MM-DD&to=YYYY-MM-DD` (by the date they start on;
/// default the last seven), each with when he fell asleep and woke, the
/// interruptions, and why.
/// `/api/work?period=day|week|month&date=YYYY-MM-DD`: commits and lines
/// (Erik's own and his agents'), and every agent's tokens with their cost at
/// API list prices, per hour of a day or per day of a week or month, with the
/// period before for comparison. Defaults: this week.
async fn api_work(State(state): State<Arc<AppState>>, q: Q) -> Response {
    use crate::views::{Kind, Period};
    let tz = state.tz;
    let today = crate::day::today(tz, state.clock.now_ms());
    let kind = match q.get("period").map(String::as_str).unwrap_or("week") {
        "day" => Kind::Day,
        "week" => Kind::Week,
        "month" => Kind::Month,
        p => {
            return (
                StatusCode::BAD_REQUEST,
                format!("period {p:?}: use day, week or month"),
            )
                .into_response()
        }
    };
    let date = match date_param(&q, "date") {
        Ok(d) => d.unwrap_or(today),
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let period = Period::containing(kind, date);
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let cur = crate::views::work_of(&state.store, tz, period)?;
        let prev = crate::views::work_of(&state.store, tz, period.prev())?;
        let mut v = cur.json(&prev.totals());
        let (labels, links) = crate::views::bucket_labels(period, cur.buckets);
        v["period"] = serde_json::json!({"kind": q.get("period").cloned().unwrap_or("week".into()),
            "start": period.start, "end": period.end, "tz": tz.name()});
        v["labels"] = labels.into();
        v["links"] = links.into();
        v["cost_note"] = "cost is the equivalent at API list prices; most of it is on subscriptions".into();
        v["sources"] = serde_json::json!({
            "commits": "github.commit events: local clones under ~/projects and ~/.dotfiles, and GitHub's commit search for commits no clone has (time-collect)",
            "agent": "a Co-Authored-By trailer naming an agent, or an agent session working in the same repository within 5 minutes before or 2 after the commit",
            "lines": "git diffs with renames detected; lockfiles, generated, vendored and data files, files over 3k lines and commits over 30k lines left out",
            "tokens": "llm.usage.hour events: T3 Code's usage scan of the Claude, Codex and Grok transcripts, and OpenCode's database",
            "cost": "LiteLLM list prices (T3 Code's usage-model-rates.json); OpenCode's own cost where it reports one",
        });
        Ok(v)
    })
    .await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn api_sleep(State(state): State<Arc<AppState>>, q: Q) -> Response {
    let today = crate::day::today(state.tz, state.clock.now_ms());
    let to = match date_param(&q, "to") {
        Ok(d) => d.unwrap_or(today),
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let from = match date_param(&q, "from") {
        Ok(d) => d.unwrap_or(to - chrono::Days::new(7)),
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let tz = state.tz;
    let done =
        tokio::task::spawn_blocking(move || crate::derive::nights(&state.store, from, to)).await;
    match done {
        Ok(Ok(nights)) => {
            let hm = |t: Option<i64>| {
                use chrono::TimeZone;
                t.and_then(|t| tz.timestamp_millis_opt(t).single())
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            };
            let rows: Vec<Value> = nights
                .iter()
                .map(|n| {
                    json!({
                        "night": n.night,
                        "fell_asleep": hm(n.fell_asleep_ms),
                        "woke_up": hm(n.woke_up_ms),
                        "minutes_asleep": n.minutes(),
                        "interruptions": n.interruptions.iter().map(|i| json!({
                            "from": hm(Some(i.from_ms)), "to": hm(Some(i.to_ms)), "what": i.what
                        })).collect::<Vec<_>>(),
                        "confidence": n.confidence,
                        "source": n.source,
                        "reason": n.reason,
                        "decision_id": n.decision_id,
                        "done": n.done,
                        "detail": n,
                    })
                })
                .collect();
            Json(json!({"nights": rows})).into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// A day as spans of activity, gaps filled, with every fill and its reason.
async fn api_spans(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(date): axum::extract::Path<String>,
) -> Response {
    let Ok(date) = date.parse::<chrono::NaiveDate>() else {
        return (StatusCode::BAD_REQUEST, "not a date (use YYYY-MM-DD)\n").into_response();
    };
    let tz = state.tz;
    let done = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let (from, to) = crate::day::bounds(tz, date)?;
        let labels = state.store.labels_between(from, to)?;
        let fills = state.store.fills_between(from, to)?;
        Ok(json!({
            "date": date,
            "spans": crate::derive::spans(&labels, &fills),
            "fills": fills,
        }))
    })
    .await;
    match done {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The decision model's log: `?kind=sleep|gaps&key=...&limit=20`. Each
/// question exactly as asked, with the answer, so a wrong decision can be
/// debugged.
async fn api_decisions(State(state): State<Arc<AppState>>, q: Q) -> Response {
    let limit = q
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(20)
        .min(500);
    let done = tokio::task::spawn_blocking(move || {
        crate::decide::log(
            &state.store,
            q.get("kind").map(String::as_str),
            q.get("key").map(String::as_str),
            limit,
        )
    })
    .await;
    match done {
        Ok(Ok(v)) => Json(json!({"decisions": v})).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
