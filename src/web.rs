use anyhow::Result;

use crate::config::ServerConfig;
use crate::db::{Db, Filter, Minute};

/// Categorical slots 1-8 live as `--series-N` custom properties in the CSS
/// below, in both light and dark. Validated as a set: adjacent-pair CVD ΔE 9.1
/// light / 8.4 dark, normal-vision 19.6 / 19.3. Three light slots sit under 3:1
/// on the surface, so the relief rule applies -- every slice carries a direct
/// label and the tables repeat the numbers, so colour is never the only carrier
/// of identity. Slices are emitted in config order, which is what keeps
/// adjacency on the validated pairlist.
///
/// `idle` and `other` are absence-of-activity, not series. Giving them a neutral
/// keeps the eight real hues for things worth distinguishing.
fn slot_for(cfg: &ServerConfig, category: &str) -> Option<usize> {
    if category == "idle" || category == "other" {
        return None;
    }
    cfg.categories
        .iter()
        .filter(|c| c.as_str() != "idle" && c.as_str() != "other")
        .position(|c| c == category)
        .filter(|i| *i < 8)
}

fn css_var(cfg: &ServerConfig, category: &str) -> String {
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

/// Percent-encode enough for a query value.
fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn fmt_hm(minutes: i64) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn pct(n: i64, total: i64) -> f64 {
    if total > 0 {
        n as f64 / total as f64 * 100.0
    } else {
        0.0
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

pub struct Query {
    pub day: i64,
    pub filter: Filter,
}

impl Query {
    /// Build a link that keeps the current view and flips one facet, so
    /// clicking the same slice twice returns to the unfiltered day.
    fn link(&self, key: &str, value: Option<&str>) -> String {
        let mut parts = vec![format!("d={}", self.day)];
        let mut add = |k: &str, v: &Option<String>| {
            if k != key {
                if let Some(v) = v {
                    parts.push(format!("{k}={}", urlenc(v)));
                }
            }
        };
        add("cat", &self.filter.category);
        add("dev", &self.filter.device);
        add("app", &self.filter.app);
        if let Some(v) = value {
            parts.push(format!("{key}={}", urlenc(v)));
        }
        format!("/?{}", parts.join("&"))
    }

    fn toggle(&self, key: &str, current: &Option<String>, value: &str) -> String {
        if current.as_deref() == Some(value) {
            self.link(key, None)
        } else {
            self.link(key, Some(value))
        }
    }
}

/// Donut. Slices are emitted in config order, never sorted by size, so a
/// category keeps its colour and its neighbours from one day to the next --
/// which is also what keeps adjacency on the validated pairlist.
fn donut(cfg: &ServerConfig, q: &Query, totals: &[(String, i64)]) -> String {
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
        let sweep = (frac * std::f64::consts::TAU).min(std::f64::consts::TAU - 0.001);
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

        let selected = q.filter.category.as_deref() == Some(cat.as_str());
        svg.push_str(&format!(
            "<a href=\"{}\"><path d=\"M{x0:.2},{y0:.2} A{r_out},{r_out} 0 {large} 1 {x1:.2},{y1:.2} \
             L{x2:.2},{y2:.2} A{r_in},{r_in} 0 {large} 0 {x3:.2},{y3:.2} Z\" \
             fill=\"{}\" class=\"slice{}\"><title>{} — {} ({:.0}%) · click to drill in</title>\
             </path></a>",
            esc(&q.toggle("cat", &q.filter.category, cat)),
            css_var(cfg, cat),
            if selected { " on" } else { "" },
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

    let centre = match &q.filter.category {
        Some(c) => format!(
            "<text x=\"150\" y=\"140\" class=\"center-top\">{}</text>\
             <text x=\"150\" y=\"162\" class=\"center-sub\">{}</text>",
            fmt_hm(total),
            esc(c)
        ),
        None => format!(
            "<text x=\"150\" y=\"144\" class=\"center-top\">{}</text>\
             <text x=\"150\" y=\"166\" class=\"center-sub\">tracked</text>",
            fmt_hm(total)
        ),
    };

    format!(
        "<svg viewBox=\"0 0 300 300\" role=\"img\" aria-label=\"Time by category\">\
         {svg}{labels}{centre}</svg>"
    )
}

/// Stacked bar per hour of the day. Shows shape -- when work happened, not just
/// how much -- which the donut deliberately throws away.
fn hourly_chart(cfg: &ServerConfig, buckets: &[Vec<(String, i64)>]) -> String {
    let max = buckets
        .iter()
        .map(|b| b.iter().map(|(_, n)| n).sum::<i64>())
        .max()
        .unwrap_or(0)
        .max(1);

    let (w, h, pad) = (720.0_f64, 130.0_f64, 18.0_f64);
    let bw = w / 24.0;
    let mut bars = String::new();
    let mut axis = String::new();

    for (hour, bucket) in buckets.iter().enumerate() {
        let x = hour as f64 * bw;
        let mut y = h;
        let mut ordered: Vec<&(String, i64)> = Vec::new();
        for cat in cfg.categories.iter() {
            if let Some(e) = bucket.iter().find(|(c, _)| c == cat) {
                ordered.push(e);
            }
        }
        for e in bucket {
            if !ordered.iter().any(|o| o.0 == e.0) {
                ordered.push(e);
            }
        }
        for (cat, n) in ordered {
            let bh = (*n as f64 / max as f64) * h;
            if bh <= 0.0 {
                continue;
            }
            y -= bh;
            bars.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{bh:.1}\" fill=\"{}\">\
                 <title>{:02}:00 {} — {}</title></rect>",
                x + 1.0,
                bw - 2.0,
                css_var(cfg, cat),
                hour,
                esc(cat),
                fmt_hm(*n)
            ));
        }
        if hour % 3 == 0 {
            axis.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" class=\"tick\">{:02}</text>",
                x + bw / 2.0,
                h + 13.0,
                hour
            ));
        }
    }

    format!(
        "<svg viewBox=\"0 0 {w} {}\" preserveAspectRatio=\"none\" class=\"hourly\" \
         role=\"img\" aria-label=\"Minutes by hour\">{bars}{axis}</svg>",
        h + pad
    )
}

/// Horizontal bars for a name/minutes list. Used for apps, projects, devices --
/// same shape of question each time, so the same mark.
fn bar_list(
    q: &Query,
    rows: &[(String, i64)],
    total: i64,
    limit: usize,
    key: &str,
    current: &Option<String>,
    colour: &str,
) -> String {
    if rows.is_empty() {
        return "<p class=\"empty\">Nothing recorded.</p>".into();
    }
    let max = rows.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1);
    let mut out = String::from("<div class=\"bars\">");
    for (name, n) in rows.iter().take(limit) {
        let on = current.as_deref() == Some(name.as_str());
        out.push_str(&format!(
            "<a class=\"bar{}\" href=\"{}\" title=\"{} — {} ({:.0}%)\">\
             <span class=\"bar-name\">{}</span>\
             <span class=\"bar-track\"><span class=\"bar-fill\" style=\"width:{:.1}%;background:{colour}\"></span></span>\
             <span class=\"bar-val\">{}</span></a>",
            if on { " on" } else { "" },
            esc(&q.toggle(key, current, name)),
            esc(name),
            fmt_hm(*n),
            pct(*n, total),
            esc(name),
            (*n as f64 / max as f64) * 100.0,
            fmt_hm(*n)
        ));
    }
    if rows.len() > limit {
        out.push_str(&format!(
            "<p class=\"more\">+{} more</p>",
            rows.len() - limit
        ));
    }
    out.push_str("</div>");
    out
}

