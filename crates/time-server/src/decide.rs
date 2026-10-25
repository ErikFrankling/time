//! The decision model: one question, one JSON answer, cached and logged.
//!
//! Used for the two judgements rules make badly: when Erik fell asleep and
//! woke up, and what he was doing in a gap between his input. It is asked
//! in batches (once a night, once per two hours with gaps), never per
//! minute. Every question and answer goes into the `decision` table: the
//! same question (same model, same prompt) is answered from there instead
//! of asking again, and a wrong decision can be read back exactly as the
//! model saw it (`/api/decisions`).

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time_core::clock::Millis;

use crate::config::DecideConfig;
use crate::store::Store;

pub struct Decider {
    pub config: DecideConfig,
    key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Answer {
    /// The `decision` row it came from.
    pub id: i64,
    pub value: Value,
    /// Answered from the table rather than by a new call.
    pub cached: bool,
}

/// One logged question, for `/api/decisions`.
#[derive(Debug, Clone, Serialize)]
pub struct Logged {
    pub id: i64,
    pub kind: String,
    pub key: String,
    pub model: String,
    pub prompt: String,
    pub answer: Option<String>,
    pub error: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub created_ms: Millis,
}

impl Decider {
    pub fn new(config: DecideConfig) -> Self {
        let key = std::env::var("TIME_DECIDE_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        Self { config, key }
    }

    /// Whether a new question can be asked at all.
    pub fn available(&self) -> bool {
        self.config.enabled && self.key.is_some()
    }

    /// The cached answer to exactly this question, if there is one.
    pub fn cached(
        &self,
        store: &Store,
        kind: &str,
        key: &str,
        system: &str,
        user: &str,
    ) -> Result<Option<Answer>> {
        let hash = self.hash(system, user);
        store.with(|c| {
            let row: Option<(i64, String)> = c
                .query_row(
                    "SELECT id, answer FROM decision
                     WHERE kind = ?1 AND key = ?2 AND input_hash = ?3 AND answer IS NOT NULL
                     ORDER BY id DESC LIMIT 1",
                    params![kind, key, hash],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            Ok(match row {
                Some((id, a)) => Some(Answer {
                    id,
                    value: serde_json::from_str(&a)?,
                    cached: true,
                }),
                None => None,
            })
        })
    }

    /// Ask, or answer from the table when the same question was asked.
    pub fn ask(
        &self,
        store: &Store,
        kind: &str,
        key: &str,
        system: &str,
        user: &str,
        now: Millis,
    ) -> Result<Answer> {
        if let Some(a) = self.cached(store, kind, key, system, user)? {
            return Ok(a);
        }
        if !self.available() {
            bail!("decision model off (enabled = false or no TIME_DECIDE_KEY)");
        }
        let prompt = format!("SYSTEM:\n{system}\n\nUSER:\n{user}");
        let result = self.call(kind, key, system, user);
        let hash = self.hash(system, user);
        let (answer, error, tin, tout) = match &result {
            Ok((v, tin, tout)) => (Some(v.to_string()), None, *tin, *tout),
            Err(e) => (None, Some(format!("{e:#}")), None, None),
        };
        let id = store.with(|c| {
            c.execute(
                "INSERT INTO decision (kind, key, input_hash, model, prompt, answer, error, input_tokens, output_tokens, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![kind, key, hash, self.config.model, prompt, answer, error, tin, tout, now],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        let (value, _, _) = result?;
        Ok(Answer {
            id,
            value,
            cached: false,
        })
    }

    fn hash(&self, system: &str, user: &str) -> String {
        let mut h = Sha256::new();
        for part in [self.config.model.as_str(), system, user] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn call(
        &self,
        kind: &str,
        key: &str,
        system: &str,
        user: &str,
    ) -> Result<(Value, Option<i64>, Option<i64>)> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()?;
        let body = json!({
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "response_format": {"type": "json_object"},
            "max_tokens": 16000,
        });
        let mut body = body;
        if let Some(e) = &self.config.reasoning_effort {
            body["reasoning_effort"] = json!(e);
        }
        let resp = client
            .post(&self.config.endpoint)
            .bearer_auth(self.key.as_deref().unwrap_or_default())
            // OpenCode Go routes by session; one per question.
            .header("x-opencode-session", format!("time-{kind}-{key}"))
            .json(&body)
            .send()
            .context("calling the decision model")?;
        let status = resp.status();
        let text = resp.text()?;
        if !status.is_success() {
            bail!(
                "decision model: HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&text).context("decision model reply is not JSON")?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .context("decision model reply has no content")?;
        let usage = |k: &str| v["usage"][k].as_i64();
        Ok((
            parse_json(content)?,
            usage("prompt_tokens"),
            usage("completion_tokens"),
        ))
    }
}

/// The JSON object in a reply, with or without a fence around it.
pub fn parse_json(content: &str) -> Result<Value> {
    let a = content.find('{').context("no JSON object in the answer")?;
    let b = content.rfind('}').context("no JSON object in the answer")?;
    Ok(serde_json::from_str(&content[a..=b])?)
}

/// The last questions of a kind, newest first.
pub fn log(
    store: &Store,
    kind: Option<&str>,
    key: Option<&str>,
    limit: usize,
) -> Result<Vec<Logged>> {
    store.with(|c| {
        let mut stmt = c.prepare(
            "SELECT id, kind, key, model, prompt, answer, error, input_tokens, output_tokens, created_ms
             FROM decision WHERE (?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR key = ?2)
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![kind, key, limit as i64], |r| {
            Ok(Logged {
                id: r.get(0)?,
                kind: r.get(1)?,
                key: r.get(2)?,
                model: r.get(3)?,
                prompt: r.get(4)?,
                answer: r.get(5)?,
                error: r.get(6)?,
                input_tokens: r.get(7)?,
                output_tokens: r.get(8)?,
                created_ms: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read_out_of_fences_and_cached_by_question() {
        assert_eq!(parse_json("```json\n{\"a\": 1}\n```").unwrap()["a"], 1);
        let store = Store::open_in_memory().unwrap();
        let d = Decider {
            config: DecideConfig::default(),
            key: None,
        };
        // Nothing asked yet and no key: an error, never a made-up answer.
        assert!(d.ask(&store, "sleep", "2026-10-24", "s", "u", 0).is_err());
        let hash = d.hash("s", "u");
        store
            .with(|c| {
                c.execute(
                    "INSERT INTO decision (kind, key, input_hash, model, prompt, answer, created_ms)
                     VALUES ('sleep', '2026-10-24', ?1, 'm', 'p', '{\"ok\":true}', 0)",
                    [hash],
                )?;
                Ok(())
            })
            .unwrap();
        let a = d.ask(&store, "sleep", "2026-10-24", "s", "u", 0).unwrap();
        assert!(a.cached);
        assert_eq!(a.value["ok"], true);
        // A different question is not answered from the cache.
        assert!(d
            .ask(&store, "sleep", "2026-10-24", "s", "other", 0)
            .is_err());
    }
}
