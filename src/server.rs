use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};

use crate::apk;
use crate::capture;
use crate::classifier::{self, Queue};
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

    // Pulls the Android release in the background. Nothing below waits on it.
    let apk = apk::start();

    // Every model call happens over here, on threads that serve no request.
    let queue = classifier::start(cfg.clone(), db.clone(), key.clone());

    let addr = format!("0.0.0.0:{}", cfg.port);
    let server = Arc::new(
        tiny_http::Server::http(&addr).map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?,
    );
    println!("listening on {addr}");

    // A pool, not a loop, and a generous one. Nothing here waits on the model
    // any more, but a worker still spends real time reading a megabyte of
    // base64 off a phone's uplink or typesetting a PDF, and every one of those
    // is a thread that cannot answer a probe. They block on I/O rather than
    // burn CPU, so the count costs stacks and nothing else.
    let workers = 16;
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (server, cfg, db) = (server.clone(), cfg.clone(), db.clone());
        let (apk, queue) = (apk.clone(), queue.clone());
        handles.push(std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                handle(&cfg, &db, &apk, &queue, req);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn handle(
    cfg: &ServerConfig,
    db: &Mutex<Db>,
    apk: &apk::Shared,
    queue: &Queue,
    mut req: tiny_http::Request,
) {
    {
        // First, and touching nothing. A probe exists to report whether this
        // process can still answer, and it can only answer that honestly if
        // it never queues behind the work it is reporting on.
        if req.url().starts_with("/healthz") {
            let _ = req.respond(tiny_http::Response::from_string("ok\n"));
            return;
        }

        // Longest first: `/v1/frames` also starts with `/v1/frame`.
        let is_batch = req.url().starts_with("/v1/frames");
        let is_ingest = is_batch || req.url().starts_with("/v1/frame");

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
        if path == "/app" || path == "/app/" || path == "/app/version" {
            let _ = req.respond(app_response(&path, apk));
            return;
        }

        if is_ingest {

            let mut body = String::new();
            if req.as_reader().read_to_string(&mut body).is_err() {
                let _ = req.respond(tiny_http::Response::from_string("bad body").with_status_code(400));
                return;
            }

            // A batch replies with an array and a single frame with an object,
            // so each client parses exactly what it sent.
            let result = if is_batch {
                serde_json::from_str::<Vec<Frame>>(&body)
                    .context("parsing frame batch")
                    .and_then(|frames| ingest_batch(cfg, db, queue, frames))
                    .and_then(|acks| Ok(serde_json::to_string(&acks)?))
            } else {
                serde_json::from_str::<Frame>(&body)
                    .context("parsing frame")
                    .and_then(|f| ingest(cfg, db, queue, f))
                    .and_then(|ack| Ok(serde_json::to_string(&ack)?))
            };

            let resp = match result {
                Ok(body) => tiny_http::Response::from_string(body)
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

/// A whole sync in one request.
///
/// The phone reports retrospectively and routinely has a backlog of dozens of
/// minutes. Sending those one at a time meant dozens of round trips, each
/// holding a server thread -- the pile-up that took the pod down. Order is
/// preserved because each minute is classified with the previous one as
/// context, and shuffling them would scramble that.
fn ingest_batch(
    cfg: &ServerConfig,
    db: &Mutex<Db>,
    queue: &Queue,
    mut frames: Vec<Frame>,
) -> Result<Vec<FrameAck>> {
    anyhow::ensure!(frames.len() <= 2000, "batch of {} frames is too large", frames.len());
    frames.sort_by_key(|f| f.ts);

    let mut acks = Vec::with_capacity(frames.len());
    for f in frames {
        let ts = f.ts;
        // One bad minute must not cost the other forty-nine. Report it in
        // place and carry on.
        match ingest(cfg, db, queue, f) {
            Ok(ack) => acks.push(ack),
            Err(e) => {
                eprintln!("ingest {ts}: {e:#}");
                acks.push(FrameAck {
                    ts,
                    category: "other".into(),
                    project: None,
                    detail: Some(format!("rejected: {e:#}")),
                    classified: false,
                    pending: false,
                });
            }
        }
    }
    Ok(acks)
}

/// Decide what a frame means and record it -- cheaply, and without ever
/// blocking on the model.
///
/// Everything here is local work measured in milliseconds: a JPEG decode, a
/// perceptual hash, one row written. A minute that genuinely needs a label is
/// stored with the previous minute's as a placeholder and handed to the
/// background queue, and the ack says so rather than pretending to know.
fn ingest(cfg: &ServerConfig, db: &Mutex<Db>, queue: &Queue, frame: Frame) -> Result<FrameAck> {
    // Read what we need and release immediately: the rest of this function
    // decodes an image, and holding SQLite's single writer across that would
    // serialise every agent behind the slowest one.
    let (existing, last) = {
        let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        (db.get(&frame.device, frame.ts)?, db.last(&frame.device)?)
    };

    if let Some(existing) = existing {
        return Ok(FrameAck {
            ts: existing.ts,
            category: existing.category,
            project: existing.project,
            detail: existing.detail,
            classified: existing.classified,
            pending: existing.pending,
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
            pending: false,
            model: None,
            tags: vec!["other".to_string()],
        };
        db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?.insert(&m)?;
        return Ok(ack(&m));
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

    // A screenshotless device -- a phone -- has one signal, and if it has not
    // changed there is nothing a model could add. Half an hour on one app is
    // thirty identical minutes, and paying for thirty judgements of the same
    // fact is how a first sync filled the classifier queue and got its work
    // dropped. Carry the label instead.
    // A phone minute whose app maps to a category needs no model at all.
    if jpeg.is_none() && !frame.blocked {
        if let Some(cat) = crate::classify::from_package(cfg, &frame.window) {
            let m = Minute {
                ts: frame.ts,
                device: frame.device,
                category: cat.clone(),
                project: None,
                detail: Some(format!("{} (by app)", frame.window)),
                window: Some(frame.window),
                domain: None,
                phash: 0,
                keys: frame.keys,
                mouse: frame.mouse,
                idle_secs: frame.idle_secs,
                apps: frame.apps,
                workspaces: frame.workspaces,
                classified: true,
                pending: false,
                model: Some("package-map".into()),
                tags: vec![cat],
            };
            db.lock()
                .map_err(|e| anyhow::anyhow!("db lock: {e}"))?
                .insert(&m)?;
            return Ok(ack(&m));
        }
    }

    if jpeg.is_none() && !frame.blocked {
        if let Some(prev) = &last {
            if prev.ts + 60 == frame.ts
                && prev.window.as_deref() == Some(frame.window.as_str())
                && prev.classified
            {
                let m = Minute {
                    ts: frame.ts,
                    device: frame.device,
                    category: prev.category.clone(),
                    project: prev.project.clone(),
                    detail: prev.detail.clone(),
                    window: Some(frame.window),
                    domain: None,
                    phash: 0,
                    keys: frame.keys,
                    mouse: frame.mouse,
                    idle_secs: frame.idle_secs,
                    apps: frame.apps,
                    workspaces: frame.workspaces,
                    classified: false,
                    pending: false,
                    model: None,
                    tags: prev.tags.clone(),
                };
                db.lock()
                    .map_err(|e| anyhow::anyhow!("db lock: {e}"))?
                    .insert(&m)?;
                return Ok(ack(&m));
            }
        }
    }

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
                pending: false,
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
            return Ok(ack(&m));
        }
    }

    // This minute needs a model, which is the one thing that must not happen
    // while a client waits. Store what we know, carrying the previous label
    // forward so the chart is approximately right in the meantime, and let the
    // background pool replace it with the truth.
    let m = Minute {
        ts: frame.ts,
        device: frame.device.clone(),
        category: last
            .as_ref()
            .map(|l| l.category.clone())
            .unwrap_or_else(|| "other".into()),
        project: last.as_ref().and_then(|l| l.project.clone()),
        detail: Some("queued for classification".into()),
        window: Some(frame.window.clone()),
        domain: frame.domain.clone(),
        phash: phash as i64,
        keys: frame.keys,
        mouse: frame.mouse,
        idle_secs: frame.idle_secs,
        apps: frame.apps.clone(),
        workspaces: frame.workspaces,
        classified: false,
        pending: true,
        tags: last.as_ref().map(|l| l.tags.clone()).unwrap_or_default(),
        model: None,
    };

    // Written before it is queued, never after: the row is what makes the
    // queue disposable. Lose the process and the minute is still on disk,
    // flagged, waiting for the next sweep to find it.
    db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?.insert(&m)?;

    queue.push(classifier::Job {
        ts: frame.ts,
        device: frame.device,
        window: frame.window,
        domain: frame.domain,
        jpeg,
        idle_secs: frame.idle_secs,
        keys: frame.keys,
        mouse: frame.mouse,
        note: frame.note,
        prev: last.map(|l| classifier::Previous {
            category: l.category,
            project: l.project,
            detail: l.detail,
        }),
    });

    Ok(ack(&m))
    // The JPEG lives on in the queue and is never written anywhere.
}

fn ack(m: &Minute) -> FrameAck {
    FrameAck {
        ts: m.ts,
        category: m.category.clone(),
        project: m.project.clone(),
        detail: m.detail.clone(),
        classified: m.classified,
        pending: m.pending,
    }
}

/// `/app` is the APK itself, `/app/version` is what the phone polls, and
/// `/app/` is the page you point a browser at.
fn app_response(path: &str, apk: &apk::Shared) -> tiny_http::ResponseBox {
    let (meta, checked, error) = {
        let s = apk.lock().unwrap_or_else(|e| e.into_inner());
        (s.meta.clone(), s.checked, s.error.clone())
    };

    if path == "/app/version" {
        let body = match &meta {
            Some(m) => serde_json::json!({
                "version": m.version,
                "versionCode": m.version_code,
                "sha256": m.sha256,
                // Deliberately this server's own URL rather than GitHub's: the
                // phone is on the LAN and may not have a route off it.
                "url": "/app",
                "published": m.published,
                "size": m.size,
                "signing": m.signing,
                "stale": !error.is_empty(),
            }),
            None => serde_json::json!({ "error": "no APK published" }),
        };
        return tiny_http::Response::from_string(body.to_string())
            .with_status_code(if meta.is_some() { 200 } else { 404 })
            .with_header::<tiny_http::Header>(
                "Content-Type: application/json".parse().unwrap(),
            )
            .boxed();
    }

    if path == "/app" {
        let file = match std::fs::File::open(apk::path()) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("apk: {}: {e}", apk::path().display());
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
                    meta.as_ref().map(|m| m.version.as_str()).unwrap_or("dev")
                )
                .parse()
                .unwrap(),
            )
            .boxed();
    }

    tiny_http::Response::from_string(app_page(meta.as_ref(), checked, &error))
        .with_header::<tiny_http::Header>("Content-Type: text/html; charset=utf-8".parse().unwrap())
        .boxed()
}

fn app_page(meta: Option<&apk::Meta>, checked: i64, error: &str) -> String {
    let obtainium = format!("https://github.com/{}", apk::repo());

    // Anyone reading this page is about to sideload from a browser, which is
    // exactly the install Android 13+ treats as untrusted: PACKAGE_USAGE_STATS
    // is a restricted permission, so the usage-access toggle comes up greyed
    // and the app looks broken rather than blocked. Say so before the download
    // button, not after.
    let restricted = "<div class=\"warn\"><b>After installing from this page</b>, the \
         usage-access toggle will refuse to turn on until you allow it: \
         <b>Settings → Apps → time → ⋮ → Allow restricted settings</b>. That menu item \
         only appears once Android has denied the permission at least once, so grant \
         usage access first, watch it fail, then go do this.</div>"
        .to_string();

    let recommended = format!(
        "<div class=\"tip\"><b>The easier route:</b> install \
         <a href=\"https://github.com/ImranR98/Obtainium\">Obtainium</a> and add \
         <code>{}</code>. It installs through the same API the app stores use, so none \
         of the restricted-settings dance above applies, and it checks for new versions \
         on its own.</div>",
        web::esc(&obtainium)
    );

    let body = match meta {
        Some(m) => {
            let when = chrono::DateTime::parse_from_rfc3339(&m.published)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|_| m.published.clone());

            // A debug-signed APK is installable but is a dead end: the release
            // key can never upgrade it, so the way out is an uninstall that
            // takes the app's data with it. Better to know before installing.
            let signing = match m.signing.as_str() {
                "release" => String::new(),
                "debug" => "<div class=\"warn\">This build is signed with the \
                     <b>debug</b> key, because the release signing secrets are not set \
                     in CI yet. It installs, but it can never be upgraded to a \
                     release-signed build — that needs an uninstall. Treat it as \
                     temporary.</div>"
                    .to_string(),
                other => format!(
                    "<div class=\"warn\">Signing key unknown ({}). This APK was placed \
                     on the server by hand rather than published by CI.</div>",
                    web::esc(other)
                ),
            };

            let stale = if !error.is_empty() {
                let ago = if checked > 0 {
                    let mins = (chrono::Utc::now().timestamp() - checked) / 60;
                    format!("last successful check {} ago", human_ago(mins))
                } else {
                    "no successful check since this server started".into()
                };
                format!(
                    "<div class=\"warn\">Serving a possibly outdated APK: {} ({}). \
                     Error: {}</div>",
                    ago,
                    web::esc(&format!("repo {}", apk::repo())),
                    web::esc(error)
                )
            } else {
                String::new()
            };

            format!(
                "<p class=\"v\">v{} <span class=\"code\">({})</span></p>\
                 <p class=\"meta\">{:.1} MB · published {} · {}-signed</p>\
                 {stale}{signing}\
                 <p><a class=\"dl\" href=\"/app\">Download APK</a></p>\
                 {restricted}{recommended}\
                 <p class=\"meta\">sha256 {}</p>\
                 <p class=\"meta\">Android blocks installs from unknown sources until you \
                  allow it for your browser. After installing, open the app and grant \
                  usage access — nothing is reported without it.</p>",
                web::esc(&m.version),
                m.version_code,
                m.size as f64 / 1_048_576.0,
                web::esc(&when),
                web::esc(&m.signing),
                web::esc(&m.sha256),
            )
        }
        None => format!(
            "<p class=\"meta\">No APK has been published yet.</p>\
             <div class=\"warn\">The server fetches the newest release from \
             <code>{}</code> on startup and hourly. {}</div>",
            web::esc(&apk::repo()),
            if error.is_empty() {
                "It has not managed a successful check yet.".to_string()
            } else {
                format!("Last error: {}", web::esc(error))
            }
        ),
    };

    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>time — Android app</title><style>\
:root {{ color-scheme: light dark; }}\
body {{ margin:0; padding:40px 24px; font:16px/1.55 ui-sans-serif,system-ui,sans-serif; \
  max-width:520px; margin:0 auto; }}\
h1 {{ font-size:22px; margin:0 0 4px; }}\
.v {{ margin:0 0 4px; font-variant-numeric:tabular-nums; }}\
.code {{ opacity:.5; }}\
.meta {{ font-size:13px; opacity:.6; overflow-wrap:anywhere; }}\
.warn, .tip {{ font-size:14px; padding:12px 14px; border-radius:10px; margin:16px 0; \
  border:1px solid currentColor; }}\
.warn {{ color:#a3560a; }}\
.tip {{ opacity:.85; }}\
code {{ font-size:13px; overflow-wrap:anywhere; }}\
.dl {{ display:block; text-align:center; padding:14px; border:1px solid currentColor; \
  border-radius:10px; text-decoration:none; color:inherit; font-weight:600; margin:24px 0; }}\
</style></head><body><h1>time</h1>{body}<p><a href=\"/\">← dashboard</a></p></body></html>"
    )
}

fn human_ago(mins: i64) -> String {
    match mins {
        m if m < 60 => format!("{m}m"),
        m if m < 60 * 48 => format!("{}h", m / 60),
        m => format!("{}d", m / (60 * 24)),
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
