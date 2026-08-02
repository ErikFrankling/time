use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};

use crate::capture;
use crate::classify;
use crate::config::{self, ServerConfig};
use crate::db::{Db, Minute};
use crate::proto::{Frame, FrameAck};
use crate::report;
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

        if req.url().starts_with("/v1/code") {
            let mut body = String::new();
            if req.as_reader().read_to_string(&mut body).is_err() {
                let _ = req.respond(tiny_http::Response::from_string("bad body").with_status_code(400));
                return;
            }
            let result = serde_json::from_str::<crate::proto::CodeReport>(&body)
                .context("parsing code report")
                .and_then(|r| {
                    let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
                    db.put_code(&r.days)
                });
            let resp = match result {
                Ok(n) => tiny_http::Response::from_string(format!("stored {n} rows\n")),
                Err(e) => {
                    eprintln!("code: {e:#}");
                    tiny_http::Response::from_string(format!("{e:#}")).with_status_code(500)
                }
            };
            let _ = req.respond(resp);
            return;
        }

        if req.url().starts_with("/v1/agents") {
            let mut body = String::new();
            if req.as_reader().read_to_string(&mut body).is_err() {
                let _ = req.respond(tiny_http::Response::from_string("bad body").with_status_code(400));
                return;
            }
            let result = serde_json::from_str::<crate::proto::AgentReport>(&body)
                .context("parsing agent report")
                .and_then(|r| {
                    let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
                    db.put_agents(&r)
                });
            let resp = match result {
                Ok(n) => tiny_http::Response::from_string(format!("stored {n} rows\n")),
                Err(e) => {
                    eprintln!("agents: {e:#}");
                    tiny_http::Response::from_string(format!("{e:#}")).with_status_code(500)
                }
            };
            let _ = req.respond(resp);
            return;
        }

        // Sideloading is the only distribution channel this app has, so the
        // server that receives the frames also hands out the APK that sends
        // them -- one URL to type into a phone, no store, no third party.
        let path = req.url().split('?').next().unwrap_or("/").to_string();
        if path == "/app" || path == "/app/" {
            let _ = req.respond(app_response(&path));
            return;
        }

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
                    domain: param("dom").map(|v| urldecode(&v)),
                },
            };

            if url.starts_with("/report.pdf") {
                let _ = req.respond(report_response(cfg, db, &q, param("range").as_deref()));
                return;
            }

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

