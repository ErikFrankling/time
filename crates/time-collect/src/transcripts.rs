//! The part of the Claude and Codex transcripts T3 Code has not scanned yet.
//!
//! T3 rescans the transcripts only while its usage page is open, so its cache
//! can be hours behind. It records, per transcript, how far it read and (for
//! Codex) the parser's state there; this goes on from that point the way T3's
//! own parsers (`claudeUsage.ts`, `codexUsage.ts`) do, so when T3 catches up
//! the same calls land in the same hours and nothing counts twice.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use time_core::clock::Millis;

use crate::pricing::Tokens;
use crate::usage::{Call, Cwd};

/// One call and the key T3 dedupes it by (Claude's message and request ids).
pub type Parsed = (Call, Option<String>);

fn ms(v: &Value) -> Option<Millis> {
    chrono::DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn count(v: &Value) -> u64 {
    v.as_f64()
        .filter(|n| n.is_finite() && *n > 0.0)
        .map_or(0, |n| n as u64)
}

/// A Claude Code transcript line with usage in it.
pub fn claude_line(v: &Value, cwd: &Cwd) -> Option<Parsed> {
    if v["type"] != "assistant" {
        return None;
    }
    let m = &v["message"];
    let u = m.get("usage")?;
    let at = ms(&v["timestamp"])?;
    let model = m["model"]
        .as_str()
        .filter(|s| !s.is_empty() && *s != "<synthetic>")?;
    let (id, req) = (m["id"].as_str(), v["requestId"].as_str());
    let key = (id.is_some() || req.is_some())
        .then(|| format!("{}:{}", id.unwrap_or(""), req.unwrap_or("")));
    Some((
        Call {
            provider: "claude".into(),
            model: model.to_owned(),
            rate_model: None,
            session: v["sessionId"].as_str().unwrap_or("").to_owned(),
            at,
            tokens: Tokens {
                uncached: count(&u["input_tokens"]),
                cache_read: count(&u["cache_read_input_tokens"]),
                cache_write: count(&u["cache_creation_input_tokens"]),
                output: count(&u["output_tokens"]),
            },
            reasoning: 0,
            reported_usd: v["costUSD"].as_f64().filter(|c| c.is_finite()),
            speed: u8::from(u["speed"] == "fast"),
            cwd: cwd.clone(),
        },
        key,
    ))
}

/// Codex's token counts: `last_token_usage` and `total_token_usage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CodexUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

impl CodexUsage {
    fn read(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        let n = |k: &str| o.get(k).map_or(Some(0), |x| x.as_u64());
        let input = o.get("input_tokens")?.as_u64()?;
        let output = o.get("output_tokens")?.as_u64()?;
        let u = Self {
            input_tokens: input,
            cached_input_tokens: n("cached_input_tokens")?,
            cache_write_input_tokens: n("cache_write_input_tokens")?,
            output_tokens: output,
            reasoning_output_tokens: n("reasoning_output_tokens")?,
            total_tokens: o
                .get("total_tokens")
                .map_or(Some(input + output), |x| x.as_u64())?,
        };
        u.valid().then_some(u)
    }

    fn valid(&self) -> bool {
        self.cached_input_tokens + self.cache_write_input_tokens <= self.input_tokens
            && self.reasoning_output_tokens <= self.output_tokens
            && self.total_tokens >= self.input_tokens + self.output_tokens
    }

    fn plus(&self, o: &Self) -> Option<Self> {
        let u = Self {
            input_tokens: self.input_tokens + o.input_tokens,
            cached_input_tokens: self.cached_input_tokens + o.cached_input_tokens,
            cache_write_input_tokens: self.cache_write_input_tokens + o.cache_write_input_tokens,
            output_tokens: self.output_tokens + o.output_tokens,
            reasoning_output_tokens: self.reasoning_output_tokens + o.reasoning_output_tokens,
            total_tokens: self.total_tokens + o.total_tokens,
        };
        u.valid().then_some(u)
    }

    fn minus(&self, o: &Self) -> Option<Self> {
        let u = Self {
            input_tokens: self.input_tokens.checked_sub(o.input_tokens)?,
            cached_input_tokens: self
                .cached_input_tokens
                .checked_sub(o.cached_input_tokens)?,
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .checked_sub(o.cache_write_input_tokens)?,
            output_tokens: self.output_tokens.checked_sub(o.output_tokens)?,
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .checked_sub(o.reasoning_output_tokens)?,
            total_tokens: self.total_tokens.checked_sub(o.total_tokens)?,
        };
        u.valid().then_some(u)
    }
}

/// T3's `CodexScanState`, as it saves it in the cache.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CodexState {
    model: String,
    speed: String,
    session_id: String,
    /// `JSON.stringify` of the last usage: compared by value here.
    last_usage_signature: Option<String>,
    last_cumulative_usage: Option<CodexUsage>,
    saw_session_meta: bool,
    suppressing_fork_copies: bool,
    fork_copy_anchor_ms: f64,
    #[serde(skip)]
    last: Option<CodexUsage>,
}

