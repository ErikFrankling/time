//! OTLP/HTTP JSON encoding of [`Envelope`]s as OTel log records.
//!
//! Hand-written against the OTLP JSON mapping rather than pulled in from the
//! OTel SDK: the subset Time needs is small, and owning it means the codec is
//! tested against exactly the records the agents send. Protobuf (what Alloy's
//! `otlphttp` exporter sends by default) is accepted by the server separately.
//!
//! Mapping:
//! - resource `host.name` -> [`Envelope::device`], `service.name` ->
//!   [`Envelope::service`], and `service.namespace` is always `time`, which is
//!   what routes the record to Time and to the long-retention Loki stream.
//! - attribute `event.name` -> the [`Event`] variant; `event.id`,
//!   `time.start`, `time.end` -> the envelope; every other attribute -> the
//!   event's fields, by their serde names.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::event::{Envelope, Event};

pub const NAMESPACE: &str = "time";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportLogsRequest {
    #[serde(default)]
    pub resource_logs: Vec<ResourceLogs>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLogs {
    #[serde(default)]
    pub resource: Resource,
    #[serde(default)]
    pub scope_logs: Vec<ScopeLogs>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeLogs {
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub log_records: Vec<LogRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRecord {
    /// Nanoseconds, as a decimal string per the protobuf JSON mapping.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "u64_string")]
    pub time_unix_nano: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "u64_string")]
    pub observed_time_unix_nano: Option<u64>,
    /// The newer dedicated field. Time writes `event.name` as an attribute too,
    /// because not every hop preserves this one yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyValue {
    pub key: String,
    pub value: AnyValue,
}

/// OTLP's tagged value. Exactly one field is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnyValue {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub string_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bool_value: Option<bool>,
    /// int64 is a string in OTLP JSON, but senders also emit bare numbers.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "i64_string")]
    pub int_value: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array_value: Option<ArrayValue>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArrayValue {
    #[serde(default)]
    pub values: Vec<AnyValue>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum DecodeError {
    #[error("record has no event.name")]
    NoName,
    #[error("record has no event.id")]
    NoId,
    #[error("resource has no host.name")]
    NoDevice,
    #[error("record has no time.start and no timeUnixNano")]
    NoTime,
    #[error("{name}: {message}")]
    Invalid { name: String, message: String },
}

/// Encode envelopes as one OTLP request, one resource per (device, service).
pub fn encode(envelopes: &[Envelope], service_version: &str) -> ExportLogsRequest {
    let mut groups: BTreeMap<(&str, &str), Vec<LogRecord>> = BTreeMap::new();
    for env in envelopes {
        groups
            .entry((env.device.as_str(), env.service.as_str()))
            .or_default()
            .push(record(env));
    }
    let resource_logs = groups
        .into_iter()
        .map(|((device, service), log_records)| ResourceLogs {
            resource: Resource {
                attributes: vec![
                    kv("service.name", Value::String(service.into())),
                    kv("service.namespace", Value::String(NAMESPACE.into())),
                    kv("service.version", Value::String(service_version.into())),
                    kv("host.name", Value::String(device.into())),
                ],
            },
            scope_logs: vec![ScopeLogs {
                scope: Scope {
                    name: "time".into(),
                    version: service_version.into(),
                },
                log_records,
            }],
        })
        .collect();
    ExportLogsRequest { resource_logs }
}

fn record(env: &Envelope) -> LogRecord {
    let mut attributes = vec![
        kv("event.id", Value::String(env.id.clone())),
        kv("time.start", Value::from(env.start_ms)),
        kv("time.end", Value::from(env.end_ms)),
    ];
    // Serializing the event yields `event.name` plus every field under its
    // attribute key; nothing is listed twice by hand.
    if let Value::Object(fields) =
        serde_json::to_value(&env.event).expect("events always serialize")
    {
        for (key, value) in fields {
            if let Some(value) = to_any(value) {
                attributes.push(KeyValue { key, value });
            }
        }
    }
    let nanos = u64::try_from(env.start_ms).unwrap_or(0) * 1_000_000;
    LogRecord {
        time_unix_nano: Some(nanos),
        observed_time_unix_nano: None,
        event_name: Some(env.event.name().into()),
        attributes,
    }
}

/// Decode every Time record in a request. One bad record does not cost the
/// others: each comes back as its own `Result`, and records from another
/// namespace are skipped entirely.
pub fn decode(req: &ExportLogsRequest) -> Vec<Result<Envelope, DecodeError>> {
    let mut out = Vec::new();
    for rl in &req.resource_logs {
        let res = attrs_to_map(&rl.resource.attributes);
        let namespace = res.get("service.namespace").and_then(Value::as_str);
        if namespace.is_some_and(|n| n != NAMESPACE) {
            continue;
        }
        let device = res
            .get("host.name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let service = res
            .get("service.name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        for sl in &rl.scope_logs {
            for rec in &sl.log_records {
                out.push(decode_record(rec, device.as_deref(), &service));
            }
        }
    }
    out
}

fn decode_record(
    rec: &LogRecord,
    device: Option<&str>,
    service: &str,
) -> Result<Envelope, DecodeError> {
    let mut fields = attrs_to_map(&rec.attributes);
    if !fields.contains_key("event.name") {
        let name = rec.event_name.clone().ok_or(DecodeError::NoName)?;
        fields.insert("event.name".into(), Value::String(name));
    }
    let name = fields["event.name"].as_str().unwrap_or_default().to_owned();
    let id = match fields.remove("event.id") {
        Some(Value::String(s)) if !s.is_empty() => s,
        _ => return Err(DecodeError::NoId),
    };
    let device = device.ok_or(DecodeError::NoDevice)?.to_owned();
    let start_ms = match fields.remove("time.start").and_then(|v| v.as_i64()) {
        Some(ms) => ms,
        None => rec
            .time_unix_nano
            .map(|n| (n / 1_000_000) as i64)
            .ok_or(DecodeError::NoTime)?,
    };
    let end_ms = fields
        .remove("time.end")
        .and_then(|v| v.as_i64())
        .unwrap_or(start_ms);
    let event: Event =
        serde_json::from_value(Value::Object(fields)).map_err(|e| DecodeError::Invalid {
            name: name.clone(),
            message: e.to_string(),
        })?;
    if end_ms < start_ms {
        return Err(DecodeError::Invalid {
            name,
            message: format!("time.end {end_ms} before time.start {start_ms}"),
        });
    }
    Ok(Envelope {
        id,
        device,
        service: service.to_owned(),
        start_ms,
        end_ms,
        event,
    })
}

fn kv(key: &str, value: Value) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: to_any(value).expect("non-null"),
    }
}

fn to_any(value: Value) -> Option<AnyValue> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(b) => AnyValue {
            bool_value: Some(b),
            ..Default::default()
        },
        Value::String(s) => AnyValue {
            string_value: Some(s),
            ..Default::default()
        },
        Value::Number(n) => match n.as_i64() {
            Some(i) => AnyValue {
                int_value: Some(i),
                ..Default::default()
            },
            None => match n.as_u64() {
                // Beyond i64: keep it exact as a string rather than rounding.
                Some(u) => AnyValue {
                    string_value: Some(u.to_string()),
                    ..Default::default()
                },
                None => AnyValue {
                    double_value: n.as_f64(),
                    ..Default::default()
                },
            },
        },
        Value::Array(items) => AnyValue {
            array_value: Some(ArrayValue {
                values: items.into_iter().filter_map(to_any).collect(),
            }),
            ..Default::default()
        },
        // Nested objects are not part of Time's schema.
        Value::Object(_) => return None,
    })
}

