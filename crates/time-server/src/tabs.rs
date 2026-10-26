//! Browser tabs the rules cannot settle, put to the decision model.
//!
//! In a browser Erik is nearly always working or studying, but a tab title
//! on its own ("The Agentic Web Must Stay Open") often says nothing to a
//! rule. The rules that used to give up with "browsing" are marked
//! `ask_model` in `rules.toml`: every distinct tab they catch is put to the
//! same cheap model the gap filler uses (`crate::decide`), a batch of tabs
//! at a time, with what Erik is working on (the `[profile]` and the
//! `[[project]]` descriptions in the rules) and how he uses a browser. Each
//! tab is asked about once: the verdict is kept in `tab_verdict`, the
//! question and answer in `decision` (`/api/decisions?kind=tabs`), and the
//! classifier applies the verdicts as `model.tab` on top of those rules.
//! When verdicts arrive, history is relabelled, exactly as when a rule is
//! edited.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::sync::Arc;

use anyhow::Result;
use rusqlite::params;
use serde_json::Value;
use sha2::{Digest, Sha256};
use time_core::clock::{Millis, MINUTE_MS};
use time_core::label::Activity;
use time_core::rules::{tab_core, Facts, RuleSet, Source, TabVerdict};

use crate::store::Store;
use crate::AppState;

/// The `decision.kind` of a batch of tabs.
pub const KIND: &str = "tabs";
/// Tabs per question.
const BATCH: usize = 40;
/// Seconds between passes.
const EVERY_SECS: u64 = 300;
/// How far back tabs are collected when `[decide] since` is not set.
const DEFAULT_DAYS: i64 = 14;
/// Below this confidence the model's answer is not trusted, and the tab is
/// browsing like the rule left it.
const MIN_CONFIDENCE: f64 = 0.5;
/// A tab titled like this says nothing: no one is asked.
const BLANK: [&str; 5] = [
    "",
    "original profile",
    "mozilla firefox",
    "new tab",
    "untitled",
];

/// A tab to ask about.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub key: String,
    /// One real title of it, as the browser showed it.
    pub title: String,
    pub browser: String,
    pub host: Option<String>,
    pub minutes: u32,
}

const SYSTEM_HEAD: &str = "You label the browser tabs Erik had in front of him, for a tool that measures how he spends his time. \
You see only the tab's title (and its site when known), never the page. Decide what the tab was for him.\n\n";

const SYSTEM_TAIL: &str = "\nActivities (use exactly these words): \
work (his job, his company, any of his own projects, tools, docs and research for them), \
study (KTH courses, homework, papers, school pages), \
messaging (chatting with friends: Discord, Messenger, WhatsApp, personal mail), \
video (watching), feeds (scrolling X, Reddit, Instagram, LinkedIn, TikTok), \
music, games, \
browsing (only news, Wikipedia, maps, shopping and pages with clearly nothing to do with any project), \
other (errands: bank, travel, bookings).\n\
Microsoft Teams, Outlook, Azure and Naia are always work. A page about agents, MCP, AI infrastructure, developer tooling, startups, VC, \
founders or a company he might compete with or sell to is work for the project it fits. \
A tab titled \"<name> on X: <post>\" is a post he opened: work when the post is about AI, agents, developer tooling, infrastructure, startups, security or his projects, feeds when it is politics, entertainment or personal. \
When unsure between work and browsing, choose work, with a lower confidence. \
For each tab give the project it belongs to, one of the project names above, or null.\n\
Answer JSON only: {\"tabs\":[{\"id\":\"T1\",\"activity\":\"work\",\"project\":\"neptune\",\"confidence\":0.0,\"reason\":\"under 15 words\"}]}";

/// The verdicts stored so far.
pub fn load(store: &Store) -> Result<HashMap<String, TabVerdict>> {
    store.with(|c| {
        let mut stmt = c.prepare("SELECT key, activity, project FROM tab_verdict")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (key, activity, project) = row?;
            if let Ok(activity) = activity.parse::<Activity>() {
                out.insert(key, TabVerdict { activity, project });
            }
        }
        Ok(out)
    })
}

