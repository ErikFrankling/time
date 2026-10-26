//! The rule table: deterministic, typed, explainable classification.
//!
//! A rule names an activity and the facts that imply it. Conditions within
//! a rule must all hold; the patterns listed for one condition are
//! alternatives. The first rule that matches wins, and its id is stored with
//! the label so any minute can be traced to the line that decided it.
//!
//! ```toml
//! [[rule]]
//! id = "video.youtube"
//! activity = "video"
//! host = ["youtube.com", "youtu.be"]     # also matches subdomains
//!
//! [[rule]]
//! id = "games.steam"
//! activity = "games"
//! class = ["steam_app_*"]                # `*` wildcards, case-insensitive
//!
//! [[project]]
//! name = "husk"
//! title = "(?i)\\bhusk\\b"               # a regex over the window title
//! paths = ["/home/erikf/projects/husk/"] # working directories (T3, terminals)
//! ```

use std::collections::HashMap;

use regex::Regex;
use serde::Deserialize;

use crate::label::Activity;

/// Which kind of device produced the facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Desktop,
    Phone,
}

/// What is known about one device in one minute, as rules see it.
#[derive(Debug, Clone, Default)]
pub struct Facts<'a> {
    pub source: Option<Source>,
    pub class: &'a str,
    pub title: &'a str,
    pub host: Option<&'a str>,
    pub package: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRules {
    #[serde(default, rename = "rule")]
    rules: Vec<RawRule>,
    #[serde(default, rename = "project")]
    projects: Vec<RawProject>,
    #[serde(default)]
    profile: Option<Profile>,
}

/// Who Erik is and how he works, for the decision model that labels the
/// browser tabs the rules cannot (`crate::tabs` in the server). Kept with
/// the rules so extending it is a config edit.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// A few sentences about Erik and his week.
    #[serde(default)]
    pub about: String,
    /// How he behaves in a browser, in the model's terms.
    #[serde(default)]
    pub habits: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    activity: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    source: Option<Source>,
    #[serde(default)]
    class: Vec<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    host: Vec<String>,
    #[serde(default)]
    package: Vec<String>,
    /// A last resort the decision model may overrule for a tab it has
    /// looked at (see [`TabVerdict`]).
    #[serde(default)]
    ask_model: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    /// `work`, `study` or `own`: what time on it counts as.
    #[serde(default)]
    kind: Option<String>,
    /// What it is and where it lives (sites, repos, tab titles), in a
    /// sentence or two, for the decision model.
    #[serde(default)]
    about: Option<String>,
}

#[derive(Debug)]
pub struct Rule {
    pub id: String,
    pub activity: Activity,
    pub project: Option<String>,
    source: Option<Source>,
    class: Vec<Glob>,
    title: Option<Regex>,
    host: Vec<String>,
    package: Vec<Glob>,
    ask_model: bool,
}

#[derive(Debug)]
pub struct Project {
    pub name: String,
    title: Option<Regex>,
    paths: Vec<String>,
    pub kind: Option<String>,
    pub about: Option<String>,
}

#[derive(Debug)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
    pub projects: Vec<Project>,
    pub profile: Profile,
    /// What the decision model said about tabs the rules left to it, by
    /// [`tab_key`]. Empty until the server loads them.
    pub tab_verdicts: HashMap<String, TabVerdict>,
    /// Changes whenever the file's content does, so stored labels can be
    /// recomputed when the rules they came from are edited.
    pub version: String,
}

/// What the decision model decided a tab is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabVerdict {
    pub activity: Activity,
    pub project: Option<String>,
}

/// A browser tab by its title without the browser's own name or an unread
/// count, lower case: "(2) Chat | Kaan Koska | Microsoft Teams - Helium"
/// and the same chat later in the day are one tab.
pub fn tab_key(title: &str) -> String {
    tab_core(title).to_lowercase()
}

