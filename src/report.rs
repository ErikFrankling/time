//! One-page PDF reports, for showing someone else.
//!
//! Typesetting is Typst's job, not ours. It is already on the binary's PATH
//! (see `runtimeDeps` in flake.nix), it does page layout, hyphenation and
//! typography we would otherwise reimplement badly, and it embeds SVG -- which
//! means the charts stay charts drawn in Rust, next to the ones the web page
//! draws, rather than a second rendering stack. A PDF-writer crate would give
//! us byte-level output and none of the layout, so every column and caption
//! would become manual coordinates.
//!
//! The charts here are deliberately close cousins of the web ones rather than
//! the same functions: the web SVG carries links, tooltips and CSS custom
//! properties, none of which survive a standalone SVG. What is shared is the
//! part that must never diverge -- the palette, the config ordering, the arc
//! geometry, and above all the `db` queries, so a number in the PDF is the same
//! number the page showed.

use anyhow::{Context, Result};
use chrono::{Datelike, Local, TimeZone};

use crate::config::ServerConfig;
use crate::db::{Db, Filter, Focus};
use crate::web::{self, arc_path, day_bounds, esc, fmt_hm, hex, in_config_order, pct};

#[derive(Clone, Copy, PartialEq)]
pub enum Range {
    Day,
    Week,
    Month,
}

impl Range {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("week") => Range::Week,
            Some("month") => Range::Month,
            _ => Range::Day,
        }
    }

    fn days(self) -> i64 {
        match self {
            Range::Day => 1,
            Range::Week => 7,
            Range::Month => 30,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Range::Day => "day",
            Range::Week => "week",
            Range::Month => "month",
        }
    }
}

/// One day's column in the weekly charts.
struct DayCol {
    label: String,
    cats: Vec<(String, i64)>,
    tracked: i64,
    longest_deep: i64,
}

enum Body {
    Day {
        hourly: Vec<Vec<(String, i64)>>,
    },
    Week {
        days: Vec<DayCol>,
    },
    Month {
        strips: Vec<(i64, Vec<Option<String>>)>,
        /// Minutes of non-idle activity per (weekday, hour).
        punch: Vec<Vec<i64>>,
    },
}

/// Everything the templates need, read in one pass. Collected while the
/// database lock is held so that the slow part -- shelling out to Typst -- can
/// happen with the lock released and the other workers free.
pub struct Data {
    range: Range,
    title: String,
    subtitle: String,
    date_slug: String,
    cats: Vec<(String, i64)>,
    apps: Vec<(String, i64)>,
    projects: Vec<(String, i64)>,
    focus: Focus,
    tracked: i64,
    span: i64,
    devices: i64,
    body: Body,
}

impl Data {
    pub fn filename(&self) -> String {
        format!("time-{}-{}.pdf", self.range.slug(), self.date_slug)
    }

    fn minutes_in(&self, category: &str) -> i64 {
        self.cats
            .iter()
            .find(|(c, _)| c == category)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }
}

pub fn collect(cfg: &ServerConfig, db: &Db, range: Range, day: i64, f: &Filter) -> Result<Data> {
    let (from, _) = day_bounds(day + range.days() - 1);
    let (_, to) = day_bounds(day);
    let deep = web::deep_categories(cfg);

    let cats = db.by_category(from, to, f)?;
    let stats = db.stats(from, to, f)?;
    let focus = db.focus(from, to, f, &deep)?;

    let body = match range {
        Range::Day => Body::Day {
            hourly: db.hourly(from, to, f)?.0,
        },
        Range::Week => {
            let mut days = Vec::new();
            for d in (day..day + 7).rev() {
                days.push(day_col(db, f, &deep, d, "%a")?);
            }
            Body::Week { days }
        }
        Range::Month => {
            // `day_strips` counts back from today, so ask for enough rows to
            // reach the anchor day and keep the oldest 30 of them.
            let mut strips = db.day_strips(day + 30, 10, f)?;
            strips.truncate(30);

            let mut punch = vec![vec![0i64; 24]; 7];
            for d in day..day + 30 {
                let (s, e) = day_bounds(d);
                let weekday = Local
                    .timestamp_opt(s, 0)
                    .single()
                    .map(|t| t.weekday().num_days_from_monday() as usize)
                    .unwrap_or(0);
                for (hour, bucket) in db.hourly(s, e, f)?.0.iter().enumerate() {
                    punch[weekday][hour] += bucket
                        .iter()
                        .filter(|(c, _)| c != "idle")
                        .map(|(_, n)| n)
                        .sum::<i64>();
                }
            }
            Body::Month { strips, punch }
        }
    };

    let (title, subtitle, date_slug) = headings(range, from, to);
    Ok(Data {
        range,
        title,
        subtitle,
        date_slug,
        cats,
        apps: db.by_app(from, to, f)?,
        projects: db.by_project(from, to, f)?,
        focus,
        tracked: stats.tracked,
        span: (to - from) / 60,
        devices: stats.devices,
        body,
    })
}