/// Tabs shown since `from` that a rule leaves to the model and no verdict
/// covers yet, most-watched first.
pub fn pending(
    store: &Store,
    rules: &RuleSet,
    known: &HashMap<String, TabVerdict>,
    from: Millis,
) -> Result<Vec<Tab>> {
    let rows: Vec<(String, String, Option<String>, i64)> = store.with(|c| {
        let mut stmt = c.prepare(
            "SELECT json_extract(body, '$.\"window.class\"'), json_extract(body, '$.\"window.title\"'),
                    json_extract(body, '$.\"browser.host\"'), count(*)
             FROM event
             WHERE name = 'time.screen.minute' AND start_ms >= ?1
               AND coalesce(json_extract(body, '$.blocked'), 0) = 0
             GROUP BY 1, 2, 3",
        )?;
        let rows = stmt.query_map([from], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?,
                r.get(3)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    })?;
    let mut by_key: BTreeMap<String, Tab> = BTreeMap::new();
    for (class, title, host, n) in rows {
        let facts = Facts {
            source: Some(Source::Desktop),
            class: &class,
            title: &title,
            host: host.as_deref().filter(|h| !h.is_empty()),
            package: None,
        };
        let Some(key) = rules.asks_model(&facts) else {
            continue;
        };
        if BLANK.contains(&key.as_str()) || known.contains_key(&key) {
            continue;
        }
        let tab = by_key.entry(key.clone()).or_insert_with(|| Tab {
            key,
            title: tab_core(&title).to_owned(),
            browser: class.clone(),
            host: host.clone().filter(|h| !h.is_empty()),
            minutes: 0,
        });
        tab.minutes += n as u32;
        if tab.host.is_none() {
            tab.host = host.filter(|h| !h.is_empty());
        }
    }
    let mut tabs: Vec<Tab> = by_key.into_values().collect();
    tabs.sort_by(|a, b| b.minutes.cmp(&a.minutes).then(a.key.cmp(&b.key)));
    Ok(tabs)
}

/// The question: who Erik is, what his projects are, and the tabs.
pub fn question(rules: &RuleSet, tabs: &[Tab]) -> (String, String) {
    let mut system = String::from(SYSTEM_HEAD);
    system.push_str(&rules.profile_prompt());
    system.push_str("\nProject names: ");
    system.push_str(&rules.project_names().join(", "));
    system.push('\n');
    system.push_str(SYSTEM_TAIL);

    let mut user = String::from("Tabs (id, minutes in front of him, browser, site, title):\n");
    for (i, t) in tabs.iter().enumerate() {
        let _ = writeln!(
            user,
            "T{} | {} min | {} | {} | {}",
            i + 1,
            t.minutes,
            t.browser,
            t.host.as_deref().unwrap_or("-"),
            serde_json::to_string(&t.title.chars().take(160).collect::<String>())
                .unwrap_or_default()
        );
    }
    (system, user)
}

/// The model's answer for a batch, as verdicts with their confidence and
/// reason. Tabs it skipped, or answered with something that is not an
/// activity, come back as `browsing`, which is what the rule said, so they
/// are not asked about again.
pub fn read_answer(
    rules: &RuleSet,
    tabs: &[Tab],
    answer: &Value,
) -> Vec<(Tab, TabVerdict, f64, String)> {
    let list = answer["tabs"].as_array().cloned().unwrap_or_default();
    let projects = rules.project_names();
    tabs.iter()
        .enumerate()
        .map(|(i, tab)| {
            let id = format!("T{}", i + 1);
            let a = list.iter().find(|a| a["id"].as_str() == Some(id.as_str()));
            let parsed = a.and_then(|a| {
                let activity: Activity = a["activity"].as_str()?.parse().ok()?;
                (activity != Activity::Unknown).then_some((activity, a))
            });
            let Some((activity, a)) = parsed else {
                return (
                    tab.clone(),
                    TabVerdict {
                        activity: Activity::Browsing,
                        project: None,
                    },
                    0.0,
                    "the model gave no usable answer".to_owned(),
                );
            };
            let confidence = a["confidence"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0);
            let reason: String = a["reason"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            let project = a["project"]
                .as_str()
                .filter(|p| projects.contains(p))
                .map(str::to_owned);
            if confidence < MIN_CONFIDENCE {
                return (
                    tab.clone(),
                    TabVerdict {
                        activity: Activity::Browsing,
                        project: None,
                    },
                    confidence,
                    format!("not sure ({activity}): {reason}"),
                );
            }
            (
                tab.clone(),
                TabVerdict { activity, project },
                confidence,
                reason,
            )
        })
        .collect()
}

fn batch_key(tabs: &[Tab]) -> String {
    let mut h = Sha256::new();
    for t in tabs {
        h.update(t.key.as_bytes());
        h.update([0]);
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("tabs-{}", &hex[..12])
}

fn save(
    store: &Store,
    rows: &[(Tab, TabVerdict, f64, String)],
    decision_id: i64,
    now: Millis,
) -> Result<()> {
    store.with(|c| {
        let tx = c.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT OR REPLACE INTO tab_verdict
                    (key, title, activity, project, confidence, reason, decision_id, computed_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (tab, v, confidence, reason) in rows {
                ins.execute(params![
                    tab.key,
                    tab.title,
                    v.activity.as_str(),
                    v.project,
                    confidence,
                    reason,
                    decision_id,
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    })
}

/// One pass: ask about the tabs nobody has been asked about, keep the
/// verdicts, and switch the classifier to them. Returns how many tabs got a
/// verdict.
pub fn run_once(state: &AppState, now: Millis) -> Result<usize> {
    let store = &state.store;
    let rules = state.classifier.current();
    let known = load(store)?;
    let from = state
        .config
        .decide
        .since
        .and_then(|d| crate::day::bounds(state.tz, d).ok().map(|b| b.0))
        .unwrap_or(now - DEFAULT_DAYS * 24 * 60 * MINUTE_MS);
    let todo = pending(store, &rules.rules, &known, from)?;
    let mut done = 0;
    if !todo.is_empty() && state.decider.available() {
        let mut batches: Vec<&[Tab]> = todo.chunks(BATCH).collect();
        batches.truncate(state.config.decide.max_calls_per_pass.max(1));
        for group in batches.chunks(state.config.decide.parallel.max(1)) {
            let answers: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = group
                    .iter()
                    .map(|tabs| {
                        let (system, user) = question(&rules.rules, tabs);
                        let key = batch_key(tabs);
                        s.spawn(move || state.decider.ask(store, KIND, &key, &system, &user, now))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(anyhow::anyhow!("panicked")))
                    })
                    .collect()
            });
            for (tabs, answer) in group.iter().zip(answers) {
                match answer {
                    Ok(a) => {
                        let rows = read_answer(&rules.rules, tabs, &a.value);
                        done += rows.len();
                        save(store, &rows, a.id, now)?;
                    }
                    Err(e) => eprintln!("tabs: {} tab(s): {e:#}", tabs.len()),
                }
            }
        }
    }
    if state.classifier.set_tab_verdicts(load(store)?) {
        if let Some(n) = state.classifier.requeue_if_rules_changed(store)? {
            eprintln!("tabs: verdicts changed, {n} minute(s) queued");
        }
    }
    Ok(done)
}

pub async fn run_forever(state: Arc<AppState>) {
    loop {
        let st = state.clone();
        match tokio::task::spawn_blocking(move || run_once(&st, st.clock.now_ms())).await {
            Ok(Ok(n)) if n > 0 => eprintln!("tabs: {n} tab(s) labelled by the decision model"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("tabs: pass failed: {e:#}"),
            Err(e) => eprintln!("tabs: pass panicked: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(EVERY_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time_core::event::{Envelope, Event, ScreenMinute};

    fn rules() -> RuleSet {
        RuleSet::parse(include_str!("../../../config/rules.toml")).unwrap()
    }

    fn tab(key: &str, minutes: u32) -> Tab {
        Tab {
            key: key.into(),
            title: key.into(),
            browser: "firefox".into(),
            host: None,
            minutes,
        }
    }

    fn minute(title: &str, t: Millis) -> Envelope {
        Envelope::new(
            "pc",
            "time-agent",
            t,
            t + MINUTE_MS,
            Event::ScreenMinute(ScreenMinute {
                window_class: "firefox".into(),
                window_title: title.into(),
                keys: 4,
                ..Default::default()
            }),
        )
    }

    #[test]
    fn only_tabs_the_rules_leave_open_are_asked_once_each() {
        let store = Store::open_in_memory().unwrap();
        let t0 = 1_800_000_000_000 - 1_800_000_000_000 % MINUTE_MS;
        let events: Vec<_> = [
            "The Agentic Web Must Stay Open — Original profile — Mozilla Firefox",
            "(2) The Agentic Web Must Stay Open — Original profile — Mozilla Firefox",
            "Chat | Kaan Koska | Microsoft Teams — Original profile — Mozilla Firefox",
            "(1) Home / X — Original profile — Mozilla Firefox",
            "Original profile — Mozilla Firefox",
            "Spontaneous generation - Wikipedia — Original profile — Mozilla Firefox",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| minute(t, t0 + i as Millis * MINUTE_MS))
        .collect();
        store.insert(&events, t0 + 10 * MINUTE_MS).unwrap();

        let r = rules();
        let tabs = pending(&store, &r, &HashMap::new(), t0).unwrap();
        let keys: Vec<_> = tabs.iter().map(|t| t.key.as_str()).collect();
        // The same tab with and without the unread count is one question;
        // Teams and X are settled by rules; a blank tab says nothing.
        assert_eq!(
            keys,
            [
                "the agentic web must stay open",
                "spontaneous generation - wikipedia"
            ]
        );
        assert_eq!(tabs[0].minutes, 2);

        // Once answered, it is not asked again.
        let mut known = HashMap::new();
        known.insert(
            tabs[0].key.clone(),
            TabVerdict {
                activity: Activity::Work,
                project: None,
            },
        );
        assert_eq!(pending(&store, &r, &known, t0).unwrap().len(), 1);
    }

    #[test]
    fn the_prompt_knows_his_projects_and_his_habits() {
        let (system, user) = question(&rules(), &[tab("naia works", 37)]);
        for needle in [
            "Naia Works",
            "DD2395",
            "x-graph",
            "almost everything he does in it is work or study",
            "Microsoft Teams, Outlook, Azure and Naia are always work",
            "Project names: neptune",
        ] {
            assert!(system.contains(needle), "{needle:?} missing:\n{system}");
        }
        assert!(user.contains("T1 | 37 min | firefox | - | \"naia works\""));
    }

    #[test]
    fn answers_become_verdicts_and_nonsense_stays_browsing() {
        let tabs = [tab("a", 5), tab("b", 3), tab("c", 2), tab("d", 1)];
        let answer = json!({"tabs": [
            {"id": "T1", "activity": "work", "project": "neptune", "confidence": 0.9, "reason": "MCP docs"},
            {"id": "T2", "activity": "study", "project": "not-a-project", "confidence": 0.7, "reason": "paper"},
            {"id": "T3", "activity": "work", "project": null, "confidence": 0.1, "reason": "guess"},
            {"id": "T4", "activity": "banana", "confidence": 1.0, "reason": "?"}
        ]});
        let v = read_answer(&rules(), &tabs, &answer);
        assert_eq!(
            (v[0].1.activity, v[0].1.project.as_deref()),
            (Activity::Work, Some("neptune"))
        );
        // A project the rules do not know is dropped, the activity kept.
        assert_eq!(
            (v[1].1.activity, v[1].1.project.clone()),
            (Activity::Study, None)
        );
        // Unsure: browsing, as the rule had it, and recorded so as not to ask again.
        assert_eq!(v[2].1.activity, Activity::Browsing);
        assert!(v[2].3.contains("not sure"));
        assert_eq!(v[3].1.activity, Activity::Browsing);
    }

    #[test]
    fn verdicts_are_stored_and_loaded() {
        let store = Store::open_in_memory().unwrap();
        let rows = vec![(
            tab("naia works", 1),
            TabVerdict {
                activity: Activity::Work,
                project: Some("neptune".into()),
            },
            0.95,
            "Naia Works is the job".to_owned(),
        )];
        save(&store, &rows, 7, 0).unwrap();
        let v = load(&store).unwrap();
        assert_eq!(v["naia works"].activity, Activity::Work);
        assert_eq!(v["naia works"].project.as_deref(), Some("neptune"));
    }
}
