//! The model call, moved out of the request.
//!
//! Classifying a minute takes tens of seconds and retries once, so doing it
//! while the client waits occupies a request worker for up to five minutes. A
//! handful of those and the server has no thread left to answer a health probe
//! with -- which is exactly how a slow model endpoint turned into a pod marked
//! unready, a route with no backend, and 503 for everybody. Ingest now stores
//! the row and hands the minute over here.
//!
//! Nothing in this queue needs to survive a crash: the `minute` row is written
//! before the job is enqueued, so a restart loses at most the screenshot, and
//! `sweep` picks the rows back up from the database.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use crate::classify;
use crate::config::ServerConfig;
use crate::db::Db;

/// Threads draining the queue. They spend their lives blocked on a socket, so
/// this is about how many model calls to have in flight, not about CPU.
const WORKERS: usize = 4;

/// Jobs held in memory at once. Each carries a downscaled JPEG, so the bound is
/// really a memory bound; a few hundred is well inside the pod's 512 MiB.
const CAPACITY: usize = 256;

/// How far back a startup sweep looks for minutes that never got their label.
/// A day is generous for a process that restarts in seconds, and it stops an
/// old database from queueing thousands of rows the first time this ships.
const SWEEP_SECS: i64 = 24 * 3600;

/// One minute waiting for a label.
///
/// The JPEG rides along in memory because it is deliberately never written
/// down -- not keeping the screenshot is the point of the whole design, and
/// spilling it to disk to make a queue durable would trade that away.
pub struct Job {
    pub ts: i64,
    pub device: String,
    pub window: String,
    pub domain: Option<String>,
    pub jpeg: Option<Vec<u8>>,
    pub idle_secs: Option<u32>,
    pub keys: u32,
    pub mouse: u32,
    pub note: Option<String>,
    /// The preceding minute as it read when this frame arrived. Captured at
    /// ingest rather than looked up in the worker: by the time a job comes up
    /// the newest row for the device may be several minutes further on, and
    /// that one is not this minute's neighbour.
    pub prev: Option<Previous>,
}

#[derive(Clone)]
pub struct Previous {
    pub category: String,
    pub project: Option<String>,
    pub detail: Option<String>,
}

struct Inner {
    jobs: VecDeque<Job>,
    /// Counted rather than logged per event, so a burst does not also become a
    /// log flood.
    dropped: u64,
}

pub struct Queue {
    inner: Mutex<Inner>,
    ready: Condvar,
}

impl Queue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                jobs: VecDeque::new(),
                dropped: 0,
            }),
            ready: Condvar::new(),
        }
    }

    /// Enqueue a minute, evicting the oldest if the queue is full.
    ///
    /// Bounded and lossy on purpose. An unbounded queue turns a model outage
    /// into an out-of-memory kill, and blocking the caller would put the
    /// original problem straight back into the request path. The evicted
    /// minute is not lost -- its row is already in the database, unlabelled,
    /// where the next sweep will find it.
    pub fn push(&self, job: Job) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while inner.jobs.len() >= CAPACITY {
            inner.jobs.pop_front();
            inner.dropped += 1;
            // Worth a line each: an eviction only happens once the model has
            // been unable to keep up for hours, and the log going quiet was
            // half of what made the original outage hard to read.
            eprintln!(
                "classifier: queue full at {CAPACITY}, dropped oldest ({} so far; \
                 they stay flagged in the database)",
                inner.dropped
            );
        }
        inner.jobs.push_back(job);
        let depth = inner.jobs.len();
        drop(inner);
        self.ready.notify_one();
        depth
    }

    fn pop(&self) -> Job {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(job) = inner.jobs.pop_front() {
                return job;
            }
            inner = self
                .ready
                .wait(inner)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Start the pool and hand back the queue ingest should push to.
pub fn start(cfg: Arc<ServerConfig>, db: Arc<Mutex<Db>>, key: String) -> Arc<Queue> {
    let queue = Arc::new(Queue::new());

    for _ in 0..WORKERS {
        let (cfg, db, key, queue) = (cfg.clone(), db.clone(), key.clone(), queue.clone());
        std::thread::spawn(move || loop {
            let job = queue.pop();
            run(&cfg, &db, &key, job);
        });
    }

    // On its own thread so a slow or large database cannot delay the listener.
    // Being reachable is the whole point of this change.
    {
        let (db, queue) = (db.clone(), queue.clone());
        std::thread::spawn(move || match sweep(&db, &queue) {
            Ok(0) => {}
            Ok(n) => eprintln!("classifier: requeued {n} unlabelled minute(s)"),
            Err(e) => eprintln!("classifier: sweep: {e:#}"),
        });
    }

    queue
}

/// Requeue minutes that were stored but never labelled -- the tail of the queue
/// when the process last died, or anything a burst evicted. Their screenshots
/// are gone, so these are classified from the window and presence alone, which
/// is the same information a phone ever sends.
fn sweep(db: &Mutex<Db>, queue: &Queue) -> Result<usize, anyhow::Error> {
    let since = chrono::Utc::now().timestamp() - SWEEP_SECS;
    let rows = {
        let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        db.pending_since(since, CAPACITY)?
    };
    let n = rows.len();
    for m in rows {
        queue.push(Job {
            ts: m.ts,
            device: m.device,
            window: m.window.unwrap_or_default(),
            domain: m.domain,
            jpeg: None,
            idle_secs: m.idle_secs,
            keys: m.keys,
            mouse: m.mouse,
            note: None,
            prev: None,
        });
    }
    Ok(n)
}

fn run(cfg: &ServerConfig, db: &Mutex<Db>, key: &str, job: Job) {
    let prev = job.prev.as_ref().map(|p| classify::Previous {
        category: &p.category,
        project: p.project.as_deref(),
        detail: p.detail.as_deref(),
    });
    let presence = classify::Presence {
        device: &job.device,
        idle_secs: job.idle_secs,
        keys: job.keys,
        mouse: job.mouse,
        note: job.note.as_deref(),
    };

    let label = classify::classify(
        cfg,
        key,
        job.jpeg.as_deref(),
        &job.window,
        job.domain.as_deref(),
        presence,
        prev,
    );

    let label = match label {
        Ok(l) => l,
        Err(e) => {
            // The row keeps its pending flag, so a later sweep gets another
            // go at it. Dropping the minute silently is what leaves holes.
            eprintln!("classify {} {}: {e:#}", job.device, job.ts);
            return;
        }
    };

    let stored = db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock: {e}"))
        .and_then(|db| db.label(&job.device, job.ts, &label, &cfg.model));
    if let Err(e) = stored {
        eprintln!("storing label for {} {}: {e:#}", job.device, job.ts);
    }
}
