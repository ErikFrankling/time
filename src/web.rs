use anyhow::Result;
use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;

/// Categorical slots 1-8, light and dark. Validated as a set: adjacent-pair CVD
/// ΔE 9.1 light / 8.4 dark, normal-vision 19.6 / 19.3. Three light slots sit
/// under 3:1 on the surface, so the relief rule applies -- every slice carries a
/// direct label and the table below repeats the numbers, meaning colour is never
/// the only carrier of identity.
const SERIES_LIGHT: [&str; 8] = [
    "#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#4a3aa7", "#e34948",
];
const SERIES_DARK: [&str; 8] = [
    "#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767",
];

/// `idle` and `other` are absence-of-activity, not series. Giving them a neutral
/// keeps the eight real hues for things worth distinguishing.
fn slot_for(cfg: &Config, category: &str) -> Option<usize> {
    if category == "idle" || category == "other" {
        return None;
    }
    cfg.categories
        .iter()
        .filter(|c| c.as_str() != "idle" && c.as_str() != "other")
        .position(|c| c == category)
        .filter(|i| *i < 8)
}

fn css_var(cfg: &Config, category: &str) -> String {
    match slot_for(cfg, category) {
        Some(i) => format!("var(--series-{})", i + 1),
        None if category == "idle" => "var(--neutral-idle)".into(),
        None => "var(--neutral-other)".into(),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn fmt_hm(minutes: i64) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Local midnight for a day offset (0 = today), as a unix-second range.
fn day_bounds(offset: i64) -> (i64, i64) {
    use chrono::{Duration, Local, TimeZone};
    let day = Local::now().date_naive() - Duration::days(offset);
    let start = Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map(|d| d.timestamp())
        .unwrap_or(0);
    (start, start + 86_400)
}

/// Donut. Slices are emitted in config order, never sorted by size, so a
/// category keeps its colour and its neighbours from one day to the next --
/// which is also what keeps adjacency on the validated pairlist.
fn donut(cfg: &Config, totals: &[(String, i64)]) -> String {
    let total: i64 = totals.iter().map(|(_, n)| n).sum();
    if total == 0 {
        return String::new();
    }

    let mut ordered: Vec<&(String, i64)> = Vec::new();
    for cat in cfg.categories.iter() {
        if let Some(t) = totals.iter().find(|(c, _)| c == cat) {
            ordered.push(t);
        }
    }
    for t in totals {
        if !ordered.iter().any(|o| o.0 == t.0) {
            ordered.push(t);
        }
    }

    let (cx, cy, r_out, r_in) = (150.0_f64, 150.0_f64, 118.0_f64, 74.0_f64);
    let mut svg = String::new();
    let mut angle = -std::f64::consts::FRAC_PI_2;
    let mut labels = String::new();

    for (cat, n) in ordered {
        let frac = *n as f64 / total as f64;
        let sweep = frac * std::f64::consts::TAU;
        // A single arc whose start and end coincide is degenerate and renders
        // as nothing, so a lone 100% category would show an empty donut. Clamp
        // the sweep just short of a full turn.
        let sweep = sweep.min(std::f64::consts::TAU - 0.001);
        let gap = if frac > 0.995 { 0.0 } else { 0.016 };
        let (a0, a1) = (angle + gap / 2.0, angle + sweep - gap / 2.0);
        angle += sweep;
        if a1 <= a0 {
            continue;
        }

        let large = if a1 - a0 > std::f64::consts::PI { 1 } else { 0 };
        let p = |r: f64, a: f64| (cx + r * a.cos(), cy + r * a.sin());
        let (x0, y0) = p(r_out, a0);
        let (x1, y1) = p(r_out, a1);
        let (x2, y2) = p(r_in, a1);
        let (x3, y3) = p(r_in, a0);

        svg.push_str(&format!(
            "<path d=\"M{x0:.2},{y0:.2} A{r_out},{r_out} 0 {large} 1 {x1:.2},{y1:.2} \
             L{x2:.2},{y2:.2} A{r_in},{r_in} 0 {large} 0 {x3:.2},{y3:.2} Z\" \
             fill=\"{}\"><title>{} — {} ({:.0}%)</title></path>",
            css_var(cfg, cat),
            esc(cat),
            fmt_hm(*n),
            frac * 100.0
        ));

        // Direct labels are the relief for the light-mode contrast warning, so
        // they are not optional decoration. Below 7% they collide, and the
        // table carries those rows instead.
        if frac >= 0.07 {
            let mid = (a0 + a1) / 2.0;
            let (lx, ly) = p((r_out + r_in) / 2.0, mid);
            labels.push_str(&format!(
                "<text x=\"{lx:.1}\" y=\"{ly:.1}\" class=\"slice-label\">{:.0}%</text>",
                frac * 100.0
            ));
        }
    }

    format!(
        "<svg viewBox=\"0 0 300 300\" role=\"img\" aria-label=\"Time by category\">\
         {svg}{labels}\
         <text x=\"150\" y=\"144\" class=\"center-top\">{}</text>\
         <text x=\"150\" y=\"166\" class=\"center-sub\">tracked</text></svg>",
        fmt_hm(total)
    )
}

fn page(cfg: &Config, db: &Db, offset: i64) -> Result<String> {
    let (from, to) = day_bounds(offset);
    let totals = db.totals(from, to)?;
    let minutes = db.range(from, to)?;
    let total: i64 = totals.iter().map(|(_, n)| n).sum();

    let heading = match offset {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        n => format!("{n} days ago"),
    };

    let mut rows = String::new();
    for (cat, n) in &totals {
        rows.push_str(&format!(
            "<tr><td><span class=\"swatch\" style=\"background:{}\"></span>{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{:.0}%</td></tr>",
            css_var(cfg, cat),
            esc(cat),
            fmt_hm(*n),
            if total > 0 {
                *n as f64 / total as f64 * 100.0
            } else {
                0.0
            }
        ));
    }
    if rows.is_empty() {
        rows = "<tr><td colspan=\"3\" class=\"empty\">No minutes recorded yet.</td></tr>".into();
    }

    // Collapse consecutive minutes sharing a category+project into one row --
    // 600 individual rows is unreadable, and the interesting unit is the block.
    let mut blocks = String::new();
    let mut i = 0;
    while i < minutes.len() {
        let m = &minutes[i];
        let mut j = i + 1;
        while j < minutes.len()
            && minutes[j].category == m.category
            && minutes[j].project == m.project
            && minutes[j].ts - minutes[j - 1].ts <= 120
        {
            j += 1;
        }
        let len = (j - i) as i64;
        let t = chrono::DateTime::from_timestamp(m.ts, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default();
        let detail = minutes[i..j]
            .iter()
            .rev()
            .find_map(|x| x.detail.as_deref())
            .unwrap_or("");
        blocks.push_str(&format!(
            "<div class=\"block\"><span class=\"swatch\" style=\"background:{}\"></span>\
             <span class=\"time\">{t}</span><span class=\"dur\">{}</span>\
             <span class=\"cat\">{}{}</span><span class=\"detail\">{}</span></div>",
            css_var(cfg, &m.category),
            fmt_hm(len),
            esc(&m.category),
            m.project
                .as_deref()
                .map(|p| format!(" · {}", esc(p)))
                .unwrap_or_default(),
            esc(detail)
        ));
        i = j;
    }
    if blocks.is_empty() {
        blocks = "<p class=\"empty\">Nothing recorded for this day.</p>".into();
    }

    Ok(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>time — {heading}</title>
<style>
:root {{
  color-scheme: light dark;
  --surface-1: #fcfcfb; --surface-2: #f2f2f0;
  --text-primary: #0b0b0b; --text-secondary: #52514e; --text-muted: #78776f;
  --rule: #e2e2dd;
  --series-1:#2a78d6; --series-2:#eb6834; --series-3:#1baf7a; --series-4:#eda100;
  --series-5:#e87ba4; --series-6:#008300; --series-7:#4a3aa7; --series-8:#e34948;
  --neutral-idle: #b9b8b0; --neutral-other: #86857d;
}}
@media (prefers-color-scheme: dark) {{
  :root:where(:not([data-theme="light"])) {{
    --surface-1:#1a1a19; --surface-2:#242422;
    --text-primary:#fff; --text-secondary:#c3c2b7; --text-muted:#8f8e85;
    --rule:#33332f;
    --series-1:#3987e5; --series-2:#d95926; --series-3:#199e70; --series-4:#c98500;
    --series-5:#d55181; --series-6:#008300; --series-7:#9085e9; --series-8:#e66767;
    --neutral-idle:#4d4c47; --neutral-other:#6d6c64;
  }}
}}
* {{ box-sizing: border-box; }}
body {{ margin:0; padding:32px 20px; background:var(--surface-1); color:var(--text-primary);
  font:15px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif; }}
main {{ max-width: 840px; margin: 0 auto; }}
header {{ display:flex; align-items:baseline; gap:16px; margin-bottom:28px; }}
h1 {{ font-size:22px; margin:0; font-weight:650; letter-spacing:-0.01em; }}
nav a {{ color:var(--text-secondary); text-decoration:none; font-size:14px; margin-right:12px; }}
nav a:hover {{ color:var(--text-primary); text-decoration:underline; }}
.top {{ display:flex; gap:36px; flex-wrap:wrap; align-items:center; margin-bottom:36px; }}
svg {{ width:300px; height:300px; flex:none; }}
.slice-label {{ font-size:12px; font-weight:600; fill:#fff; text-anchor:middle;
  dominant-baseline:middle; paint-order:stroke; stroke:rgba(0,0,0,.35); stroke-width:2.5px; }}
.center-top {{ font-size:26px; font-weight:650; fill:var(--text-primary); text-anchor:middle; }}
.center-sub {{ font-size:12px; fill:var(--text-muted); text-anchor:middle;
  text-transform:uppercase; letter-spacing:.08em; }}
table {{ border-collapse:collapse; min-width:300px; flex:1; }}
th, td {{ padding:7px 12px 7px 0; text-align:left; border-bottom:1px solid var(--rule); }}
th {{ font-size:11px; font-weight:600; color:var(--text-muted);
  text-transform:uppercase; letter-spacing:.07em; }}
.num {{ text-align:right; font-variant-numeric:tabular-nums; }}
.swatch {{ display:inline-block; width:10px; height:10px; border-radius:3px;
  margin-right:8px; vertical-align:baseline; }}
h2 {{ font-size:13px; text-transform:uppercase; letter-spacing:.07em;
  color:var(--text-muted); font-weight:600; margin:0 0 12px; }}
.block {{ display:grid; grid-template-columns:18px 52px 62px minmax(140px,auto) 1fr;
  gap:10px; align-items:baseline; padding:7px 0; border-bottom:1px solid var(--rule);
  font-size:14px; }}
.time {{ color:var(--text-secondary); font-variant-numeric:tabular-nums; }}
.dur {{ color:var(--text-muted); font-variant-numeric:tabular-nums; font-size:13px; }}
.cat {{ font-weight:550; }}
.detail {{ color:var(--text-secondary); overflow:hidden; text-overflow:ellipsis;
  white-space:nowrap; }}
.empty {{ color:var(--text-muted); }}
@media (max-width:640px) {{
  .block {{ grid-template-columns:18px 52px 1fr; }}
  .dur, .detail {{ display:none; }}
}}
</style></head><body><main>
<header><h1>{heading}</h1><nav>
<a href="/?d=0">Today</a><a href="/?d=1">Yesterday</a><a href="/?d=2">2d</a><a href="/?d=7">7d</a>
</nav></header>
<div class="top">{}
<table><thead><tr><th>Category</th><th class="num">Time</th><th class="num">Share</th></tr></thead>
<tbody>{rows}</tbody></table></div>
<h2>Timeline</h2>{blocks}
</main></body></html>"#,
        donut(cfg, &totals)
    ))
}

pub fn serve(cfg: Arc<Config>) -> Result<()> {
    let addr = format!("127.0.0.1:{}", cfg.port);
    let server = tiny_http::Server::http(&addr)
        .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
    println!("ui: http://{addr}");

    for req in server.incoming_requests() {
        let offset = req
            .url()
            .split_once("d=")
            .and_then(|(_, v)| v.split('&').next())
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            .clamp(0, 3650);

        let body = Db::open()
            .and_then(|db| page(&cfg, &db, offset))
            .unwrap_or_else(|e| format!("<pre>error: {}</pre>", esc(&e.to_string())));

        let header = "Content-Type: text/html; charset=utf-8".parse().unwrap();
        let _ = req.respond(
            tiny_http::Response::from_string(body).with_header::<tiny_http::Header>(header),
        );
    }
    Ok(())
}
