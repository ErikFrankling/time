//! The model call, moved out of the request.
//!
//! Classifying a minute takes tens of seconds and retries once, so doing it
//! while the client waits occupies a request worker for up to five minutes. A
//! handful of those and the server has no thread left to answer a health probe
//! with -- which is exactly how a slow model endpoint turned into a pod marked
//! unready, a route with no backend, and 503 for everybody. Ingest now stores
//! the row and hands the minute over here.
//!
//! Minutes are then labelled in runs rather than one at a time. A weekly
//! allowance shared with the user's own coding does not survive one request per
//! minute per device, and the long system prompt was being re-sent every single
//! time to say the same thing.
//!
//! Nothing in this queue needs to survive a crash: the `minute` row is written
//! before the job is enqueued, so a restart loses at most the screenshot, and
//! `sweep` picks the rows back up from the database.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::classify;
use crate::config::ServerConfig;
use crate::db::Db;

/// Threads draining the queue. They spend their lives blocked on a socket, so
/// this is about how many model calls to have in flight, not about CPU.
const WORKERS: usize = 4;

/// Jobs held in memory at once. Each carries a downscaled JPEG, so the bound is
/// really a memory bound; a few hundred is well inside the pod's 512 MiB.
const CAPACITY: usize = 256;

/// How far back a sweep looks for minutes that never got their label. A day is
/// generous for a process that restarts in seconds, and it stops an old
/// database from queueing thousands of rows the first time this ships.
const SWEEP_SECS: i64 = 24 * 3600;

/// How often to look for minutes still owed a label.
///
/// This used to run once, at startup. That was enough when every failure was
/// transient, and useless the moment the allowance ran out: the hour-long
/// pause dropped every job it was holding, their rows stayed flagged, and
/// nothing ever went looking for them again. A live database was found with
/// 1953 minutes pending and no process that would ever pick them up.
const SWEEP_EVERY: Duration = Duration::from_secs(600);

/// How many times a minute may be offered to the model before the sweep leaves
/// it alone. A minute that comes back unlabelled twice is usually one the model
/// will never label, and the sweep is otherwise a loop that pays for the same
/// refusal every ten minutes forever.
const MAX_ATTEMPTS: u32 = 3;

/// How many pending rows the free package-map pass considers per sweep. Larger
/// than `CAPACITY` because none of these cost a call -- the bound is only there
/// to keep one sweep from holding the database lock across the whole history.
const FREE_PASS: usize = 2000;

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
    /// Batches the collector has closed and workers have not yet taken.
    batches: Mutex<VecDeque<Vec<Job>>>,
    filled: Condvar,
    /// Unix seconds until which the endpoint has told us not to bother. Shared
    /// by every worker: a per-worker sleep meant the other three carried on
    /// hammering a closed allowance.
    paused_until: AtomicI64,
    /// Minutes taken off `jobs` and not yet labelled -- sitting in a collector
    /// bucket waiting for company, or out at the model. Their rows are still
    /// flagged pending, so without this count the sweep would offer every one
    /// of them a second time while the first attempt was still in the air.
    held: AtomicUsize,
}

impl Queue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                jobs: VecDeque::new(),
                dropped: 0,
            }),
            ready: Condvar::new(),
            batches: Mutex::new(VecDeque::new()),
            filled: Condvar::new(),
            paused_until: AtomicI64::new(0),
            held: AtomicUsize::new(0),
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

    /// Take the next job, or None if none arrived within `wait`.
    fn pop_timeout(&self, wait: Duration) -> Option<Job> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.jobs.is_empty() {
            let (waited, _) = self
                .ready
                .wait_timeout(inner, wait)
                .unwrap_or_else(|e| e.into_inner());
            inner = waited;
        }
        let job = inner.jobs.pop_front();
        if job.is_some() {
            self.held.fetch_add(1, Ordering::Relaxed);
        }
        job
    }

    fn release(&self, n: usize) {
        self.held.fetch_sub(n, Ordering::Relaxed);
    }

    /// True when there is nothing in flight, which is when a sweep can safely
    /// offer pending rows again without duplicating work already queued.
    fn idle(&self) -> bool {
        self.held.load(Ordering::Relaxed) == 0
            && self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .jobs
                .is_empty()
            && self
                .batches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }

    fn put_batch(&self, batch: Vec<Job>) {
        self.batches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(batch);
        self.filled.notify_one();
    }

    fn take_batch(&self) -> Vec<Job> {
        let mut batches = self.batches.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(b) = batches.pop_front() {
                return b;
            }
            batches = self
                .filled
                .wait(batches)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Block until the allowance is plausibly open again.
    fn wait_out_pause(&self) {
        loop {
            let until = self.paused_until.load(Ordering::Relaxed);
            let now = chrono::Utc::now().timestamp();
            if until <= now {
                return;
            }
            // Capped so a nonsense Retry-After cannot wedge the classifier for
            // a day, and so the log says something at least hourly.
            let nap = (until - now).clamp(1, 600) as u64;
            eprintln!("classifier: allowance closed for another {}s", until - now);
            std::thread::sleep(Duration::from_secs(nap));
        }
    }

    fn pause_for(&self, d: Duration) {
        let until = chrono::Utc::now().timestamp() + d.as_secs() as i64;
        self.paused_until.fetch_max(until, Ordering::Relaxed);
    }
}

