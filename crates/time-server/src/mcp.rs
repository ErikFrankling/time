//! Time as an MCP server, so Erik's personal agent can ask what he is doing.
//!
//! Streamable HTTP, stateless: every `POST /mcp` carries one JSON-RPC message
//! (or a batch) and is answered with plain JSON, no sessions and no SSE. Four
//! read-only tools:
//!
//! - `time_now`: what he is doing right now and in the last 5 minutes
//! - `time_summary`: a window (today, 3h, ...) by category, app and site
//! - `time_timeline`: the window as blocks
//! - `time_listening`: what was audible, per source (Spotify, YouTube, ...)
//!
//! Each answers with one paragraph and the same facts as compact JSON
//! ([`crate::digest`]). Auth: when `TIME_MCP_TOKEN` is set, `/mcp` requires
//! `Authorization: Bearer <token>`. The route is lan-only on top of that.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::{digest, AppState};

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Time records what Erik does on his devices (computers and phone) and labels every minute. \
Use time_now to see what he is doing right now and whether it is unproductive (video, feeds like X and Reddit; gaming counts as neutral), \
time_summary for minutes per category over today or the last N hours, time_timeline for the blocks, time_listening for what he listened to (Spotify, YouTube, ...) by audible minutes. \
time_summary also has his commits, lines of code and AI token use with its API-price equivalent (window 'week' for this week). \
Every result starts with a one-paragraph summary followed by the JSON. Minutes of 'away' mean no device was in use.";

