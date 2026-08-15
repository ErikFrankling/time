//! Re-judge stored minutes, offline, from the raw data the server now keeps.
//!
//! The point of retaining screenshots and raw replies is that a label stops
//! being final: a better model, a cheaper model, or a changed category list
//! can be run over history and compared against what the live classifier said
//! at the time. By default nothing is touched -- results land in
//! `minute_trial` under a run id and a comparison is printed. `--apply` is the
//! version that believes the new model and rewrites the live labels.
//!
//! Runs where the data is: this reads the server's database and its `frames/`
//! directory, so it is a server-side command, not an agent one.

use std::collections::HashMap;

use anyhow::Result;

use crate::classify;
use crate::config::{self, ServerConfig};
use crate::db::{Db, Filter, Minute};

#[derive(Debug, Default)]
pub struct Opts {
    pub days: i64,
    pub device: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub run_id: Option<String>,
    pub apply: bool,
    pub limit: Option<usize>,
}

pub fn run(mut cfg: ServerConfig, opts: Opts) -> Result<()> {
    // `--model` overrides both models on purpose: a backtest asks "what would
    // model X have said", and letting text-only batches quietly slip through
    // to the old `model_text` would answer a different question.
    if let Some(m) = &opts.model {
        cfg.model = m.clone();
        cfg.model_text = m.clone();
    }
    if let Some(e) = &opts.endpoint {
        cfg.endpoint = e.clone();
    }
    let key = config::api_key();
    let data_dir = config::data_dir()?;
    let db = Db::open()?;

    let now = chrono::Utc::now().timestamp();
    let from = now - opts.days.max(0) * 86_400;
    let filter = Filter {
        device: opts.device.clone(),
        ..Default::default()
    };
    // Everything, idle included: a backtest exists to second-guess old
    // judgments, and "was this really idle" is one of the judgments.
    let rows = db.range(from, now + 60, &filter)?;
    anyhow::ensure!(
        !rows.is_empty(),
        "no minutes in the last {} day(s)",
        opts.days
    );

    let mut batches = chunk(rows, cfg.batch_minutes.max(1));
    if let Some(limit) = opts.limit {
        truncate_to(&mut batches, limit);
    }
    let total: usize = batches.iter().map(|b| b.len()).sum();

    let run_id = opts.run_id.clone().unwrap_or_else(|| {
        format!(
            "{}-{}",
            cfg.model,
            chrono::Local::now().format("%Y%m%d-%H%M")
        )
    });
    eprintln!(
        "reclassify: {} minute(s) in {} batch(es) via {} at {}{}",
        total,
        batches.len(),
        cfg.model,
        cfg.endpoint,
        if opts.apply {
            " — APPLYING to live labels"
        } else {
            ""
        },
    );

    let mut pairs: Vec<(String, String)> = Vec::new();
    for batch in &batches {
        // A batch spans devices, exactly like the live classifier's; the log
        // line names all of them.
        let device = {
            let mut d: Vec<&str> = batch.iter().map(|m| m.device.as_str()).collect();
            d.sort();
            d.dedup();
            d.join(",")
        };
        let span = (batch[0].ts, batch[batch.len() - 1].ts);

        // Read the kept pixels back. A missing file is the pre-retention era
        // or a phone minute, and both were always classified without an image.
        let jpegs: Vec<Option<Vec<u8>>> = batch
            .iter()
            .map(|m| {
                m.image_path
                    .as_deref()
                    .and_then(|rel| std::fs::read(data_dir.join(rel)).ok())
            })
            .collect();
        let items: Vec<classify::Item<'_>> = batch
            .iter()
            .zip(&jpegs)
            .map(|(m, jpeg)| classify::Item {
                ts: m.ts,
                jpeg: jpeg.as_deref(),
                window: m.window.as_deref().unwrap_or(""),
                domain: m.domain.as_deref(),
                presence: classify::Presence {
                    device: &m.device,
                    idle_secs: m.idle_secs,
                    keys: m.keys,
                    mouse: m.mouse,
                    note: m.note.as_deref(),
                },
            })
            .collect();

        let (labels, usage, _raw) = match classify::classify(&cfg, key.as_deref(), &items, &[]) {
            Ok(v) => v,
            Err(e) => {
                // A rate limit shuts the whole endpoint, not this batch, so
                // hammering on through the rest would only dig the hole deeper.
                if e.downcast_ref::<classify::RateLimited>().is_some() {
                    return Err(e.context("stopping: the endpoint rate limited this run"));
                }
                eprintln!("reclassify {device} {}..{}: {e:#}", span.0, span.1);
                continue;
            }
        };

        let old: HashMap<(&str, i64), &Minute> = batch
            .iter()
            .map(|m| ((m.device.as_str(), m.ts), m))
            .collect();
        for ((dev, ts), label) in &labels {
            let Some(orig) = old.get(&(dev.as_str(), *ts)) else {
                continue;
            };
            if opts.apply {
                db.label(&orig.device, *ts, label, &usage.model)?;
            } else {
                db.put_trial(&run_id, &orig.device, *ts, label, &usage.model)?;
            }
            pairs.push((orig.category.clone(), label.category.clone()));
        }
        eprintln!(
            "reclassify: {device} {}..{} {}/{} labelled via {}, {} in / {} out",
            span.0,
            span.1,
            labels.len(),
            batch.len(),
            usage.model,
            usage.prompt,
            usage.completion,
        );
    }

    print!("{}", summary(&run_id, opts.apply, total, &pairs));
    Ok(())
}

