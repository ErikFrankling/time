//! The unknown inbox: what no rule matched, grouped so one answer covers
//! every minute of it, and turned into a rule when Erik answers.

use std::collections::HashMap;

use serde::Serialize;
use time_core::clock::{Millis, MINUTE_MS};
use time_core::event::Event;
use time_core::label::{Activity, Place};

use crate::store::Store;

/// How far back the inbox looks.
pub const WINDOW_MS: Millis = 14 * 24 * 60 * MINUTE_MS;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    Window { class: String, title: String },
    PhoneApp { package: String, label: String },
}

#[derive(Debug, Serialize)]
pub struct Item {
    pub subject: Subject,
    pub minutes: u32,
    pub last_ms: Millis,
}

/// Unknown minutes since `since`, grouped by what was on screen.
pub fn items(store: &Store, since: Millis, now: Millis) -> anyhow::Result<Vec<Item>> {
    let labels = store.raw_labels_between(since, now)?;
    let mut groups: HashMap<Subject, (u32, Millis)> = HashMap::new();
    for l in labels
        .iter()
        .filter(|l| l.activity == Some(Activity::Unknown))
    {
        let Some(device) = &l.device else { continue };
        let events = store.events_between(l.ts, l.ts + MINUTE_MS)?;
        let subject =
            events
                .iter()
                .filter(|e| &e.device == device)
                .find_map(|e| match (&e.event, l.place) {
                    (Event::ScreenMinute(m), Place::Computer) if e.start_ms == l.ts => {
                        Some(Subject::Window {
                            class: m.window_class.clone(),
                            title: m.window_title.clone(),
                        })
                    }
                    (Event::PhoneApp(a), Place::Phone) => Some(Subject::PhoneApp {
                        package: a.package.clone(),
                        label: a.label.clone(),
                    }),
                    _ => None,
                });
        if let Some(s) = subject {
            let g = groups.entry(s).or_insert((0, 0));
            g.0 += 1;
            g.1 = g.1.max(l.ts);
        }
    }
    let mut out: Vec<Item> = groups
        .into_iter()
        .map(|(subject, (minutes, last_ms))| Item {
            subject,
            minutes,
            last_ms,
        })
        .collect();
    out.sort_by(|a, b| b.minutes.cmp(&a.minutes).then(b.last_ms.cmp(&a.last_ms)));
    Ok(out)
}

/// What a new rule matches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOn {
    /// Every window of this app.
    Class,
    /// Only windows with exactly this title.
    Title,
    Package,
}

#[derive(Serialize)]
struct LocalRule<'a> {
    id: String,
    activity: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    class: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    package: Vec<&'a str>,
}

/// The TOML for one rule, ready to append to the local rules file. Values
/// go through the TOML serializer, so a title with quotes or newlines cannot
/// break the file, and an exact title is regex-escaped.
pub fn rule_toml(
    on: MatchOn,
    value: &str,
    class: &str,
    activity: Activity,
    id: &str,
) -> anyhow::Result<String> {
    anyhow::ensure!(!value.is_empty(), "nothing to match on");
    let rule = match on {
        MatchOn::Class => LocalRule {
            id: id.into(),
            activity: activity.as_str(),
            class: vec![value],
            title: None,
            package: vec![],
        },
        MatchOn::Title => LocalRule {
            id: id.into(),
            activity: activity.as_str(),
            // The title alone could match other apps; tie it to its window class.
            class: if class.is_empty() {
                vec![]
            } else {
                vec![class]
            },
            title: Some(format!("^{}$", regex::escape(value))),
            package: vec![],
        },
        MatchOn::Package => LocalRule {
            id: id.into(),
            activity: activity.as_str(),
            class: vec![],
            title: None,
            package: vec![value],
        },
    };
    Ok(format!("\n[[rule]]\n{}", toml::to_string(&rule)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time_core::rules::{Facts, RuleSet};

    #[test]
    fn generated_rules_parse_and_match_exactly() {
        let title = "Weird \"title\" (beta) [1] — Firefox";
        let toml = rule_toml(MatchOn::Title, title, "firefox", Activity::Work, "local.1").unwrap();
        let set = RuleSet::parse(&toml).unwrap();
        let hit = set
            .classify(&Facts {
                class: "firefox",
                title,
                ..Default::default()
            })
            .unwrap();
        assert_eq!((hit.rule, hit.activity), ("local.1", Activity::Work));
        assert!(set
            .classify(&Facts {
                class: "firefox",
                title: "Weird",
                ..Default::default()
            })
            .is_none());
        assert!(set
            .classify(&Facts {
                class: "zen",
                title,
                ..Default::default()
            })
            .is_none());

        let toml = rule_toml(
            MatchOn::Package,
            "se.example.app",
            "",
            Activity::Other,
            "local.2",
        )
        .unwrap();
        assert!(RuleSet::parse(&toml).is_ok());
        assert!(rule_toml(MatchOn::Class, "", "", Activity::Other, "x").is_err());
    }
}
