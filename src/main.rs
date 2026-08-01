mod capture;
mod classify;
mod config;
mod db;
mod web;

use anyhow::Result;
use std::sync::Arc;

use config::Config;
use db::{Db, Minute};

const USAGE: &str = "time — minute-by-minute activity tracking

  time run      capture + classify every minute, and serve the UI
  time serve    serve the UI only
  time once     capture and classify a single minute, print the result
  time config   print the config file path
";

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let cfg = Arc::new(Config::load()?);

    match cmd.as_str() {
        "run" => run(cfg),
        "serve" => web::serve(cfg),
        "once" => {
            let db = Db::open()?;
            let key = cfg.api_key()?;
            match tick(&cfg, &db, &key)? {
                Some(m) => {
                    println!(
                        "{} [{}] {} — {}",
                        chrono::DateTime::from_timestamp(m.ts, 0)
                            .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
                            .unwrap_or_default(),
                        m.category,
                        m.project.as_deref().unwrap_or("-"),
                        m.detail.as_deref().unwrap_or("")
                    );
                }
                None => println!("skipped (already recorded this minute)"),
            }
            Ok(())
        }
        "config" => {
            println!("{}", config::config_path()?.display());
            println!("{}", config::key_path()?.display());
            println!("{}", config::db_path()?.display());
            Ok(())
        }
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

fn run(cfg: Arc<Config>) -> Result<()> {
    // Fail fast rather than discovering a missing key an hour into a session.
    let key = cfg.api_key()?;
    let db = Db::open()?;
    println!("db: {}", config::db_path()?.display());

    {
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            if let Err(e) = web::serve(cfg) {
                eprintln!("ui: {e:#}");
            }
        });
    }

    loop {
        match tick(&cfg, &db, &key) {
            Ok(Some(m)) => println!(
                "{} [{}] {}",
                chrono::Local::now().format("%H:%M"),
                m.category,
                m.detail.as_deref().unwrap_or("")
            ),
            Ok(None) => {}
            // A transient API or compositor failure must not kill a daemon
            // that's meant to run for months.
            Err(e) => eprintln!("{} error: {e:#}", chrono::Local::now().format("%H:%M")),
        }
        sleep_to_next_minute();
    }
}

fn sleep_to_next_minute() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let wait = 60 - (now % 60);
    std::thread::sleep(std::time::Duration::from_secs(wait.max(1)));
}

/// Capture, classify and record the current minute. Returns None if this minute
/// is already recorded.
fn tick(cfg: &Config, db: &Db, key: &str) -> Result<Option<Minute>> {
    let now = chrono::Local::now().timestamp();
    let ts = now - (now % 60);

    let last = db.last()?;
    if last.as_ref().is_some_and(|l| l.ts == ts) {
        return Ok(None);
    }

    let window = capture::active_window();

    // Blocked windows are recorded so the minute isn't a hole in the day, but
    // no screenshot is taken and nothing leaves the machine.
    if window.blocked(&cfg.blocklist) {
        let m = Minute {
            ts,
            category: "other".into(),
            project: None,
            detail: Some("blocked window — not captured".into()),
            window: None,
            phash: 0,
            model: None,
        };
        db.insert(&m)?;
        return Ok(Some(m));
    }

    let img = capture::screenshot(cfg.width)?;
    let phash = capture::dhash(&img);

    // If the screen is unchanged from the previous minute, there is nothing new
    // to classify. Carry the label forward when the machine was already in use,
    // and call it idle otherwise. This skip is what keeps the API bill sane.
    if let Some(prev) = &last {
        if prev.ts + 60 == ts
            && capture::hamming(phash, prev.phash as u64) <= cfg.idle_distance
        {
            let m = Minute {
                ts,
                category: if prev.category == "idle" {
                    "idle".into()
                } else {
                    prev.category.clone()
                },
                project: prev.project.clone(),
                detail: Some("screen unchanged".into()),
                window: Some(window.describe()),
                phash: phash as i64,
                model: None,
            };
            db.insert(&m)?;
            return Ok(Some(m));
        }
    }

    let jpeg = capture::to_jpeg(&img)?;
    // The image exists only in this scope; nothing writes it to disk.
    drop(img);

    let prev = last.as_ref().map(|l| classify::Previous {
        category: &l.category,
        project: l.project.as_deref(),
        detail: l.detail.as_deref(),
    });

    let label = classify::classify(cfg, key, &jpeg, &window.describe(), prev)?;

    let m = Minute {
        ts,
        category: label.category,
        project: label.project,
        detail: label.detail,
        window: Some(window.describe()),
        phash: phash as i64,
        model: Some(cfg.model.clone()),
    };
    db.insert(&m)?;
    Ok(Some(m))
}