fn timeline(cfg: &ServerConfig, minutes: &[Minute], multi_device: bool) -> String {
    if minutes.is_empty() {
        return "<p class=\"empty\">Nothing recorded for this view.</p>".into();
    }
    let mut out = String::new();
    let mut i = 0;
    while i < minutes.len() {
        let m = &minutes[i];
        let mut j = i + 1;
        while j < minutes.len()
            && minutes[j].category == m.category
            && minutes[j].project == m.project
            && minutes[j].device == m.device
            && minutes[j].ts - minutes[j - 1].ts <= 120
        {
            j += 1;
        }
        let block = &minutes[i..j];
        let len = block.len() as i64;
        let t = chrono::DateTime::from_timestamp(m.ts, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default();
        let detail = block
            .iter()
            .rev()
            .find_map(|x| x.detail.as_deref())
            .unwrap_or("");
        let keys: u32 = block.iter().map(|x| x.keys).sum();
        let mouse: u32 = block.iter().map(|x| x.mouse).sum();
        let app = block.iter().rev().find_map(|x| x.app()).unwrap_or("");

        out.push_str(&format!(
            "<div class=\"block\"><span class=\"swatch\" style=\"background:{}\"></span>\
             <span class=\"time\">{t}</span><span class=\"dur\">{}</span>\
             <span class=\"cat\">{}{}</span>\
             <span class=\"app\">{}{}</span>\
             <span class=\"io\">{}</span>\
             <span class=\"detail\">{}</span></div>",
            css_var(cfg, &m.category),
            fmt_hm(len),
            esc(&m.category),
            m.project
                .as_deref()
                .map(|p| format!(" · {}", esc(p)))
                .unwrap_or_default(),
            esc(app),
            if multi_device {
                format!(" <span class=\"dev\">{}</span>", esc(&m.device))
            } else {
                String::new()
            },
            if keys + mouse > 0 {
                format!("⌨{keys} 🖱{mouse}")
            } else {
                String::new()
            },
            esc(detail)
        ));
        i = j;
    }
    out
}

pub fn page(cfg: &ServerConfig, db: &Db, q: &Query) -> Result<String> {
    let (from, to) = day_bounds(q.day);
    let f = &q.filter;

    let cats = db.by_category(from, to, f)?;
    let devices = db.by_device(from, to, f)?;
    let apps = db.by_app(from, to, f)?;
    let open = db.open_apps(from, to, f)?;
    let projects = db.by_project(from, to, f)?;
    let minutes = db.range(from, to, f)?;
    let stats = db.stats(from, to, f)?;
    let (buckets, _input) = db.hourly(from, to, f)?;
    let all_devices = db.all_devices()?;
    let total = stats.tracked;

    let heading = match q.day {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        n => format!("{n} days ago"),
    };

    // Active chips make it obvious the numbers are filtered, and give a way out.
    let mut chips = String::new();
    for (key, val) in [
        ("cat", &f.category),
        ("dev", &f.device),
        ("app", &f.app),
    ] {
        if let Some(v) = val {
            chips.push_str(&format!(
                "<a class=\"chip\" href=\"{}\">{}: {} ✕</a>",
                esc(&q.link(key, None)),
                key,
                esc(v)
            ));
        }
    }
    if !f.is_empty_pub() {
        chips.push_str(&format!(
            "<a class=\"chip clear\" href=\"/?d={}\">clear all</a>",
            q.day
        ));
    }

    let mut cat_rows = String::new();
    for (cat, n) in &cats {
        cat_rows.push_str(&format!(
            "<tr{}><td><a href=\"{}\"><span class=\"swatch\" style=\"background:{}\"></span>{}</a></td>\
             <td class=\"num\">{}</td><td class=\"num\">{:.0}%</td></tr>",
            if f.category.as_deref() == Some(cat.as_str()) { " class=\"on\"" } else { "" },
            esc(&q.toggle("cat", &f.category, cat)),
            css_var(cfg, cat),
            esc(cat),
            fmt_hm(*n),
            pct(*n, total)
        ));
    }
    if cat_rows.is_empty() {
        cat_rows = "<tr><td colspan=\"3\" class=\"empty\">No minutes recorded yet.</td></tr>".into();
    }

    let day_links = (0..7)
        .map(|d| {
            let label = match d {
                0 => "Today".to_string(),
                1 => "Yest".to_string(),
                n => format!("-{n}d"),
            };
            format!(
                "<a href=\"/?d={d}\"{}>{label}</a>",
                if d == q.day { " class=\"on\"" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("");

    Ok(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>time — {heading}</title>
<style>
:root {{
  color-scheme: light dark;
  --surface-1: #fcfcfb; --surface-2: #f4f4f2;
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
body {{ margin:0; padding:28px 20px 60px; background:var(--surface-1); color:var(--text-primary);
  font:15px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif; }}
main {{ max-width: 980px; margin: 0 auto; }}
a {{ color: inherit; }}
header {{ display:flex; align-items:baseline; gap:16px; flex-wrap:wrap; margin-bottom:8px; }}
h1 {{ font-size:22px; margin:0; font-weight:650; letter-spacing:-0.01em; }}
nav a {{ color:var(--text-secondary); text-decoration:none; font-size:13px; margin-right:10px; }}
nav a.on {{ color:var(--text-primary); font-weight:600; }}
nav a:hover {{ text-decoration:underline; }}
.chips {{ display:flex; gap:8px; flex-wrap:wrap; margin:10px 0 22px; }}
.chip {{ font-size:12px; padding:3px 9px; border:1px solid var(--rule); border-radius:99px;
  text-decoration:none; color:var(--text-secondary); background:var(--surface-2); }}
.chip:hover {{ color:var(--text-primary); }}
.chip.clear {{ border-style:dashed; }}
.statrow {{ display:flex; gap:28px; flex-wrap:wrap; margin-bottom:24px; }}
.stat b {{ display:block; font-size:21px; font-weight:650; font-variant-numeric:tabular-nums; }}
.stat span {{ font-size:11px; color:var(--text-muted); text-transform:uppercase; letter-spacing:.07em; }}
.top {{ display:flex; gap:34px; flex-wrap:wrap; align-items:center; margin-bottom:30px; }}
svg {{ max-width:100%; }}
.top svg {{ width:300px; height:300px; flex:none; }}
.slice {{ transition: opacity .12s; }}
.slice:hover {{ opacity:.82; }}
.slice.on {{ stroke: var(--text-primary); stroke-width: 2px; }}
.slice-label {{ font-size:12px; font-weight:600; fill:#fff; text-anchor:middle;
  dominant-baseline:middle; paint-order:stroke; stroke:rgba(0,0,0,.35); stroke-width:2.5px;
  pointer-events:none; }}
.center-top {{ font-size:25px; font-weight:650; fill:var(--text-primary); text-anchor:middle; }}
.center-sub {{ font-size:11px; fill:var(--text-muted); text-anchor:middle;
  text-transform:uppercase; letter-spacing:.08em; }}
table {{ border-collapse:collapse; min-width:290px; flex:1; }}
th, td {{ padding:6px 12px 6px 0; text-align:left; border-bottom:1px solid var(--rule); }}
td a {{ text-decoration:none; }}
td a:hover {{ text-decoration:underline; }}
tr.on {{ background:var(--surface-2); }}
th {{ font-size:11px; font-weight:600; color:var(--text-muted);
  text-transform:uppercase; letter-spacing:.07em; }}
.num {{ text-align:right; font-variant-numeric:tabular-nums; }}
.swatch {{ display:inline-block; width:10px; height:10px; border-radius:3px;
  margin-right:8px; vertical-align:baseline; }}
h2 {{ font-size:12px; text-transform:uppercase; letter-spacing:.07em;
  color:var(--text-muted); font-weight:600; margin:30px 0 12px; }}
.grid {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(280px,1fr)); gap:28px; }}
.hourly {{ width:100%; height:148px; }}
.tick {{ font-size:9px; fill:var(--text-muted); text-anchor:middle; }}
.bars {{ display:flex; flex-direction:column; gap:5px; }}
.bar {{ display:grid; grid-template-columns:minmax(80px,150px) 1fr 58px; gap:10px;
  align-items:center; text-decoration:none; font-size:13px; padding:1px 0; }}
.bar:hover .bar-name {{ text-decoration:underline; }}
.bar.on {{ font-weight:650; }}
.bar-name {{ overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }}
.bar-track {{ background:var(--surface-2); border-radius:3px; height:11px; overflow:hidden; }}
.bar-fill {{ display:block; height:100%; border-radius:3px; }}
.bar-val {{ text-align:right; color:var(--text-secondary); font-variant-numeric:tabular-nums;
  font-size:12px; }}
.more {{ font-size:12px; color:var(--text-muted); margin:4px 0 0; }}
.block {{ display:grid;
  grid-template-columns:16px 50px 58px minmax(130px,1.1fr) minmax(90px,.8fr) 74px 1.6fr;
  gap:9px; align-items:baseline; padding:6px 0; border-bottom:1px solid var(--rule);
  font-size:13px; }}
.time {{ color:var(--text-secondary); font-variant-numeric:tabular-nums; }}
.dur, .io {{ color:var(--text-muted); font-variant-numeric:tabular-nums; font-size:12px; }}
.cat {{ font-weight:550; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }}
.app {{ color:var(--text-secondary); overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }}
.dev {{ color:var(--text-muted); font-size:11px; }}
.detail {{ color:var(--text-secondary); overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }}
.empty {{ color:var(--text-muted); font-size:13px; }}
@media (max-width:760px) {{
  .block {{ grid-template-columns:16px 48px 1fr; }}
  .dur, .detail, .io, .app {{ display:none; }}
}}
</style></head><body><main>
<header><h1>{heading}</h1><nav>{day_links}</nav></header>
<div class="chips">{chips}</div>

<div class="statrow">
  <div class="stat"><b>{tracked}</b><span>tracked</span></div>
  <div class="stat"><b>{active}</b><span>with input</span></div>
  <div class="stat"><b>{idle}</b><span>idle</span></div>
  <div class="stat"><b>{keys}</b><span>keys</span></div>
  <div class="stat"><b>{mouse}</b><span>pointer events</span></div>
  <div class="stat"><b>{ndev}</b><span>device{devs}</span></div>
  <div class="stat"><b>{classified}</b><span>model calls</span></div>
</div>

<div class="top">{donut}
<table><thead><tr><th>Category</th><th class="num">Time</th><th class="num">Share</th></tr></thead>
<tbody>{cat_rows}</tbody></table></div>

<h2>By hour</h2>{hourly}

<div class="grid">
  <div><h2>Apps in focus</h2>{app_bars}</div>
  <div><h2>Apps open</h2>{open_bars}</div>
  <div><h2>Projects</h2>{proj_bars}</div>
  <div><h2>Devices{known}</h2>{dev_bars}</div>
</div>

<h2>Timeline</h2>{timeline}
</main></body></html>"#,
        donut = donut(cfg, q, &cats),
        hourly = hourly_chart(cfg, &buckets),
        app_bars = bar_list(q, &apps, total, 12, "app", &f.app, "var(--series-1)"),
        open_bars = bar_list(q, &open, total, 12, "app", &f.app, "var(--series-3)"),
        proj_bars = bar_list(q, &projects, total, 10, "cat", &None, "var(--series-7)"),
        dev_bars = bar_list(q, &devices, total, 10, "dev", &f.device, "var(--series-2)"),
        timeline = timeline(cfg, &minutes, all_devices.len() > 1),
        tracked = fmt_hm(stats.tracked),
        active = fmt_hm(stats.active),
        idle = fmt_hm(stats.idle),
        keys = stats.keys,
        mouse = stats.mouse,
        ndev = stats.devices,
        devs = if stats.devices == 1 { "" } else { "s" },
        classified = stats.classified,
        known = if all_devices.len() > 1 {
            String::new()
        } else {
            format!(" ({} known)", all_devices.len())
        },
    ))
}