fn day_col(db: &Db, f: &Filter, deep: &[String], day: i64, fmt: &str) -> Result<DayCol> {
    let (s, e) = day_bounds(day);
    let cats = db.by_category(s, e, f)?;
    Ok(DayCol {
        label: Local
            .timestamp_opt(s, 0)
            .single()
            .map(|t| t.format(fmt).to_string())
            .unwrap_or_default(),
        tracked: cats.iter().map(|(_, n)| n).sum(),
        longest_deep: db.focus(s, e, f, deep)?.longest,
        cats,
    })
}

fn headings(range: Range, from: i64, to: i64) -> (String, String, String) {
    let start = Local.timestamp_opt(from, 0).single();
    let end = Local.timestamp_opt(to - 60, 0).single();
    let slug = end
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    let long = |d: Option<chrono::DateTime<Local>>, f: &str| {
        d.map(|d| d.format(f).to_string()).unwrap_or_default()
    };
    match range {
        Range::Day => (
            long(start, "%A %-d %B %Y"),
            "A day of screen time, recorded minute by minute.".into(),
            slug,
        ),
        Range::Week => (
            format!("{} – {}", long(start, "%-d %B"), long(end, "%-d %B %Y")),
            "Seven days of screen time, recorded minute by minute.".into(),
            slug,
        ),
        Range::Month => (
            format!("{} – {}", long(start, "%-d %B"), long(end, "%-d %B %Y")),
            "Thirty days of screen time, recorded minute by minute.".into(),
            slug,
        ),
    }
}

/// Typeset the collected data. Does no database work, so the caller can and
/// should drop the lock before getting here.
pub fn render(cfg: &ServerConfig, d: &Data) -> Result<Vec<u8>> {
    let dir = scratch_dir()?;
    let out = dir.join("report.pdf");
    std::fs::write(dir.join("report.typ"), template(cfg, d, &dir)?)
        .context("writing report source")?;

    let status = std::process::Command::new("typst")
        .arg("compile")
        // A report is a document, not a build artifact: it should carry the
        // date it was made. Inherited from a Nix shell, SOURCE_DATE_EPOCH would
        // stamp every one of them 1980.
        .env_remove("SOURCE_DATE_EPOCH")
        .arg("--root")
        .arg(&dir)
        .arg(dir.join("report.typ"))
        .arg(&out)
        .output()
        .context("running typst (is it on PATH?)")?;
    if !status.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        anyhow::bail!("typst: {}", String::from_utf8_lossy(&status.stderr));
    }

    let pdf = std::fs::read(&out).context("reading compiled report")?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(pdf)
}

/// A private directory per request: several workers can be typesetting at once
/// and they must not overwrite each other's charts.
fn scratch_dir() -> Result<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "time-report-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).context("creating report scratch dir")?;
    Ok(dir)
}

// ---------------------------------------------------------------- typst

const INK: &str = "#0b0b0b";
const MUTED: &str = "#78776f";
const RULE: &str = "#e2e2dd";
const TRACK: &str = "#f0f0ed";

/// A Typst string literal. Category, app and project names come from the model
/// and the window title, so they must never be read as Typst markup.
fn tq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn svg_file(dir: &std::path::Path, name: &str, svg: &str) -> Result<String> {
    std::fs::write(dir.join(name), svg).with_context(|| format!("writing {name}"))?;
    Ok(format!("#image(\"{name}\", width: 100%)"))
}

