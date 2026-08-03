use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::Deserialize;

use crate::config::ServerConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub category: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    /// Everything happening this minute, not just the main thing. Coding with
    /// music on is two tags and one category, which is what makes "how often do
    /// I work distracted" answerable at all.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One label as it comes back from a batched call: the same fields plus the
/// timestamp that says which minute it belongs to. Order alone would be enough
/// if models never dropped or reordered an entry, which they do.
#[derive(Debug, Deserialize)]
struct Row {
    ts: i64,
    #[serde(flatten)]
    label: Label,
}

/// The previous minute's label, passed back into the next call. One string, and
/// it's what stops labels flickering between categories during a single
/// continuous activity.
pub struct Previous<'a> {
    pub category: &'a str,
    pub project: Option<&'a str>,
    pub detail: Option<&'a str>,
}

/// Whether a human was actually at the machine. Without this the model can only
/// describe the screen, which is a different question from what the person was
/// doing.
pub struct Presence<'a> {
    pub device: &'a str,
    pub idle_secs: Option<u32>,
    pub keys: u32,
    pub mouse: u32,
    pub note: Option<&'a str>,
}

/// One minute to be labelled. A call takes a run of these from a single device
/// in time order.
pub struct Item<'a> {
    pub ts: i64,
    pub jpeg: Option<&'a [u8]>,
    pub window: &'a str,
    pub domain: Option<&'a str>,
    pub presence: Presence<'a>,
}

/// The endpoint said no and told us for how long. Distinguished from every
/// other failure because it is the one the caller must not retry into: the
/// weekly allowance is shared with the user's own coding, so a classifier that
/// keeps knocking spends their working day on 429s.
#[derive(Debug)]
pub struct RateLimited {
    pub retry_after: std::time::Duration,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rate limited for {}s", self.retry_after.as_secs())
    }
}

impl std::error::Error for RateLimited {}

/// What one call cost, as the endpoint counted it. Logged rather than stored:
/// the point is to be able to read real numbers out of the pod when deciding
/// whether the batch size or the screenshot width is worth changing, and
/// guessing at image token counts is how the previous estimate went wrong.
#[derive(Default)]
pub struct Usage {
    pub prompt: u64,
    pub completion: u64,
    /// Which model actually answered. The caller cannot assume `cfg.model` any
    /// more -- the batch decides -- and the label row records what judged it.
    pub model: String,
}

