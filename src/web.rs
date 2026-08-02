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
///
/// The values live here as data and are interpolated into the CSS below, rather
/// than being written out twice: the PDF report has no stylesheet to inherit
/// from and must resolve the same category to the same hue, and two hand-kept
/// copies of a validated palette would drift the moment one is edited.
pub const SERIES_LIGHT: [&str; 8] = [
    "#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#4a3aa7", "#e34948",
];
pub const SERIES_DARK: [&str; 8] = [
    "#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767",
];
pub const IDLE_LIGHT: &str = "#b9b8b0";
pub const OTHER_LIGHT: &str = "#86857d";
pub const IDLE_DARK: &str = "#4d4c47";
pub const OTHER_DARK: &str = "#6d6c64";

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

/// The same assignment as `css_var`, resolved to a literal light-mode hex.
/// Paper is white, so the report always takes the light values.
pub fn hex(cfg: &ServerConfig, category: &str) -> &'static str {
    match slot_for(cfg, category) {
        Some(i) => SERIES_LIGHT[i],
        None if category == "idle" => IDLE_LIGHT,
        None => OTHER_LIGHT,
    }
}

/// Config order, with anything unknown to the config appended. Emitting slices
/// this way is what keeps a category's colour and its neighbours stable from
/// one day to the next, and so keeps adjacency on the validated pairlist.
pub fn in_config_order<'a, T>(
    cfg: &ServerConfig,
    items: &'a [T],
    name: impl Fn(&T) -> &str,
) -> Vec<&'a T> {
    let mut out: Vec<&T> = Vec::new();
    for cat in cfg.categories.iter() {
        if let Some(t) = items.iter().find(|t| name(t) == cat) {
            out.push(t);
        }
    }
    for t in items {
        if !out.iter().any(|o| name(o) == name(t)) {
            out.push(t);
        }
    }
    out
}

/// One annular sector as an SVG path. Shared so the web charts and the report
/// cannot disagree about geometry.
pub fn arc_path(cx: f64, cy: f64, r0: f64, r1: f64, a0: f64, a1: f64) -> String {
    let large = if a1 - a0 > std::f64::consts::PI { 1 } else { 0 };
    let p = |r: f64, a: f64| (cx + r * a.cos(), cy + r * a.sin());
    let (x0, y0) = p(r1, a0);
    let (x1, y1) = p(r1, a1);
    let (x2, y2) = p(r0, a1);
    let (x3, y3) = p(r0, a0);
    format!(
        "M{x0:.2},{y0:.2} A{r1},{r1} 0 {large} 1 {x1:.2},{y1:.2} \
         L{x2:.2},{y2:.2} A{r0},{r0} 0 {large} 0 {x3:.2},{y3:.2} Z"
    )
}