fn template(cfg: &ServerConfig, d: &Data, dir: &std::path::Path) -> Result<String> {
    let mut t = String::new();
    t.push_str(&format!(
        r#"#set page(paper: "a4", margin: (x: 34pt, top: 30pt, bottom: 24pt))
#set text(font: ("Liberation Sans", "DejaVu Sans", "Libertinus Serif"),
          size: 9pt, fill: rgb("{INK}"))
#set par(leading: 0.55em)
#let muted = rgb("{MUTED}")
#let cap(s) = text(7.5pt, fill: muted)[#s]
#let head(t, c) = block(width: 100%, below: 6pt)[
  #text(11pt, weight: "bold")[#t] #linebreak() #cap(c)
]
#let stat(val, lab) = box[
  #text(14pt, weight: "bold")[#val] #linebreak()
  #text(6.5pt, fill: muted)[#upper(lab)]
]

#block(below: 10pt)[
  #text(18pt, weight: "bold")[#{}] #linebreak()
  #v(1pt)
  #cap[#{}]
]
#line(length: 100%, stroke: 0.7pt + rgb("{RULE}"))
#v(8pt)
"#,
        tq(&d.title),
        tq(&d.subtitle)
    ));

    match &d.body {
        Body::Day { hourly } => day_page(cfg, d, hourly, dir, &mut t)?,
        Body::Week { days } => week_page(cfg, d, days, dir, &mut t)?,
        Body::Month { strips, punch } => month_page(cfg, d, strips, punch, dir, &mut t)?,
    }

    t.push_str("\n#v(1fr)\n");
    t.push_str(&footnote(d));
    Ok(t)
}

fn day_page(
    cfg: &ServerConfig,
    d: &Data,
    hourly: &[Vec<(String, i64)>],
    dir: &std::path::Path,
    t: &mut String,
) -> Result<()> {
    let donut = svg_file(dir, "donut.svg", &donut_svg(cfg, &d.cats, d.tracked))?;
    let hours = svg_file(
        dir,
        "hourly.svg",
        &columns_svg(cfg, &hour_columns(hourly), Some(60), 3, false),
    )?;

    t.push_str(&format!(
        r#"#grid(columns: (190pt, 1fr), column-gutter: 22pt, align: horizon,
  [{donut}],
  [
    #head[What the day was spent on][Every slice is one activity. The middle number is all the time recorded.]
    {}
  ]
)
#v(1fr)
#head[When it happened][One column per hour of the day. A full-height column means the whole hour was recorded.]
{hours}
#v(1fr)
#grid(columns: (1fr, 1fr), column-gutter: 22pt,
  [
    #head[How the day held together][An unbroken stretch is time on one activity without switching away.]
    {}
  ],
  [
    #head[Apps in front][Time each app spent as the window actually in focus.]
    {}
  ]
)
#v(1fr)
#grid(columns: (1fr, 1fr), column-gutter: 22pt,
  [
    #head[Projects][Time attributed to a named project.]
    {}
  ],
  [],
)
"#,
        cat_table(cfg, &d.cats, d.tracked, 8),
        focus_stats(&d.focus),
        bar_list(&d.apps, d.tracked, 6, web::SERIES_LIGHT[0]),
        bar_list(&d.projects, d.tracked, 5, web::SERIES_LIGHT[6]),
    ));
    Ok(())
}

fn week_page(
    cfg: &ServerConfig,
    d: &Data,
    days: &[DayCol],
    dir: &std::path::Path,
    t: &mut String,
) -> Result<()> {
    let cols: Vec<(String, Vec<(String, i64)>)> = days
        .iter()
        .map(|c| (c.label.clone(), c.cats.clone()))
        .collect();
    let per_day = svg_file(dir, "days.svg", &columns_svg(cfg, &cols, None, 1, true))?;
    let donut = svg_file(dir, "donut.svg", &donut_svg(cfg, &d.cats, d.tracked))?;
    let trend = svg_file(
        dir,
        "trend.svg",
        &trend_svg(
            &days
                .iter()
                .map(|c| (c.label.clone(), c.longest_deep))
                .collect::<Vec<_>>(),
        ),
    )?;

    let busiest = match days.iter().max_by_key(|c| c.tracked) {
        Some(c) if c.tracked > 0 => format!("{} was the fullest day.", c.label),
        _ => "Nothing was recorded this week.".to_string(),
    };

    t.push_str(&format!(
        r#"#head[Each day of the week][Bar height is time recorded that day, coloured by activity. {}]
{per_day}
#v(1fr)
#grid(columns: (190pt, 1fr), column-gutter: 22pt, align: horizon,
  [{donut}],
  [
    #head[The week in total][Every slice is one activity across all seven days.]
    {}
  ]
)
#v(1fr)
#grid(columns: (1fr, 1fr), column-gutter: 22pt,
  [
    #head[Longest unbroken stretch of deep work][Deep work is the work and study categories. Taller is better: fewer interruptions.]
    {trend}
    #v(4pt)
    {}
  ],
  [
    #head[Projects][Time attributed to a named project.]
    {}
  ]
)
"#,
        busiest,
        cat_table(cfg, &d.cats, d.tracked, 8),
        focus_stats(&d.focus),
        bar_list(&d.projects, d.tracked, 6, web::SERIES_LIGHT[6]),
    ));
    Ok(())
}