fn system_prompt(cfg: &ServerConfig) -> String {
    format!(
        "You are labelling minutes of a developer's day, to be aggregated \
into a chart of where their time went.

The question is NOT \"what is on this screen\". It is \"what was THE PERSON \
doing during this minute\". Those differ more often than you would expect, and \
getting the difference right is the whole job.

Choose exactly one category per minute:
{}

## Presence comes first

You are given the seconds since the last real human input, plus counts of key \
presses and mouse movements for this minute. Input counts come only from \
physical devices; input injected by automation is already excluded.

A screen can change continuously with nobody in the room. Builds scroll, videos \
play, tests run, and AI agents drive the machine on their own. So:

- No input for several minutes and the screen is merely *changing* is NOT the \
person working. If nothing indicates a human is watching, that is \"idle\".
- Substantial idle time with no keys and no mouse is strong evidence they are \
away. Weigh it heavily. Do not label work just because a work-looking window \
is visible.
- But you decide. Reading a long document, watching a video, or sitting in a \
meeting are all real activity with little or no input. If the screen genuinely \
suggests a person is consuming something, label that, not \"idle\".
- Recent input with an idle-looking screen usually means they are still there.

If the idle time is unknown, say so in your reasoning by leaning on the screen \
and previous label, and be more cautious about claiming active work. Zero key \
presses and zero pointer movements are then no information either -- the same \
device that cannot time the last input usually cannot count them, and a phone \
reports neither. Treat them as unknown, not as evidence nobody was there; what \
is in the foreground is the whole signal.

## One minute, several things

People do more than one thing at once. Coding with music playing, writing
with YouTube on a second monitor, reading docs while a chat window sits open
and active.

- \"category\" is the ONE thing that best describes the minute -- what they
would say they were doing. Time is accounted against this, so it must be
singular.
- \"tags\" lists EVERYTHING going on, including the category itself. If code
is being written while a video plays, that is category \"work_personal\" with
tags [\"work_personal\", \"youtube\"].
- Only tag what is genuinely active. A minimised window or an idle tab is not
an activity. Audible media, a visible video, a live chat all count.
- Tags must come from the same category list. Nothing invented.

## Several minutes at once

You are given a run of minutes from one machine, in time order. They are \
usually but not always consecutive -- each one states its own timestamp, so \
check before assuming two of them are a minute apart. Read them as a \
trajectory, not as unrelated screenshots: a build that starts \
in minute 3 and is still running in minute 7 is the same stretch of work, and \
someone who stops touching the keyboard halfway through has left, even if the \
screen keeps moving. Use the later minutes to understand the earlier ones.

Label every minute you are given. Do not merge them, drop them or invent \
extra ones. Consecutive minutes of the same activity should get the same \
category and project -- flickering between categories mid-activity is the \
most common way these labels go wrong -- but \"detail\" should still say what \
was specifically happening in that minute.

## The other fields

- \"project\" is the repository, course, or topic if you can identify one, else \
null.
- \"detail\" is ONE concrete sentence naming specifics: file paths, repo names, \
page titles, what a terminal is running. This is the only record that survives \
after the screenshot is deleted, so be specific rather than vague. When the \
person is away, say what the machine was doing instead, e.g. \"away; an agent \
was editing files in the terminal\".
- Use \"other\" when nothing on the list genuinely fits. Do not force a bad fit.
- If the previous minute's label is given and the first minute continues the \
same activity, reuse the same category and project.

Respond with a JSON array only, no markdown fence, one object per minute given, \
in the same order, each carrying back the \"ts\" it was labelled from:
[{{\"ts\": 1234567890, \"category\": \"...\", \"tags\": [\"...\"], \
\"project\": \"...\" or null, \"detail\": \"...\"}}]",
        cfg.categories
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The per-minute preamble that sits in front of that minute's screenshot.
fn item_context(item: &Item<'_>, n: usize, total: usize, prev: Option<&Previous<'_>>) -> String {
    let mut context = String::new();
    let p = &item.presence;

    // Once for the run, not once per minute: every minute in a batch comes
    // from the same machine, and repeating its description twenty times says
    // nothing new at full price.
    if n == 1 {
        if let Some(note) = p.note.filter(|n| !n.trim().is_empty()) {
            context.push_str(&format!("About this machine: {note}\n"));
        }
    }
    context.push_str(&format!(
        "--- Minute {n} of {total} --- ts={} ({})\n",
        item.ts,
        chrono::DateTime::from_timestamp(item.ts, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default()
    ));
    context.push_str(&format!("Machine: {}\n", p.device));
    match p.idle_secs {
        Some(s) => context.push_str(&format!("Seconds since last human input: {s}\n")),
        // "no input device readable" was written for a desktop whose evdev read
        // failed. A phone has no such device to begin with and never will, so
        // the phrasing has to cover both without implying a fault.
        None => context.push_str(
            "Seconds since last human input: UNKNOWN (this device does not report it)\n",
        ),
    }
    context.push_str(&format!(
        "Human input this minute: {} key presses, {} pointer movements\n",
        p.keys, p.mouse
    ));
    context.push_str(&format!("Active window: {}", item.window));
    // "Firefox" tells the model nothing; the site being read tells it almost
    // everything, and reading it off the screenshot is guesswork the browser
    // itself can answer exactly.
    if let Some(d) = item.domain.filter(|d| !d.trim().is_empty()) {
        context.push_str(&format!("\nFocused browser tab is on: {d}"));
    }
    if item.jpeg.is_none() {
        // Otherwise the model is left to wonder which of the images below
        // belongs to this minute.
        context.push_str("\n(no screenshot available for this minute)");
    }

    if let Some(p) = prev {
        context.push_str(&format!("\nPrevious minute: category={}", p.category));
        if let Some(proj) = p.project.filter(|s| !s.is_empty()) {
            context.push_str(&format!(", project={proj}"));
        }
        if let Some(d) = p.detail.filter(|s| !s.is_empty()) {
            context.push_str(&format!(", detail={d}"));
        }
    }
    context.push('\n');
    context
}

/// Which model gets this batch.
///
/// Vision is what makes the expensive model necessary, so a batch without a
/// single screenshot -- every phone batch, and everything the sweep picks up
/// after the image is gone -- goes to the cheap text model instead. Decided
/// from the payload rather than the device name, so a desktop whose capture
/// failed is routed correctly too, and so a text-only model can never be sent
/// a picture of the user's screen by accident.
fn model_for<'a>(cfg: &'a ServerConfig, items: &[Item<'_>]) -> &'a str {
    if items.iter().any(|i| i.jpeg.is_some()) {
        &cfg.model
    } else {
        &cfg.model_text
    }
}

/// Label a run of minutes from one device, in time order, in a single call.
///
/// One call for twenty minutes instead of twenty calls is not only twenty
/// times fewer requests against a weekly allowance -- the long system prompt
/// is sent once instead of twenty times, and the model gets to see the
/// trajectory, which is what it needs to tell "reading" from "left the room"
/// in the first place.
///
/// `jpeg` is None for devices that cannot produce a screenshot -- a phone,
/// where Android forbids silent capture entirely. There the foreground app name
/// carries most of the signal anyway: "Instagram" says what you were doing in a
/// way "Firefox" never does on a desktop.
///
/// Minutes the model declines to label are simply absent from the result. They
/// keep their pending flag and the sweep offers them again, which is a great
/// deal safer than pairing labels to minutes by position.
pub fn classify(
    cfg: &ServerConfig,
    key: &str,
    items: &[Item<'_>],
    prev: Option<Previous<'_>>,
) -> Result<(Vec<(i64, Label)>, Usage)> {
    anyhow::ensure!(!items.is_empty(), "nothing to classify");

    let mut content: Vec<serde_json::Value> = Vec::with_capacity(items.len() * 2 + 1);
    for (i, item) in items.iter().enumerate() {
        let prev = if i == 0 { prev.as_ref() } else { None };
        content.push(serde_json::json!({
            "type": "text",
            "text": item_context(item, i + 1, items.len(), prev),
        }));
        if let Some(j) = item.jpeg {
            content.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!(
                    "data:image/jpeg;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(j)) }
            }));
        }
    }
    content.push(serde_json::json!({
        "type": "text",
        "text": format!(
            "Classify all {} minutes above. Return a JSON array of exactly {} objects \
             in the same order, each with its own \"ts\".",
            items.len(),
            items.len()
        ),
    }));

    let model = model_for(cfg, items);

    let body = serde_json::json!({
        "model": model,
        "temperature": 0,
        // Per minute, plus headroom for a reasoning model's preamble. Too low
        // truncates the array and costs the whole batch, which is far more
        // expensive than the few unused tokens this leaves on the table.
        // Measured peak is 212 tokens for a minute with a long detail line, so
        // 200 left about 6% headroom at a batch of twenty -- and an overrun
        // does not truncate one label, it loses the whole batch. Unused
        // headroom is free: this caps the reply, it does not reserve anything.
        "max_tokens": 400 * items.len() + 512,
        "messages": [
            { "role": "system", "content": system_prompt(cfg) },
            { "role": "user", "content": serde_json::Value::Array(content) }
        ]
    });

    let client = reqwest::blocking::Client::builder()
        // A full-screen image on a busy endpoint regularly takes well over a
        // minute, and a batch carries twenty of them. Timing out does not save
        // anything -- the minutes are simply lost -- so wait rather than give up.
        .timeout(std::time::Duration::from_secs(600))
        .build()?;

    // One retry, because these failures are transient far more often than not
    // and a dropped minute leaves a permanent hole in the day.
    let mut last_err = String::new();
    for _ in 0..2 {
        let sent = client
            .post(&cfg.endpoint)
            .bearer_auth(key)
            .json(&body)
            .send();

        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                let timed_out = e.is_timeout();
                last_err = format!("calling the model endpoint: {e}");
                // A timeout is not a failed call, only an unheard one: the
                // endpoint has very likely already generated the answer and
                // charged for it. Retrying pays a second time for a batch the
                // sweep will offer again anyway.
                if timed_out {
                    break;
                }
                continue;
            }
        };

        let status = resp.status();
        // The endpoint states how long the allowance is shut for. Believe it:
        // a guessed hour either wastes fifty minutes of good time or wakes up
        // into the same wall four more times.
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let text = resp.text().unwrap_or_default();

        if status.as_u16() == 429 {
            return Err(RateLimited {
                retry_after: std::time::Duration::from_secs(retry_after.unwrap_or(3600)),
            }
            .into());
        }
        if !status.is_success() {
            last_err = format!(
                "model returned {status}: {}",
                text.chars().take(400).collect::<String>()
            );
            // A refusal is deterministic; retrying just spends the allowance
            // twice for the same answer.
            if status.as_u16() < 500 {
                break;
            }
            continue;
        }

        let v: serde_json::Value = serde_json::from_str(&text).with_context(|| {
            format!("parsing response: {}", text.chars().take(400).collect::<String>())
        })?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .context("no content in model response")?;
        let usage = Usage {
            prompt: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            completion: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            model: model.to_string(),
        };
        return Ok((parse_labels(content, cfg)?, usage));
    }
    bail!("{last_err}");
}

/// Reasoning models narrate before they answer, and the narration is full of
/// braces and brackets. Cutting the think block off first is what makes the
/// cheaper reasoning models on the plan usable at all.
fn strip_reasoning(content: &str) -> &str {
    let mut rest = content;
    // Deliberately tolerant about the tag: the plan's models spell it
    // `<think>`, `<thinking>` and `<reasoning>` between them.
    while let Some(open) = rest.find("<think") {
        let after = &rest[open..];
        match after.find("</").and_then(|c| after[c..].find('>').map(|e| open + c + e + 1)) {
            Some(end) => rest = &rest[end..],
            // An unterminated block means the answer was truncated before it
            // ever started. Nothing to salvage.
            None => return "",
        }
    }
    rest
}

/// Models wrap JSON in prose or fences often enough that trusting the raw body
/// is a guaranteed source of intermittent failures. Take the outermost
/// brackets, and accept a bare object for the batch-of-one case.
fn parse_labels(content: &str, cfg: &ServerConfig) -> Result<Vec<(i64, Label)>> {
    let content = strip_reasoning(content);
    let json = match (content.find('['), content.rfind(']')) {
        (Some(s), Some(e)) if e > s => &content[s..=e],
        _ => match (content.find('{'), content.rfind('}')) {
            (Some(s), Some(e)) if e > s => &content[s..=e],
            _ => bail!("no JSON in model output: {content}"),
        },
    };

    let rows: Vec<Row> = if json.starts_with('[') {
        serde_json::from_str(json).with_context(|| format!("parsing label array: {json}"))?
    } else {
        vec![serde_json::from_str(json).with_context(|| format!("parsing label JSON: {json}"))?]
    };

    Ok(rows
        .into_iter()
        .map(|r| (r.ts, clean(r.label, cfg)))
        .collect())
}

fn clean(mut label: Label, cfg: &ServerConfig) -> Label {
    // An invented category would silently create a phantom pie slice, so fold
    // anything off-list into `other` rather than trusting the model here.
    let cat = label.category.trim().to_lowercase();
    label.category = if cfg.categories.iter().any(|c| c.to_lowercase() == cat) {
        cat
    } else {
        "other".into()
    };

    // Drop invented tags for the same reason as the category: a phantom tag
    // would show up as real concurrent activity that never happened.
    label.tags = label
        .tags
        .iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| cfg.categories.iter().any(|c| c.to_lowercase() == *t))
        .collect();
    // The category is always part of what was happening, even if the model
    // omitted it from the list.
    if !label.tags.contains(&label.category) {
        label.tags.push(label.category.clone());
    }
    label.tags.sort();
    label.tags.dedup();

    label.project = label.project.filter(|s| !s.trim().is_empty());
    label.detail = label.detail.filter(|s| !s.trim().is_empty());
    label
}

/// Category for a phone minute, from the package name alone.
///
/// A phone frame has no screenshot, so the model is being asked to read
/// "Instagram — com.instagram.android" and say what it is. That is a lookup
/// wearing a model's clothes: it costs a call, a queue slot and part of a
/// weekly allowance to restate the input. On a phone the foreground app *is*
/// the activity, in a way it never is on a desktop where everything is a
/// browser or a terminal.
///
/// Unmatched packages still go to the model, so a new app gets a real answer
/// once and this list is where the answer belongs afterwards.
pub fn from_package(cfg: &ServerConfig, window: &str) -> Option<String> {
    let hay = window.to_lowercase();
    cfg.phone_categories
        .iter()
        .find_map(|rule| {
            let (needle, cat) = rule.split_once('=')?;
            hay.contains(&needle.trim().to_lowercase())
                .then(|| cat.trim().to_string())
        })
        .filter(|c| cfg.categories.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ServerConfig {
        ServerConfig::default()
    }

    #[test]
    fn parses_an_array_keyed_by_ts() {
        let out = parse_labels(
            r#"[{"ts":100,"category":"idle","tags":[],"project":null,"detail":"away"},
                {"ts":160,"category":"work_personal","tags":["youtube"],"detail":"hacking"}]"#,
            &cfg(),
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 100);
        assert_eq!(out[1].1.category, "work_personal");
        // The category joins its own tag list even when the model forgets it.
        assert!(out[1].1.tags.contains(&"work_personal".to_string()));
        assert!(out[1].1.tags.contains(&"youtube".to_string()));
    }

    #[test]
    fn survives_a_fence_and_prose() {
        let out = parse_labels(
            "Here you go:\n```json\n[{\"ts\":7,\"category\":\"idle\"}]\n```\nHope that helps!",
            &cfg(),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 7);
    }

    /// The reason `minimax-m3` was written off; it is 2.5x cheaper on output.
    #[test]
    fn survives_a_think_block_full_of_braces() {
        let out = parse_labels(
            "<think>Maybe {\"category\": \"idle\"}? No, [1,2,3] suggests work.</think>\
             [{\"ts\":9,\"category\":\"work_husk\",\"tags\":[\"work_husk\"]}]",
            &cfg(),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.category, "work_husk");
    }

    #[test]
    fn an_unclosed_think_block_is_not_an_answer() {
        assert!(parse_labels("<think>still thinking about [{\"ts\":1}", &cfg()).is_err());
    }

    #[test]
    fn folds_an_invented_category_into_other() {
        let out = parse_labels(r#"[{"ts":1,"category":"gardening","tags":["gardening"]}]"#, &cfg())
            .unwrap();
        assert_eq!(out[0].1.category, "other");
        assert_eq!(out[0].1.tags, vec!["other".to_string()]);
    }

    fn item(ts: i64, jpeg: Option<&'static [u8]>) -> Item<'static> {
        Item {
            ts,
            jpeg,
            window: "w",
            domain: None,
            presence: Presence {
                device: "d",
                idle_secs: None,
                keys: 0,
                mouse: 0,
                note: None,
            },
        }
    }

    #[test]
    fn a_batch_without_pictures_goes_to_the_cheap_model() {
        let cfg = cfg();
        assert_eq!(model_for(&cfg, &[item(1, None), item(2, None)]), cfg.model_text);
    }

    /// The safety half of the split: a text-only model must never be handed a
    /// screenshot, so one image in the batch is enough to route the whole call
    /// to the vision model.
    #[test]
    fn one_picture_sends_the_whole_batch_to_the_vision_model() {
        let cfg = cfg();
        assert_eq!(
            model_for(&cfg, &[item(1, None), item(2, Some(b"jpeg"))]),
            cfg.model
        );
    }

    #[test]
    fn accepts_a_bare_object_for_a_single_minute() {
        let out = parse_labels(r#"{"ts":42,"category":"idle"}"#, &cfg()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 42);
    }
}