const FORK_COPY_MAX_GAP_MS: Millis = 1000;

impl CodexState {
    pub fn from_cache(v: Option<&Value>) -> Self {
        let mut s: Self = v
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        s.last = s
            .last_usage_signature
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .and_then(|v| CodexUsage::read(&v));
        s
    }

    fn speed(&self) -> u8 {
        match self.speed.as_str() {
            "fast" => 1,
            "ultrafast" => 2,
            _ => 0,
        }
    }

    /// One rollout line; a call when it is a new token count.
    pub fn line(&mut self, v: &Value, cwd: &Cwd) -> Option<Parsed> {
        let p = v.get("payload")?;
        if v["type"] == "session_meta" {
            if self.saw_session_meta {
                return None;
            }
            self.saw_session_meta = true;
            if let Some(id) = p["id"].as_str().or(p["session_id"].as_str()) {
                self.session_id = id.to_owned();
            }
            let forked = p["forked_from_id"].is_string()
                || p["source"]["subagent"]["thread_spawn"]["parent_thread_id"].is_string();
            if let (Some(at), true) = (ms(&v["timestamp"]), forked) {
                self.suppressing_fork_copies = true;
                self.fork_copy_anchor_ms = at as f64;
            }
            return None;
        }
        if v["type"] == "turn_context" {
            if let Some(m) = p["model"].as_str() {
                self.model = m.to_owned();
            }
            return None;
        }
        if p["type"] == "thread_settings_applied" {
            self.speed = match p["thread_settings"]["service_tier"].as_str() {
                Some("priority" | "fast") => "fast",
                Some("ultrafast") => "ultrafast",
                _ => "standard",
            }
            .into();
            return None;
        }
        if p["type"] != "token_count" {
            return None;
        }
        let info = p.get("info").filter(|i| i.is_object())?;
        let at = ms(&v["timestamp"])?;
        if self.model.is_empty() {
            return None;
        }
        let last = CodexUsage::read(&info["last_token_usage"]);
        let has_cum = !info["total_token_usage"].is_null();
        let cum = if has_cum {
            CodexUsage::read(&info["total_token_usage"])
        } else {
            None
        };
        if self.suppressing_fork_copies {
            if (at as f64) - self.fork_copy_anchor_ms < FORK_COPY_MAX_GAP_MS as f64 {
                self.fork_copy_anchor_ms = at as f64;
                if cum.is_some() {
                    self.last_cumulative_usage = cum;
                } else if let Some(l) = last.filter(|l| !has_cum && Some(*l) != self.last) {
                    self.last_cumulative_usage =
                        self.last_cumulative_usage.unwrap_or_default().plus(&l);
                }
                self.last = last;
                return None;
            }
            self.suppressing_fork_copies = false;
            self.last = None;
        }
        let previous = self.last_cumulative_usage;
        if cum.is_some() {
            self.last_cumulative_usage = cum;
        }
        let last = last?;
        if has_cum && cum.is_none() {
            return None;
        }
        if last.input_tokens + last.output_tokens == 0 {
            self.last = None;
            return None;
        }
        if let (Some(c), Some(p)) = (cum, previous) {
            if c == p {
                return None;
            }
        }
        if last.total_tokens != last.input_tokens + last.output_tokens {
            return None;
        }
        match cum {
            Some(c) => {
                let restarted = c == last;
                let reconciled = match previous {
                    None => c.minus(&last).is_some(),
                    Some(p) => c.minus(&p) == Some(last),
                };
                if !restarted && !reconciled {
                    return None;
                }
            }
            None => {
                if Some(last) == self.last {
                    return None;
                }
                self.last_cumulative_usage =
                    Some(self.last_cumulative_usage.unwrap_or_default().plus(&last)?);
            }
        }
        self.last = Some(last);
        let uncached = last.input_tokens - last.cached_input_tokens - last.cache_write_input_tokens;
        if uncached + last.cached_input_tokens + last.cache_write_input_tokens + last.output_tokens
            == 0
        {
            return None;
        }
        Some((
            Call {
                provider: "codex".into(),
                model: self.model.clone(),
                rate_model: None,
                session: self.session_id.clone(),
                at,
                tokens: Tokens {
                    uncached,
                    cache_read: last.cached_input_tokens,
                    cache_write: last.cache_write_input_tokens,
                    output: last.output_tokens,
                },
                reasoning: last.reasoning_output_tokens,
                reported_usd: None,
                speed: self.speed(),
                cwd: cwd.clone(),
            },
            None,
        ))
    }
}

