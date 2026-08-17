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
/// device and timestamp that say which minute it belongs to. Order alone would
/// be enough if models never dropped or reordered an entry, which they do; and
/// a batch spans every device now, so the timestamp alone is not a key either.
/// `device` is optional only for the batch-of-one-machine case, where it is
/// unambiguous.
///
/// `same` is the compression: a minute that continues the previous minute's
/// activity on the same device comes back as `{"device", "ts", "same": true}`
/// and nothing else, and the parser copies that device's previous label in
/// this response forward verbatim. A continuing minute is ~15 output tokens
/// instead of a full object -- most minutes continue, so most of the decode
/// disappears -- and deciding where "same" stops IS the temporal judgment.
/// The label fields are individually optional because a `same` row omits
/// them; a full row must still carry a category or it is skipped.
#[derive(Debug, Deserialize)]
struct Row {
    #[serde(default)]
    device: Option<String>,
    ts: i64,
    #[serde(default)]
    same: bool,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

/// A device's most recent labelled minute, passed back into the next call.
/// One line per device, and it's what stops labels flickering between
/// categories during a single continuous activity.
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

/// One minute to be labelled. A call takes a run of these across every device
/// at once, sorted by (ts, device) so simultaneity is visible.
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
        "You are labelling minutes of one person's day, observed across ALL of \
their devices at once, to be aggregated into a chart of where the time went.

The question is never \"what is on this screen\" -- it is \"what was THE \
PERSON doing during this minute\". You decide; there is no rule that \
overrides your judgment. Choose one category per minute from:
{}

## One person, several devices

The timeline below interleaves every device, in time order, and items marked \
\"same minute\" happened simultaneously. There is one person behind them all, \
so correlate the devices the way a human would:

- Key presses and pointer movements are counted from physical input devices \
and say where the hands actually were. Typing on one machine while a video \
plays on another means the typing is the foreground activity and the video is \
background -- categorise the typing, tag the video.
- A video playing with no input on any device is probably being watched.
- A long stretch of zero input everywhere while screens merely change -- \
builds scrolling, videos looping, AI agents editing files -- is absence: \
label those minutes \"idle\" however work-like the windows look.
- A device whose idle time is UNKNOWN cannot count input either (a phone \
reports neither), so its zeros mean nothing; the foreground app is the whole \
signal there.
- Recent input with a still screen usually means reading or thinking, which \
is real activity, not \"idle\".

## Categories are only chart buckets

The list exists so time can be charted, not to constrain what you may say \
happened. When nothing on it genuinely fits, use \"other\" -- and put the \
natural one-word category you would have used FIRST in \"tags\", so the list \
can be grown from real data later. Never force a bad fit.

\"tags\" otherwise lists everything genuinely active this minute, from the \
list, the category itself included. Audible media, a visible video, a live \
chat all count; a minimised window or an idle tab is not an activity. Tags \
must ALSO always name the specific app, service or site in use as its own \
lowercase word (snapchat, discord, whatsapp, netflix, youtube, jodel, \
gmail, ...) whenever one is identifiable from the window title, package name \
or the screen -- the chart splits every category by that word.

## The other fields

- \"project\" is the repository, course, or topic if you can identify one, \
else null -- except communication, streaming and social minutes, where it is \
the app's name (\"discord\", \"snapchat\") so the drill-down groups by app.
- \"detail\" is ONE concrete sentence naming specifics: file paths, repo \
names, page titles, what a terminal is running. It is what a later reader \
sees without the screenshot, so be specific. For communication, name who the \
conversation is with when the screen shows it -- contact, channel or server \
(\"chatting with Anna on Discord #general\"). When the person is away, say \
what the machine was doing instead.
- Each device's previous label is given at the top. Consecutive minutes of \
one continuous activity keep the same category and project -- flickering \
mid-activity is the most common way these labels go wrong.

## Output

Respond with a JSON array only, no markdown fence, one object per (device, \
ts) item. Every item you are given must appear in the array; never merge, \
drop or invent items. For each device's FIRST minute, and for every minute \
where the activity changes, give the full object, with \"detail\" at most \
12 words:
[{{\"device\": \"pc\", \"ts\": 1234567890, \"category\": \"...\", \
\"tags\": [\"...\"], \"project\": \"...\" or null, \"detail\": \"...\"}}]
For a minute that CONTINUES the same continuous activity as that device's \
previous minute, output only
{{\"device\": \"pc\", \"ts\": 1234567950, \"same\": true}}
and no other fields -- the previous label is carried forward verbatim. If a \
device's first minute simply continues the \"previous label\" given above, \
it too may be {{\"same\": true}}. \"same\" is a claim that NOTHING \
meaningfully changed: a new conversation partner, a new file, a new video \
each count as a change and need a full object. Where you stop saying \
\"same\" is the temporal judgment this task asks for.",
        cfg.categories
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The per-item preambles, one per item in order, each sitting in front of
/// that minute's screenshot. Split out from `classify` so the same-minute
/// grouping is testable at the string level.
fn contexts(items: &[Item<'_>]) -> Vec<String> {
    let total = items.len();
    let mut out = Vec::with_capacity(total);
    let mut seen: Vec<&str> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let mut context = String::new();
        let p = &item.presence;

        // Once per device, not once per minute: repeating a machine's
        // description twenty times says nothing new at full price.
        if !seen.contains(&p.device) {
            seen.push(p.device);
            if let Some(note) = p.note.filter(|n| !n.trim().is_empty()) {
                context.push_str(&format!("About the machine \"{}\": {note}\n", p.device));
            }
        }
        let when = chrono::DateTime::from_timestamp(item.ts, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        // Simultaneity is the whole point of a cross-device batch, so it must
        // be legible in the text rather than left for the model to derive
        // from timestamp arithmetic across twenty items.
        if i > 0 && items[i - 1].ts / 60 == item.ts / 60 {
            context.push_str(&format!(
                "--- same minute, on {}: --- ts={} ({when})\n",
                p.device, item.ts
            ));
        } else {
            context.push_str(&format!(
                "--- Item {} of {total} --- ts={} ({when}), on {}\n",
                i + 1,
                item.ts,
                p.device
            ));
        }
        match p.idle_secs {
            Some(s) => context.push_str(&format!("Seconds since last human input: {s}\n")),
            // "no input device readable" was written for a desktop whose evdev
            // read failed. A phone has no such device to begin with and never
            // will, so the phrasing has to cover both without implying a fault.
            None => context.push_str(
                "Seconds since last human input: UNKNOWN (this device does not report it)\n",
            ),
        }
        context.push_str(&format!(
            "Human input this minute: {} key presses, {} pointer movements\n",
            p.keys, p.mouse
        ));
        context.push_str(&format!("Active window: {}", item.window));
        // "Firefox" tells the model nothing; the site being read tells it
        // almost everything, and reading it off the screenshot is guesswork
        // the browser itself can answer exactly.
        if let Some(d) = item.domain.filter(|d| !d.trim().is_empty()) {
            context.push_str(&format!("\nFocused browser tab is on: {d}"));
        }
        if item.jpeg.is_none() {
            // Otherwise the model is left to wonder which of the images below
            // belongs to this item.
            context.push_str("\n(no screenshot available for this minute)");
        }
        context.push('\n');
        out.push(context);
    }
    out
}

/// Input-token accounting for assembling a batch, shared by the live
/// collector and `reclassify` so both cut batches the same way. The numbers
/// are tied to the llama-server behind the endpoint: a batch whose input
/// overflows its slot is truncated or refused, losing every minute in it. A
/// 768px screenshot measures ~450 input tokens, a minute without one ~80
/// (window title plus presence lines), and the system prompt with the
/// previous-label block ~600. What a slot can take is deployment, not code
/// -- it moves when the GPU's context grows -- so the ceiling itself is
/// `ServerConfig::batch_token_budget`; only the per-minute estimates live
/// here.
pub const TOKENS_PER_IMAGE_MINUTE: u32 = 450;
pub const TOKENS_PER_TEXT_MINUTE: u32 = 80;
pub const TOKENS_FIXED: u32 = 600;

/// Estimated input cost of a batch: one bool per minute, true when that
/// minute carries a screenshot.
pub fn estimated_input_tokens(minutes_with_image: impl Iterator<Item = bool>) -> u32 {
    TOKENS_FIXED
        + minutes_with_image
            .map(|img| {
                if img {
                    TOKENS_PER_IMAGE_MINUTE
                } else {
                    TOKENS_PER_TEXT_MINUTE
                }
            })
            .sum::<u32>()
}

/// Which model gets this batch.
///
/// Vision is what makes the expensive model necessary, so a batch without a
/// single screenshot -- one that is all phone minutes, or rows the sweep
/// picked up after their image was gone -- goes to the text model instead;
/// note that a cross-device batch mixing one desktop screenshot in routes the
/// phone minutes to the vision model along with it. Decided
/// from the payload rather than the device name, so a desktop whose capture
/// failed is routed correctly too, and so a text-only model can never be sent
/// a picture of the user's screen by accident.
pub fn model_for<'a>(cfg: &'a ServerConfig, items: &[Item<'_>]) -> &'a str {
    if items.iter().any(|i| i.jpeg.is_some()) {
        &cfg.model
    } else {
        &cfg.model_text
    }
}

/// Label a run of minutes across every device, in time order, in one call.
///
/// One call for twenty minutes instead of twenty calls is not only twenty
/// times fewer requests -- the long system prompt is sent once instead of
/// twenty times, and the model gets to see the trajectory, which is what it
/// needs to tell "reading" from "left the room" in the first place. The run
/// deliberately mixes devices: input on one machine while a video plays on
/// another is one person working with something in the background, and only
/// a model that sees both screens in one timeline can say so.
///
/// `jpeg` is None for devices that cannot produce a screenshot -- a phone,
/// where Android forbids silent capture entirely. There the foreground app name
/// carries most of the signal anyway: "Instagram" says what you were doing in a
/// way "Firefox" never does on a desktop.
///
/// Minutes the model declines to label are simply absent from the result. They
/// keep their pending flag and the sweep offers them again, which is a great
/// deal safer than pairing labels to minutes by position.
///
/// `key` is None for endpoints without auth -- a local llama-swap -- and then
/// no Authorization header is sent at all.
///
/// The third element of the result is the raw reply text (content, plus the
/// reasoning half when the model split them), so the caller can keep an audit
/// trail the labels can later be re-derived or second-guessed from.
pub fn classify(
    cfg: &ServerConfig,
    key: Option<&str>,
    items: &[Item<'_>],
    prev: &[(&str, Previous<'_>)],
) -> Result<(Vec<((String, i64), Label)>, Usage, String)> {
    anyhow::ensure!(!items.is_empty(), "nothing to classify");

    let mut content: Vec<serde_json::Value> = Vec::with_capacity(items.len() * 2 + 2);
    if !prev.is_empty() {
        // Once, at the top, rather than woven into the items: each device gets
        // exactly one "where you left off" line, which is what carries an
        // activity across a batch boundary without repeating itself.
        let mut text = String::from(
            "Previous label for each device (its most recent labelled minute, \
             before this batch):\n",
        );
        for (device, p) in prev {
            text.push_str(&format!("- {device}: category={}", p.category));
            if let Some(proj) = p.project.filter(|s| !s.is_empty()) {
                text.push_str(&format!(", project={proj}"));
            }
            if let Some(d) = p.detail.filter(|s| !s.is_empty()) {
                text.push_str(&format!(", detail={d}"));
            }
            text.push('\n');
        }
        content.push(serde_json::json!({ "type": "text", "text": text }));
    }
    for (item, text) in items.iter().zip(contexts(items)) {
        content.push(serde_json::json!({ "type": "text", "text": text }));
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
            "Classify all {} items above. Return a JSON array of exactly {} objects \
             in the same order, each carrying back its own \"device\" and \"ts\", \
             using {{\"same\": true}} for minutes that continue the previous one.",
            items.len(),
            items.len()
        ),
    }));

    let model = model_for(cfg, items);

    let mut body = request_body(cfg, model, items.len());
    body["messages"] = serde_json::json!([
        { "role": "system", "content": system_prompt(cfg) },
        { "role": "user", "content": serde_json::Value::Array(content) }
    ]);

    let client = reqwest::blocking::Client::builder()
        // A full-screen image on a busy endpoint regularly takes well over a
        // minute, and a batch carries twenty of them. Timing out does not save
        // anything -- the minutes are simply lost -- so wait rather than give up.
        // Long enough for a 20-minute reasoning batch PLUS its wait in the
        // llama-swap queue behind other batches; 600 was the queue-starvation
        // timeout that killed the first backlog run.
        .timeout(std::time::Duration::from_secs(1800))
        .build()?;

    // One retry, because these failures are transient far more often than not
    // and a dropped minute leaves a permanent hole in the day.
    let mut last_err = String::new();
    for _ in 0..2 {
        let mut req = client.post(&cfg.endpoint);
        // Only when there is one: a local llama-swap has no key, and a Bearer
        // header carrying an empty secret is the kind of thing strict servers
        // reject outright.
        if let Some(key) = key {
            req = req.bearer_auth(key);
        }
        let sent = req.json(&body).send();

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
            format!(
                "parsing response: {}",
                text.chars().take(400).collect::<String>()
            )
        })?;
        let msg = &v["choices"][0]["message"];
        let content = msg["content"].as_str().unwrap_or("");
        // A reasoning model splits its reply in two, and which half the answer
        // lands in is the gateway's decision, not ours: normally the JSON is in
        // `content` and the narration in `reasoning_content`, but a reply that
        // stops early can leave `content` empty with the whole answer stranded
        // in the other field. Reading only the first field threw away a batch
        // whose labels were right there.
        let reasoning = msg["reasoning_content"].as_str().unwrap_or("");
        let usage = Usage {
            prompt: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            completion: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            model: model.to_string(),
        };
        // Both halves, because either may hold the answer, and the audit row
        // exists precisely so a later reading can disagree with this one.
        let raw = if reasoning.is_empty() {
            content.to_string()
        } else {
            format!("{content}\n--- reasoning ---\n{reasoning}")
        };
        // A batch from a single machine forgives replies that omit "device":
        // there is only one it could mean.
        let lone = {
            let mut devices = items.iter().map(|i| i.presence.device);
            let first = devices.next();
            if devices.all(|d| Some(d) == first) {
                first
            } else {
                None
            }
        };
        match parse_labels(content, cfg, lone, prev) {
            Ok(rows) => return Ok((rows, usage, raw)),
            Err(e) => match parse_labels(reasoning, cfg, lone, prev) {
                Ok(rows) if !rows.is_empty() => return Ok((rows, usage, raw)),
                // Neither field held an array. Say enough to tell the three
                // causes apart without ever logging the reply itself, which
                // describes what was on screen.
                _ => {
                    last_err = format!(
                        "{e} (finish_reason={}, content={}B, reasoning={}B, completion={} tokens)",
                        v["choices"][0]["finish_reason"].as_str().unwrap_or("?"),
                        content.len(),
                        reasoning.len(),
                        usage.completion,
                    );
                    continue;
                }
            },
        }
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
        match after
            .find("</")
            .and_then(|c| after[c..].find('>').map(|e| open + c + e + 1))
        {
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
///
/// `same` rows are expanded here: the row gets that device's previous label
/// in this response, verbatim -- which is the deliberate trade of per-minute
/// `detail` granularity for a decode that no longer repeats an unchanged
/// label twenty times. A device's FIRST row may also be `same`: it then
/// continues the "previous label" block the prompt opened with, i.e. `prev`
/// -- the same map that built that block, so the parser and the prompt can
/// never disagree about what "previous" meant. (`Previous` carries no tags,
/// so a label continued across the batch boundary restarts its tag list at
/// just the category; the next full row rebuilds it.)
///
/// The guards fail rows, never the batch: a `same` row for a device with no
/// previous row here and none in `prev` has nothing to continue and is
/// logged and skipped, as is a full row without a category; either minute
/// stays pending and the caller logs the shortfall. A `same` row that also
/// carries label fields is trusted on the `same` and the fields are ignored.
///
/// `lone_device` is Some when the whole batch came from one machine; a row
/// missing its "device" is then unambiguous and accepted. In a mixed batch
/// such a row is dropped instead -- its minute stays pending, which beats
/// guessing which machine it belonged to.
fn parse_labels(
    content: &str,
    cfg: &ServerConfig,
    lone_device: Option<&str>,
    prev: &[(&str, Previous<'_>)],
) -> Result<Vec<((String, i64), Label)>> {
    // An unterminated think block strips to nothing. That is right when the
    // narration ran out of budget mid-sentence, and wrong when a stray "<think"
    // appears after an array that was already complete -- so only accept the
    // stripped form if it still has something to parse.
    let stripped = strip_reasoning(content);
    let content = if stripped.trim().is_empty() {
        content
    } else {
        stripped
    };
    let json = match (content.find('['), content.rfind(']')) {
        (Some(s), Some(e)) if e > s => &content[s..=e],
        _ => match (content.find('{'), content.rfind('}')) {
            (Some(s), Some(e)) if e > s => &content[s..=e],
            // Not the reply itself: it describes what was on the user's screen,
            // and this line goes to a pod log. The caller appends the shape of
            // the response, which is what actually diagnoses this.
            _ => bail!("no JSON in model output"),
        },
    };

    let rows: Vec<Row> = if json.starts_with('[') {
        serde_json::from_str(json).with_context(|| format!("parsing label array: {json}"))?
    } else {
        vec![serde_json::from_str(json).with_context(|| format!("parsing label JSON: {json}"))?]
    };

    // Every field except the key is optional now, so brace-shaped debris (a
    // truncated think block, say) can deserialize without containing a single
    // label. That is a failed parse, not an empty answer -- the caller should
    // go on to try the reasoning half -- whereas rows skipped further down
    // for ambiguity or a dangling "same" are answers we refused to place.
    if !rows.is_empty() && rows.iter().all(|r| !r.same && r.category.is_none()) {
        bail!("rows carry neither \"same\" nor a category");
    }

    // What "the previous minute's activity" resolves to, per device. Seeded
    // from the prompt's previous-label block so a batch can open with a
    // `same` row, then updated by every full row in reply order.
    let mut last: std::collections::HashMap<String, Label> = prev
        .iter()
        .map(|(device, p)| {
            (
                device.to_string(),
                clean(
                    Label {
                        category: p.category.to_string(),
                        project: p.project.map(str::to_string),
                        detail: p.detail.map(str::to_string),
                        tags: Vec::new(),
                    },
                    cfg,
                ),
            )
        })
        .collect();

    let mut out = Vec::new();
    for r in rows {
        let Some(device) = r.device.or_else(|| lone_device.map(str::to_string)) else {
            continue;
        };
        let label = if r.same {
            // "same" wins even when label fields tagged along: the claim of
            // continuity is the judgment asked for, and half-filled extras
            // next to it are decoration, not a second answer.
            match last.get(&device) {
                Some(l) => l.clone(),
                None => {
                    // Only device and timestamp, never the label: this goes
                    // to a pod log, and labels describe the user's screen.
                    eprintln!(
                        "classify: {device} {} says \"same\" with nothing to continue, skipping",
                        r.ts
                    );
                    continue;
                }
            }
        } else {
            let Some(category) = r.category else {
                eprintln!(
                    "classify: {device} {} has neither \"same\" nor a category, skipping",
                    r.ts
                );
                continue;
            };
            clean(
                Label {
                    category,
                    project: r.project,
                    detail: r.detail,
                    tags: r.tags,
                },
                cfg,
            )
        };
        last.insert(device.clone(), label.clone());
        out.push(((device, r.ts), label));
    }
    Ok(out)
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

    // Tags keep their order and are allowed off the list, on purpose: when
    // the category fell back to `other`, the model's first tag is the name it
    // would have used, and that is precisely the data the category list is
    // grown from later. Filtering it out here would make `other` a dead end.
    let mut tags: Vec<String> = Vec::new();
    for t in &label.tags {
        let t = t.trim().to_lowercase();
        if !t.is_empty() && !tags.contains(&t) {
            tags.push(t);
        }
    }
    // The category is always part of what was happening, even if the model
    // omitted it from the list.
    if !tags.contains(&label.category) {
        tags.push(label.category.clone());
    }
    label.tags = tags;

    label.project = label.project.filter(|s| !s.trim().is_empty());
    label.detail = label.detail.filter(|s| !s.trim().is_empty());
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ServerConfig {
        ServerConfig::default()
    }

    #[test]
    fn parses_an_array_keyed_by_device_and_ts() {
        let out = parse_labels(
            r#"[{"device":"pc","ts":100,"category":"idle","tags":[],"project":null,"detail":"away"},
                {"device":"phone","ts":100,"category":"work_personal","tags":["youtube"],"detail":"hacking"}]"#,
            &cfg(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        // Two devices may answer for the same timestamp; the pair is the key.
        assert_eq!(out[0].0, ("pc".to_string(), 100));
        assert_eq!(out[1].0, ("phone".to_string(), 100));
        assert_eq!(out[1].1.category, "work_personal");
        // The category joins its own tag list even when the model forgets it.
        assert!(out[1].1.tags.contains(&"work_personal".to_string()));
        assert!(out[1].1.tags.contains(&"youtube".to_string()));
    }

    /// The compression this whole format exists for: a `same` row copies that
    /// device's previous label in the response -- per device, so interleaved
    /// machines never bleed into each other.
    #[test]
    fn same_carries_the_previous_label_forward_per_device() {
        let out = parse_labels(
            r#"[{"device":"pc","ts":60,"category":"work_husk","tags":["work_husk"],"project":"time","detail":"editing db.rs"},
                {"device":"phone","ts":60,"category":"youtube","tags":["youtube"],"project":"youtube","detail":"video"},
                {"device":"pc","ts":120,"same":true},
                {"device":"phone","ts":120,"same":true},
                {"device":"pc","ts":180,"same":true}]"#,
            &cfg(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(out.len(), 5);
        let get = |dev: &str, ts: i64| {
            &out.iter()
                .find(|((d, t), _)| d == dev && *t == ts)
                .unwrap()
                .1
        };
        assert_eq!(get("pc", 120).category, "work_husk");
        assert_eq!(get("pc", 120).project.as_deref(), Some("time"));
        assert_eq!(get("pc", 120).detail.as_deref(), Some("editing db.rs"));
        assert_eq!(get("phone", 120).category, "youtube");
        // A chain of `same` keeps carrying the same full label.
        assert_eq!(get("pc", 180).category, "work_husk");
    }

    /// A device's first row may be `same` only when the prompt's
    /// previous-label block gave it something to continue -- the same `prev`
    /// map the prompt was built from, so prompt and parser cannot disagree.
    #[test]
    fn a_leading_same_continues_the_prompts_previous_label() {
        let prev = [(
            "pc",
            Previous {
                category: "work_husk",
                project: Some("time"),
                detail: Some("editing db.rs"),
            },
        )];
        let out = parse_labels(
            r#"[{"device":"pc","ts":60,"same":true}]"#,
            &cfg(),
            None,
            &prev,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, ("pc".to_string(), 60));
        assert_eq!(out[0].1.category, "work_husk");
        assert_eq!(out[0].1.project.as_deref(), Some("time"));
    }

    /// With no previous label anywhere, a leading `same` has nothing to
    /// continue: the row is dropped and the minute stays pending. Other
    /// devices' rows are unaffected.
    #[test]
    fn a_leading_same_with_nothing_to_continue_stays_pending() {
        let out = parse_labels(
            r#"[{"device":"pc","ts":60,"same":true},
                {"device":"phone","ts":60,"category":"idle"}]"#,
            &cfg(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, ("phone".to_string(), 60));
    }

    /// `same` is the answer even when label fields tag along beside it, and a
    /// row claiming neither `same` nor a category has said nothing at all.
    #[test]
    fn same_beats_stray_fields_and_an_empty_row_is_skipped() {
        let out = parse_labels(
            r#"[{"device":"pc","ts":60,"category":"work_husk","tags":["work_husk"]},
                {"device":"pc","ts":120,"same":true,"category":"youtube"},
                {"device":"pc","ts":180}]"#,
            &cfg(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(out.len(), 2, "the empty row is skipped, not an error");
        assert_eq!(out[1].0, ("pc".to_string(), 120));
        assert_eq!(
            out[1].1.category, "work_husk",
            "same wins over the stray category"
        );
    }

    /// A batch from a single machine forgives a reply without "device"; a
    /// mixed batch cannot, and drops the row rather than guessing.
    #[test]
    fn a_single_device_batch_tolerates_rows_missing_device() {
        let reply = r#"[{"ts":100,"category":"idle"},{"ts":160,"same":true}]"#;
        let out = parse_labels(reply, &cfg(), Some("laptop"), &[]).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, ("laptop".to_string(), 100));
        // The carry-forward works for device-less rows too.
        assert_eq!(out[1].0, ("laptop".to_string(), 160));
        assert_eq!(out[1].1.category, "idle");

        let out = parse_labels(reply, &cfg(), None, &[]).unwrap();
        assert!(out.is_empty(), "ambiguous rows are dropped, not guessed");
    }

    #[test]
    fn survives_a_fence_and_prose() {
        let out = parse_labels(
            "Here you go:\n```json\n[{\"device\":\"pc\",\"ts\":7,\"category\":\"idle\"}]\n```\nHope that helps!",
            &cfg(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, ("pc".to_string(), 7));
    }

    /// The reason `minimax-m3` was written off; it is 2.5x cheaper on output.
    #[test]
    fn survives_a_think_block_full_of_braces() {
        let out = parse_labels(
            "<think>Maybe {\"category\": \"idle\"}? No, [1,2,3] suggests work.</think>\
             [{\"device\":\"pc\",\"ts\":9,\"category\":\"work_husk\",\"tags\":[\"work_husk\"]}]",
            &cfg(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.category, "work_husk");
    }

    #[test]
    fn an_unclosed_think_block_is_not_an_answer() {
        assert!(
            parse_labels("<think>still thinking about [{\"ts\":1}", &cfg(), None, &[]).is_err()
        );
    }

    /// The category folds into `other` -- an invented one cannot be charted --
    /// but the invented name survives, first, in the tags: that ordering is
    /// what the category list is grown from later.
    #[test]
    fn an_invented_category_folds_to_other_but_survives_in_tags() {
        let out = parse_labels(
            r#"[{"ts":1,"category":"gardening","tags":["gardening","music"]}]"#,
            &cfg(),
            Some("pc"),
            &[],
        )
        .unwrap();
        assert_eq!(out[0].1.category, "other");
        assert_eq!(
            out[0].1.tags,
            vec![
                "gardening".to_string(),
                "music".to_string(),
                "other".to_string()
            ]
        );
    }

    /// The dashboard's per-app drill-down hangs on these instructions: the
    /// service tagged as its own word, comms projects carrying the app name,
    /// and the counterpart named in detail.
    #[test]
    fn the_prompt_asks_for_app_attribution() {
        let p = system_prompt(&cfg());
        assert!(p.contains("lowercase word"), "app-as-tag line missing");
        assert!(p.contains("snapchat, discord"), "app examples missing");
        assert!(
            p.contains("the app's name"),
            "comms project convention missing"
        );
        assert!(
            p.contains("who the conversation is with"),
            "counterpart-in-detail line missing"
        );
    }

    /// The output contract the parser depends on: the `same` shorthand, its
    /// leading-row form, and that it must be an explicit claim of continuity.
    #[test]
    fn the_prompt_defines_the_same_shorthand() {
        let p = system_prompt(&cfg());
        assert!(p.contains("\"same\": true"), "same example missing");
        assert!(
            p.contains("NOTHING meaningfully changed"),
            "continuity-claim line missing"
        );
        assert!(
            p.contains("first minute simply continues"),
            "leading-same-from-previous-label line missing"
        );
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
        assert_eq!(
            model_for(&cfg, &[item(1, None), item(2, None)]),
            cfg.model_text
        );
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
        let out = parse_labels(r#"{"ts":42,"category":"idle"}"#, &cfg(), Some("pc"), &[]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, ("pc".to_string(), 42));
    }

    /// The grouping the whole cross-device design hangs on: two items in the
    /// same minute must say so in words, with the device named, rather than
    /// leaving simultaneity for the model to derive from timestamps.
    #[test]
    fn contexts_mark_items_sharing_a_minute() {
        let mut a = item(120, None);
        a.presence.device = "pc";
        let mut b = item(120, None);
        b.presence.device = "laptop";
        let mut c = item(180, None);
        c.presence.device = "pc";
        let texts = contexts(&[a, b, c]);
        assert!(texts[0].contains("--- Item 1 of 3 ---"), "{}", texts[0]);
        assert!(texts[0].contains("on pc"), "{}", texts[0]);
        assert!(texts[1].contains("same minute, on laptop"), "{}", texts[1]);
        // A later minute goes back to being an ordinary item.
        assert!(texts[2].contains("--- Item 3 of 3 ---"), "{}", texts[2]);
    }

    /// Each machine's note appears once, the first time the device does --
    /// not per minute, and not attached to the wrong machine.
    #[test]
    fn a_device_note_is_sent_once_per_device() {
        let mut a = item(60, None);
        a.presence.note = Some("runs agents");
        let mut b = item(120, None);
        b.presence.note = Some("runs agents");
        let mut c = item(120, None);
        c.presence.device = "phone";
        let texts = contexts(&[a, b, c]);
        assert!(texts[0].contains("About the machine \"d\": runs agents"));
        assert!(!texts[1].contains("About the machine"));
        assert!(!texts[2].contains("About the machine"));
    }

    #[test]
    fn budget_override_beats_the_name_heuristic() {
        let mut cfg = cfg();
        // The heuristic would say 1500 for deepseek and 400 for anything else;
        // the config, when set, wins over both.
        assert_eq!(per_minute_budget(&cfg, "deepseek-v4-flash"), 1500);
        assert_eq!(per_minute_budget(&cfg, "time-vision"), 400);
        cfg.max_tokens_per_minute = Some(1600);
        assert_eq!(per_minute_budget(&cfg, "deepseek-v4-flash"), 1600);
        assert_eq!(per_minute_budget(&cfg, "time-vision"), 1600);
    }

    /// A reply that exactly follows the enforced schema must round-trip
    /// through the parser: the schema and `parse_labels` are two descriptions
    /// of one shape, and this is what keeps them from drifting apart.
    #[test]
    fn schema_shaped_reply_parses() {
        let schema = response_format();
        assert_eq!(schema["type"], "json_schema");
        for field in [
            "device", "ts", "same", "category", "tags", "project", "detail",
        ] {
            assert!(
                schema["json_schema"]["schema"]["items"]["properties"][field].is_object(),
                "schema is missing {field}"
            );
        }
        // Only the key may be required: a `same` row omits every label field,
        // so a grammar demanding `category` would forbid the shorthand.
        assert_eq!(
            schema["json_schema"]["schema"]["items"]["required"],
            serde_json::json!(["device", "ts"])
        );
        let out = parse_labels(
            r#"[{"device":"pc","ts":100,"category":"idle","tags":["idle"],"project":null,"detail":"away"},
                {"device":"pc","ts":160,"same":true}]"#,
            &cfg(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(out[0].1.category, "idle");
        assert_eq!(out[0].0, ("pc".to_string(), 100));
        assert_eq!(out[1].1.category, "idle");
    }

    /// `thinking = false` must reach the chat template; anything else must
    /// leave the request alone so the model's own default wins.
    #[test]
    fn thinking_false_disables_thinking_in_the_request() {
        let mut cfg = cfg();
        let body = request_body(&cfg, "time-vision", 20);
        assert!(
            body.get("chat_template_kwargs").is_none(),
            "unset thinking must not touch the template"
        );
        cfg.thinking = Some(true);
        let body = request_body(&cfg, "time-vision", 20);
        assert!(
            body.get("chat_template_kwargs").is_none(),
            "explicit true is also the model default; send nothing"
        );
        cfg.thinking = Some(false);
        let body = request_body(&cfg, "time-vision", 20);
        assert_eq!(
            body["chat_template_kwargs"],
            serde_json::json!({ "enable_thinking": false })
        );
    }

    /// The output cap assumes a full object per three minutes plus 15 tokens
    /// per `same` row plus reasoning headroom -- and however the numbers move,
    /// a 60-minute thinking batch must clear its worst case: 60 full objects
    /// (a model that ignores the shorthand, ~200 tokens each) plus a maximal
    /// 3072-token think block.
    #[test]
    fn max_tokens_scales_by_thirds_not_per_minute() {
        let mut cfg = cfg();
        // Non-reasoning heuristic budget is 400/object.
        assert_eq!(
            max_tokens(&cfg, "time-vision", 60),
            400 * 22 + 15 * 60 + 4096
        );
        // The deployed shape: an explicit budget for a thinking model behind
        // an alias. Even the worst case -- a model that ignores the shorthand
        // and writes 60 full objects (~200 tokens each) after a maximal
        // 3072-token think block -- must clear the cap with room to spare.
        cfg.max_tokens_per_minute = Some(1600);
        assert!(max_tokens(&cfg, "time-vision", 60) >= 60 * 200 + 3072);
        // A batch of one is still generous enough for a full think block.
        assert!(max_tokens(&cfg, "time-vision", 1) >= 3072 + 200);
    }

    /// The input estimate the batch builders cut on: screenshots dominate, so
    /// 60 text minutes fit the default budget while screenshot minutes hit
    /// the ceiling after ~27.
    #[test]
    fn input_estimate_flags_image_heavy_batches() {
        let budget = cfg().batch_token_budget;
        assert_eq!(
            estimated_input_tokens((0..60).map(|_| false)),
            TOKENS_FIXED + 60 * TOKENS_PER_TEXT_MINUTE
        );
        assert!(estimated_input_tokens((0..60).map(|_| false)) < budget);
        assert!(estimated_input_tokens((0..27).map(|_| true)) <= budget);
        assert!(estimated_input_tokens((0..28).map(|_| true)) > budget);
    }
}

/// The request minus the messages: model, output cap, and the switches.
fn request_body(cfg: &ServerConfig, model: &str, n: usize) -> serde_json::Value {
    // No temperature, top_p or top_k: the server's own sampling defaults must
    // win. Greedy decoding is a documented endless-repetition failure mode for
    // the Qwen thinking models this runs against, and a hardcoded 0 here once
    // forced exactly that.
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens(cfg, model, n),
    });
    // Verified against llama-server: the Qwen chat template takes
    // `enable_thinking`, and with it false the reply carries no
    // reasoning_content at all. Only ever sent as an explicit false; None
    // (the config default) leaves the model's own default alone, which is
    // right for live classification -- the thinking is what buys the
    // presence judgment (see DESIGN.md) -- and Some(false) exists for
    // backfills where the GPU bill outweighs it.
    if cfg.thinking == Some(false) {
        body["chat_template_kwargs"] = serde_json::json!({ "enable_thinking": false });
    }
    if cfg.json_schema {
        body["response_format"] = response_format();
    }
    body
}

/// Output cap for a batch of `n` minutes.
///
/// With `same` compression the output no longer scales like n full label
/// objects: most minutes continue their neighbour and cost ~15 tokens of
/// `{"device", "ts", "same": true}`. So assume a full object for a third of
/// the minutes -- a generous rate of activity changes -- at the per-minute
/// budget, plus 15 tokens for every minute as if it were also a `same` row,
/// plus 4096 of headroom so a full reasoning block (the server backstops
/// thinking at 3072) and the JSON can never starve; the `n / 3 + 2` keeps a
/// tiny batch generous too. Too low truncates the array and costs the whole
/// batch, which is far more expensive than the unused tokens this leaves on
/// the table -- the cap reserves nothing and is not billed unless used.
fn max_tokens(cfg: &ServerConfig, model: &str, n: usize) -> u32 {
    let n = n as u32;
    per_minute_budget(cfg, model) * (n / 3 + 2) + 15 * n + 4096
}

/// Output tokens to allow per full label object in a batch.
///
/// A reasoning model spends its budget thinking before it writes anything, and
/// that thinking counts against `max_tokens` even though it arrives in a
/// separate `reasoning_content` field that this code never reads. One minute
/// fits in a small budget; twenty starve, and the model returns an empty
/// `content` having spent everything on deliberation -- which reads here as
/// "no JSON in model output" and loses the whole batch.
///
/// The cap reserves nothing and is not billed unless used, so the reasoning
/// budget is deliberately generous.
fn per_minute_budget(cfg: &ServerConfig, model: &str) -> u32 {
    // The config wins when set: behind a llama-swap alias like "time-vision"
    // the name says nothing about whether the model reasons, so the substring
    // guess below has nothing to go on.
    if let Some(n) = cfg.max_tokens_per_minute {
        return n;
    }
    // Named rather than inferred: a model that reasons is a property of the
    // model, and guessing from the id would silently mis-size a new one.
    const REASONING: [&str; 3] = ["deepseek", "minimax", "think"];
    let m = model.to_lowercase();
    if REASONING.iter().any(|r| m.contains(r)) {
        1500
    } else {
        400
    }
}

/// OpenAI-style `response_format` describing exactly the array `parse_labels`
/// expects. llama.cpp's llama-server compiles this to a grammar and the reply
/// cannot then be anything but the array -- no fences, no prose, no think
/// block. The prompt keeps describing the same shape in words, so switching
/// this off (or a gateway ignoring it) changes nothing.
fn response_format() -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "labels",
            "strict": true,
            "schema": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "device": { "type": "string" },
                        "ts": { "type": "integer" },
                        "same": { "type": "boolean" },
                        "category": { "type": "string" },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "project": { "type": ["string", "null"] },
                        "detail": { "type": "string" }
                    },
                    // Only the key is enforced: a `same` row legitimately
                    // omits every label field, so "either same=true or a
                    // category" is the parser's check, not the grammar's.
                    "required": ["device", "ts"],
                    "additionalProperties": false
                }
            }
        }
    })
}
