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

fn system_prompt(cfg: &ServerConfig) -> String {
    format!(
        "You are labelling one minute of a developer's day, to be aggregated \
into a chart of where their time went.

The question is NOT \"what is on this screen\". It is \"what was THE PERSON \
doing during this minute\". Those differ more often than you would expect, and \
getting the difference right is the whole job.

Choose exactly one category:
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
and previous label, and be more cautious about claiming active work.

## The other fields

- \"project\" is the repository, course, or topic if you can identify one, else \
null.
- \"detail\" is ONE concrete sentence naming specifics: file paths, repo names, \
page titles, what a terminal is running. This is the only record that survives \
after the screenshot is deleted, so be specific rather than vague. When the \
person is away, say what the machine was doing instead, e.g. \"away; an agent \
was editing files in the terminal\".
- Use \"other\" when nothing on the list genuinely fits. Do not force a bad fit.
- If the previous minute's label is given and this minute continues the same \
activity, reuse the same category and project.

Respond with JSON only, no markdown fence:
{{\"category\": \"...\", \"project\": \"...\" or null, \"detail\": \"...\"}}",
        cfg.categories
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

pub fn classify(
    cfg: &ServerConfig,
    key: &str,
    jpeg: &[u8],
    window: &str,
    presence: Presence<'_>,
    prev: Option<Previous<'_>>,
) -> Result<Label> {
    let mut context = String::new();

    if let Some(note) = presence.note.filter(|n| !n.trim().is_empty()) {
        context.push_str(&format!("About this machine: {note}\n"));
    }
    context.push_str(&format!("Machine: {}\n", presence.device));
    match presence.idle_secs {
        Some(s) => context.push_str(&format!("Seconds since last human input: {s}\n")),
        None => context.push_str(
            "Seconds since last human input: UNKNOWN (no input device readable)\n",
        ),
    }
    context.push_str(&format!(
        "Human input this minute: {} key presses, {} pointer movements\n",
        presence.keys, presence.mouse
    ));
    context.push_str(&format!("Active window: {window}"));

    if let Some(p) = prev {
        context.push_str(&format!("\nPrevious minute: category={}", p.category));
        if let Some(proj) = p.project.filter(|s| !s.is_empty()) {
            context.push_str(&format!(", project={proj}"));
        }
        if let Some(d) = p.detail.filter(|s| !s.is_empty()) {
            context.push_str(&format!(", detail={d}"));
        }
    }
    context.push_str("\n\nClassify this minute.");

    let data_url = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg)
    );

    let body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0,
        "max_tokens": 300,
        "messages": [
            { "role": "system", "content": system_prompt(cfg) },
            { "role": "user", "content": [
                { "type": "text", "text": context },
                { "type": "image_url", "image_url": { "url": data_url } }
            ]}
        ]
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .build()?;

    let resp = client
        .post(&cfg.endpoint)
        .bearer_auth(key)
        .json(&body)
        .send()
        .context("calling the model endpoint")?;

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("model returned {}: {}", status, text.chars().take(400).collect::<String>());
    }

    let v: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing response: {}", text.chars().take(400).collect::<String>()))?;
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .context("no content in model response")?;

    parse_label(content, cfg)
}

/// Models wrap JSON in prose or fences often enough that trusting the raw body
/// is a guaranteed source of intermittent failures. Take the outermost braces.
fn parse_label(content: &str, cfg: &ServerConfig) -> Result<Label> {
    let start = content.find('{');
    let end = content.rfind('}');
    let json = match (start, end) {
        (Some(s), Some(e)) if e > s => &content[s..=e],
        _ => bail!("no JSON object in model output: {content}"),
    };

    let mut label: Label =
        serde_json::from_str(json).with_context(|| format!("parsing label JSON: {json}"))?;

    // An invented category would silently create a phantom pie slice, so fold
    // anything off-list into `other` rather than trusting the model here.
    let cat = label.category.trim().to_lowercase();
    label.category = if cfg.categories.iter().any(|c| c.to_lowercase() == cat) {
        cat
    } else {
        "other".into()
    };

    label.project = label.project.filter(|s| !s.trim().is_empty());
    label.detail = label.detail.filter(|s| !s.trim().is_empty());
    Ok(label)
}
