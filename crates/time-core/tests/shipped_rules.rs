//! The rules Time ships (`config/rules.toml`), asked about the tabs, windows
//! and apps Erik actually has. Each case is a fact from his data or a thing
//! he said about it.

use time_core::label::Activity;
use time_core::rules::{tab_key, Facts, RuleSet, Source, TabVerdict};

fn rules() -> RuleSet {
    RuleSet::parse(include_str!("../../../config/rules.toml")).unwrap()
}

fn window<'a>(class: &'a str, title: &'a str, host: Option<&'a str>) -> Facts<'a> {
    Facts {
        source: Some(Source::Desktop),
        class,
        title,
        host,
        package: None,
    }
}

fn phone(package: &str) -> Facts<'_> {
    Facts {
        source: Some(Source::Phone),
        package: Some(package),
        ..Default::default()
    }
}

/// (activity, rule id, project) of the first matching rule.
fn decide(r: &RuleSet, f: &Facts) -> (Activity, String, Option<String>) {
    let m = r.classify(f).expect("a rule matches");
    let project = m
        .project
        .map(str::to_owned)
        .or_else(|| r.project_for_title(f.title).map(str::to_owned));
    (m.activity, m.rule.to_owned(), project)
}

fn activity(r: &RuleSet, f: &Facts) -> Activity {
    decide(r, f).0
}

#[test]
fn teams_is_work_wherever_it_shows_up() {
    let r = rules();
    // Erik, 2026-10-26: the title that was counted as messaging.
    for title in [
        "Chat | Kaan Koska | Microsoft Teams - Helium",
        "(2) Chat | NaiaClaw Infrastructure | Microsoft Teams - Helium",
        "Calendar | Weekly | Microsoft Teams — Original profile — Mozilla Firefox",
        "Microsoft Teams",
    ] {
        for class in [
            "helium",
            "firefox",
            "zen-beta",
            "google-chrome",
            "chromium",
            "whatever",
        ] {
            let (a, rule, project) = decide(&r, &window(class, title, None));
            assert_eq!(a, Activity::Work, "{class} / {title}: {rule}");
            assert_eq!(project.as_deref(), Some("neptune"), "{class} / {title}");
        }
    }
    // The site, whatever the title, in any browser.
    for host in [
        "teams.microsoft.com",
        "teams.cloud.microsoft",
        "x.teams.microsoft.com",
    ] {
        assert_eq!(
            activity(&r, &window("zen-beta", "Loading…", Some(host))),
            Activity::Work,
            "{host}"
        );
    }
    // The desktop app and the installed web app.
    for class in [
        "teams-for-linux",
        "Microsoft Teams - Preview",
        "chrome-teams.microsoft.com__-Default",
        "chrome-teams.cloud.microsoft__v2-Default",
    ] {
        assert_eq!(
            activity(&r, &window(class, "Chat", None)),
            Activity::Work,
            "{class}"
        );
    }
    // The phone app.
    assert_eq!(activity(&r, &phone("com.microsoft.teams")), Activity::Work);
}

#[test]
fn personal_messaging_stays_messaging() {
    let r = rules();
    for (class, title) in [
        ("webcord", "[598] WebCord - @Purë"),
        ("discord", "Discord"),
        ("signal", "Signal"),
        ("helium", "Messenger"),
        ("firefox", "WhatsApp Web"),
        (
            "firefox",
            "Inbox (3) - erik@gmail.com - Gmail — Original profile — Mozilla Firefox",
        ),
    ] {
        assert_eq!(
            activity(&r, &window(class, title, None)),
            Activity::Messaging,
            "{class} / {title}"
        );
    }
    for package in [
        "com.discord",
        "com.whatsapp",
        "org.thoughtcrime.securesms",
        "com.facebook.orca",
    ] {
        assert_eq!(
            activity(&r, &phone(package)),
            Activity::Messaging,
            "{package}"
        );
    }
}