/// A tab title without the browser's name and profile ("Article — Original
/// profile — Mozilla Firefox" is "Article") and without a leading "(2) ".
pub fn tab_core(title: &str) -> &str {
    let mut t = title.trim();
    if let Some((n, rest)) = t.strip_prefix('(').and_then(|r| r.split_once(") ")) {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            t = rest.trim();
        }
    }
    let mut again = true;
    while again {
        again = false;
        for sep in [" — ", " - "] {
            if let Some((head, tail)) = t.rsplit_once(sep) {
                let tail = tail.to_ascii_lowercase();
                // Only a bare browser or profile name is the browser's: a tail with
                // the other separator in it is part of the page's title.
                if !tail.contains(" — ")
                    && !tail.contains(" - ")
                    && ["firefox", "zen", "chrom", "brave", "helium", "profile"]
                        .iter()
                        .any(|b| tail.contains(b))
                {
                    t = head.trim();
                    again = true;
                    break;
                }
            }
        }
    }
    t
}

/// The decision a rule made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match<'r> {
    pub rule: &'r str,
    pub activity: Activity,
    pub project: Option<&'r str>,
}

impl RuleSet {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: RawRules = toml::from_str(text)?;
        let mut rules = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for r in raw.rules {
            if !seen.insert(r.id.clone()) {
                anyhow::bail!("rule id {:?} is used twice", r.id);
            }
            let activity: Activity = r
                .activity
                .parse()
                .map_err(|e| anyhow::anyhow!("rule {:?}: {e}", r.id))?;
            if r.class.is_empty() && r.title.is_none() && r.host.is_empty() && r.package.is_empty()
            {
                anyhow::bail!(
                    "rule {:?} has no conditions and would match everything",
                    r.id
                );
            }
            let title = r
                .title
                .as_deref()
                .map(Regex::new)
                .transpose()
                .map_err(|e| anyhow::anyhow!("rule {:?}: title regex: {e}", r.id))?;
            rules.push(Rule {
                id: r.id,
                activity,
                project: r.project,
                source: r.source,
                class: r.class.iter().map(|p| Glob::new(p)).collect(),
                title,
                host: r.host.iter().map(|h| h.to_ascii_lowercase()).collect(),
                package: r.package.iter().map(|p| Glob::new(p)).collect(),
                ask_model: r.ask_model,
            });
        }
        let mut projects = Vec::new();
        for p in raw.projects {
            let title = p
                .title
                .as_deref()
                .map(Regex::new)
                .transpose()
                .map_err(|e| anyhow::anyhow!("project {:?}: title regex: {e}", p.name))?;
            projects.push(Project {
                name: p.name,
                title,
                paths: p.paths,
                kind: p.kind,
                about: p.about,
            });
        }
        Ok(Self {
            rules,
            projects,
            profile: raw.profile.unwrap_or_default(),
            tab_verdicts: HashMap::new(),
            version: format!("{:016x}", fnv1a(text.as_bytes())),
        })
    }

    /// The first rule that matches, if any.
    pub fn classify(&self, f: &Facts) -> Option<Match<'_>> {
        let r = self.rules.iter().find(|r| r.matches(f))?;
        if r.ask_model {
            if let Some(v) = self.tab_verdicts.get(&tab_key(f.title)) {
                return Some(Match {
                    rule: "model.tab",
                    activity: v.activity,
                    project: v.project.as_deref(),
                });
            }
        }
        Some(Match {
            rule: &r.id,
            activity: r.activity,
            project: r.project.as_deref(),
        })
    }

    /// The key to ask the decision model about this tab under, when the
    /// rule that catches it leaves it to the model.
    pub fn asks_model(&self, f: &Facts) -> Option<String> {
        let r = self.rules.iter().find(|r| r.matches(f))?;
        r.ask_model.then(|| tab_key(f.title))
    }

    /// The same rules, with the model's verdicts on tabs. The version
    /// changes with them, so labels are recomputed when they do.
    pub fn with_tab_verdicts(mut self, verdicts: HashMap<String, TabVerdict>) -> Self {
        let mut sorted: Vec<_> = verdicts.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut text = self.version.clone();
        for (k, v) in sorted {
            text.push_str(&format!(
                "\n{k}\t{}\t{}",
                v.activity,
                v.project.as_deref().unwrap_or("")
            ));
        }
        self.version = format!("{:016x}", fnv1a(text.as_bytes()));
        self.tab_verdicts = verdicts;
        self
    }

    /// What the decision model is told about Erik and his projects.
    pub fn profile_prompt(&self) -> String {
        let mut out = String::new();
        if !self.profile.about.trim().is_empty() {
            out.push_str(self.profile.about.trim());
            out.push_str("\n\n");
        }
        out.push_str("His projects, and what time on each counts as:\n");
        for p in self.projects.iter().filter(|p| p.about.is_some()) {
            out.push_str(&format!(
                "- {} ({}): {}\n",
                p.name,
                p.kind.as_deref().unwrap_or("work"),
                p.about.as_deref().unwrap_or("").trim()
            ));
        }
        if !self.profile.habits.trim().is_empty() {
            out.push('\n');
            out.push_str(self.profile.habits.trim());
            out.push('\n');
        }
        out
    }

    /// The project names, for the model's answer.
    pub fn project_names(&self) -> Vec<&str> {
        self.projects.iter().map(|p| p.name.as_str()).collect()
    }

    /// A project named by the window title.
    pub fn project_for_title(&self, title: &str) -> Option<&str> {
        self.projects
            .iter()
            .find(|p| p.title.as_ref().is_some_and(|re| re.is_match(title)))
            .map(|p| p.name.as_str())
    }

    /// A project owning a working directory (longest prefix wins).
    pub fn project_for_path(&self, path: &str) -> Option<&str> {
        self.projects
            .iter()
            .flat_map(|p| p.paths.iter().map(move |prefix| (prefix, p)))
            .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, p)| p.name.as_str())
    }
}

