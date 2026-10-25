//! What a minute *was*: the typed output of classification.
//!
//! These are closed enums on purpose. v1 stored whatever string the model
//! returned and folded the surprises into `other` afterwards; here a label
//! that is not in the list cannot be constructed, stored or drawn.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::clock::Millis;

/// Where Erik's attention was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// At a desktop or laptop (which one is in [`MinuteLabel::device`]).
    Computer,
    Phone,
    /// Every device reported and none was in use.
    Away,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Work,
    Study,
    Comms,
    Video,
    Social,
    Browsing,
    Music,
    Games,
    Other,
    /// Nothing decided it. Shown in the inbox, never guessed.
    Unknown,
}

impl Activity {
    pub const ALL: [Activity; 10] = [
        Activity::Work,
        Activity::Study,
        Activity::Comms,
        Activity::Video,
        Activity::Social,
        Activity::Browsing,
        Activity::Music,
        Activity::Games,
        Activity::Other,
        Activity::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Work => "work",
            Activity::Study => "study",
            Activity::Comms => "comms",
            Activity::Video => "video",
            Activity::Social => "social",
            Activity::Browsing => "browsing",
            Activity::Music => "music",
            Activity::Games => "games",
            Activity::Other => "other",
            Activity::Unknown => "unknown",
        }
    }

    /// Counts toward "doing something useful" for focus nudges.
    pub fn is_productive(self) -> bool {
        matches!(self, Activity::Work | Activity::Study)
    }
}

impl fmt::Display for Activity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Activity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Activity::ALL
            .into_iter()
            .find(|a| a.as_str() == s)
            .ok_or_else(|| {
                format!(
                    "unknown activity {s:?}; expected one of {}",
                    Activity::ALL.map(|a| a.as_str()).join(", ")
                )
            })
    }
}

/// Who decided a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decider {
    Rule,
    Model,
    Manual,
    /// Presence logic: away, or no data.
    Presence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinuteLabel {
    pub ts: Millis,
    pub place: Place,
    /// The device in use, when there was one.
    pub device: Option<String>,
    /// `None` only when away.
    pub activity: Option<Activity>,
    pub project: Option<String>,
    /// What another device was doing at the same time: something playing,
    /// or another device in use.
    pub background: Option<Activity>,
    /// The device `background` was on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_device: Option<String>,
    /// The rule (or model, or "manual") that decided `activity`, so every
    /// label can be explained and fixed.
    pub rule: String,
    pub decider: Decider,
    /// False when a configured device had not yet reported past this minute.
    /// Recomputed when it does.
    pub complete: bool,
    /// The place, from the network the devices were on: `home`, `KTH`,
    /// `Wi-Fi <name>`. None when no device said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// Why an away minute was away. Never an activity: it is what the data
/// says about Erik when no device had him.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwayKind {
    /// The phone counted steps: out, walking.
    Walking,
    /// The phone was on another network than the computers: out.
    Out,
    /// Devices reported and nobody was at any of them.
    Idle,
    /// Nothing reported at all.
    Untracked,
}

impl AwayKind {
    pub const ALL: [AwayKind; 4] = [
        AwayKind::Walking,
        AwayKind::Out,
        AwayKind::Idle,
        AwayKind::Untracked,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AwayKind::Walking => "walking",
            AwayKind::Out => "out",
            AwayKind::Idle => "idle",
            AwayKind::Untracked => "untracked",
        }
    }

    /// For people: "away: out, walking".
    pub fn describe(self) -> &'static str {
        match self {
            AwayKind::Walking => "away: out, walking",
            AwayKind::Out => "away: out",
            AwayKind::Idle => "away: not at any device",
            AwayKind::Untracked => "untracked",
        }
    }
}

impl fmt::Display for AwayKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl MinuteLabel {
    /// Why this minute was away; None when a device was in use.
    pub fn away_kind(&self) -> Option<AwayKind> {
        if self.place != Place::Away {
            return None;
        }
        Some(match self.rule.as_str() {
            "presence.moving" => AwayKind::Walking,
            "presence.out" => AwayKind::Out,
            "presence.no-data" => AwayKind::Untracked,
            _ => AwayKind::Idle,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_strings_round_trip_and_reject_unknown_names() {
        for a in Activity::ALL {
            assert_eq!(a.as_str().parse::<Activity>().unwrap(), a);
            assert_eq!(
                serde_json::to_string(&a).unwrap(),
                format!("\"{}\"", a.as_str())
            );
        }
        assert!("twitter".parse::<Activity>().is_err());
    }
}