#[test]
fn the_neptune_mail_and_cloud_are_work() {
    let r = rules();
    for (class, title, host) in [
        (
            "firefox",
            "Mail - Erik Frankling - Outlook — Original profile — Mozilla Firefox",
            None,
        ),
        ("zen-beta", "Calendar", Some("outlook.office.com")),
        (
            "firefox",
            "Microsoft 365 Agents Playground — Original profile — Mozilla Firefox",
            None,
        ),
        ("zen-beta", "Home", Some("portal.azure.com")),
        ("firefox", "Boards - Azure DevOps", None),
        ("zen-beta", "Pipelines", Some("dev.azure.com")),
        (
            "firefox",
            "Naia Works — Original profile — Mozilla Firefox",
            None,
        ),
        (
            "firefox",
            "Naia Works Admin — Original profile — Mozilla Firefox",
            None,
        ),
        (
            "zen-beta",
            "Dashboard",
            Some("acme.naiaworks.neptune-software.com"),
        ),
        (
            "firefox",
            "Releases · neptune-software/naiaclaw — Mozilla Firefox",
            None,
        ),
        (
            "firefox",
            "NaiaClaw / Overview - Dashboards - Grafana",
            None,
        ),
        // Neptune mail in Thunderbird, a Neptune Slack: work; the rest of
        // those apps stays messaging.
        (
            "thunderbird",
            "Re: [neptune-software/naiaclaw] Aproval system",
            None,
        ),
        ("Slack", "general - Neptune - Slack", None),
    ] {
        let (a, rule, project) = decide(&r, &window(class, title, host));
        assert_eq!(a, Activity::Work, "{class} / {title}: {rule}");
        assert_eq!(
            project.as_deref(),
            Some("neptune"),
            "{class} / {title}: {rule}"
        );
    }
    assert_eq!(
        activity(&r, &phone("com.microsoft.office.outlook")),
        Activity::Work
    );
    assert_eq!(
        activity(&r, &window("thunderbird", "Inbox", None)),
        Activity::Messaging
    );
    assert_eq!(
        activity(&r, &window("Slack", "friends - Slack", None)),
        Activity::Messaging
    );
}

#[test]
fn course_and_school_tabs_are_study() {
    let r = rules();
    for (class, title, host) in [
        (
            "firefox",
            "School — Original profile — Mozilla Firefox",
            None,
        ),
        ("firefox", "hw_4_vettig.pdf — Mozilla Firefox", None),
        (
            "firefox",
            "Study visit, autumn 2026: DD2300 prosamm 26/27 — Mozilla Firefox",
            None,
        ),
        (
            "firefox",
            "DD2395 Lecture 4 — Original profile — Mozilla Firefox",
            None,
        ),
        ("zen-beta", "Schedule", Some("cloud.timeedit.net")),
        ("firefox", "Attention Is All You Need", Some("arxiv.org")),
        (
            "firefox",
            "[2410.01234] Sparse attention — Mozilla Firefox",
            None,
        ),
        ("firefox", "Canvas", Some("canvas.kth.se")),
    ] {
        let (a, rule, project) = decide(&r, &window(class, title, host));
        assert_eq!(a, Activity::Study, "{class} / {title}: {rule}");
        assert_eq!(project.as_deref(), Some("kth"), "{class} / {title}");
    }
    // The tab called School only counts in a browser.
    assert_ne!(
        activity(&r, &window("kitty", "School", None)),
        Activity::Study
    );
}

#[test]
fn his_own_projects_are_work_with_the_project_named() {
    let r = rules();
    for (class, title, host, project) in [
        (
            "firefox",
            "Time · October 2026 — Original profile — Mozilla Firefox",
            None,
            "time",
        ),
        (
            "zen-beta",
            "Today",
            Some("time.erikfrankling.duckdns.org"),
            "time",
        ),
        (
            "firefox",
            "x-graph — Original profile — Mozilla Firefox",
            None,
            "x-graph",
        ),
        (
            "zen-beta",
            "Archive",
            Some("x.erikfrankling.duckdns.org"),
            "x-graph",
        ),
        (
            "firefox",
            "T3 Code (Beta) — Mozilla Firefox",
            None,
            "t3code",
        ),
        (
            "zen-beta",
            "Brain",
            Some("brain.erikfrankling.duckdns.org"),
            "homelab",
        ),
        ("firefox", "Traefik Dashboard", None, "homelab"),
        ("firefox", "NixOS Search - Packages", None, "dotfiles"),
        ("firefox", "Husk Security — brand hub", None, "husk"),
    ] {
        let (a, rule, p) = decide(&r, &window(class, title, host));
        assert_eq!(a, Activity::Work, "{title}: {rule}");
        assert_eq!(p.as_deref(), Some(project), "{title}: {rule}");
    }
    // Plain GitHub and localhost are work without a project.
    assert_eq!(
        activity(&r, &window("firefox", "Pull requests", Some("github.com"))),
        Activity::Work
    );
}