/// Start the pool and hand back the queue ingest should push to.
pub fn start(cfg: Arc<ServerConfig>, db: Arc<Mutex<Db>>, key: String) -> Arc<Queue> {
    let queue = Arc::new(Queue::new());

    for _ in 0..WORKERS {
        let (cfg, db, key, queue) = (cfg.clone(), db.clone(), key.clone(), queue.clone());
        std::thread::spawn(move || loop {
            let batch = queue.take_batch();
            // After taking, not before: a worker that blocks on an empty queue
            // would otherwise wake straight into a call it had already been
            // told not to make.
            queue.wait_out_pause();
            run(&cfg, &db, &key, &queue, batch);
        });
    }

    {
        let (cfg, queue) = (cfg.clone(), queue.clone());
        std::thread::spawn(move || collect(&cfg, &queue));
    }

    // On its own thread so a slow or large database cannot delay the listener.
    // Being reachable is the whole point of this change.
    {
        let (cfg, db, queue) = (cfg.clone(), db.clone(), queue.clone());
        std::thread::spawn(move || loop {
            match sweep(&cfg, &db, &queue) {
                Ok(0) => {}
                Ok(n) => eprintln!("classifier: requeued {n} unlabelled minute(s)"),
                Err(e) => eprintln!("classifier: sweep: {e:#}"),
            }
            std::thread::sleep(SWEEP_EVERY);
        });
    }

    queue
}

struct Release<'a>(&'a Queue, usize);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.release(self.1);
    }
}