/// Calls in the complete lines of `path` from byte `offset` on.
pub fn parse_from(
    provider: &str,
    path: &Path,
    offset: u64,
    state: Option<&Value>,
    cwd: &Cwd,
) -> Vec<Parsed> {
    let mut buf = Vec::new();
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    if f.seek(SeekFrom::Start(offset)).is_err() || f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    // A line still being written is read next time.
    let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut codex = CodexState::from_cache(state);
    let mut out = Vec::new();
    for line in buf[..end].split(|b| *b == b'\n') {
        // Most lines are tool output: skip what cannot hold a count.
        let wanted: &[&[u8]] = match provider {
            "claude" => &[b"\"usage\""],
            _ => &[
                b"token_count",
                b"session_meta",
                b"turn_context",
                b"thread_settings_applied",
            ],
        };
        if !wanted.iter().any(|w| contains(line, w)) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let parsed = match provider {
            "claude" => claude_line(&v, cwd),
            "codex" => codex.line(&v, cwd),
            _ => None,
        };
        out.extend(parsed);
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Transcripts under `dir` (any depth) changed since `since`.
pub fn changed_since(dir: &Path, since: std::time::SystemTime, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(meta) = e.metadata() else {
            continue;
        };
        if meta.is_dir() {
            changed_since(&p, since, out);
        } else if p.extension().is_some_and(|x| x == "jsonl")
            && meta.modified().is_ok_and(|m| m >= since)
        {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_lines_count_and_key_like_t3() {
        let v = json!({"type": "assistant", "timestamp": "2026-10-26T10:00:00Z", "requestId": "req_1",
            "sessionId": "s", "message": {"id": "msg_1", "model": "claude-opus-5-5",
            "usage": {"input_tokens": 2, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5,
                "output_tokens": 7, "speed": "fast"}}});
        let (c, k) = claude_line(&v, &Cwd::Unknown).unwrap();
        assert_eq!(k.as_deref(), Some("msg_1:req_1"));
        assert_eq!((c.tokens.cache_read, c.tokens.output, c.speed), (100, 7, 1));
        assert!(claude_line(&json!({"type": "user"}), &Cwd::Unknown).is_none());
    }

    #[test]
    fn codex_counts_each_turn_once() {
        let mut s = CodexState::default();
        let cwd = Cwd::Unknown;
        s.line(&json!({"type": "session_meta", "timestamp": "2026-10-26T10:00:00Z", "payload": {"id": "sess"}}), &cwd);
        s.line(
            &json!({"type": "turn_context", "payload": {"model": "gpt-6-luna", "turn_id": "t"}}),
            &cwd,
        );
        let tc = |last: Value, total: Value| {
            json!({"type": "event_msg", "timestamp": "2026-10-26T10:00:05Z",
            "payload": {"type": "token_count", "info": {"last_token_usage": last, "total_token_usage": total}}})
        };
        let u1 = json!({"input_tokens": 100, "cached_input_tokens": 60, "output_tokens": 10, "total_tokens": 110});
        let (c, _) = s.line(&tc(u1.clone(), u1.clone()), &cwd).unwrap();
        assert_eq!(
            (c.tokens.uncached, c.tokens.cache_read, c.session.as_str()),
            (40, 60, "sess")
        );
        // The same count again (Codex repeats it) is not a second call.
        assert!(s.line(&tc(u1.clone(), u1), &cwd).is_none());
        let u2 = json!({"input_tokens": 50, "output_tokens": 5, "total_tokens": 55});
        let t2 = json!({"input_tokens": 150, "cached_input_tokens": 60, "output_tokens": 15, "total_tokens": 165});
        assert!(s.line(&tc(u2, t2), &cwd).is_some());
    }

    #[test]
    fn only_complete_lines_from_the_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let line = |id: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-10-26T10:00:00Z","requestId":"{id}","message":{{"id":"m","model":"claude-opus-5-5","usage":{{"input_tokens":1,"output_tokens":1}}}}}}"#
            )
        };
        let first = format!("{}\n", line("a"));
        std::fs::write(&p, format!("{first}{}\n{}", line("b"), line("c"))).unwrap();
        let got = parse_from("claude", &p, first.len() as u64, None, &Cwd::Unknown);
        assert_eq!(got.len(), 1, "b only: a was read before, c is unfinished");
        assert_eq!(got[0].1.as_deref(), Some("m:b"));
    }
}