#[test]
fn what_stays_leisure_stays_leisure() {
    let r = rules();
    assert_eq!(
        activity(
            &r,
            &window(
                "firefox",
                "(1) Home / X — Original profile — Mozilla Firefox",
                None
            )
        ),
        Activity::Feeds
    );
    assert_eq!(
        activity(&r, &window("firefox", "Wednesday - YouTube", None)),
        Activity::Video
    );
    assert_eq!(
        activity(
            &r,
            &window(
                "firefox",
                "Dagens Nyheter – senaste nyheterna — Mozilla Firefox",
                None
            )
        ),
        Activity::Browsing
    );
}

#[test]
fn only_the_last_resorts_are_left_to_the_model() {
    let r = rules();
    let asks = |class: &str, title: &str| r.asks_model(&window(class, title, None));
    // Nothing says what these are: the model is asked.
    assert!(asks(
        "firefox",
        "The Agentic Web Must Stay Open — a manifesto — Original profile — Mozilla Firefox"
    )
    .is_some());
    assert!(asks(
        "firefox",
        "Spontaneous generation - Wikipedia — Mozilla Firefox"
    )
    .is_some());
    // A post he opened is judged by what it says; the home feed is not.
    assert!(asks(
        "firefox",
        "(1) Jon Barron on X: \"Hi all, I have a new result\" / X — Original profile — Mozilla Firefox"
    )
    .is_some());
    // Settled by a rule: never asked.
    assert!(asks("helium", "Chat | Kaan Koska | Microsoft Teams - Helium").is_none());
    assert!(asks(
        "firefox",
        "(1) Home / X — Original profile — Mozilla Firefox"
    )
    .is_none());
    assert!(asks("kitty", "fish").is_none());
}

#[test]
fn the_models_verdict_beats_a_last_resort_and_nothing_else() {
    let title = "The Agentic Web Must Stay Open — a manifesto — Original profile — Mozilla Firefox";
    let mut verdicts = std::collections::HashMap::new();
    verdicts.insert(
        tab_key(title),
        TabVerdict {
            activity: Activity::Work,
            project: Some("neptune".into()),
        },
    );
    // Even a verdict for the Teams tab changes nothing: a rule settled it.
    verdicts.insert(
        tab_key("Chat | Kaan Koska | Microsoft Teams - Helium"),
        TabVerdict {
            activity: Activity::Browsing,
            project: None,
        },
    );
    let plain = rules();
    let r = rules().with_tab_verdicts(verdicts);
    assert_ne!(
        plain.version, r.version,
        "labels are recomputed with new verdicts"
    );

    let before = decide(&plain, &window("firefox", title, None));
    assert_eq!(
        (before.0, before.1.as_str()),
        (Activity::Browsing, "fallback.browser-tab")
    );
    let after = decide(&r, &window("firefox", title, None));
    assert_eq!(
        after,
        (Activity::Work, "model.tab".into(), Some("neptune".into()))
    );
    // The unread count and the browser's name do not change the tab.
    let same = decide(
        &r,
        &window(
            "helium",
            &format!(
                "(3) {}",
                title.replace(" — Original profile — Mozilla Firefox", " - Helium")
            ),
            None,
        ),
    );
    assert_eq!(same.1, "model.tab");

    let teams = decide(
        &r,
        &window(
            "helium",
            "Chat | Kaan Koska | Microsoft Teams - Helium",
            None,
        ),
    );
    assert_eq!(
        (teams.0, teams.1.as_str()),
        (Activity::Work, "work.teams-title")
    );
}

#[test]
fn the_model_is_told_who_erik_is_and_what_his_projects_are() {
    let p = rules().profile_prompt();
    for needle in [
        "Naia Works",
        "KTH",
        "DD2395",
        "x-graph",
        "homelab",
        "Teams",
        "almost everything he does in it is work or study",
    ] {
        assert!(p.contains(needle), "profile prompt lacks {needle:?}:\n{p}");
    }
}