fn month_page(
    cfg: &ServerConfig,
    d: &Data,
    strips: &[(i64, Vec<Option<String>>)],
    punch: &[Vec<i64>],
    dir: &std::path::Path,
    t: &mut String,
) -> Result<()> {
    let strip = svg_file(dir, "strip.svg", &strip_svg(cfg, strips))?;
    let donut = svg_file(dir, "donut.svg", &donut_svg(cfg, &d.cats, d.tracked))?;
    let punchcard = svg_file(dir, "punch.svg", &punch_svg(punch))?;

    t.push_str(&format!(
        r#"#head[Every day, hour by hour][One row per day, midnight on the left to midnight on the right. Colour is the activity; gaps are time not recorded.]
{strip}
#v(1fr)
#grid(columns: (190pt, 1fr), column-gutter: 22pt, align: horizon,
  [{donut}],
  [
    #head[The month in total][Every slice is one activity across all thirty days.]
    {}
  ]
)
#v(1fr)
#grid(columns: (1fr, 200pt), column-gutter: 22pt,
  [
    #head[The usual week][Darker means more active time in that hour, averaged over the month. Idle time is left out.]
    {punchcard}
  ],
  [
    #head[Apps in front][Time each app spent as the window actually in focus.]
    {}
  ]
)
"#,
        cat_table(cfg, &d.cats, d.tracked, 8),
        bar_list(&d.apps, d.tracked, 6, web::SERIES_LIGHT[0]),
    ));
    Ok(())
}

/// The honesty line. A report that shows only what was captured invites the
/// reader to treat the pie as the whole of life, which it is not.
fn footnote(d: &Data) -> String {
    let idle = d.minutes_in("idle");
    let other = d.minutes_in("other");
    let window = match d.range {
        Range::Day => "the day",
        _ => "this period",
    };
    if d.tracked == 0 {
        return format!(
            "#line(length: 100%, stroke: 0.5pt + rgb(\"{RULE}\"))\n#v(3pt)\n\
             #cap[Nothing was recorded in this period — no machine was running or reporting.]\n"
        );
    }
    format!(
        r#"#line(length: 100%, stroke: 0.5pt + rgb("{RULE}"))
#v(3pt)
#cap[#{}]
"#,
        tq(&format!(
            "Recorded {} of the {} hours in {} ({:.0}%) — the rest is time no machine was running or reporting. \
             Of what was recorded, {} was idle (screen untouched) and {} ({:.0}%) could not be sorted into a named \
             activity. Both are in the charts above. {} device{} reporting; a minute active on two machines counts once.",
            fmt_hm(d.tracked),
            d.span / 60,
            window,
            pct(d.tracked, d.span),
            fmt_hm(idle),
            fmt_hm(other),
            pct(other, d.tracked),
            d.devices,
            if d.devices == 1 { "" } else { "s" },
        ))
    )
}

fn cat_table(cfg: &ServerConfig, cats: &[(String, i64)], total: i64, limit: usize) -> String {
    if cats.is_empty() {
        return "#cap[Nothing recorded.]".into();
    }
    let mut rows = String::new();
    for (cat, n) in cats.iter().take(limit) {
        rows.push_str(&format!(
            "[#box(width: 7pt, height: 7pt, radius: 1.5pt, fill: rgb(\"{}\")) #h(3pt) #{}], \
             [#text(fill: muted)[#{}]], [#{}],\n",
            hex(cfg, cat),
            tq(cat),
            tq(&fmt_hm(*n)),
            tq(&format!("{:.0}%", pct(*n, total)))
        ));
    }
    // Sized to its contents rather than the column, so the times sit beside the
    // names instead of drifting to the far edge of the page.
    format!(
        "#grid(columns: (auto, auto, 26pt), row-gutter: 4.5pt, column-gutter: 14pt,\n  align: (left, right, right),\n  {rows})"
    )
}