/// Gather single minutes into runs worth one call.
///
/// Devices report a minute at a time, so waiting is the only way a batch ever
/// forms: without a linger the queue holds exactly one job and nothing is
/// saved. The cost is that a label appears up to `batch_wait_secs` after the
/// minute it describes -- acceptable because ingest already writes the row
/// with the previous label carried forward, so the chart is approximately
/// right in the meantime and exactly right once the batch lands.
///
/// Runs are kept per device. Minutes are labelled with their neighbours as
/// context, and interleaving two machines' screens would destroy exactly the
/// continuity that context is for.
fn collect(cfg: &ServerConfig, queue: &Queue) {
    let size = cfg.batch_minutes.max(1);
    let linger = Duration::from_secs(cfg.batch_wait_secs.max(1));
    let mut open: HashMap<String, (Instant, Vec<Job>)> = HashMap::new();

    loop {
        // `jobs` is bounded, but buckets and closed batches are not, so with
        // the workers stood down for an hour this loop would happily move an
        // unbounded number of JPEGs out of the bounded queue and into memory.
        // Stop drawing once as much is held as the queue itself would hold;
        // ingest then evicts as it always did, and those rows stay pending.
        if queue.held.load(Ordering::Relaxed) >= CAPACITY {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        // Short enough that a bucket flushes close to its deadline rather than
        // whenever the next frame happens to arrive.
        if let Some(job) = queue.pop_timeout(Duration::from_secs(5)) {
            let bucket = open
                .entry(job.device.clone())
                .or_insert_with(|| (Instant::now(), Vec::new()));
            bucket.1.push(job);
        }

        let ready: Vec<String> = open
            .iter()
            .filter(|(_, (since, jobs))| jobs.len() >= size || since.elapsed() >= linger)
            .map(|(device, _)| device.clone())
            .collect();

        for device in ready {
            if let Some((_, mut jobs)) = open.remove(&device) {
                // The model is told these are consecutive; a phone catching up
                // on a week can deliver them in any order.
                jobs.sort_by_key(|j| j.ts);
                queue.put_batch(jobs);
            }
        }
    }
}

/// Requeue minutes that were stored but never labelled -- the tail of the queue
/// when the process last died, anything a burst evicted, and everything the
/// allowance was closed for. Their screenshots are gone, so these are
/// classified from the window and presence alone, which is the same
/// information a phone ever sends.
fn sweep(cfg: &ServerConfig, db: &Mutex<Db>, queue: &Queue) -> Result<usize, anyhow::Error> {
    // A pending row is pending until it is labelled, in-flight ones included,
    // so sweeping over live work would queue every minute twice.
    if !queue.idle() {
        return Ok(0);
    }

    // The free pass first, and over the whole history rather than the model's
    // window: a phone minute whose package names the activity costs nothing to
    // resolve, and the rows that predate this shortcut would otherwise sit
    // pending forever behind a paid call they never needed. On a real database
    // this was two thirds of everything waiting.
    let stale = {
        let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        // Deliberately not filtered by attempts: those count failed *calls*,
        // and a lookup that costs nothing should still rescue a row the model
        // has already given up on.
        db.pending_since(0, FREE_PASS, u32::MAX)?
    };
    let mut freed = 0;
    for m in &stale {
        let Some(window) = m.window.as_deref() else { continue };
        let Some(cat) = crate::classify::from_package(cfg, window) else { continue };
        let label = crate::classify::Label {
            category: cat.clone(),
            project: None,
            detail: Some(format!("{window} (by app)")),
            tags: vec![cat],
        };
        let put = db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))
            .and_then(|db| db.label(&m.device, m.ts, &label, "package-map"));
        match put {
            Ok(_) => freed += 1,
            Err(e) => eprintln!("classifier: package-map {} {}: {e:#}", m.device, m.ts),
        }
    }
    if freed > 0 {
        eprintln!("classifier: labelled {freed} minute(s) from the package map, no call made");
    }

    let since = chrono::Utc::now().timestamp() - SWEEP_SECS;
    let rows = {
        let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        db.pending_since(since, CAPACITY, MAX_ATTEMPTS)?
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

fn run(cfg: &ServerConfig, db: &Mutex<Db>, key: &str, queue: &Queue, batch: Vec<Job>) {
    // However this returns, these minutes are no longer in flight and the
    // sweep is free to reconsider whichever of them are still pending.
    let _done = Release(queue, batch.len());

    let Some(first) = batch.first() else { return };
    let device = first.device.clone();
    let span = (first.ts, batch[batch.len() - 1].ts);

    let prev = first.prev.as_ref().map(|p| classify::Previous {
        category: &p.category,
        project: p.project.as_deref(),
        detail: p.detail.as_deref(),
    });
    let items: Vec<classify::Item<'_>> = batch
        .iter()
        .map(|job| classify::Item {
            ts: job.ts,
            jpeg: job.jpeg.as_deref(),
            window: &job.window,
            domain: job.domain.as_deref(),
            presence: classify::Presence {
                device: &job.device,
                idle_secs: job.idle_secs,
                keys: job.keys,
                mouse: job.mouse,
                note: job.note.as_deref(),
            },
        })
        .collect();

    let (labels, usage) = match classify::classify(cfg, key, &items, prev) {
        Ok(v) => v,
        Err(e) => {
            // A weekly cap is not a transient failure. Hammering it burns
            // queue slots on work that cannot succeed for hours and starves
            // the minutes that could -- so every worker stands down until the
            // endpoint's own Retry-After has elapsed. The rows keep their
            // pending flag and the next sweep collects them.
            if let Some(rl) = e.downcast_ref::<classify::RateLimited>() {
                eprintln!(
                    "classifier: rate limited, standing down for {}s",
                    rl.retry_after.as_secs()
                );
                queue.pause_for(rl.retry_after);
            } else {
                eprintln!("classify {device} {}..{}: {e:#}", span.0, span.1);
                // A rate limit is the allowance's fault and these minutes will
                // succeed later untouched, but any other failure is very often
                // the batch itself -- an answer that will not parse however
                // many times it is bought. Count it against the rows.
                let sent: Vec<i64> = batch.iter().map(|j| j.ts).collect();
                if let Err(e) = db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("db lock: {e}"))
                    .and_then(|db| db.bump_attempts(&device, &sent))
                {
                    eprintln!("classify {device}: recording failed attempts: {e:#}");
                }
            }
            return;
        }
    };

    // Real numbers, because deciding whether the screenshot width or the batch
    // size is worth changing needs the endpoint's own count and not an
    // estimate of how many tokens an image is worth.
    eprintln!(
        "classifier: {device} {}..{} {} minute(s) via {}, {} in / {} out ({} in per minute)",
        span.0,
        span.1,
        batch.len(),
        usage.model,
        usage.prompt,
        usage.completion,
        usage.prompt / batch.len().max(1) as u64,
    );

    // Labels are matched by ts, never by position: a model that drops one
    // minute out of twenty would otherwise shift every label after it onto
    // the wrong minute, which is worse than leaving them pending.
    let wanted: std::collections::HashSet<i64> = batch.iter().map(|j| j.ts).collect();
    let mut got = std::collections::HashSet::new();
    let mut stored = 0;
    for (ts, label) in &labels {
        if !wanted.contains(ts) {
            eprintln!("classify {device}: model returned an unasked-for minute {ts}, ignoring");
            continue;
        }
        let put = db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))
            .and_then(|db| db.label(&device, *ts, label, &usage.model));
        match put {
            Ok(_) => {
                stored += 1;
                got.insert(*ts);
            }
            Err(e) => eprintln!("storing label for {device} {ts}: {e:#}"),
        }
    }
    if stored < batch.len() {
        // Left pending on purpose; the sweep will offer them again -- but only
        // a few times, so a minute this model cannot label stops being a
        // standing charge.
        let missed: Vec<i64> = batch.iter().map(|j| j.ts).filter(|ts| !got.contains(ts)).collect();
        eprintln!(
            "classify {device}: {} of {} minute(s) came back unlabelled",
            missed.len(),
            batch.len()
        );
        if let Err(e) = db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))
            .and_then(|db| db.bump_attempts(&device, &missed))
        {
            eprintln!("classify {device}: recording failed attempts: {e:#}");
        }
    }
}