pub fn esc(s: &str) -> String {
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

pub fn fmt_hm(minutes: i64) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

pub fn pct(n: i64, total: i64) -> f64 {
    if total > 0 {
        n as f64 / total as f64 * 100.0
    } else {
        0.0
    }
}

/// Local midnight for a day offset (0 = today), as a unix-second range.
pub fn day_bounds(offset: i64) -> (i64, i64) {
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
        add("dom", &self.filter.domain);
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

/// Two-ring sunburst: inner ring is what you were mainly doing, outer ring is
/// what was going on alongside it. Coding for 5h reads as one inner slice; the
/// 3h of it spent with YouTube on is an outer segment nested inside that slice.
///
/// The outer ring is drawn in the *companion's* colour, so YouTube-while-coding
/// is visibly YouTube. Undivided time keeps the parent colour, dimmed, so a
/// solid-looking ring means focus and a stripey one means distraction.
fn sunburst(
    cfg: &ServerConfig,
    q: &Query,
    layers: &[(String, i64, Vec<(Vec<String>, i64)>)],
) -> String {
    let total: i64 = layers.iter().map(|(_, n, _)| n).sum();
    if total == 0 {
        return String::new();
    }

    let ordered = in_config_order(cfg, layers, |l| l.0.as_str());

    let (cx, cy) = (150.0_f64, 150.0_f64);
    let (r_in, r_mid, r_gap, r_out) = (62.0_f64, 104.0_f64, 107.0_f64, 132.0_f64);
    let mut inner = String::new();
    let mut outer = String::new();
    let mut labels = String::new();
    let mut angle = -std::f64::consts::FRAC_PI_2;

    let arc = |r0: f64, r1: f64, a0: f64, a1: f64| arc_path(cx, cy, r0, r1, a0, a1);

    for (cat, n, segs) in ordered {
        let frac = *n as f64 / total as f64;
        let sweep = (frac * std::f64::consts::TAU).min(std::f64::consts::TAU - 0.001);
        let gap = if frac > 0.995 { 0.0 } else { 0.016 };
        let (a0, a1) = (angle + gap / 2.0, angle + sweep - gap / 2.0);
        let cat_start = angle;
        angle += sweep;
        if a1 <= a0 {
            continue;
        }

        let selected = q.filter.category.as_deref() == Some(cat.as_str());
        inner.push_str(&format!(
            "<a href=\"{}\"><path d=\"{}\" fill=\"{}\" class=\"slice{}\">\
             <title>{} — {} ({:.0}%) · click to drill in</title></path></a>",
            esc(&q.toggle("cat", &q.filter.category, cat)),
            arc(r_in, r_mid, a0, a1),
            css_var(cfg, cat),
            if selected { " on" } else { "" },
            esc(cat),
            fmt_hm(*n),
            frac * 100.0
        ));

        // Outer segments subdivide this slice only, so they always sum to it.
        let mut sub = cat_start;
        for (with, sn) in segs {
            let sfrac = *sn as f64 / total as f64;
            let ssweep = (sfrac * std::f64::consts::TAU).min(std::f64::consts::TAU - 0.001);
            let (b0, b1) = (sub + gap / 2.0, sub + ssweep - gap / 2.0);
            sub += ssweep;
            if b1 <= b0 {
                continue;
            }
            let (fill, label, op) = if with.is_empty() {
                (css_var(cfg, cat), "undivided".to_string(), "0.28")
            } else {
                (
                    css_var(cfg, &with[0]),
                    with.join(" + "),
                    "1",
                )
            };
            outer.push_str(&format!(
                "<path d=\"{}\" fill=\"{fill}\" fill-opacity=\"{op}\">\
                 <title>{} while {} — {} ({:.0}% of {})</title></path>",
                arc(r_gap, r_out, b0, b1),
                esc(cat),
                esc(&label),
                fmt_hm(*sn),
                *sn as f64 / *n as f64 * 100.0,
                esc(cat)
            ));
        }

        if frac >= 0.08 {
            let mid = (a0 + a1) / 2.0;
            let (lx, ly) = (
                cx + (r_in + r_mid) / 2.0 * mid.cos(),
                cy + (r_in + r_mid) / 2.0 * mid.sin(),
            );
            labels.push_str(&format!(
                "<text x=\"{lx:.1}\" y=\"{ly:.1}\" class=\"slice-label\">{:.0}%</text>",
                frac * 100.0
            ));
        }
    }

    let centre = match &q.filter.category {
        Some(c) => format!(
            "<text x=\"150\" y=\"144\" class=\"center-top\">{}</text>\
             <text x=\"150\" y=\"164\" class=\"center-sub\">{}</text>",
            fmt_hm(total),
            esc(c)
        ),
        None => format!(
            "<text x=\"150\" y=\"146\" class=\"center-top\">{}</text>\
             <text x=\"150\" y=\"166\" class=\"center-sub\">tracked</text>",
            fmt_hm(total)
        ),
    };

    format!(
        "<svg viewBox=\"0 0 300 300\" role=\"img\" \
         aria-label=\"Time by category, with concurrent activity\">\
         {inner}{outer}{labels}{centre}</svg>"
    )
}

/// Donut. Slices are emitted in config order, never sorted by size, so a
/// category keeps its colour and its neighbours from one day to the next --
/// which is also what keeps adjacency on the validated pairlist.
#[allow(dead_code)]
fn donut(cfg: &ServerConfig, q: &Query, totals: &[(String, i64)]) -> String {
    let total: i64 = totals.iter().map(|(_, n)| n).sum();
    if total == 0 {
        return String::new();
    }

    let ordered = in_config_order(cfg, totals, |t| t.0.as_str());

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

        let p = |r: f64, a: f64| (cx + r * a.cos(), cy + r * a.sin());
        let selected = q.filter.category.as_deref() == Some(cat.as_str());
        svg.push_str(&format!(
            "<a href=\"{}\"><path d=\"{}\" \
             fill=\"{}\" class=\"slice{}\"><title>{} — {} ({:.0}%) · click to drill in</title>\
             </path></a>",
            esc(&q.toggle("cat", &q.filter.category, cat)),
            arc_path(cx, cy, r_in, r_out, a0, a1),
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
        for (cat, n) in in_config_order(cfg, bucket, |e| e.0.as_str()) {
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

/// "Deep" is work and study; everything else is shallow by construction. The
/// report reads focus the same way the page does, so this lives in one place.
pub fn deep_categories(cfg: &ServerConfig) -> Vec<String> {
    cfg.categories
        .iter()
        .filter(|c| c.starts_with("work_") || c.as_str() == "kth")
        .cloned()
        .collect()
}

fn series_css(slots: &[&str; 8]) -> String {
    slots
        .iter()
        .enumerate()
        .map(|(i, c)| format!("--series-{}:{c};", i + 1))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn page(cfg: &ServerConfig, db: &Db, q: &Query) -> Result<String> {
    let (from, to) = day_bounds(q.day);
    let f = &q.filter;

    let cats = db.by_category(from, to, f)?;
    let layers = db.layered(from, to, f)?;
    let devices = db.by_device(from, to, f)?;
    let apps = db.by_app(from, to, f)?;
    let open = db.open_apps(from, to, f)?;
    let domains = db.by_domain(from, to, f)?;
    let projects = db.by_project(from, to, f)?;
    // The timeline shows raw per-device rows: when two machines disagree the
    // summary above resolves it, but the detail below should still say plainly
    // what each machine reported.
    let minutes = db.range(from, to, f)?;
    let stats = db.stats(from, to, f)?;
    let (buckets, _input) = db.hourly(from, to, f)?;

    let deep = deep_categories(cfg);
    let focus = db.focus(from, to, f, &deep)?;
    let strips = db.day_strips(30, 10, f)?;

    // Output is not filterable by category or app -- a commit has no window
    // class -- so these two reads ignore `f` deliberately rather than by
    // omission.
    let day_key = |ts: i64| {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
            .unwrap_or_default()
    };
    let today = day_key(from);
    let code = db.code_between(&today, &today)?;
    let agent_days = db.agent_days(&today, &today)?;
    let agent_minutes = db.agent_minutes(from, to)?;
    let strip_from = strips.first().map(|(s, _)| day_key(*s)).unwrap_or_default();
    let mut code_by_day: std::collections::HashMap<String, (i64, i64)> = Default::default();
    for r in db.code_between(&strip_from, &today)? {
        if r.source == crate::code::SOURCE_GIT {
            let e = code_by_day.entry(r.day).or_default();
            e.0 += r.commits;
            e.1 += r.added + r.removed;
        }
    }

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
        ("dom", &f.domain),
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
  {series_light}
  --neutral-idle: {IDLE_LIGHT}; --neutral-other: {OTHER_LIGHT};
}}
@media (prefers-color-scheme: dark) {{
  :root:where(:not([data-theme="light"])) {{
    --surface-1:#1a1a19; --surface-2:#242422;
    --text-primary:#fff; --text-secondary:#c3c2b7; --text-muted:#8f8e85;
    --rule:#33332f;
    {series_dark}
    --neutral-idle:{IDLE_DARK}; --neutral-other:{OTHER_DARK};
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
.dl {{ margin-left:auto; font-size:13px; color:var(--text-muted); }}
.dl a {{ text-decoration:underline; text-underline-offset:2px; margin-right:0; }}
.dl a + a {{ margin-left:6px; }}
.dl .sep {{ margin:0 4px; opacity:.5; }}
.chips {{ display:flex; gap:8px; flex-wrap:wrap; margin:10px 0 22px; }}
.chip {{ font-size:12px; padding:3px 9px; border:1px solid var(--rule); border-radius:99px;
  text-decoration:none; color:var(--text-secondary); background:var(--surface-2); }}
.chip:hover {{ color:var(--text-primary); }}
.chip.clear {{ border-style:dashed; }}
.statrow {{ display:flex; gap:28px; flex-wrap:wrap; margin-bottom:24px; }}
.stat b {{ display:block; font-size:21px; font-weight:650; font-variant-numeric:tabular-nums; }}
.stat span {{ font-size:11px; color:var(--text-muted); text-transform:uppercase; letter-spacing:.07em; }}
.stat b.add {{ color:var(--series-6); }}
.stat b.del {{ color:var(--series-2); }}
code {{ font-size:.92em; }}
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
.strip {{ width:100%; height:auto; }}
.strip-label {{ font-size:8px; fill:var(--text-muted); }}
.strip-grid {{ stroke:var(--rule); stroke-width:1; }}
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
<header><h1>{heading}</h1><nav>{day_links}</nav>
<nav class="dl">PDF report:
  <a href="/report.pdf?d={d}&amp;range=day">day</a>
  <a href="/report.pdf?d={d}&amp;range=week">week</a>
  <a href="/report.pdf?d={d}&amp;range=month">month</a>
  <span class="sep">·</span>
  <a href="/app/">Android app</a></nav></header>
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

<h2>Focus</h2>{focus_panel}

<h2>Code written</h2>{code_panel}

<h2>Agents</h2>{agent_panel}

<h2>Last 30 days — each row is one day, midnight to midnight, with commits on the right</h2>{strip}

<h2>By hour</h2>{hourly}

<div class="grid">
  <div><h2>Apps in focus</h2>{app_bars}</div>
  <div><h2>Top domains</h2>{domain_bars}</div>
  <div><h2>Apps open</h2>{open_bars}</div>
  <div><h2>Projects</h2>{proj_bars}</div>
  <div><h2>Devices{known}</h2>{dev_bars}</div>
</div>

<h2>Timeline</h2>{timeline}
</main></body></html>"#,
        d = q.day,
        series_light = series_css(&SERIES_LIGHT),
        series_dark = series_css(&SERIES_DARK),
        donut = sunburst(cfg, q, &layers),
        hourly = hourly_chart(cfg, &buckets),
        focus_panel = focus_panel(&focus),
        strip = day_strip(cfg, &strips, 10, &code_by_day),
        code_panel = code_panel(&code),
        agent_panel = agent_panel(&agent_days, &agent_minutes),
        app_bars = bar_list(q, &apps, total, 12, "app", &f.app, "var(--series-1)"),
        domain_bars = bar_list(q, &domains, total, 12, "dom", &f.domain, "var(--series-4)"),
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

/// One row per day, midnight to midnight, coloured by category.
///
/// This is the form the whole field converged on independently -- it is what
/// nearly every long-run tracker ends up drawing, because totals hide the thing
/// that actually matters. A day that reads "4h work" in the pie can be one
/// solid block or sixty fragments, and only this shows which.
///
/// A gutter on the right carries the day's committed output on the same row, so
/// "eight hours of work_husk" and "nothing shipped" line up on one line instead
/// of living in two panels that nobody compares.
fn day_strip(
    cfg: &ServerConfig,
    rows: &[(i64, Vec<Option<String>>)],
    bucket_mins: i64,
    code: &std::collections::HashMap<String, (i64, i64)>,
) -> String {
    if rows.is_empty() {
        return "<p class=\"empty\">No history yet.</p>".into();
    }
    let per_day = rows[0].1.len().max(1);
    let (w, row_h, label_w) = (760.0_f64, 11.0_f64, 42.0_f64);
    // Zero when nothing was ever collected, so the strip keeps its full width
    // rather than reserving space for an empty column.
    let gutter = if code.is_empty() { 0.0 } else { 92.0_f64 };
    let cell_w = (w - label_w - gutter) / per_day as f64;
    // Scaled to the busiest day on screen: the question is which days were
    // productive relative to each other, not how many lines that is in absolute.
    let peak = code.values().map(|(_, l)| *l).max().unwrap_or(1).max(1);
    let mut out = String::new();

    for (i, (start, buckets)) in rows.iter().enumerate() {
        let y = i as f64 * row_h;
        let stamp = chrono::DateTime::from_timestamp(*start, 0).map(|d| d.with_timezone(&chrono::Local));
        let label = stamp
            .map(|d| d.format("%a %d").to_string())
            .unwrap_or_default();

        if gutter > 0.0 {
            let key = stamp.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
            let (commits, lines) = code.get(&key).copied().unwrap_or((0, 0));
            if commits > 0 {
                let bar_w = (lines as f64 / peak as f64) * (gutter - 34.0);
                out.push_str(&format!(
                    "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" \
                     fill=\"var(--series-6)\" rx=\"1\"><title>{} — {commits} commit(s), \
                     {lines} lines changed</title></rect>\
                     <text x=\"{:.1}\" y=\"{:.1}\" class=\"strip-label\">{commits}</text>",
                    w - gutter + 4.0,
                    y + 1.5,
                    bar_w.max(1.0),
                    row_h - 4.0,
                    esc(&label),
                    w - 26.0,
                    y + row_h - 2.0
                ));
            }
        }

        out.push_str(&format!(
            "<text x=\"0\" y=\"{:.1}\" class=\"strip-label\">{}</text>",
            y + row_h - 2.0,
            esc(&label)
        ));
        for (b, cat) in buckets.iter().enumerate() {
            let Some(cat) = cat else { continue };
            let mins = b as i64 * bucket_mins;
            out.push_str(&format!(
                "<rect x=\"{:.2}\" y=\"{y:.1}\" width=\"{:.2}\" height=\"{:.1}\" fill=\"{}\">\
                 <title>{} {:02}:{:02} — {}</title></rect>",
                label_w + b as f64 * cell_w,
                cell_w + 0.4,
                row_h - 1.0,
                css_var(cfg, cat),
                esc(&label),
                mins / 60,
                mins % 60,
                esc(cat)
            ));
        }
    }

    // Hour ruler along the bottom, so a row position reads as a time of day.
    let base = rows.len() as f64 * row_h;
    let mut ticks = String::new();
    for h in (0..=24).step_by(3) {
        let x = label_w + (h as f64 / 24.0) * (w - label_w - gutter);
        ticks.push_str(&format!(
            "<line x1=\"{x:.1}\" y1=\"0\" x2=\"{x:.1}\" y2=\"{base:.1}\" class=\"strip-grid\"/>\
             <text x=\"{x:.1}\" y=\"{:.1}\" class=\"tick\">{h:02}</text>",
            base + 11.0
        ));
    }

    format!(
        "<svg viewBox=\"0 0 {w} {:.1}\" class=\"strip\" role=\"img\" \
         aria-label=\"Each row is one day, midnight to midnight\">{ticks}{out}</svg>",
        base + 16.0
    )
}

/// What got produced, next to how the time went.
///
/// Deliberately two clusters rather than one total. Commits and lines come from
/// the local clones, which is the only place diffs and private repositories
/// exist; pull requests, issues and reviews come from GitHub, which is the only
/// place *those* exist. Adding the two commit counts together would be double
/// counting, so the panel never does.
fn code_panel(rows: &[crate::code::CodeDay]) -> String {
    let sum = |src: &str, f: fn(&crate::code::CodeDay) -> i64| -> i64 {
        rows.iter().filter(|r| r.source == src).map(f).sum()
    };
    use crate::code::{SOURCE_GIT as G, SOURCE_GITHUB as H};

    let commits = sum(G, |r| r.commits);
    let added = sum(G, |r| r.added);
    let removed = sum(G, |r| r.removed);
    let files = sum(G, |r| r.files);
    let collaboration = [
        ("PRs opened", sum(H, |r| r.prs_opened)),
        ("PRs merged", sum(H, |r| r.prs_merged)),
        ("issues opened", sum(H, |r| r.issues_opened)),
        ("issues closed", sum(H, |r| r.issues_closed)),
        ("reviews", sum(H, |r| r.reviews)),
    ];

    if commits == 0 && collaboration.iter().all(|(_, n)| *n == 0) {
        return "<p class=\"empty\">No commits recorded for this day. \
                Run <code>time collect</code>.</p>"
            .into();
    }

    let mut stats = format!(
        "<div class=\"statrow\">\
         <div class=\"stat\"><b>{commits}</b><span>commits</span></div>\
         <div class=\"stat\"><b class=\"add\">+{added}</b><span>lines added</span></div>\
         <div class=\"stat\"><b class=\"del\">−{removed}</b><span>lines removed</span></div>\
         <div class=\"stat\"><b>{files}</b><span>files touched</span></div>"
    );
    for (label, n) in collaboration {
        // A zero review count is a fact worth seeing; a zero next to four other
        // zeros is noise. Only show the ones that happened.
        if n > 0 {
            stats.push_str(&format!(
                "<div class=\"stat\"><b>{n}</b><span>{label}</span></div>"
            ));
        }
    }
    stats.push_str("</div>");

    // Per repository, ranked by churn rather than commits: one commit that
    // rewrites a module is more of a day's work than nine that fix typos.
    let mut per_repo: Vec<(String, i64, i64)> = Vec::new();
    for r in rows.iter().filter(|r| r.source == G) {
        match per_repo.iter_mut().find(|(n, _, _)| *n == r.repo) {
            Some(e) => {
                e.1 += r.commits;
                e.2 += r.added + r.removed;
            }
            None => per_repo.push((r.repo.clone(), r.commits, r.added + r.removed)),
        }
    }
    per_repo.sort_by_key(|r| std::cmp::Reverse(r.2));
    let max = per_repo.iter().map(|(_, _, l)| *l).max().unwrap_or(1).max(1);

    let mut bars = String::from("<div class=\"bars\">");
    for (name, c, lines) in per_repo.iter().take(10) {
        bars.push_str(&format!(
            "<div class=\"bar\" title=\"{} — {c} commit(s), {lines} lines changed\">\
             <span class=\"bar-name\">{}</span>\
             <span class=\"bar-track\"><span class=\"bar-fill\" \
             style=\"width:{:.1}%;background:var(--series-6)\"></span></span>\
             <span class=\"bar-val\">{lines}</span></div>",
            esc(name),
            esc(name),
            (*lines as f64 / max as f64) * 100.0
        ));
    }
    bars.push_str("</div>");

    format!("{stats}{bars}")
}

/// Token counts run to eight digits, and eight digits in a stat tile is a
/// number nobody reads. Thousands are already too fine a grain to care about
/// here, so anything below a million rounds away entirely.
fn compact(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// What the agents did, next to what the day looked like.
///
/// The header stat is elapsed time, not summed time: agent activity routinely
/// runs 18 hours in a 24-hour day precisely because several sessions overlap,
/// and adding their minutes up would report a day two or three times longer
/// than one exists. Distinct minutes answers "how much of the day was there an
/// agent working", which is the question; peak concurrency answers "how many at
/// once", which is the other one.
///
/// Tools are never summed into one token figure. claude bills cache reads
/// separately, codex folds them into the input count, opencode maintains its
/// own columns -- three numbers that do not mean the same thing, so they get
/// three rows.
fn agent_panel(days: &[crate::agents::AgentDay], minutes: &[crate::agents::AgentMinute]) -> String {
    if days.is_empty() && minutes.is_empty() {
        return "<p class=\"empty\">No agent activity recorded for this day. \
                Run <code>time agents</code>.</p>"
            .into();
    }

    let mut per_minute: std::collections::HashMap<i64, i64> = Default::default();
    for m in minutes {
        *per_minute.entry(m.ts).or_default() += m.sessions;
    }
    let active = per_minute.len() as i64;
    let peak = per_minute.values().copied().max().unwrap_or(0);

    let sessions: i64 = days.iter().map(|d| d.sessions).sum();
    let prompts: i64 = days.iter().map(|d| d.prompts).sum();
    let tokens_out: i64 = days.iter().map(|d| d.tokens_out).sum();
    // Typed characters, not tokens: this is a measure of how much of the work
    // was described by hand, and characters are what was actually typed.
    let typed: i64 = days.iter().map(|d| d.prompt_chars).sum();

    let stats = format!(
        "<div class=\"statrow\">\
         <div class=\"stat\"><b>{}</b><span>agents active</span></div>\
         <div class=\"stat\"><b>{peak}</b><span>peak at once</span></div>\
         <div class=\"stat\"><b>{sessions}</b><span>sessions</span></div>\
         <div class=\"stat\"><b>{prompts}</b><span>prompts</span></div>\
         <div class=\"stat\"><b>{}</b><span>chars typed</span></div>\
         <div class=\"stat\"><b>{}</b><span>output tokens</span></div>\
         </div>",
        fmt_hm(active),
        compact(typed),
        compact(tokens_out)
    );

    // Per tool, because the token columns are not comparable across them.
    let mut per_tool: Vec<(String, i64, i64, i64, i64)> = Vec::new();
    for d in days {
        match per_tool.iter_mut().find(|(t, ..)| *t == d.tool) {
            Some(e) => {
                e.1 += d.sessions;
                e.2 += d.prompts;
                e.3 += d.tokens_out;
                e.4 = e.4.max(d.peak_parallel);
            }
            None => per_tool.push((
                d.tool.clone(),
                d.sessions,
                d.prompts,
                d.tokens_out,
                d.peak_parallel,
            )),
        }
    }
    per_tool.sort_by_key(|t| std::cmp::Reverse(t.2));

    let mut tools = String::from(
        "<table><thead><tr><th>Tool</th><th class=\"num\">Sessions</th>\
         <th class=\"num\">Prompts</th><th class=\"num\">Out</th>\
         <th class=\"num\">Peak</th></tr></thead><tbody>",
    );
    for (tool, s, p, out, pk) in &per_tool {
        tools.push_str(&format!(
            "<tr><td>{}</td><td class=\"num\">{s}</td><td class=\"num\">{p}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{pk}</td></tr>",
            esc(tool),
            compact(*out)
        ));
    }
    tools.push_str("</tbody></table>");

    // Ranked by minutes rather than tokens: which repository pulled the most
    // agent work is a question about attention, and a project can burn tokens
    // on one long run or earn them over a whole afternoon.
    //
    // These minutes are summed across tools, so a project where claude and
    // codex ran together counts that minute twice. That is the intent -- this
    // is agent effort spent on a project, not wall-clock time it occupied.
    let mut per_project: Vec<(String, i64, i64)> = Vec::new();
    for d in days {
        match per_project.iter_mut().find(|(n, _, _)| *n == d.project) {
            Some(e) => {
                e.1 += d.active_minutes;
                e.2 += d.prompts;
            }
            None => per_project.push((d.project.clone(), d.active_minutes, d.prompts)),
        }
    }
    per_project.sort_by_key(|p| std::cmp::Reverse(p.1));
    let max = per_project.iter().map(|(_, m, _)| *m).max().unwrap_or(1).max(1);

    let mut bars = String::from("<div class=\"bars\">");
    for (name, mins, p) in per_project.iter().take(10) {
        bars.push_str(&format!(
            "<div class=\"bar\" title=\"{} — {} of agent work, {p} prompt(s)\">\
             <span class=\"bar-name\">{}</span>\
             <span class=\"bar-track\"><span class=\"bar-fill\" \
             style=\"width:{:.1}%;background:var(--series-7)\"></span></span>\
             <span class=\"bar-val\">{}</span></div>",
            esc(name),
            fmt_hm(*mins),
            esc(name),
            (*mins as f64 / max as f64) * 100.0,
            fmt_hm(*mins)
        ));
    }
    bars.push_str("</div>");

    format!("{stats}<div class=\"grid\">{tools}<div>{bars}</div></div>")
}

fn focus_panel(f: &crate::db::Focus) -> String {
    let ttfd = match f.time_to_first_deep {
        Some(m) => fmt_hm(m),
        None => "—".into(),
    };
    format!(
        "<div class=\"statrow\">\
         <div class=\"stat\"><b>{}</b><span>longest block</span></div>\
         <div class=\"stat\"><b>{}</b><span>blocks ≥25m</span></div>\
         <div class=\"stat\"><b>{}</b><span>blocks ≥50m</span></div>\
         <div class=\"stat\"><b>{ttfd}</b><span>to first deep block</span></div>\
         <div class=\"stat\"><b>{}</b><span>median block</span></div>\
         <div class=\"stat\"><b>{}</b><span>switches</span></div>\
         </div>",
        fmt_hm(f.longest),
        f.blocks_25,
        f.blocks_50,
        fmt_hm(f.median_block),
        f.switches
    )
}