fn bar_list(rows: &[(String, i64)], total: i64, limit: usize, colour: &str) -> String {
    if rows.is_empty() {
        return "#cap[Nothing recorded.]".into();
    }
    let max = rows.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1);
    let mut out = String::new();
    for (name, n) in rows.iter().take(limit) {
        out.push_str(&format!(
            "[#text(8pt)[#{}]], \
             [#box(width: 100%, height: 8pt, radius: 2pt, fill: rgb(\"{TRACK}\"))[\
              #box(width: {:.1}%, height: 8pt, radius: 2pt, fill: rgb(\"{colour}\"))]], \
             [#text(7.5pt, fill: muted)[#{}]],\n",
            tq(name),
            (*n as f64 / max as f64) * 100.0,
            tq(&format!("{}  {:.0}%", fmt_hm(*n), pct(*n, total)))
        ));
    }
    format!(
        "#grid(columns: (78pt, 1fr, auto), row-gutter: 4pt, column-gutter: 7pt, align: horizon,\n  {out})"
    )
}

fn focus_stats(f: &Focus) -> String {
    let ttfd = f
        .time_to_first_deep
        .map(fmt_hm)
        .unwrap_or_else(|| "—".into());
    format!(
        "#grid(columns: 3, column-gutter: 14pt, row-gutter: 11pt,\n  \
         stat({}, \"longest stretch\"), stat({}, \"25m+ stretches\"), \
         stat({}, \"50m+ stretches\"),\n  \
         stat({}, \"typical stretch\"), stat({}, \"switches\"), \
         stat({}, \"to first deep stretch\"))",
        tq(&fmt_hm(f.longest)),
        tq(&f.blocks_25.to_string()),
        tq(&f.blocks_50.to_string()),
        tq(&fmt_hm(f.median_block)),
        tq(&f.switches.to_string()),
        tq(&ttfd),
    )
}

// ---------------------------------------------------------------- charts
//
// Standalone SVG: literal hex from the shared palette, explicit font and size
// on every label. Nothing here may rely on a stylesheet, because the renderer
// inside Typst has none.

const FONT: &str = "font-family=\"Liberation Sans, DejaVu Sans, sans-serif\"";

fn label(x: f64, y: f64, size: f64, fill: &str, anchor: &str, text: &str) -> String {
    format!(
        "<text x=\"{x:.1}\" y=\"{y:.1}\" {FONT} font-size=\"{size}\" fill=\"{fill}\" \
         text-anchor=\"{anchor}\">{}</text>",
        esc(text)
    )
}

/// Single-ring donut. Slices in config order, so a category keeps its colour
/// and its neighbours -- the adjacency the palette was validated on.
fn donut_svg(cfg: &ServerConfig, cats: &[(String, i64)], total: i64) -> String {
    if total == 0 {
        return format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 300 300\">{}</svg>",
            label(150.0, 150.0, 13.0, MUTED, "middle", "Nothing recorded")
        );
    }

    let (cx, cy, r_out, r_in) = (150.0_f64, 150.0_f64, 130.0_f64, 82.0_f64);
    let mut svg = String::new();
    let mut labels = String::new();
    let mut angle = -std::f64::consts::FRAC_PI_2;

    for (cat, n) in in_config_order(cfg, cats, |t| t.0.as_str()) {
        let frac = *n as f64 / total as f64;
        let sweep = (frac * std::f64::consts::TAU).min(std::f64::consts::TAU - 0.001);
        let gap = if frac > 0.995 { 0.0 } else { 0.016 };
        let (a0, a1) = (angle + gap / 2.0, angle + sweep - gap / 2.0);
        angle += sweep;
        if a1 <= a0 {
            continue;
        }
        svg.push_str(&format!(
            "<path d=\"{}\" fill=\"{}\"/>",
            arc_path(cx, cy, r_in, r_out, a0, a1),
            hex(cfg, cat)
        ));

        // Direct labels are the relief for the light-mode contrast warning, so
        // they are not decoration. Below 7% they collide and the table beside
        // the chart carries those rows instead.
        if frac >= 0.07 {
            let mid = (a0 + a1) / 2.0;
            let (lx, ly) = (
                cx + (r_out + r_in) / 2.0 * mid.cos(),
                cy + (r_out + r_in) / 2.0 * mid.sin(),
            );
            labels.push_str(&format!(
                "<text x=\"{lx:.1}\" y=\"{ly:.1}\" {FONT} font-size=\"13\" font-weight=\"bold\" \
                 fill=\"#fff\" text-anchor=\"middle\" dominant-baseline=\"central\" \
                 paint-order=\"stroke\" stroke=\"rgba(0,0,0,.35)\" stroke-width=\"2.5\">{:.0}%</text>",
                frac * 100.0
            ));
        }
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 300 300\">{svg}{labels}\
         <text x=\"150\" y=\"148\" {FONT} font-size=\"26\" font-weight=\"bold\" fill=\"{INK}\" \
         text-anchor=\"middle\">{}</text>{}</svg>",
        esc(&fmt_hm(total)),
        label(150.0, 168.0, 10.0, MUTED, "middle", "RECORDED")
    )
}

