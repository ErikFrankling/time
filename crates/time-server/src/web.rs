//! The website. Server-rendered HTML, mobile-first, no build step, works
//! without JavaScript. The phone app shows these same pages in a WebView.

use std::fmt::Write;

use chrono::TimeZone;
use time_core::clock::Millis;

use crate::status::{ChannelState, DeviceState, Status};

/// Escape text for HTML.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// "3 min ago", "in 8 s", "never".
pub fn ago(now: Millis, t: Option<Millis>) -> String {
    let Some(t) = t else { return "never".into() };
    let d = (now - t) / 1000;
    let (future, d) = if d < 0 { (true, -d) } else { (false, d) };
    let span = if d < 60 {
        format!("{d} s")
    } else if d < 3600 {
        format!("{} min", d / 60)
    } else if d < 48 * 3600 {
        format!("{} h {} min", d / 3600, d % 3600 / 60)
    } else {
        format!("{} days", d / 86400)
    };
    if future {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

pub fn clock_time(tz: chrono_tz::Tz, t: Option<Millis>) -> String {
    match t.and_then(|t| tz.timestamp_millis_opt(t).single()) {
        Some(dt) => dt.format("%a %d %b %H:%M").to_string(),
        None => "—".into(),
    }
}

const STYLE: &str = r#"
:root { color-scheme: light dark; --bg:#fbfbfa; --fg:#1d1d1b; --muted:#6b6b66; --card:#fff; --line:#e6e5e1;
  --ok:#1f7a4d; --warn:#9a6200; --bad:#b3261e; --off:#6b6b66; }
@media (prefers-color-scheme: dark) { :root { --bg:#141413; --fg:#ecebe6; --muted:#9d9c95; --card:#1d1d1b; --line:#2e2e2b;
  --ok:#5cc28d; --warn:#e3a33b; --bad:#f2847b; --off:#9d9c95; } }
* { box-sizing: border-box; }
body { margin:0; font:15px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; background:var(--bg); color:var(--fg); }
header { display:flex; gap:1rem; align-items:baseline; padding:.9rem 1rem; border-bottom:1px solid var(--line); }
header a { color:var(--muted); text-decoration:none; } header a.here { color:var(--fg); font-weight:600; }
h1 { font-size:1.05rem; margin:0 auto 0 0; }
main { max-width:56rem; margin:0 auto; padding:1rem; display:grid; grid-template-columns:minmax(0,1fr); gap:1rem; }
section { background:var(--card); border:1px solid var(--line); border-radius:12px; padding:.9rem 1rem; }
h2 { font-size:.8rem; text-transform:uppercase; letter-spacing:.06em; color:var(--muted); margin:0 0 .6rem; }
.banner { font-size:1.05rem; font-weight:600; }
.row { display:grid; grid-template-columns: 1.2rem minmax(0,1fr) auto; gap:.5rem; padding:.45rem 0; border-top:1px solid var(--line); align-items:start; }
.row:first-of-type { border-top:0; }
.dot { width:.7rem; height:.7rem; border-radius:50%; margin-top:.35rem; }
.ok { background:var(--ok); } .warn { background:var(--warn); } .bad { background:var(--bad); } .off { background:transparent; border:2px solid var(--off); }
.t-ok { color:var(--ok); } .t-warn { color:var(--warn); } .t-bad { color:var(--bad); } .t-off { color:var(--off); }
.name { font-weight:600; } .sub { color:var(--muted); font-size:.88rem; }
.state { font-size:.88rem; font-weight:600; text-align:right; white-space:nowrap; }
dl { display:grid; grid-template-columns: max-content 1fr; gap:.3rem 1rem; margin:0; }
dt { color:var(--muted); } dd { margin:0; font-variant-numeric: tabular-nums; }
dd, .sub { overflow-wrap:anywhere; }
@media (max-width: 34rem) { dl { grid-template-columns: 1fr; gap:0; } dt { margin-top:.5rem; font-size:.85rem; } }
footer { color:var(--muted); font-size:.8rem; text-align:center; padding:0 1rem 1.5rem; }
"#;

pub fn page(title: &str, here: &str, refresh_secs: Option<u32>, body: &str) -> String {
    let refresh = refresh_secs
        .map(|s| format!(r#"<meta http-equiv="refresh" content="{s}">"#))
        .unwrap_or_default();
    let nav = [("/", "Today"), ("/status", "Status")]
        .iter()
        .map(|(href, label)| {
            let class = if *href == here {
                r#" class="here""#
            } else {
                ""
            };
            format!(r#"<a href="{href}"{class}>{label}</a>"#)
        })
        .collect::<String>();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">{refresh}<title>{t}</title><style>{STYLE}</style></head><body><header><h1>Time</h1>{nav}</header><main>{body}</main></body></html>"#,
        t = esc(title),
    )
}

fn device_dot(s: DeviceState) -> (&'static str, &'static str) {
    match s {
        DeviceState::Connected => ("ok", "Connected"),
        DeviceState::Late => ("warn", "Late"),
        DeviceState::Offline => ("bad", "Offline"),
        DeviceState::Never => ("bad", "Never heard from"),
    }
}

fn channel_dot(s: ChannelState) -> (&'static str, &'static str) {
    match s {
        ChannelState::Live => ("ok", "Recording"),
        ChannelState::Quiet => ("warn", "Quiet"),
        ChannelState::Missing => ("bad", "No data"),
        ChannelState::Off => ("off", "Switched off"),
        ChannelState::NotSetUp => ("off", "Not set up"),
    }
}

/// The status screen.
pub fn status_page(s: &Status, tz: chrono_tz::Tz) -> String {
    let now = s.now_ms;
    let c = &s.pipeline.classifier;
    let mut body = String::new();

    // One sentence that answers "is it working?".
    let bad_devices: Vec<&str> = s
        .devices
        .iter()
        .filter(|d| !matches!(d.state, DeviceState::Connected))
        .map(|d| d.name.as_str())
        .collect();
    let (cls, text) = if let Some(e) = &c.last_error {
        ("t-bad", format!("Classification is failing: {}", esc(e)))
    } else if !bad_devices.is_empty() {
        (
            "t-warn",
            format!("Not hearing from {}.", esc(&bad_devices.join(", "))),
        )
    } else if c.waiting > 0 && c.oldest_waiting_ms.is_some_and(|t| now - t > 15 * 60_000) {
        (
            "t-warn",
            format!("Classification is behind: {} minute(s) waiting.", c.waiting),
        )
    } else {
        ("t-ok", "Everything is working.".to_owned())
    };
    let _ = write!(
        body,
        r#"<section><div class="banner {cls}">{text}</div><div class="sub">As of {}</div></section>"#,
        clock_time(tz, Some(now))
    );

    // Devices.
    let _ = write!(body, "<section><h2>Devices</h2>");
    for d in &s.devices {
        let (dot, word) = device_dot(d.state);
        let kind = d
            .kind
            .map(|k| format!("{k:?}").to_lowercase())
            .unwrap_or_else(|| "not configured".into());
        let mut sub = format!("{kind} · last heard {}", ago(now, d.last_heartbeat_ms));
        if let Some(b) = d.behind_ms.filter(|b| *b > 5 * 60_000) {
            let _ = write!(
                sub,
                " · reporting {} behind",
                ago(now, Some(now - b)).trim_end_matches(" ago")
            );
        }
        if let Some(p) = d.spool_pending.filter(|p| *p > 0) {
            let _ = write!(sub, " · {p} waiting to send");
        }
        if let Some(v) = &d.app_version {
            let _ = write!(sub, " · v{}", esc(v));
        }
        let missing = d
            .missing
            .iter()
            .map(|m| format!(r#"<div class="sub t-warn">{}</div>"#, esc(m)))
            .collect::<String>();
        let _ = write!(
            body,
            r#"<div class="row"><span class="dot {dot}"></span><div><div class="name">{}</div><div class="sub">{sub}</div>{missing}</div><span class="state t-{dot}">{word}</span></div>"#,
            esc(&d.name)
        );
    }
    if s.devices.is_empty() {
        body.push_str(r#"<div class="sub">No devices configured or heard from.</div>"#);
    }
    body.push_str("</section>");

    // Channels.
    let _ = write!(body, "<section><h2>What is being recorded</h2>");
    for ch in &s.channels {
        let (dot, word) = channel_dot(ch.state);
        let sub = match ch.state {
            ChannelState::Off => "switched off in the config".to_owned(),
            ChannelState::NotSetUp => "nothing has sent this yet".to_owned(),
            _ => format!(
                "last {} · {} in the last hour",
                ago(now, ch.last_ms),
                ch.last_hour
            ),
        };
        let _ = write!(
            body,
            r#"<div class="row"><span class="dot {dot}"></span><div><div class="name">{}</div><div class="sub">{sub}</div></div><span class="state t-{dot}">{word}</span></div>"#,
            esc(ch.label)
        );
    }
    body.push_str("</section>");

    // Classification.
    let _ = write!(
        body,
        r#"<section><h2>Classification</h2><dl>
<dt>Labelled</dt><dd>{} minutes in total · {} in the last 24 h</dd>
<dt>Waiting</dt><dd>{} minute(s){}</dd>
<dt>Unknown, last 24 h</dt><dd>{} minute(s)</dd>
<dt>Waiting on a late device</dt><dd>{} minute(s), relabelled when it reports</dd>
<dt>Last run</dt><dd>{}</dd>
<dt>Next run</dt><dd>{} (every {} s)</dd>
<dt>Rules</dt><dd>{} rules · version {}</dd>
<dt>Method</dt><dd>rules on the CPU; no model, no GPU</dd>
</dl></section>"#,
        c.labelled_total,
        c.last_24h_minutes,
        c.waiting,
        c.oldest_waiting_ms
            .map(|t| format!(", oldest from {}", clock_time(tz, Some(t))))
            .unwrap_or_default(),
        c.last_24h_unknown,
        c.last_24h_incomplete,
        c.last_run_ms
            .map(|t| format!("{} ({})", ago(now, Some(t)), clock_time(tz, Some(t))))
            .unwrap_or_else(|| "not yet".into()),
        c.next_run_ms
            .map(|t| ago(now, Some(t)))
            .unwrap_or_else(|| "when the server starts its loop".into()),
        c.every_secs,
        c.rules,
        esc(&c.rules_version),
    );

    // System.
    let _ = write!(
        body,
        r#"<section><h2>System</h2><dl>
<dt>Server</dt><dd>v{} · up {}</dd>
<dt>Database</dt><dd>{:.1} MB · schema {} · {} events</dd>
<dt>Ingest since start</dt><dd>{} accepted · {} duplicates ignored · {} rejected</dd>
</dl></section>"#,
        esc(s.system.version),
        ago(now, Some(s.system.started_ms)).trim_end_matches(" ago"),
        s.system.db_bytes as f64 / 1e6,
        s.system.schema,
        s.pipeline.events_total,
        s.pipeline.accepted,
        s.pipeline.duplicate,
        s.pipeline.rejected,
    );

    page("Time · Status", "/status", Some(30), &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_relative_times() {
        assert_eq!(
            esc("<a href='x'>&</a>"),
            "&lt;a href=&#39;x&#39;&gt;&amp;&lt;/a&gt;"
        );
        assert_eq!(ago(100_000, Some(40_000)), "1 min ago");
        assert_eq!(ago(0, Some(8_000)), "in 8 s");
        assert_eq!(ago(0, None), "never");
        assert_eq!(ago(3 * 86_400_000, Some(0)), "3 days ago");
    }
}