impl Rule {
    fn matches(&self, f: &Facts) -> bool {
        if self.source.is_some() && self.source != f.source {
            return false;
        }
        if !self.class.is_empty() && !self.class.iter().any(|g| g.matches(f.class)) {
            return false;
        }
        if let Some(re) = &self.title {
            if !re.is_match(f.title) {
                return false;
            }
        }
        if !self.host.is_empty() {
            let Some(host) = f.host else { return false };
            let host = host.to_ascii_lowercase();
            let host = host.strip_prefix("www.").unwrap_or(&host);
            if !self
                .host
                .iter()
                .any(|h| host == h || host.ends_with(&format!(".{h}")))
            {
                return false;
            }
        }
        if !self.package.is_empty() {
            let Some(pkg) = f.package else { return false };
            if !self.package.iter().any(|g| g.matches(pkg)) {
                return false;
            }
        }
        true
    }
}

/// Case-insensitive `*` wildcard pattern.
#[derive(Debug)]
struct Glob(Vec<String>);

impl Glob {
    fn new(p: &str) -> Self {
        Glob(p.to_lowercase().split('*').map(str::to_owned).collect())
    }

    fn matches(&self, s: &str) -> bool {
        let s = s.to_lowercase();
        let parts = &self.0;
        if parts.len() == 1 {
            return s == parts[0];
        }
        let (first, last) = (&parts[0], &parts[parts.len() - 1]);
        if !s.starts_with(first.as_str()) || !s[first.len()..].ends_with(last.as_str()) {
            return false;
        }
        let mut rest = &s[first.len()..s.len() - last.len()];
        for mid in &parts[1..parts.len() - 1] {
            match rest.find(mid.as_str()) {
                Some(i) => rest = &rest[i + mid.len()..],
                None => return false,
            }
        }
        true
    }
}