fn from_any(v: &AnyValue) -> Value {
    if let Some(s) = &v.string_value {
        Value::String(s.clone())
    } else if let Some(b) = v.bool_value {
        Value::Bool(b)
    } else if let Some(i) = v.int_value {
        Value::from(i)
    } else if let Some(d) = v.double_value {
        Number::from_f64(d)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    } else if let Some(a) = &v.array_value {
        Value::Array(a.values.iter().map(from_any).collect())
    } else {
        Value::Null
    }
}

fn attrs_to_map(attrs: &[KeyValue]) -> Map<String, Value> {
    attrs
        .iter()
        .map(|kv| (kv.key.clone(), from_any(&kv.value)))
        .collect()
}

/// Accept int64 as either a JSON string or a number; write it as a string.
mod i64_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(i) => s.serialize_str(&i.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            S(String),
            N(i64),
        }
        Ok(match Option::<Raw>::deserialize(d)? {
            None => None,
            Some(Raw::N(n)) => Some(n),
            Some(Raw::S(s)) => Some(s.parse().map_err(serde::de::Error::custom)?),
        })
    }
}

mod u64_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(i) => s.serialize_str(&i.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            S(String),
            N(u64),
        }
        Ok(match Option::<Raw>::deserialize(d)? {
            None => None,
            Some(Raw::N(n)) => Some(n),
            Some(Raw::S(s)) => Some(s.parse().map_err(serde::de::Error::custom)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;

    fn sample() -> Vec<Envelope> {
        vec![
            Envelope::new(
                "pc",
                "time-agent",
                1_700_000_000_000,
                1_700_000_060_000,
                Event::ScreenMinute(ScreenMinute {
                    window_class: "firefox".into(),
                    window_title: "Video — YouTube".into(),
                    browser_host: Some("youtube.com".into()),
                    keys: 3,
                    mouse: 40,
                    idle_seconds: Some(12),
                    ..Default::default()
                }),
            ),
            Envelope::new(
                "phone",
                "time-android",
                1_700_000_000_000,
                1_700_000_300_000,
                Event::HealthSteps(HealthSteps {
                    steps: 420,
                    distance_m: 312.5,
                }),
            ),
            Envelope::new(
                "pc",
                "t3code",
                1_700_000_010_000,
                1_700_000_010_000,
                Event::TokensUsed(TokensUsed {
                    provider: "anthropic".into(),
                    model: "claude-opus-5-5".into(),
                    account: "max-1".into(),
                    input_tokens: 12_000_000_000,
                    output_tokens: 800,
                    cached_input_tokens: 0,
                    reasoning_tokens: 0,
                    thread_id: "t1".into(),
                    project: Some("time".into()),
                    origin: Origin::Human,
                }),
            ),
        ]
    }

    #[test]
    fn round_trips_through_json() {
        let envs = sample();
        let wire = serde_json::to_string(&encode(&envs, "test")).unwrap();
        let back: ExportLogsRequest = serde_json::from_str(&wire).unwrap();
        let mut decoded: Vec<Envelope> = decode(&back).into_iter().map(Result::unwrap).collect();
        let mut expected = envs;
        decoded.sort_by(|a, b| a.id.cmp(&b.id));
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(decoded, expected);
    }

    #[test]
    fn int_values_are_strings_on_the_wire_and_numbers_are_accepted() {
        let wire = serde_json::to_value(encode(&sample()[..1], "t")).unwrap();
        let attrs = &wire["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"];
        let keys = attrs
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "keys")
            .unwrap();
        assert_eq!(keys["value"]["intValue"], "3");

        let v: AnyValue = serde_json::from_str(r#"{"intValue": 7}"#).unwrap();
        assert_eq!(v.int_value, Some(7));
    }

    #[test]
    fn foreign_namespaces_are_skipped_and_bad_records_are_isolated() {
        let mut req = encode(&sample(), "t");
        // A record with an unknown event name next to good ones.
        req.resource_logs[0].scope_logs[0]
            .log_records
            .push(LogRecord {
                attributes: vec![
                    kv("event.name", Value::String("time.nonsense".into())),
                    kv("event.id", Value::String("x".into())),
                    kv("time.start", Value::from(1)),
                ],
                ..Default::default()
            });
        req.resource_logs.push(ResourceLogs {
            resource: Resource {
                attributes: vec![kv("service.namespace", Value::String("ops".into()))],
            },
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord::default()],
                ..Default::default()
            }],
        });
        let results = decode(&req);
        assert_eq!(
            results.len(),
            4,
            "ops record skipped, four time records seen"
        );
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
        assert!(matches!(
            results.iter().find(|r| r.is_err()),
            Some(Err(DecodeError::Invalid { .. }))
        ));
    }

    #[test]
    fn missing_identity_is_an_error_not_a_guess() {
        let rec = LogRecord {
            attributes: vec![kv("event.name", Value::String("time.heartbeat".into()))],
            ..Default::default()
        };
        assert_eq!(decode_record(&rec, Some("pc"), "s"), Err(DecodeError::NoId));
        let rec = LogRecord {
            attributes: vec![
                kv("event.name", Value::String("time.heartbeat".into())),
                kv("event.id", Value::String("a".into())),
            ],
            ..Default::default()
        };
        assert_eq!(decode_record(&rec, None, "s"), Err(DecodeError::NoDevice));
        assert_eq!(
            decode_record(&rec, Some("pc"), "s"),
            Err(DecodeError::NoTime)
        );
    }

    #[test]
    fn stable_ids_dedupe_rereads() {
        let ev = || {
            Event::PhoneApp(PhoneApp {
                package: "com.x".into(),
                ..Default::default()
            })
        };
        let a = Envelope::stable("phone", "time-android", 1, 2, ev());
        let b = Envelope::stable("phone", "time-android", 1, 2, ev());
        assert_eq!(a.id, b.id);
    }
}