/// Read the numbers under the lock, typeset with it released.
///
/// Typst is a subprocess and takes long enough to matter; holding the database
/// mutex across it would stall every agent posting a frame, for the sake of a
/// report nobody is waiting on but the one person who clicked. The worker
/// itself is occupied either way, which is what the other three are for.
fn report_response(
    cfg: &ServerConfig,
    db: &Mutex<Db>,
    q: &web::Query,
    range: Option<&str>,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let range = report::Range::parse(range);
    let built = db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock: {e}"))
        .and_then(|db| report::collect(cfg, &db, range, q.day, &q.filter))
        .and_then(|data| Ok((data.filename(), report::render(cfg, &data)?)));

    match built {
        Ok((name, pdf)) => tiny_http::Response::from_data(pdf)
            .with_header::<tiny_http::Header>("Content-Type: application/pdf".parse().unwrap())
            .with_header::<tiny_http::Header>(
                format!("Content-Disposition: attachment; filename=\"{name}\"")
                    .parse()
                    .unwrap(),
            ),
        Err(e) => {
            eprintln!("report: {e:#}");
            tiny_http::Response::from_data(format!("report failed: {e:#}").into_bytes())
                .with_status_code(500)
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
            // A blocked minute records nothing about itself, the site least
            // of all -- that is the whole point of the blocklist.
            domain: None,
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

    // A phone cannot screenshot itself, so an image-less frame is normal
    // rather than broken -- as long as it says what was on screen.
    if frame.image.is_none() && frame.window.trim().is_empty() {
        anyhow::bail!("frame has no image, no window and is not marked blocked");
    }

    let jpeg = match frame.image.as_deref() {
        Some(b64) => {
            use base64::Engine;
            Some(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .context("decoding frame image")?,
            )
        }
        None => None,
    };

    // No image means no perceptual hash, so the free skip below simply
    // never fires for those devices -- correct, since there is no screen
    // to compare.
    let phash = match &jpeg {
        Some(j) => {
            let img = image::load_from_memory(j).context("decoding frame JPEG")?;
            capture::dhash(&img)
        }
        None => 0,
    };

    // Unchanged screen AND no human input means nothing new to classify. Both
    // halves matter: a screen can sit still while someone reads, and it can
    // change constantly with nobody there. Only when neither moved is the
    // minute genuinely uninteresting enough to skip the model.
    let no_input = frame.keys == 0 && frame.mouse == 0;
    if let Some(prev) = &last {
        if prev.ts + 60 == frame.ts
            && no_input
            && jpeg.is_some()
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
                domain: frame.domain.clone(),
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
            {
                let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
                db.insert(&m)?;
                // The absence started when input stopped, not when we
                // noticed. Correct the record backwards.
                if m.category == "idle" && prev.category != "idle" {
                    if let Some(s) = frame.idle_secs {
                        let n = db.backdate_idle(&m.device, m.ts, s)?;
                        if n > 0 {
                            eprintln!("backdated {n} minute(s) to idle on {}", m.device);
                        }
                    }
                }
            }
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
    let label = classify::classify(
        cfg,
        key,
        jpeg.as_deref(),
        &frame.window,
        frame.domain.as_deref(),
        presence,
        prev,
    )?;

    let m = Minute {
        ts: frame.ts,
        device: frame.device,
        category: label.category,
        project: label.project,
        detail: label.detail,
        window: Some(frame.window),
        domain: frame.domain.clone(),
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

/// Where the APK to serve lives. Defaults inside the data volume so the
/// deployment can drop a build next to the database without extra plumbing.
fn apk_path() -> std::path::PathBuf {
    std::env::var("TIME_APK_PATH")
        .unwrap_or_else(|_| "/data/time.apk".into())
        .into()
}

/// The APK's own versionName is only in its binary manifest, and pulling in an
/// AXML parser to read one string is not worth it. The build that publishes the
/// APK knows the version already, so it says so here.
fn apk_version() -> String {
    std::env::var("TIME_APK_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into())
}

/// `/app` is the APK itself; `/app/` is the page you point a phone at.
fn app_response(path: &str) -> tiny_http::ResponseBox {
    let apk = apk_path();

    if path == "/app" {
        let file = match std::fs::File::open(&apk) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("apk: {}: {e}", apk.display());
                return tiny_http::Response::from_string("no APK published\n")
                    .with_status_code(404)
                    .boxed();
            }
        };
        // tiny_http switches to chunked above 32 KB, which drops the
        // Content-Length and leaves a 25 MB download with no progress bar on a
        // phone. The length is known here, so say it.
        let len = file.metadata().ok().map(|m| m.len() as usize);
        return tiny_http::Response::new(
            tiny_http::StatusCode(200),
            Vec::new(),
            file,
            len,
            None,
        )
            .with_chunked_threshold(usize::MAX)
            .with_header::<tiny_http::Header>(
                "Content-Type: application/vnd.android.package-archive"
                    .parse()
                    .unwrap(),
            )
            // Without a filename Chrome on Android saves it as "app", and the
            // package installer refuses anything not ending in .apk.
            .with_header::<tiny_http::Header>(
                format!(
                    "Content-Disposition: attachment; filename=\"time-{}.apk\"",
                    apk_version()
                )
                .parse()
                .unwrap(),
            )
            .boxed();
    }

    let built = std::fs::metadata(&apk).ok().map(|m| {
        let secs = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        (m.len(), secs)
    });

    let body = match built {
        Some((bytes, secs)) => {
            use chrono::TimeZone;
            let when = chrono::Local
                .timestamp_opt(secs, 0)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            format!(
                "<p class=\"v\">v{}</p>\
                 <p class=\"meta\">{:.1} MB · built {}</p>\
                 <p><a class=\"dl\" href=\"/app\">Download APK</a></p>\
                 <p class=\"meta\">Android blocks installs from unknown sources until you \
                  allow it for your browser. After installing, open the app and grant \
                  usage access — nothing is reported without it.</p>",
                web::esc(&apk_version()),
                bytes as f64 / 1_048_576.0,
                web::esc(&when),
            )
        }
        None => "<p class=\"meta\">No APK has been published on this server yet.</p>".to_string(),
    };

    tiny_http::Response::from_string(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>time — Android app</title><style>\
:root {{ color-scheme: light dark; }}\
body {{ margin:0; padding:40px 24px; font:16px/1.55 ui-sans-serif,system-ui,sans-serif; \
  max-width:520px; margin:0 auto; }}\
h1 {{ font-size:22px; margin:0 0 4px; }}\
.v {{ margin:0 0 24px; opacity:.6; font-variant-numeric:tabular-nums; }}\
.meta {{ font-size:13px; opacity:.6; }}\
.dl {{ display:block; text-align:center; padding:14px; border:1px solid currentColor; \
  border-radius:10px; text-decoration:none; color:inherit; font-weight:600; margin:24px 0; }}\
</style></head><body><h1>time</h1>{body}<p><a href=\"/\">← dashboard</a></p></body></html>"
    ))
    .with_header::<tiny_http::Header>("Content-Type: text/html; charset=utf-8".parse().unwrap())
    .boxed()
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
