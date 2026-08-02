use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};

use crate::capture;
use crate::classify;
use crate::config::{self, ServerConfig};
use crate::db::{Db, Minute};
use crate::proto::{Frame, FrameAck};
use crate::web;

pub fn run(cfg: Arc<ServerConfig>) -> Result<()> {
    // Fail at startup rather than an hour in, when the first frame arrives.
    let key = config::api_key()?;

    // SQLite allows one writer; a mutex keeps concurrent agents from colliding.
    let db = Arc::new(Mutex::new(Db::open()?));
    println!("db: {}", config::db_path()?.display());

    let addr = format!("0.0.0.0:{}", cfg.port);
    let server = Arc::new(
        tiny_http::Server::http(&addr).map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?,
    );
    println!("listening on {addr}");

    // A pool, not a loop. Classifying a frame blocks on the model for tens of
    // seconds, and a single-threaded accept loop serves nothing else meanwhile
    // -- health probes included, which is enough to get the pod restarted and
    // the route pulled out from under it. Workers are cheap; being unreachable
    // for the length of every model call is not.
    let workers = 4;
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (server, cfg, db, key) = (server.clone(), cfg.clone(), db.clone(), key.clone());
        handles.push(std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                handle(&cfg, &db, &key, req);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn handle(cfg: &ServerConfig, db: &Mutex<Db>, key: &str, mut req: tiny_http::Request) {
    {
        let is_ingest = req.url().starts_with("/v1/frame");

        if is_ingest {

            let mut body = String::new();
            if req.as_reader().read_to_string(&mut body).is_err() {
                let _ = req.respond(tiny_http::Response::from_string("bad body").with_status_code(400));
                return;
            }

            let result = serde_json::from_str::<Frame>(&body)
                .context("parsing frame")
                .and_then(|f| ingest(&cfg, &db, &key, f));

            let resp = match result {
                Ok(ack) => tiny_http::Response::from_string(
                    serde_json::to_string(&ack).unwrap_or_else(|_| "{}".into()),
                )
                    .with_header::<tiny_http::Header>(
                        "Content-Type: application/json".parse().unwrap(),
                    ),
                Err(e) => {
                    eprintln!("ingest: {e:#}");
                    tiny_http::Response::from_string(format!("{e:#}")).with_status_code(500)
                }
            };
            let _ = req.respond(resp);
        } else {
            let url = req.url().to_string();
            let param = |k: &str| -> Option<String> {
                url.split(&['?', '&'][..]).find_map(|p| {
                    p.strip_prefix(&format!("{k}=")).map(|v| {
                        v.replace('+', " ")
                    })
                }).filter(|v| !v.is_empty())
            };
            let q = crate::web::Query {
                day: param("d").and_then(|v| v.parse().ok()).unwrap_or(0).clamp(0, 3650),
                filter: crate::db::Filter {
                    category: param("cat").map(|v| urldecode(&v)),
                    device: param("dev").map(|v| urldecode(&v)),
                    app: param("app").map(|v| urldecode(&v)),
                },
            };

            let body = db
                .lock()
                .map_err(|e| anyhow::anyhow!("db lock: {e}"))
                .and_then(|db| web::page(&cfg, &db, &q))
                .unwrap_or_else(|e| format!("<pre>error: {e:#}</pre>"));

            let _ = req.respond(
                tiny_http::Response::from_string(body).with_header::<tiny_http::Header>(
                    "Content-Type: text/html; charset=utf-8".parse().unwrap(),
                ),
            );
        }
    }
}

/// Decide what a frame means and record it. Everything expensive happens here:
/// the idle check, the model call, and the write.
fn ingest(cfg: &ServerConfig, db: &Mutex<Db>, key: &str, frame: Frame) -> Result<FrameAck> {
    // Read what we need and release immediately. The model call below takes
    // tens of seconds, and holding the lock across it would block the UI, stall
    // every other agent, and time out the liveness probe into a restart loop.
    let (existing, last) = {
        let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        (db.get(&frame.device, frame.ts)?, db.last(&frame.device)?)
    };

    if let Some(existing) = existing {
        return Ok(FrameAck {
            category: existing.category,
            project: existing.project,
            detail: existing.detail,
            classified: false,
        });
    }

    if frame.blocked {
        let m = Minute {
            ts: frame.ts,
            device: frame.device,
            category: "other".into(),
            project: None,
            detail: Some("blocked window — not captured".into()),
            window: None,
            phash: 0,
            keys: frame.keys,
            mouse: frame.mouse,
            idle_secs: frame.idle_secs,
            apps: Vec::new(),
            workspaces: frame.workspaces,
            classified: false,
            model: None,
            tags: vec!["other".to_string()],
        };
        db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?.insert(&m)?;
        return Ok(ack(m, false));
    }

    let Some(image_b64) = frame.image.as_deref() else {
        anyhow::bail!("frame has neither an image nor the blocked flag");
    };
    let jpeg = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(image_b64)
            .context("decoding frame image")?
    };

    let img = image::load_from_memory(&jpeg).context("decoding frame JPEG")?;
    let phash = capture::dhash(&img);
    drop(img);

    // Unchanged screen AND no human input means nothing new to classify. Both
    // halves matter: a screen can sit still while someone reads, and it can
    // change constantly with nobody there. Only when neither moved is the
    // minute genuinely uninteresting enough to skip the model.
    let no_input = frame.keys == 0 && frame.mouse == 0;
    if let Some(prev) = &last {
        if prev.ts + 60 == frame.ts
            && no_input
            && capture::hamming(phash, prev.phash as u64) <= cfg.idle_distance
        {
            // A still screen with nobody touching it for minutes is idle, and
            // saying so needs no model -- it is a fact, not a judgment.
            // Without this the last real label propagates forever and an
            // empty room reads as a full working day.
            let long_gone = frame
                .idle_secs
                .is_some_and(|s| s >= cfg.idle_after_secs);

            let (category, project, detail) = if long_gone || prev.category == "idle" {
                (
                    "idle".to_string(),
                    None,
                    "screen unchanged, no input".to_string(),
                )
            } else {
                // A short pause is still the same activity -- thinking,
                // reading a paragraph -- so carry the label.
                (
                    prev.category.clone(),
                    prev.project.clone(),
                    "screen unchanged".to_string(),
                )
            };

            let m = Minute {
                ts: frame.ts,
                device: frame.device,
                category,
                project,
                detail: Some(detail),
                window: Some(frame.window),
                phash: phash as i64,
                keys: frame.keys,
                mouse: frame.mouse,
                idle_secs: frame.idle_secs,
                apps: frame.apps.clone(),
                workspaces: frame.workspaces,
                classified: false,
                tags: prev.tags.clone(),
                model: None,
            };
            db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?.insert(&m)?;
            return Ok(ack(m, false));
        }
    }

    let prev = last.as_ref().map(|l| classify::Previous {
        category: &l.category,
        project: l.project.as_deref(),
        detail: l.detail.as_deref(),
    });
    let presence = classify::Presence {
        device: &frame.device,
        idle_secs: frame.idle_secs,
        keys: frame.keys,
        mouse: frame.mouse,
        note: frame.note.as_deref(),
    };
    let label = classify::classify(cfg, key, &jpeg, &frame.window, presence, prev)?;

    let m = Minute {
        ts: frame.ts,
        device: frame.device,
        category: label.category,
        project: label.project,
        detail: label.detail,
        window: Some(frame.window),
        phash: phash as i64,
        keys: frame.keys,
        mouse: frame.mouse,
        idle_secs: frame.idle_secs,
        apps: frame.apps.clone(),
        workspaces: frame.workspaces,
        classified: true,
        tags: label.tags.clone(),
        model: Some(cfg.model.clone()),
    };
    db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?.insert(&m)?;
    Ok(ack(m, true))
    // The JPEG goes out of scope here and is never written anywhere.
}

fn ack(m: Minute, classified: bool) -> FrameAck {
    FrameAck {
        category: m.category,
        project: m.project,
        detail: m.detail,
        classified,
    }
}

/// Minimal percent-decoding for query values.
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