fn hour_columns(hourly: &[Vec<(String, i64)>]) -> Vec<(String, Vec<(String, i64)>)> {
    hourly
        .iter()
        .enumerate()
        .map(|(h, b)| (format!("{h:02}"), b.clone()))
        .collect()
}

/// Stacked columns: hours of a day, or days of a week. `full` is the value that
/// means a full-height column when the scale should be absolute (60 minutes in
/// an hour) rather than relative to the tallest column.
fn columns_svg(
    cfg: &ServerConfig,
    cols: &[(String, Vec<(String, i64)>)],
    full: Option<i64>,
    label_every: usize,
    totals: bool,
) -> String {
    let (w, h, top) = (720.0_f64, 150.0_f64, if totals { 14.0 } else { 2.0 });
    let peak = cols
        .iter()
        .map(|(_, b)| b.iter().map(|(_, n)| n).sum::<i64>())
        .max()
        .unwrap_or(0);
    let max = full.unwrap_or(peak).max(1);
    let cw = w / cols.len().max(1) as f64;
    let mut out = String::new();

    for (i, (name, bucket)) in cols.iter().enumerate() {
        let x = i as f64 * cw;
        let mut y = top + h;
        let total: i64 = bucket.iter().map(|(_, n)| n).sum();
        for (cat, n) in in_config_order(cfg, bucket, |e| e.0.as_str()) {
            let bh = (*n as f64 / max as f64) * h;
            if bh <= 0.0 {
                continue;
            }
            y -= bh;
            out.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{bh:.1}\" fill=\"{}\"/>",
                x + cw * 0.08,
                cw * 0.84,
                hex(cfg, cat)
            ));
        }
        if totals && total > 0 {
            out.push_str(&label(
                x + cw / 2.0,
                y - 4.0,
                11.0,
                MUTED,
                "middle",
                &fmt_hm(total),
            ));
        }
        if i % label_every == 0 {
            out.push_str(&label(
                x + cw / 2.0,
                top + h + 14.0,
                11.0,
                MUTED,
                "middle",
                name,
            ));
        }
    }

    // Baseline, so an empty column reads as "nothing recorded" rather than as a
    // hole in the chart.
    out.push_str(&format!(
        "<line x1=\"0\" y1=\"{:.1}\" x2=\"{w}\" y2=\"{:.1}\" stroke=\"{RULE}\" stroke-width=\"1\"/>",
        top + h,
        top + h
    ));
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {:.1}\">{out}</svg>",
        top + h + 19.0
    )
}