/// A stable hash (unlike `DefaultHasher`, which may change between Rust releases).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"
        [[rule]]
        id = "video.youtube"
        activity = "video"
        host = ["youtube.com"]

        [[rule]]
        id = "phone.x"
        activity = "feeds"
        source = "phone"
        package = ["com.twitter.android"]

        [[rule]]
        id = "games.steam"
        activity = "games"
        class = ["steam_app_*"]

        [[rule]]
        id = "work.t3"
        activity = "work"
        class = ["com.t3tools.T3Code"]

        [[rule]]
        id = "video.netflix-title"
        activity = "video"
        class = ["firefox", "zen*"]
        title = "(?i)netflix|hbo max"

        [[project]]
        name = "husk"
        title = "(?i)\\bhusk\\b"
        paths = ["/home/erikf/projects/husk/"]

        [[project]]
        name = "husk-web"
        paths = ["/home/erikf/projects/husk/web/"]
    "#;

    fn set() -> RuleSet {
        RuleSet::parse(RULES).unwrap()
    }

    #[test]
    fn first_match_wins_and_names_itself() {
        let s = set();
        let m = s
            .classify(&Facts {
                class: "firefox",
                host: Some("m.youtube.com"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!((m.rule, m.activity), ("video.youtube", Activity::Video));
        assert!(s
            .classify(&Facts {
                class: "firefox",
                host: Some("notyoutube.com"),
                ..Default::default()
            })
            .is_none());
        let m = s
            .classify(&Facts {
                class: "zen-beta",
                title: "Angel Has Fallen • HBO Max — Zen",
                ..Default::default()
            })
            .unwrap();
        assert_eq!(m.rule, "video.netflix-title");
    }

    #[test]
    fn source_and_package_conditions() {
        let s = set();
        let phone = Facts {
            source: Some(Source::Phone),
            package: Some("com.twitter.android"),
            ..Default::default()
        };
        assert_eq!(s.classify(&phone).unwrap().activity, Activity::Feeds);
        let desktop = Facts {
            source: Some(Source::Desktop),
            ..phone
        };
        assert!(s.classify(&desktop).is_none());
    }

    #[test]
    fn globs() {
        assert!(Glob::new("steam_app_*").matches("steam_app_410900"));
        assert!(Glob::new("*Castles*").matches("KingdomsAndCastles.x86_64"));
        assert!(!Glob::new("steam_app_*").matches("steam"));
        assert!(Glob::new("a*b*c").matches("aXbYc"));
        assert!(!Glob::new("a*b*c").matches("acb"));
    }

    #[test]
    fn projects_by_title_and_longest_path() {
        let s = set();
        assert_eq!(s.project_for_title("Husk sprint planning"), Some("husk"));
        assert_eq!(s.project_for_title("husky dogs"), None);
        assert_eq!(
            s.project_for_path("/home/erikf/projects/husk/web/src"),
            Some("husk-web")
        );
        assert_eq!(
            s.project_for_path("/home/erikf/projects/husk/api"),
            Some("husk")
        );
    }

    #[test]
    fn mistakes_are_errors_at_load() {
        assert!(
            RuleSet::parse("[[rule]]\nid='a'\nactivity='twitter'\nclass=['x']").is_err(),
            "unknown activity"
        );
        assert!(
            RuleSet::parse("[[rule]]\nid='a'\nactivity='work'").is_err(),
            "no conditions"
        );
        assert!(RuleSet::parse("[[rule]]\nid='a'\nactivity='work'\nclass=['x']\n[[rule]]\nid='a'\nactivity='work'\nclass=['y']").is_err(), "dup id");
        assert!(
            RuleSet::parse("[[rule]]\nid='a'\nactivity='work'\ntitle='('").is_err(),
            "bad regex"
        );
        assert!(
            RuleSet::parse("[[rule]]\nid='a'\nactivity='work'\nklass=['x']").is_err(),
            "typo"
        );
    }

    #[test]
    fn version_tracks_content() {
        assert_eq!(set().version, set().version);
        assert_ne!(
            set().version,
            RuleSet::parse(&RULES.replace("video.youtube", "video.yt"))
                .unwrap()
                .version
        );
    }
}