/// Sort minutes into one (ts, device) timeline across every device and cut it
/// into runs of at most `batch` -- the same shape the live classifier sends,
/// so the backtest measures the model under the conditions it would actually
/// run in: simultaneous minutes from different machines sit in one batch,
/// side by side.
pub fn chunk(mut rows: Vec<Minute>, batch: usize) -> Vec<Vec<Minute>> {
    // `range` returns ts order already, but that is a property of a query
    // this function cannot see -- and the device tiebreak is this function's
    // own contract.
    rows.sort_by(|a, b| (a.ts, &a.device).cmp(&(b.ts, &b.device)));
    let mut out = Vec::new();
    let mut it = rows.into_iter().peekable();
    while it.peek().is_some() {
        out.push(it.by_ref().take(batch).collect());
    }
    out
}

/// Cap the total minutes across batches, dropping whole trailing batches and
/// trimming the one on the boundary.
pub fn truncate_to(batches: &mut Vec<Vec<Minute>>, limit: usize) {
    let mut left = limit;
    batches.retain_mut(|b| {
        if left == 0 {
            return false;
        }
        if b.len() > left {
            b.truncate(left);
        }
        left -= b.len();
        !b.is_empty()
    });
}

/// The comparison the run exists for: how often the new model agrees with the
/// stored labels, and where the disagreements concentrate.
pub fn summary(run_id: &str, applied: bool, sent: usize, pairs: &[(String, String)]) -> String {
    let mut s = String::new();
    let agree = pairs.iter().filter(|(old, new)| old == new).count();
    let pct = if pairs.is_empty() {
        0.0
    } else {
        100.0 * agree as f64 / pairs.len() as f64
    };
    s.push_str(&format!(
        "run {run_id}: {sent} minute(s) sent, {} labelled, {agree} unchanged ({pct:.1}% agreement)\n",
        pairs.len(),
    ));
    if applied {
        s.push_str("labels updated in place\n");
    } else {
        s.push_str(&format!(
            "stored in minute_trial under run_id '{run_id}' — live labels untouched\n"
        ));
    }

    let mut changes: HashMap<(&str, &str), usize> = HashMap::new();
    for (old, new) in pairs {
        if old != new {
            *changes.entry((old, new)).or_default() += 1;
        }
    }
    let mut top: Vec<_> = changes.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if !top.is_empty() {
        s.push_str("top changes:\n");
        for ((old, new), n) in top.into_iter().take(10) {
            s.push_str(&format!("  {old} → {new}: {n}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(device: &str, ts: i64, category: &str) -> Minute {
        Minute {
            ts,
            device: device.into(),
            category: category.into(),
            ..Default::default()
        }
    }

    /// The backtest batches exactly like the live classifier: one timeline
    /// across every device, sorted by (ts, device), so a batch may -- and
    /// should -- mix machines, with simultaneous minutes adjacent.
    #[test]
    fn chunks_one_cross_device_timeline() {
        // Interleaved devices, shuffled timestamps, batch of 3.
        let rows = vec![
            m("pc", 180, "other"),
            m("laptop", 60, "other"),
            m("pc", 60, "other"),
            m("pc", 120, "other"),
        ];
        let batches = chunk(rows, 3);
        assert_eq!(batches.len(), 2);
        let key = |x: &Minute| (x.ts, x.device.clone());
        assert_eq!(
            batches[0].iter().map(key).collect::<Vec<_>>(),
            [
                (60, "laptop".to_string()),
                (60, "pc".to_string()),
                (120, "pc".to_string()),
            ]
        );
        assert_eq!(
            batches[1].iter().map(key).collect::<Vec<_>>(),
            [(180, "pc".to_string())]
        );
    }

    #[test]
    fn limit_trims_the_boundary_batch_and_drops_the_rest() {
        let rows: Vec<Minute> = (1..=10).map(|i| m("pc", i * 60, "other")).collect();
        let mut batches = chunk(rows, 4); // 4 + 4 + 2
        truncate_to(&mut batches, 5);
        let sizes: Vec<usize> = batches.iter().map(|b| b.len()).collect();
        assert_eq!(sizes, [4, 1]);
    }

    #[test]
    fn summary_counts_agreement_and_ranks_changes() {
        let pairs = vec![
            ("idle".to_string(), "idle".to_string()),
            ("browsing".to_string(), "twitter".to_string()),
            ("browsing".to_string(), "twitter".to_string()),
            ("other".to_string(), "work_personal".to_string()),
        ];
        let s = summary("qwen-20260815-1200", false, 5, &pairs);
        assert!(
            s.contains("5 minute(s) sent, 4 labelled, 1 unchanged (25.0% agreement)"),
            "{s}"
        );
        assert!(s.contains("minute_trial"), "{s}");
        // The bigger change ranks first.
        let tw = s.find("browsing → twitter: 2").expect("change line");
        let wp = s.find("other → work_personal: 1").expect("change line");
        assert!(tw < wp, "{s}");
    }

    #[test]
    fn summary_says_when_labels_were_rewritten() {
        let s = summary("r", true, 1, &[("a".into(), "a".into())]);
        assert!(s.contains("updated in place"), "{s}");
        assert!(!s.contains("minute_trial"), "{s}");
    }
}