/// One row per day, midnight to midnight -- the same mark as the web strip.
fn strip_svg(cfg: &ServerConfig, rows: &[(i64, Vec<Option<String>>)]) -> String {
    if rows.is_empty() {
        return format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 720 20\">{}</svg>",
            label(0.0, 14.0, 11.0, MUTED, "start", "No history yet.")
        );
    }
    let per_day = rows[0].1.len().max(1);
    let (w, row_h, label_w) = (720.0_f64, 12.0_f64, 46.0_f64);
    let cell_w = (w - label_w) / per_day as f64;
    let mut out = String::new();
    let base = rows.len() as f64 * row_h;

    for h in (0..=24).step_by(3) {
        let x = label_w + (h as f64 / 24.0) * (w - label_w);
        // The end labels would hang off the viewBox and be clipped, so they
        // hug the edge instead of centring on the rule.
        let anchor = match h {
            0 => "start",
            24 => "end",
            _ => "middle",
        };
        out.push_str(&format!(
            "<line x1=\"{x:.1}\" y1=\"0\" x2=\"{x:.1}\" y2=\"{base:.1}\" stroke=\"{RULE}\" \
             stroke-width=\"1\"/>{}",
            label(x, base + 12.0, 10.0, MUTED, anchor, &format!("{h:02}"))
        ));
    }

    for (i, (start, buckets)) in rows.iter().enumerate() {
        let y = i as f64 * row_h;
        let day = Local
            .timestamp_opt(*start, 0)
            .single()
            .map(|d| d.format("%a %d").to_string())
            .unwrap_or_default();
        out.push_str(&label(0.0, y + row_h - 3.0, 9.5, MUTED, "start", &day));
        for (b, cat) in buckets.iter().enumerate() {
            let Some(cat) = cat else { continue };
            out.push_str(&format!(
                "<rect x=\"{:.2}\" y=\"{y:.1}\" width=\"{:.2}\" height=\"{:.1}\" fill=\"{}\"/>",
                label_w + b as f64 * cell_w,
                cell_w + 0.4,
                row_h - 1.5,
                hex(cfg, cat)
            ));
        }
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {:.1}\">{out}</svg>",
        base + 17.0
    )
}

/// Weekday × hour intensity. The ramp is one ink at varying strength, not a
/// palette slot: this encodes "how much", and borrowing a categorical hue would
/// claim a category the cell does not have.
fn punch_svg(punch: &[Vec<i64>]) -> String {
    let max = punch
        .iter()
        .flat_map(|r| r.iter())
        .copied()
        .max()
        .unwrap_or(0)
        .max(1);
    let (w, label_w, row_h) = (520.0_f64, 30.0_f64, 15.0_f64);
    let cell = (w - label_w) / 24.0;
    let mut out = String::new();

    for (d, row) in punch.iter().enumerate() {
        let y = d as f64 * row_h;
        let name = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][d.min(6)];
        out.push_str(&label(0.0, y + row_h - 4.0, 10.0, MUTED, "start", name));
        for (h, n) in row.iter().enumerate() {
            // A floor on the visible cells keeps a single quiet hour from
            // vanishing into the paper.
            let o = if *n == 0 {
                0.0
            } else {
                0.12 + 0.88 * (*n as f64 / max as f64)
            };
            out.push_str(&format!(
                "<rect x=\"{:.2}\" y=\"{y:.1}\" width=\"{:.2}\" height=\"{:.1}\" rx=\"1.5\" \
                 fill=\"{INK}\" fill-opacity=\"{o:.3}\"/>",
                label_w + h as f64 * cell,
                cell - 1.2,
                row_h - 2.0
            ));
        }
    }

    let base = punch.len() as f64 * row_h;
    for h in (0..24).step_by(3) {
        out.push_str(&label(
            label_w + (h as f64 + 0.5) * cell,
            base + 11.0,
            10.0,
            MUTED,
            "middle",
            &format!("{h:02}"),
        ));
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {:.1}\">{out}</svg>",
        base + 15.0
    )
}

/// Minutes per day as plain columns, for the focus trend.
fn trend_svg(days: &[(String, i64)]) -> String {
    let max = days.iter().map(|(_, n)| *n).max().unwrap_or(0).max(1);
    let (w, h) = (420.0_f64, 78.0_f64);
    let cw = w / days.len().max(1) as f64;
    let mut out = String::new();

    for (i, (name, n)) in days.iter().enumerate() {
        let x = i as f64 * cw;
        let bh = (*n as f64 / max as f64) * h;
        if bh > 0.0 {
            out.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{bh:.1}\" rx=\"2\" \
                 fill=\"{}\"/>",
                x + cw * 0.18,
                h - bh + 14.0,
                cw * 0.64,
                web::SERIES_LIGHT[0]
            ));
        }
        out.push_str(&label(
            x + cw / 2.0,
            h - bh + 10.0,
            10.5,
            MUTED,
            "middle",
            &if *n > 0 { fmt_hm(*n) } else { "—".into() },
        ));
        out.push_str(&label(x + cw / 2.0, h + 27.0, 10.5, MUTED, "middle", name));
    }
    out.push_str(&format!(
        "<line x1=\"0\" y1=\"{:.1}\" x2=\"{w}\" y2=\"{:.1}\" stroke=\"{RULE}\" stroke-width=\"1\"/>",
        h + 14.0,
        h + 14.0
    ));
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {:.1}\">{out}</svg>",
        h + 32.0
    )
}
