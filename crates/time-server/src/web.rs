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
/* Dark by default, whatever the system says: Erik never wants a white
   screen he did not ask for. Light is a choice (the ☀ button), remembered. */
:root { color-scheme: dark; --bg:#141413; --fg:#ecebe6; --muted:#9d9c95; --card:#1d1d1b; --line:#2e2e2b;
  --ok:#5cc28d; --warn:#e3a33b; --bad:#f2847b; --off:#9d9c95; }
:root[data-theme="light"] { color-scheme: light; --bg:#f4f3ef; --fg:#1d1d1b; --muted:#5c5b55; --card:#ffffff; --line:#cfcdc4;
  --ok:#1f8a55; --warn:#a86a00; --bad:#c4382c; --off:#65645e; }
html, body { background:var(--bg); }
.theme { margin-left:.2rem; background:none; border:1px solid var(--line); color:var(--fg); border-radius:99px; width:2rem; height:2rem; font-size:1rem; line-height:1; cursor:pointer; }
/* Clear of notches, the status bar and the gesture bar on phones (zero
   inside the app, which pads natively). */
* { box-sizing: border-box; }
body { margin:0; font:15px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; background:var(--bg); color:var(--fg); }
header { display:flex; gap:1rem; align-items:baseline; padding:calc(.9rem + env(safe-area-inset-top)) calc(1rem + env(safe-area-inset-right)) .9rem calc(1rem + env(safe-area-inset-left)); border-bottom:1px solid var(--line); }
header a { color:var(--muted); text-decoration:none; } header a.here { color:var(--fg); font-weight:600; }
h1 { font-size:1.05rem; margin:0 auto 0 0; } h1 a { color:var(--fg); text-decoration:none; }
main { max-width:56rem; margin:0 auto; padding:1rem 1rem calc(1rem + env(safe-area-inset-bottom)); display:grid; grid-template-columns:minmax(0,1fr); gap:1rem; }
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

/// Before the first paint: the remembered theme (dark unless light was
/// chosen), and `?theme=light|dark` to choose one by link.
const THEME_JS: &str = r#"(function(){var q=new URLSearchParams(location.search).get('theme');try{if(q==='light'||q==='dark')localStorage.setItem('theme',q);var t=localStorage.getItem('theme');}catch(e){var t=q;}if(t==='light'){document.documentElement.dataset.theme='light';}})();
function timeTheme(){var l=document.documentElement.dataset.theme!=='light';if(l){document.documentElement.dataset.theme='light';}else{delete document.documentElement.dataset.theme;}try{localStorage.setItem('theme',l?'light':'dark');}catch(e){}var m=document.querySelector('meta[name=theme-color]');if(m)m.content=l?'#f4f3ef':'#141413';window.dispatchEvent(new Event('timetheme'));}"#;

pub fn page(title: &str, here: &str, refresh_secs: Option<u32>, body: &str) -> String {
    page_with(title, here, refresh_secs, "", body)
}

pub fn page_with(
    title: &str,
    here: &str,
    refresh_secs: Option<u32>,
    extra_style: &str,
    body: &str,
) -> String {
    let refresh = refresh_secs
        .map(|s| format!(r#"<meta http-equiv="refresh" content="{s}">"#))
        .unwrap_or_default();
    let nav = [("/inbox", "Inbox"), ("/status", "Status")]
        .iter()
        .map(|(href, label)| {
            let on = *href == here;
            let class = if on { r#" class="here""# } else { "" };
            format!(r#"<a href="{href}"{class}>{label}</a>"#)
        })
        .collect::<String>();
    format!(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover"><meta name="color-scheme" content="dark"><meta name="theme-color" content="#141413">{refresh}<title>{t}</title><script>{THEME_JS}</script><style>{STYLE}{extra_style}</style></head><body><header><h1><a href="/">Time</a></h1>{nav}<button class="theme" type="button" onclick="timeTheme()" aria-label="Light or dark">☀</button></header><main>{body}</main></body></html>"##,
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
            let v = v.strip_prefix('v').unwrap_or(v);
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

    // Focus nudges.
    let nd = &s.nudges;
    let sinks = if nd.sinks.is_empty() {
        r#"<span class="t-warn">nowhere: no notify-gateway token or agent webhook configured</span>"#
            .to_owned()
    } else {
        nd.sinks.join(" and ")
    };
    let focus = match &nd.focus {
        Some(f) => format!(
            "until {}: {}",
            clock_time(tz, Some(f.until_ms)),
            esc(&f.reason)
        ),
        None => "none".into(),
    };
    let _ = write!(
        body,
        r#"<section><h2>Focus nudges</h2><dl>
<dt>State</dt><dd>{}</dd>
<dt>Goes to</dt><dd>{sinks}</dd>
<dt>Rule</dt><dd>{}</dd>
<dt>Focus day</dt><dd>{focus}</dd>
<dt>Sent today</dt><dd>{}</dd>
<dt>Last check</dt><dd>{} · {}</dd>
</dl></section>"#,
        if nd.enabled {
            "on"
        } else {
            "switched off in the config"
        },
        esc(&nd.rule),
        nd.sent_today,
        nd.last_check_ms
            .map(|t| ago(now, Some(t)))
            .unwrap_or_else(|| "not yet".into()),
        esc(&nd.last_result),
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

/// Activity colours: the validated categorical palette in a fixed order,
/// one hue per activity forever. `other` and `unknown` are neutral greys;
/// away is the empty surface, except out (hatched grey) and out walking
/// (hatched sage), which are not activities and look it.
pub const ACTIVITY_STYLE: &str = r#"
.a-work{--c:#3987e5} .a-feeds{--c:#d95926} .a-study{--c:#199e70} .a-video{--c:#c98500}
 .a-messaging{--c:#d55181} .a-browsing{--c:#008300} .a-music{--c:#9085e9} .a-games{--c:#e66767}
 .a-other{--c:#7d7c76} .a-unknown{--c:#4a4a46}
.aw-walking, .aw-out { background-color:var(--c); background-image: repeating-linear-gradient(135deg, transparent 0 3px, var(--bg) 3px 5px); }
.aw-walking{--c:#8fb8a8} .aw-out{--c:#6b6a64}
.sw { display:inline-block; width:.75rem; height:.75rem; border-radius:3px; background-color:var(--c); vertical-align:-.1rem; }
.ph, .strip > span.ph, .tot .bar > span.ph { background-image: repeating-linear-gradient(45deg, transparent 0 3px, var(--card) 3px 5px); }
.stats { display:grid; grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr)); gap:.8rem; }
.stat .v { font-size:1.35rem; font-weight:650; font-variant-numeric: tabular-nums; }
.stat .k { color:var(--muted); font-size:.85rem; }
.daynav { display:flex; justify-content:space-between; align-items:baseline; }
.daynav a { color:var(--muted); text-decoration:none; }
.hours { display:grid; gap:3px; }
.hour { display:grid; grid-template-columns: 1.6rem minmax(0,1fr); gap:.4rem; align-items:center; }
.hour .h { color:var(--muted); font-size:.75rem; font-variant-numeric: tabular-nums; }
.strip { display:flex; gap:1px; height:14px; background:var(--line); border-radius:4px; overflow:hidden; }
.strip > span { min-width:0; background-color:var(--c, var(--bg)); }
.strip > span.empty { background-color:var(--bg); }
.legend { display:flex; flex-wrap:wrap; gap:.3rem .9rem; font-size:.85rem; color:var(--muted); margin-top:.7rem; }
.tot { display:grid; grid-template-columns: 6.5rem minmax(0,1fr) max-content; gap:.6rem; align-items:center; padding:.3rem 0; }
.tot .bar { display:flex; gap:2px; height:12px; }
.tot .bar > span { background-color:var(--c); border-radius:0 4px 4px 0; min-width:0; }
.tot .bar > span + span { border-radius:0; }
.tot a { color:var(--fg); text-decoration:none; }
.tot .bar > span:first-child:not(:last-child) { border-radius:0; }
.tot .n { text-align:right; font-variant-numeric: tabular-nums; font-size:.9rem; white-space:nowrap; }
.steps { display:flex; align-items:flex-end; gap:2px; height:56px; }
.steps > span { flex:1; background:#3987e5; border-radius:3px 3px 0 0; min-height:1px; }
.axis { display:flex; justify-content:space-between; color:var(--muted); font-size:.75rem; margin-top:.2rem; }
"#;

pub fn duration(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// The day view: the period header and donut shared with week and month,
/// then the day's own detail.
pub fn day_page(
    day: &crate::day::Day,
    tz: chrono_tz::Tz,
    today: chrono::NaiveDate,
    cur: &crate::views::Summary,
    prev: &crate::views::Summary,
    phones: &[String],
    step_goal: u32,
) -> String {
    use crate::views;
    let is_today = day.date == today;
    let mut body = views::nav(cur.period, today);
    body.push_str(&views::charts(cur, prev, step_goal, Some(day)));
    // The detail below folds away: each card opens with a tap.
    let detail_start = body.len();
    body.push_str(&views::lanes(
        &day.minutes,
        day.start_ms,
        day.end_ms,
        tz,
        phones,
    ));
    if day.unknown_minutes > 0 {
        let _ = write!(
            body,
            r#"<section><div class="sub">{} unknown: no rule matched. <a href="/inbox">Label them in the inbox</a>.</div></section>"#,
            duration(day.unknown_minutes)
        );
    }

    // Places and signals: where he was and what the devices were doing.
    let sg = &day.signals;
    if !sg.places.is_empty()
        || !sg.phone.is_empty()
        || !sg.computers.is_empty()
        || sg.phone_unlocks > 0
    {
        let span = |s: &crate::day::Span| {
            format!(
                "{} {}–{}",
                esc(&s.what),
                clock_hm(tz, s.from_ms),
                clock_hm(tz, s.to_ms)
            )
        };
        let list = |v: &[crate::day::Span]| v.iter().map(span).collect::<Vec<_>>().join(" · ");
        let _ = write!(body, r#"<section><h2>Places and signals</h2><dl>"#);
        if !sg.places.is_empty() {
            let _ = write!(body, "<dt>Where</dt><dd>{}</dd>", list(&sg.places));
        }
        if !sg.phone.is_empty() || sg.phone_unlocks > 0 {
            let mut phone = list(&sg.phone);
            if sg.phone_unlocks > 0 {
                if !phone.is_empty() {
                    phone.push_str(" · ");
                }
                phone.push_str(&format!("unlocked {} times", sg.phone_unlocks));
            }
            let _ = write!(body, "<dt>Phone</dt><dd>{phone}</dd>");
        }
        if !sg.computers.is_empty() {
            let _ = write!(body, "<dt>Computers</dt><dd>{}</dd>", list(&sg.computers));
        }
        body.push_str("</dl></section>");
    }

    // How: typing, voice, listening, screen; then what the AI did.
    let md = &day.modalities;
    if !md.is_empty() || day.computer_minutes + day.phone_minutes > 0 {
        let _ = write!(body, r#"<section><h2>Typing, voice, listening</h2><dl>"#);
        let _ = write!(
            body,
            "<dt>Typing</dt><dd>{} with keys pressed · {} key presses{}</dd>",
            duration(md.typing_minutes),
            group_thousands(md.key_presses),
            if md.prompts_typed > 0 {
                format!(
                    " · {} typed prompts, {} words",
                    md.prompts_typed,
                    group_thousands(md.words_typed)
                )
            } else {
                String::new()
            }
        );
        let _ = write!(
            body,
            "<dt>Voice</dt><dd>{} dictations · {} dictated prompts, {} words</dd>",
            md.dictations,
            md.prompts_dictated,
            group_thousands(md.words_dictated)
        );
        let _ = write!(
            body,
            "<dt>Listening</dt><dd>{} with sound playing{}</dd>",
            duration(md.listening_minutes),
            if md.read_aloud_minutes > 0 {
                format!(" · read-aloud {}", duration(md.read_aloud_minutes))
            } else {
                String::new()
            }
        );
        if md.voice_mode_minutes > 0 {
            let _ = write!(
                body,
                "<dt>Voice mode</dt><dd>{} talking with an agent</dd>",
                duration(md.voice_mode_minutes)
            );
        }
        if !md.focus.is_empty() {
            let projects: Vec<String> = md
                .focus
                .iter()
                .map(|f| format!("{} {}", esc(&f.project), duration(f.minutes)))
                .collect();
            let _ = write!(body, "<dt>In T3 Code</dt><dd>{}</dd>", projects.join(" · "));
        }
        let _ = write!(
            body,
            "<dt>Screen</dt><dd>{} at a computer · {} on the phone</dd>",
            duration(day.computer_minutes),
            duration(day.phone_minutes)
        );
        body.push_str("</dl></section>");
    }
    if !md.tokens.is_empty() || md.prompts_by_agents > 0 {
        let _ = write!(
            body,
            r#"<section><h2>AI</h2><div class="sub">{} prompts from Erik · {} from agents and schedules · {} tokens</div><dl>"#,
            md.prompts_typed + md.prompts_dictated,
            md.prompts_by_agents,
            tokens_text(md.tokens_total())
        );
        for t in &md.tokens {
            let _ = write!(
                body,
                "<dt>{}</dt><dd>{} in ({} cached) · {} out · {} turns, {} from Erik's prompts</dd>",
                esc(&t.account),
                tokens_text(t.input),
                tokens_text(t.cached_input),
                tokens_text(t.output),
                t.turns,
                t.turns_by_erik
            );
        }
        body.push_str("</dl></section>");
    }

    // Code.
    let c = &day.code;
    if !c.is_empty() {
        let _ = write!(
            body,
            r#"<section><h2>Code</h2><dl>
<dt>Commits</dt><dd>{} · +{} −{} lines</dd>
<dt>Pull requests</dt><dd>{} opened · {} merged · {} reviews</dd>
<dt>Repositories</dt><dd>{}</dd>
</dl></section>"#,
            c.commits,
            group_thousands(c.lines_added),
            group_thousands(c.lines_removed),
            c.prs_opened,
            c.prs_merged,
            c.reviews,
            esc(&c.repos.join(", ")),
        );
    }

    let detail = body.split_off(detail_start);
    body.push_str(
        &detail
            .replace("<section><h2>", r#"<details class="card"><summary>"#)
            .replace("</h2>", "</summary>")
            .replace("</section>", "</details>"),
    );

    let title = format!("Time · {}", day.date);
    let style = format!("{ACTIVITY_STYLE}{}", crate::views::STYLE);
    page_with(
        &title,
        "/",
        if is_today { Some(60) } else { None },
        &style,
        &body,
    )
}

fn clock_hm(tz: chrono_tz::Tz, t: Millis) -> String {
    tz.timestamp_millis_opt(t)
        .single()
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// 1234 → "1.2k", 5_600_000 → "5.6M".
fn tokens_text(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

pub fn group_thousands(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

/// The unknown inbox: one row per window or phone app no rule matched, each
/// with a form that turns the answer into a rule.
pub fn inbox_page(items: &[crate::inbox::Item], tz: chrono_tz::Tz, added: Option<&str>) -> String {
    use crate::inbox::Subject;
    use time_core::label::Activity;

    let options: String = Activity::ALL
        .iter()
        .filter(|a| **a != Activity::Unknown)
        .map(|a| format!(r#"<option value="{a}">{a}</option>"#))
        .collect();
    let mut body = String::new();
    if let Some(id) = added {
        let _ = write!(
            body,
            r#"<section><div class="banner t-ok">Rule {} added. History has been relabelled with it.</div></section>"#,
            esc(id)
        );
    }
    let total: u32 = items.iter().map(|i| i.minutes).sum();
    let _ = write!(
        body,
        r#"<section><h2>Unknown, last 14 days</h2><div class="sub">{} across {} windows and apps. Answer once and every minute like it, past and future, is labelled.</div></section>"#,
        duration(total),
        items.len()
    );
    for item in items.iter().take(100) {
        let (name, detail) = match &item.subject {
            Subject::Window { class, title } => (
                if title.is_empty() {
                    class.clone()
                } else {
                    title.clone()
                },
                format!(
                    "window · {}",
                    if class.is_empty() {
                        "no window class"
                    } else {
                        class
                    }
                ),
            ),
            Subject::PhoneApp { package, label } => (
                if label.is_empty() {
                    package.clone()
                } else {
                    label.clone()
                },
                format!("phone app · {package}"),
            ),
        };
        // One form per answer, so each button means exactly one thing.
        let forms = match &item.subject {
            Subject::Window { class, title } => {
                let mut f = String::new();
                if !title.is_empty() {
                    let _ = write!(
                        f,
                        r#"<form method="post" action="/inbox/rule"><input type="hidden" name="on" value="title"><input type="hidden" name="value" value="{}"><input type="hidden" name="class" value="{}"><select name="activity">{options}</select> <button>Just this title</button></form>"#,
                        esc(title),
                        esc(class)
                    );
                }
                if !class.is_empty() {
                    let _ = write!(
                        f,
                        r#"<form method="post" action="/inbox/rule"><input type="hidden" name="on" value="class"><input type="hidden" name="value" value="{}"><select name="activity">{options}</select> <button>All of {}</button></form>"#,
                        esc(class),
                        esc(class)
                    );
                }
                f
            }
            Subject::PhoneApp { package, .. } => format!(
                r#"<form method="post" action="/inbox/rule"><input type="hidden" name="on" value="package"><input type="hidden" name="value" value="{}"><select name="activity">{options}</select> <button>This app</button></form>"#,
                esc(package)
            ),
        };
        let _ = write!(
            body,
            r#"<section class="inbox"><div class="name">{}</div><div class="sub">{} · {} · last {}</div>{forms}</section>"#,
            esc(&name),
            esc(&detail),
            duration(item.minutes),
            clock_time(tz, Some(item.last_ms)),
        );
    }
    if items.is_empty() {
        body.push_str(r#"<section><div class="banner t-ok">Nothing unknown. Every minute in the last 14 days matched a rule.</div></section>"#);
    }
    page_with("Time · Inbox", "/inbox", None, INBOX_STYLE, &body)
}

const INBOX_STYLE: &str = r#"
.inbox form { display:flex; flex-wrap:wrap; gap:.4rem; align-items:center; margin-top:.5rem; }
.inbox select, .inbox button { font:inherit; padding:.35rem .6rem; border-radius:8px; border:1px solid var(--line); background:var(--bg); color:var(--fg); }
.inbox button { cursor:pointer; font-weight:600; }
"#;

/// `/app/`: what the server is handing out to the phone, and how to install it.
pub fn app_page(s: &crate::apk::State, repo: &str, tz: chrono_tz::Tz) -> String {
    let mut body = String::new();
    match &s.meta {
        Some(m) => {
            let _ = write!(
                body,
                r#"<section><h2>Android app</h2><dl>
<dt>Version</dt><dd>{} (versionCode {})</dd>
<dt>Signed with</dt><dd>{} key</dd>
<dt>Size</dt><dd>{:.1} MB</dd>
<dt>Published</dt><dd>{}</dd>
<dt>SHA-256</dt><dd>{}</dd>
</dl><p><a href="/app">Download the APK</a></p></section>"#,
                esc(&m.version),
                m.version_code,
                esc(&m.signing),
                m.size as f64 / 1e6,
                esc(&m.published),
                esc(&m.sha256),
            );
        }
        None => body.push_str(
            r#"<section><div class="banner t-warn">No APK has been fetched yet.</div></section>"#,
        ),
    }
    if !s.error.is_empty() {
        let _ = write!(
            body,
            r#"<section><div class="sub t-warn">Last check failed, so this may be out of date: {}</div></section>"#,
            esc(&s.error)
        );
    }
    let checked = (s.checked > 0).then_some(s.checked * 1000);
    let _ = write!(
        body,
        r#"<section><h2>Installing</h2><div class="sub">Recommended: point <a href="https://github.com/ImranR98/Obtainium">Obtainium</a> at <code>https://github.com/{}</code>. Apps installed from a browser download get usage access greyed out on Android 13+; then use Settings → Apps → time → ⋮ → Allow restricted settings. Last checked for a new release: {}.</div></section>"#,
        esc(repo),
        clock_time(tz, checked),
    );
    page("Time · App", "/app/", None, &body)
}

/// The page script that draws the charts from the page's JSON.
pub const TIME_JS: &str = include_str!("../assets/time.js");
/// ECharts 5.6.0 (Apache-2.0), served from here so the app works offline.
pub const ECHARTS_JS: &[u8] = include_bytes!("../assets/echarts.min.js");

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
