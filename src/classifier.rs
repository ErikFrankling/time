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
// Two, not four: the endpoint is one local GPU. Extra workers do not add
// throughput, they just queue long batches against each other until the
// client timeout kills whoever is last in line.
const WORKERS: usize = 2;

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

/// One minute waiting for a label.
///
/// The JPEG rides along in memory because that is faster than re-reading the
/// copy ingest just wrote under `frames/` -- the on-disk copy is what makes a
/// dropped job recoverable with pixels intact, which the sweep relies on.
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
            batches = self.filled.wait(batches).unwrap_or_else(|e| e.into_inner());
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
///
/// `key` is None for endpoints without auth -- the local llama-swap setup --
/// and every call then goes out without an Authorization header.
pub fn start(cfg: Arc<ServerConfig>, db: Arc<Mutex<Db>>, key: Option<String>) -> Arc<Queue> {
    let queue = Arc::new(Queue::new());

    for _ in 0..WORKERS {
        let (cfg, db, key, queue) = (cfg.clone(), db.clone(), key.clone(), queue.clone());
        std::thread::spawn(move || loop {
            let batch = queue.take_batch();
            // After taking, not before: a worker that blocks on an empty queue
            // would otherwise wake straight into a call it had already been
            // told not to make.
            queue.wait_out_pause();
            run(&cfg, &db, key.as_deref(), &queue, batch);
        });
    }

    {
        let (cfg, queue) = (cfg.clone(), queue.clone());
        std::thread::spawn(move || collect(&cfg, &queue));
    }

    // On its own thread so a slow or large database cannot delay the listener.
    // Being reachable is the whole point of this change.
    {
        let (db, queue) = (db.clone(), queue.clone());
        std::thread::spawn(move || loop {
            match sweep(&db, &queue) {
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

/// Gather single minutes into one cross-device run worth one call.
///
/// Devices report a minute at a time, so waiting is the only way a batch ever
/// forms: without a linger the queue holds exactly one job and nothing is
/// saved. The cost is that a label appears up to `batch_wait_secs` after the
/// minute it describes -- acceptable because ingest already writes the row
/// with the previous label carried forward, so the chart is approximately
/// right in the meantime and exactly right once the batch lands.
///
/// The bucket is global, deliberately. The model reasons about the person,
/// not a machine, and only a batch that shows every device in one timeline
/// lets it tell "typing here while a video plays there" from two independent
/// activities. Per-device continuity survives interleaving because every item
/// names its device and each device's previous label rides along.
fn collect(cfg: &ServerConfig, queue: &Queue) {
    let size = cfg.batch_minutes.max(1);
    let budget = cfg.batch_token_budget;
    let linger = Duration::from_secs(cfg.batch_wait_secs.max(1));
    let mut open = Bucket::default();

    loop {
        // `jobs` is bounded, but the bucket and closed batches are not, so with
        // the workers stood down for an hour this loop would happily move an
        // unbounded number of JPEGs out of the bounded queue and into memory.
        // Stop drawing once as much is held as the queue itself would hold;
        // ingest then evicts as it always did, and those rows stay pending.
        if queue.held.load(Ordering::Relaxed) >= CAPACITY {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        // Short enough that the bucket flushes close to its deadline rather
        // than whenever the next frame happens to arrive.
        if let Some(job) = queue.pop_timeout(Duration::from_secs(5)) {
            open.push(job, Instant::now());
        }

        if open.ready(size, budget, linger, Instant::now()) {
            queue.put_batch(open.close());
        }
    }
}

/// The one open batch: every device's minutes, waiting for company.
#[derive(Default)]
struct Bucket {
    /// When the oldest item still in the bucket arrived.
    since: Option<Instant>,
    jobs: Vec<Job>,
}

impl Bucket {
    fn push(&mut self, job: Job, now: Instant) {
        if self.jobs.is_empty() {
            self.since = Some(now);
        }
        self.jobs.push(job);
    }

    /// A batch closes once it is full, once its estimated input would no
    /// longer fit the server slot with one more minute in it, or once its
    /// oldest item has waited out the linger -- whichever comes first. The
    /// token check is what lets `batch_minutes` be 60: sixty text minutes
    /// are one cheap call, sixty screenshots are not, and only the estimate
    /// can tell them apart.
    fn ready(&self, size: usize, budget: u32, linger: Duration, now: Instant) -> bool {
        !self.jobs.is_empty()
            && (self.jobs.len() >= size
                || self.over_budget(budget)
                || self.since.is_some_and(|s| now.duration_since(s) >= linger))
    }

    /// Whether the bucket has room for one more minute of the expensive kind.
    /// Checked after every push, and against a screenshot minute because the
    /// collector cannot know what arrives next -- so a closed batch is always
    /// within budget, at the price of a text-heavy batch closing at most one
    /// screenshot-minute's worth of tokens early.
    fn over_budget(&self, budget: u32) -> bool {
        classify::estimated_input_tokens(self.jobs.iter().map(|j| j.jpeg.is_some()))
            + classify::TOKENS_PER_IMAGE_MINUTE
            > budget
    }

    /// Take the batch, sorted by (ts, device) so the prompt reads as a single
    /// timeline with simultaneous minutes adjacent -- a phone catching up on a
    /// week can deliver its jobs in any order.
    fn close(&mut self) -> Vec<Job> {
        self.since = None;
        let mut jobs = std::mem::take(&mut self.jobs);
        jobs.sort_by(|a, b| (a.ts, &a.device).cmp(&(b.ts, &b.device)));
        jobs
    }
}

/// Requeue minutes that were stored but never labelled -- the tail of the queue
/// when the process last died, anything a burst evicted, and everything the
/// allowance was closed for. Ingest keeps each screenshot under `frames/`, so
/// a re-queued minute is re-read from disk and classified with the same pixels
/// the first attempt had; rows without a file -- phones, and history from
/// before frames were kept -- fall back to window and presence alone.
fn sweep(db: &Mutex<Db>, queue: &Queue) -> Result<usize, anyhow::Error> {
    // A pending row is pending until it is labelled, in-flight ones included,
    // so sweeping over live work would queue every minute twice.
    if !queue.idle() {
        return Ok(0);
    }

    let since = chrono::Utc::now().timestamp() - SWEEP_SECS;
    let rows = {
        let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        db.pending_since(since, CAPACITY, MAX_ATTEMPTS)?
    };
    let n = rows.len();
    for m in rows {
        // Quietly None when the file is missing -- pruned by hand, or a row
        // from before frames were kept -- because that is exactly the old
        // text-only sweep, not an error.
        let jpeg = m
            .image_path
            .as_deref()
            .and_then(|rel| crate::config::data_dir().ok().map(|d| d.join(rel)))
            .and_then(|p| std::fs::read(p).ok());
        queue.push(Job {
            ts: m.ts,
            device: m.device,
            window: m.window.unwrap_or_default(),
            domain: m.domain,
            jpeg,
            idle_secs: m.idle_secs,
            keys: m.keys,
            mouse: m.mouse,
            note: m.note,
            prev: None,
        });
    }
    Ok(n)
}

fn run(cfg: &ServerConfig, db: &Mutex<Db>, key: Option<&str>, queue: &Queue, batch: Vec<Job>) {
    // However this returns, these minutes are no longer in flight and the
    // sweep is free to reconsider whichever of them are still pending.
    let _done = Release(queue, batch.len());

    let Some(first) = batch.first() else { return };
    // A batch spans devices now; the audit row and the logs name all of them.
    let device = {
        let mut d: Vec<&str> = batch.iter().map(|j| j.device.as_str()).collect();
        d.sort();
        d.dedup();
        d.join(",")
    };
    let span = (first.ts, batch[batch.len() - 1].ts);

    // One previous label per device: from that device's earliest job that
    // carries one, captured at its ingest -- i.e. the most recent labelled
    // minute of that device before this batch.
    let mut prev: Vec<(&str, classify::Previous)> = Vec::new();
    for job in &batch {
        if prev.iter().any(|(d, _)| *d == job.device.as_str()) {
            continue;
        }
        if let Some(p) = job.prev.as_ref() {
            prev.push((
                job.device.as_str(),
                classify::Previous {
                    category: &p.category,
                    project: p.project.as_deref(),
                    detail: p.detail.as_deref(),
                },
            ));
        }
    }
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

    let audit = |call: crate::db::LlmCall| {
        // The audit row is a nice-to-have next to the labels; failing to write
        // it must never take the batch down with it.
        if let Err(e) = db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))
            .and_then(|db| db.record_llm_call(&call))
        {
            eprintln!("classify {}: recording llm_call: {e:#}", call.device);
        }
    };
    let (labels, usage, raw) = match classify::classify(cfg, key, &items, &prev) {
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
                let mut sent: HashMap<&str, Vec<i64>> = HashMap::new();
                for j in &batch {
                    sent.entry(j.device.as_str()).or_default().push(j.ts);
                }
                for (dev, ts) in &sent {
                    if let Err(e) = db
                        .lock()
                        .map_err(|e| anyhow::anyhow!("db lock: {e}"))
                        .and_then(|db| db.bump_attempts(dev, ts))
                    {
                        eprintln!("classify {dev}: recording failed attempts: {e:#}");
                    }
                }
                // The final failure is part of the audit trail too: a backtest
                // needs to know these minutes were bought and lost, not
                // skipped. Which model failed is known even though no usage
                // came back -- the batch decides.
                audit(crate::db::LlmCall {
                    created: chrono::Utc::now().timestamp(),
                    device: device.clone(),
                    ts_from: span.0,
                    ts_to: span.1,
                    n: batch.len() as i64,
                    model: classify::model_for(cfg, &items).to_string(),
                    endpoint: cfg.endpoint.clone(),
                    error: Some(format!("{e:#}")),
                    ..Default::default()
                });
            }
            return;
        }
    };
    audit(crate::db::LlmCall {
        created: chrono::Utc::now().timestamp(),
        device: device.clone(),
        ts_from: span.0,
        ts_to: span.1,
        n: batch.len() as i64,
        model: usage.model.clone(),
        endpoint: cfg.endpoint.clone(),
        prompt_tokens: Some(usage.prompt as i64),
        completion_tokens: Some(usage.completion as i64),
        raw_response: Some(raw),
        error: None,
    });

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

    // Labels are matched by (device, ts), never by position: a model that
    // drops one minute out of twenty would otherwise shift every label after
    // it onto the wrong minute, which is worse than leaving them pending.
    let wanted: std::collections::HashSet<(&str, i64)> =
        batch.iter().map(|j| (j.device.as_str(), j.ts)).collect();
    let mut got = std::collections::HashSet::new();
    let mut stored = 0;
    for ((dev, ts), label) in &labels {
        if !wanted.contains(&(dev.as_str(), *ts)) {
            eprintln!(
                "classify {device}: model returned an unasked-for minute {dev} {ts}, ignoring"
            );
            continue;
        }
        let put = db
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock: {e}"))
            .and_then(|db| db.label(dev, *ts, label, &usage.model));
        match put {
            Ok(_) => {
                stored += 1;
                got.insert((dev.as_str(), *ts));
            }
            Err(e) => eprintln!("storing label for {dev} {ts}: {e:#}"),
        }
    }
    if stored < batch.len() {
        // Left pending on purpose; the sweep will offer them again -- but only
        // a few times, so a minute this model cannot label stops being a
        // standing charge.
        let mut missed: HashMap<&str, Vec<i64>> = HashMap::new();
        for j in &batch {
            if !got.contains(&(j.device.as_str(), j.ts)) {
                missed.entry(j.device.as_str()).or_default().push(j.ts);
            }
        }
        eprintln!(
            "classify {device}: {} of {} minute(s) came back unlabelled",
            batch.len() - stored,
            batch.len()
        );
        for (dev, ts) in &missed {
            if let Err(e) = db
                .lock()
                .map_err(|e| anyhow::anyhow!("db lock: {e}"))
                .and_then(|db| db.bump_attempts(dev, ts))
            {
                eprintln!("classify {dev}: recording failed attempts: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(device: &str, ts: i64) -> Job {
        Job {
            ts,
            device: device.into(),
            window: String::new(),
            domain: None,
            jpeg: None,
            idle_secs: None,
            keys: 0,
            mouse: 0,
            note: None,
            prev: None,
        }
    }

    /// A budget no batch reaches, for tests about the other two triggers.
    const ROOMY: u32 = u32::MAX;

    /// The bucket is global: minutes from any mix of devices count towards
    /// one batch, and closing sorts them into a single (ts, device) timeline
    /// so simultaneous minutes sit next to each other.
    #[test]
    fn a_bucket_closes_on_size_across_devices() {
        let mut b = Bucket::default();
        let now = Instant::now();
        let linger = Duration::from_secs(600);
        b.push(job("pc", 120), now);
        b.push(job("phone", 60), now);
        assert!(!b.ready(3, ROOMY, linger, now));
        b.push(job("laptop", 120), now);
        assert!(b.ready(3, ROOMY, linger, now));

        let order: Vec<(i64, String)> = b.close().into_iter().map(|j| (j.ts, j.device)).collect();
        assert_eq!(
            order,
            [
                (60, "phone".to_string()),
                (120, "laptop".to_string()),
                (120, "pc".to_string()),
            ]
        );
        assert!(b.jobs.is_empty() && b.since.is_none());
    }

    /// The linger runs from the oldest item still waiting, not from the most
    /// recent arrival -- otherwise a steady trickle would postpone the batch
    /// forever -- and an empty bucket never fires at all.
    #[test]
    fn a_bucket_closes_when_the_oldest_item_has_waited_out_the_linger() {
        let mut b = Bucket::default();
        let now = Instant::now();
        let linger = Duration::from_secs(600);
        assert!(
            !b.ready(20, ROOMY, linger, now + linger * 2),
            "empty never fires"
        );

        b.push(job("pc", 60), now);
        b.push(job("pc", 120), now + linger / 2);
        assert!(!b.ready(20, ROOMY, linger, now + linger / 2));
        assert!(b.ready(20, ROOMY, linger, now + linger));

        b.close();
        assert!(
            !b.ready(20, ROOMY, linger, now + linger * 2),
            "closing resets it"
        );
    }

    /// The token guard behind batch_minutes = 60: sixty text minutes are one
    /// batch, but screenshot minutes close the bucket at the budget -- well
    /// before sixty -- and every closed batch's estimate is within it.
    #[test]
    fn a_bucket_closes_early_when_screenshots_fill_the_token_budget() {
        let budget = crate::config::ServerConfig::default().batch_token_budget;
        let now = Instant::now();
        let linger = Duration::from_secs(600);

        let mut b = Bucket::default();
        for i in 0..60 {
            b.push(job("phone", i * 60), now);
            assert!(
                i == 59 || !b.ready(60, budget, linger, now),
                "a text-only bucket must not close early (minute {i})"
            );
        }
        assert!(b.ready(60, budget, linger, now), "still closes on size");

        let mut b = Bucket::default();
        let mut closed_at = None;
        for i in 0..60 {
            let mut j = job("pc", i * 60);
            j.jpeg = Some(vec![0; 8]);
            b.push(j, now);
            if b.ready(60, budget, linger, now) {
                closed_at = Some(b.close().len());
                break;
            }
        }
        let n = closed_at.expect("an image batch must split before 60");
        assert!(n < 60);
        assert!(
            classify::estimated_input_tokens((0..n).map(|_| true)) <= budget,
            "a closed batch fits its slot"
        );
    }
}