pub fn tools() -> Value {
    let window = |default: &str| {
        json!({
            "type": "string",
            "description": format!("today, yesterday, week (Monday until now), or a length like 30m, 3h, 2d (up to 7 days). Default {default}."),
        })
    };
    json!([
        {
            "name": "time_now",
            "title": "What is Erik doing right now",
            "description": "What Erik is doing right now and in the last 5 minutes: active app or site, category (and whether it counts as productive, unproductive or neutral), device, idle or away, how long, plus the last hour's totals. Says so when no device has reported recently.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "time_summary",
            "title": "Time summary for a window",
            "description": "Minutes per category for a window (today so far, the last N hours), the top apps and sites, projects, productive vs unproductive vs neutral totals, sleep (when Erik fell asleep and woke up, hours asleep, averages for longer windows), and code and AI (code_and_ai: commits and lines added/removed, split into Erik's own and his agents', per repository; every agent's tokens (input, cache read, cache write, output) and their cost at API list prices, per model, account, project and T3 thread). Which categories are unproductive is configured on the server (video and feeds by default; gaming is neutral).",
            "inputSchema": {
                "type": "object",
                "properties": {"window": window("today")},
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "time_timeline",
            "title": "Timeline blocks for a window",
            "description": "The window as consecutive blocks (category, device, apps, sites, project, start and end), oldest first. Blocks shorter than min_minutes are left out; only the latest `limit` blocks are returned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": window("3h"),
                    "min_minutes": {"type": "integer", "minimum": 1, "description": "Hide blocks shorter than this. Default 2."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Latest blocks to return. Default 60."},
                },
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "time_listening",
            "title": "What Erik listened to",
            "description": "Minutes of sound that could really be heard on the computers (playing, not muted, above silence), per source (Spotify, YouTube, Netflix, ...), with the top tracks or titles, how much of it was while away from the keyboard, and what is audible right now. A paused or muted tab never counts.",
            "inputSchema": {
                "type": "object",
                "properties": {"window": window("today")},
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
    ])
}

/// Run one tool. Errors here are tool errors (shown to the model), not
/// protocol errors.
pub fn call_tool(state: &AppState, name: &str, args: &Value) -> anyhow::Result<digest::Digest> {
    let tz = state.tz;
    let now = state.clock.now_ms();
    let cfg = &state.config.mcp;
    let window_arg = |default: &str| -> anyhow::Result<digest::Window> {
        let spec = args
            .get("window")
            .and_then(Value::as_str)
            .unwrap_or(default);
        digest::parse_window(spec, tz, now)
    };
    match name {
        "time_now" => {
            let from = now - digest::NOW_LOOKBACK_MIN * time_core::clock::MINUTE_MS;
            let (labels, events) = digest::load(&state.store, from, now)?;
            Ok(digest::now(cfg, tz, now, &labels, &events))
        }
        "time_summary" => {
            let w = window_arg("today")?;
            let (labels, events) = digest::load(&state.store, w.from_ms, w.to_ms)?;
            let mut d = digest::summary(cfg, tz, &w, &labels, &events);
            add_sleep(state, &w, &mut d)?;
            Ok(d)
        }
        "time_timeline" => {
            let w = window_arg("3h")?;
            let min_minutes = args
                .get("min_minutes")
                .and_then(Value::as_u64)
                .unwrap_or(2)
                .clamp(1, 1000) as u32;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(60)
                .clamp(1, 200) as usize;
            let (labels, events) = digest::load(&state.store, w.from_ms, w.to_ms)?;
            Ok(digest::timeline(
                cfg,
                tz,
                &w,
                &labels,
                &events,
                min_minutes,
                limit,
            ))
        }
        "time_listening" => {
            let w = window_arg("today")?;
            let (labels, events) = digest::load(&state.store, w.from_ms, w.to_ms)?;
            Ok(crate::listening::digest(&w, &labels, &events, now))
        }
        other => anyhow::bail!("unknown tool {other:?}"),
    }
}

/// The nights whose sleep falls in the window: when Erik fell asleep and
/// woke up, hours asleep and why, and the averages.
fn add_sleep(state: &AppState, w: &digest::Window, d: &mut digest::Digest) -> anyhow::Result<()> {
    use chrono::{TimeZone, Timelike};
    let tz = state.tz;
    let day = |t| crate::day::today(tz, t);
    let nights = crate::derive::nights(
        &state.store,
        crate::derive::night_before(day(w.from_ms)),
        day(w.to_ms),
    )?;
    let nights: Vec<_> = nights
        .into_iter()
        .filter(|n| match (n.fell_asleep_ms, n.woke_up_ms) {
            (Some(a), Some(b)) => a < w.to_ms && b > w.from_ms,
            _ => false,
        })
        .collect();
    let hm = |t: Option<i64>| {
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
                "hours_asleep": n.minutes().map(|m| (m as f64 / 6.0).round() / 10.0),
                "interruptions": n.interruptions.len(),
                "decided_by": n.source,
                "confidence": n.confidence,
                "reason": n.reason,
            })
        })
        .collect();
    let mins: Vec<i64> = nights.iter().filter_map(|n| n.minutes()).collect();
    if mins.is_empty() {
        d.json["sleep"] = json!({"nights": [], "note": "no night's sleep found in this window"});
        return Ok(());
    }
    let avg_h = mins.iter().sum::<i64>() as f64 / mins.len() as f64 / 60.0;
    // Clock times averaged from 18:00 so 23:30 and 02:30 give 01:00.
    let clock = |f: &dyn Fn(&crate::derive::Night) -> Option<i64>| {
        let v: Vec<i64> = nights
            .iter()
            .filter_map(f)
            .filter_map(|t| tz.timestamp_millis_opt(t).single())
            .map(|d| (i64::from(d.hour() * 60 + d.minute()) - 1080).rem_euclid(1440))
            .collect();
        let m = (v.iter().sum::<i64>() / v.len().max(1) as i64 + 1080) % 1440;
        format!("{:02}:{:02}", m / 60, m % 60)
    };
    let bed = clock(&|n| n.fell_asleep_ms);
    let up = clock(&|n| n.woke_up_ms);
    d.json["sleep"] = json!({
        "nights": rows,
        "average_hours_asleep": (avg_h * 10.0).round() / 10.0,
        "average_bedtime": bed,
        "average_wake_up": up,
    });
    let text = if nights.len() == 1 {
        let n = &nights[0];
        format!(
            " Sleep: {} to {}, {:.1} h asleep ({}).",
            hm(n.fell_asleep_ms).unwrap_or_default(),
            hm(n.woke_up_ms).unwrap_or_default(),
            avg_h,
            n.reason
        )
    } else {
        format!(
            " Sleep: {} nights, {avg_h:.1} h asleep on average, bedtime {bed}, up {up}.",
            nights.len()
        )
    };
    d.text.push_str(&text);
    Ok(())
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One JSON-RPC message in, the reply out (None for notifications).
fn handle_one(state: &AppState, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        // A notification (initialized, cancelled, ...): nothing to say.
        return None;
    };
    // A response to a request we never made has no method.
    msg.get("method")?;
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = asked
                .filter(|v| SUPPORTED_VERSIONS.contains(v))
                .unwrap_or(SUPPORTED_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "time", "title": "Time", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if !tools()
                .as_array()
                .is_some_and(|t| t.iter().any(|t| t["name"] == name))
            {
                return Some(rpc_error(id, -32602, &format!("unknown tool {name:?}")));
            }
            match call_tool(state, name, &args) {
                Ok(d) => {
                    json!({"content": [{"type": "text", "text": d.tool_text()}], "isError": false})
                }
                Err(e) => {
                    json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true})
                }
            }
        }
        _ => {
            return Some(rpc_error(
                id,
                -32601,
                &format!("method not found: {method}"),
            ))
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn authorised(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = &state.mcp_token else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == token)
}

pub async fn post(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorised(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "missing or wrong bearer token",
        )
            .into_response();
    }
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return axum::Json(rpc_error(Value::Null, -32700, &format!("parse error: {e}")))
                .into_response()
        }
    };
    let replies = tokio::task::spawn_blocking(move || match &parsed {
        Value::Array(batch) => {
            let r: Vec<Value> = batch.iter().filter_map(|m| handle_one(&state, m)).collect();
            (!r.is_empty()).then_some(Value::Array(r))
        }
        one => handle_one(&state, one),
    })
    .await;
    match replies {
        Ok(Some(v)) => axum::Json(v).into_response(),
        // Only notifications: accepted, no body.
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Stateless server: no stream to open, no session to close.
pub async fn not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "this MCP server only answers POST\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_schema_and_is_read_only() {
        for t in tools().as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
            assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
            assert!(t["description"].as_str().unwrap().len() > 40);
        }
        let names: Vec<_> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            [
                "time_now",
                "time_summary",
                "time_timeline",
                "time_listening"
            ]
        );
    }
}
