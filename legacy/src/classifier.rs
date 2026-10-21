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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use chrono::TimeZone;

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
///
/// It also bounds what `classify_at` can recover: windows have to be closer
/// together than this, and a drain that keeps hitting `MAX_DRAIN` will
/// eventually let its oldest minutes age out of reach. Those are then
/// `time reclassify --pending` work, not the sweep's.
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

/// How often the drain thread looks at the clock while the classifier is stood
/// down between windows. Coarse on purpose: nothing waits on it, and a window
/// opening up to this late is invisible next to the hours between them.
const CLOSED_TICK: Duration = Duration::from_secs(30);

/// How often a running drain re-reads the pending set. Short because this is
/// one indexed SELECT, not a model call, and the workers should be handed the
/// next slice the moment they finish the last -- `SWEEP_EVERY` would leave the
/// GPU idle for ten minutes between every 256 minutes of backlog.
const DRAIN_TICK: Duration = Duration::from_secs(10);

/// How long the first minute of a batch waits for company during a drain.
///
/// `batch_wait_secs` is ten minutes because a live batch has to wait for
/// devices reporting one minute at a time. A drain already holds the whole
/// backlog, so its batches fill on size or token budget and this only ever
/// applies to the last, partial one -- where the full linger would hold the
/// window, and the loaded model, open for ten idle minutes at the end of
/// every drain.
const DRAIN_LINGER: Duration = Duration::from_secs(15);

/// The longest one drain may run before it is cut off.
///
/// A day is ~2000 minutes across devices, ~35 batches at `batch_minutes` = 60,
/// and two workers against a two-slot llama-server get through that well
/// inside an hour. A drain still going after this is not making progress --
/// a model that has started babbling burns the entire output cap per batch and
/// then fails to parse -- and the whole point of windowing is that the GPU is
/// never the thing keeping the room warm. Cut it off, say so, and let the next
/// window try again: the rows stay pending and lose nothing but time.
const MAX_DRAIN: Duration = Duration::from_secs(90 * 60);

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
    /// Whether the model may be called at all right now. Always true without
    /// `classify_at`; otherwise false between drain windows, and read by
    /// ingest, by the workers, and by nothing else.
    open: AtomicBool,
    /// Minutes ingest declined to enqueue because the window was shut.
    /// Reported when the next one opens rather than logged one at a time: a
    /// night of deferrals is several hundred lines that all say the same thing.
    deferred: AtomicU64,
    /// Newest minute the whole device fleet has reported past. Minutes after it
    /// are held: the timeline there is still missing machines, and a model shown
    /// an incomplete timeline reads the gaps as absence.
    complete_through: AtomicI64,
    /// In-service devices behind the frontier, and how far behind. Kept here so
    /// a worker can name them in the prompt without re-querying per batch.
    stale: Mutex<Vec<(String, i64)>>,
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
            open: AtomicBool::new(true),
            deferred: AtomicU64::new(0),
            // i64::MAX until the drain thread computes the real one, which it
            // does before any minute can be swept. Starting at 0 would defer
            // every minute for that first instant.
            complete_through: AtomicI64::new(i64::MAX),
            stale: Mutex::new(Vec::new()),
        }
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// Enqueue a minute, evicting the oldest if the queue is full.
    ///
    /// Bounded and lossy on purpose. An unbounded queue turns a model outage
    /// into an out-of-memory kill, and blocking the caller would put the
    /// original problem straight back into the request path. The evicted
    /// minute is not lost -- its row is already in the database, unlabelled,
    /// where the next sweep will find it.
    pub fn push(&self, job: Job) -> usize {
        // Between drain windows the model is not called at all, so holding the
        // job -- and its JPEG -- would buy nothing and cost a day of frames in
        // memory. Dropping it here is not a loss: the row is already written
        // and flagged pending, its screenshot is under `frames/`, and the next
        // drain sweeps it back up from disk. That is the same recovery path a
        // restart and a full-queue eviction already use.
        if !self.is_open() {
            self.deferred.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        // Too new to judge: some device has not reported this far yet, so the
        // timeline around this minute still has a machine missing from it.
        // Deferred exactly like a shut window -- the row is pending, and the
        // sweep takes it once the frontier passes it.
        if job.ts > self.complete_through.load(Ordering::Relaxed) {
            self.deferred.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
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

/// How far the timeline is complete, and who is missing from it.
///
/// A device that has reported past minute T has, by reporting, said what it
/// was doing at T -- including nothing at all, which is then a fact about the
/// minute. A device whose newest minute is older than T has said nothing about
/// T, and that silence is not evidence of anything. The classifier used to
/// treat the two identically, which is how a phone that had simply not synced
/// yet became a model reading an empty room.
struct Frontier {
    /// Newest minute every in-service device has reported past. Nothing after
    /// this is offered to the model.
    through: i64,
    /// In-service devices still behind `through` -- only ever non-empty when
    /// `device_wait_hours` ran out and the frontier moved without them. The
    /// model is told their silence means nothing, because for these minutes it
    /// genuinely does not.
    stale: Vec<(String, i64)>,
}

/// Where the timeline stops being trustworthy.
fn frontier(cfg: &ServerConfig, marks: &[crate::db::Watermark], now: i64) -> Frontier {
    let no_wait = Frontier {
        through: now,
        stale: Vec::new(),
    };
    if cfg.device_wait_hours == 0 {
        return no_wait;
    }
    let in_service = now - (cfg.device_active_hours.saturating_mul(3600)) as i64;
    let active: Vec<&crate::db::Watermark> =
        marks.iter().filter(|w| w.through >= in_service).collect();
    // Nothing in service: a fresh database, or every device retired. Waiting
    // for a set with no members would stall forever on nobody.
    if active.is_empty() {
        return no_wait;
    }
    let slowest = active.iter().map(|w| w.through).min().unwrap_or(now);
    // The ceiling is what stops a dead phone from freezing the whole timeline.
    // Past it the minutes go out regardless, labelled as incomplete rather
    // than silently pretending the device said something.
    let ceiling = now - (cfg.device_wait_hours.saturating_mul(3600)) as i64;
    let through = slowest.max(ceiling);
    Frontier {
        through,
        stale: active
            .iter()
            .filter(|w| w.through < through)
            .map(|w| (w.device.clone(), w.through))
            .collect(),
    }
}

/// When the classifier is allowed to call the model.
enum Schedule {
    /// No `classify_at`: continuous, the way this has always worked. Right for
    /// a hosted endpoint, where a call costs someone else's watt-hours.
    Always,
    /// Local times of day at which a drain starts. Between them no model call
    /// is made at all, and minutes queue up in the database instead.
    At(Vec<(u32, u32)>),
}

impl Schedule {
    fn new(cfg: &ServerConfig) -> Self {
        match cfg.classify_windows() {
            Ok(w) if !w.is_empty() => Self::At(w),
            Ok(_) => Self::Always,
            // Already validated at startup, so this is close to unreachable.
            // Continuous is the safe reading of "I cannot tell when you wanted
            // this to run": labels keep appearing, and the cost is the noise
            // the setting was meant to remove -- which is at least loud.
            Err(e) => {
                eprintln!("classifier: {e:#} -- classifying continuously");
                Self::Always
            }
        }
    }

    /// The most recent occurrence of any configured time at or before `now`,
    /// as a unix timestamp: the identity of the drain that should have run.
    /// The drain loop opens a window whenever this changes.
    ///
    /// None for `Always`, which has no windows to identify.
    fn slot_at(&self, now: chrono::DateTime<chrono::Local>) -> Option<i64> {
        let Self::At(times) = self else { return None };
        let today = now.date_naive();
        let yesterday = today.pred_opt()?;
        times
            .iter()
            .filter_map(|&(h, m)| {
                let at = |d: chrono::NaiveDate| {
                    let naive = d.and_hms_opt(h, m, 0)?;
                    // Twice a year a wall-clock time is ambiguous or does not
                    // exist at all. Either end of the fold will do -- picking
                    // one keeps the window, where `single()` would silently
                    // drop it for a day -- and the worst case is one drain an
                    // hour early or late.
                    let local = chrono::Local.from_local_datetime(&naive);
                    local.earliest().or_else(|| local.latest())
                };
                let t = at(today)?;
                if t <= now {
                    Some(t.timestamp())
                } else {
                    at(yesterday).map(|t| t.timestamp())
                }
            })
            .max()
    }
}

/// Start the pool and hand back the queue ingest should push to.
///
/// `key` is None for endpoints without auth -- the local llama-swap setup --
/// and every call then goes out without an Authorization header.
pub fn start(cfg: Arc<ServerConfig>, db: Arc<Mutex<Db>>, key: Option<String>) -> Arc<Queue> {
    let queue = Arc::new(Queue::new());

    // `classify = false`: hand back a queue that is shut forever and start no
    // threads at all. Ingest keeps working exactly as it does between drain
    // windows -- the row is written, the screenshot is kept, the minute is
    // counted as deferred -- so nothing is lost but the label, and the backlog
    // is `time reclassify --pending` work whenever this comes back on.
    if !cfg.classify {
        queue.open.store(false, Ordering::Relaxed);
        eprintln!(
            "classifier: classify = false -- no model calls will be made; \
             minutes are stored unlabelled for later reclassification"
        );
        return queue;
    }

    let schedule = Schedule::new(&cfg);

    // Windowed mode starts shut and waits for its first window. Continuous
    // mode is open from boot and never closes, which is what makes every gate
    // on this flag a no-op there.
    let linger = match &schedule {
        Schedule::Always => Duration::from_secs(cfg.batch_wait_secs.max(1)),
        Schedule::At(times) => {
            queue.open.store(false, Ordering::Relaxed);
            let at: Vec<String> = times
                .iter()
                .map(|(h, m)| format!("{h:02}:{m:02}"))
                .collect();
            eprintln!("classifier: draining at {} local", at.join(", "));
            DRAIN_LINGER
        }
    };

    for _ in 0..WORKERS {
        let (cfg, db, key, queue) = (cfg.clone(), db.clone(), key.clone(), queue.clone());
        std::thread::spawn(move || loop {
            let batch = queue.take_batch();
            // After taking, not before: a worker that blocks on an empty queue
            // would otherwise wake straight into a call it had already been
            // told not to make.
            queue.wait_out_pause();
            // A window can shut while batches are already queued behind it --
            // the drain cap trips, or the collector's last bucket lingers out
            // after the drain was declared finished. Dropping the batch is
            // what makes the cap a real stop rather than an intention: `run`
            // would otherwise spend a full model call per batch still in the
            // pipe. The rows are pending and lose nothing but time.
            if !queue.is_open() {
                queue.release(batch.len());
                continue;
            }
            run(&cfg, &db, key.as_deref(), &queue, batch);
        });
    }

    {
        let (cfg, queue) = (cfg.clone(), queue.clone());
        std::thread::spawn(move || collect(&cfg, &queue, linger));
    }

    // On its own thread so a slow or large database cannot delay the listener.
    // Being reachable is the whole point of this change.
    {
        let (db, queue) = (db.clone(), queue.clone());
        std::thread::spawn(move || drain(&cfg, &db, &queue, &schedule));
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
/// saved. `linger` is `batch_wait_secs` when minutes trickle in live, and the
/// much shorter `DRAIN_LINGER` in windowed mode, where the backlog is already
/// in hand and waiting for company buys nothing.
///
/// The cost is that a label appears up to `linger` after the
/// minute it describes -- acceptable because ingest already writes the row
/// with the previous label carried forward, so the chart is approximately
/// right in the meantime and exactly right once the batch lands.
///
/// The bucket is global, deliberately. The model reasons about the person,
/// not a machine, and only a batch that shows every device in one timeline
/// lets it tell "typing here while a video plays there" from two independent
/// activities. Per-device continuity survives interleaving because every item
/// names its device and each device's previous label rides along.
fn collect(cfg: &ServerConfig, queue: &Queue, linger: Duration) {
    let size = cfg.batch_minutes.max(1);
    let budget = cfg.batch_token_budget;
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
/// `Ok(None)` means it did not look, because work is still in flight;
/// `Ok(Some(0))` means it looked and there is nothing owed. A drain uses that
/// difference to know it has finished, so the two cannot collapse into one
/// zero the way they did when this only ever ran on a timer.
fn sweep(db: &Mutex<Db>, queue: &Queue) -> Result<Option<usize>, anyhow::Error> {
    // A pending row is pending until it is labelled, in-flight ones included,
    // so sweeping over live work would queue every minute twice.
    if !queue.idle() {
        return Ok(None);
    }

    let since = chrono::Utc::now().timestamp() - SWEEP_SECS;
    let rows = {
        let db = db.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        db.pending_since(
            since,
            queue.complete_through.load(Ordering::Relaxed),
            CAPACITY,
            MAX_ATTEMPTS,
        )?
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
    Ok(Some(n))
}

/// Recompute where the timeline is complete and publish it for ingest and the
/// sweep to read. Cheap enough for every tick: one grouped read, on a table
/// the classifier is otherwise sitting idle on.
fn refresh_frontier(cfg: &ServerConfig, db: &Mutex<Db>, queue: &Queue) {
    let marks = db
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock: {e}"))
        .and_then(|db| db.watermarks());
    let marks = match marks {
        Ok(m) => m,
        Err(e) => {
            // Leave the previous frontier standing rather than guessing. A
            // stale frontier holds minutes back, which is the safe direction.
            eprintln!("classifier: watermarks: {e:#}");
            return;
        }
    };
    let f = frontier(cfg, &marks, chrono::Utc::now().timestamp());
    let moved = queue.complete_through.swap(f.through, Ordering::Relaxed) != f.through;
    // Only when it moves, or a device that stays behind prints this every tick.
    if moved && !f.stale.is_empty() {
        let now = chrono::Utc::now().timestamp();
        let who: Vec<String> = f
            .stale
            .iter()
            .map(|(d, t)| {
                // Two different faults wearing the same face, and this is what
                // tells them apart. A device that is hours behind but was heard
                // from minutes ago is syncing and merely retrospective -- the
                // phone, normally. One whose last row also arrived hours ago is
                // off, flat, or broken. Only the second is worth chasing.
                let heard = marks
                    .iter()
                    .find(|w| w.device == *d)
                    .and_then(|w| w.arrived)
                    .map(|a| format!("last heard from {}h ago", (now - a) / 3600))
                    .unwrap_or_else(|| "never heard from since this column existed".into());
                format!("{d} ({}h behind, {heard})", (f.through - t) / 3600)
            })
            .collect();
        eprintln!(
            "classifier: device_wait_hours ran out -- labelling without {}; \
             their minutes will be marked unknown, not absent",
            who.join(", ")
        );
    }
    *queue.stale.lock().unwrap_or_else(|e| e.into_inner()) = f.stale;
}

/// Decide when the model may be called, and keep the pending set moving while
/// it may.
///
/// Both jobs live in one loop because they are the same question asked at two
/// rates: is anything owed a label, and am I allowed to pay for it right now.
fn drain(cfg: &ServerConfig, db: &Mutex<Db>, queue: &Queue, schedule: &Schedule) {
    // Continuous mode is the old sweep thread verbatim: the window opened at
    // boot and never shuts, so this is only ever the timer.
    let Schedule::At(_) = schedule else {
        loop {
            refresh_frontier(cfg, db, queue);
            match sweep(db, queue) {
                Ok(None) | Ok(Some(0)) => {}
                Ok(Some(n)) => eprintln!("classifier: requeued {n} unlabelled minute(s)"),
                Err(e) => eprintln!("classifier: sweep: {e:#}"),
            }
            std::thread::sleep(SWEEP_EVERY);
        }
    };

    // Seeded with the window that has already passed, so booting does not fire
    // one. A crash-looping pod would otherwise drain on every restart, which
    // is the exact opposite of what this setting is for; the minutes it skips
    // are pending, and the next window takes them along with everything else.
    let mut last_slot = schedule.slot_at(chrono::Local::now()).unwrap_or(0);
    let mut started: Option<Instant> = None;

    loop {
        refresh_frontier(cfg, db, queue);
        let Some(since) = started else {
            let slot = schedule.slot_at(chrono::Local::now()).unwrap_or(0);
            if slot != last_slot {
                last_slot = slot;
                started = Some(Instant::now());
                let waiting = queue.deferred.swap(0, Ordering::Relaxed);
                eprintln!(
                    "classifier: drain window open, {waiting} minute(s) deferred since the last"
                );
                // Last, so no worker can take a batch before the log line that
                // explains why the GPU is about to spin up.
                queue.open.store(true, Ordering::Relaxed);
                // Straight to the first sweep rather than through the closed
                // tick: nothing has been queued yet, so that sleep would be
                // half a minute of a window that exists to be short.
                continue;
            }
            std::thread::sleep(CLOSED_TICK);
            continue;
        };

        match sweep(db, queue) {
            // Nothing in flight and nothing owed: the backlog is labelled.
            Ok(Some(0)) => {
                queue.open.store(false, Ordering::Relaxed);
                started = None;
                eprintln!(
                    "classifier: drain done in {}s, going quiet until the next window",
                    since.elapsed().as_secs()
                );
                continue;
            }
            Ok(Some(n)) => eprintln!("classifier: drain queued {n} minute(s)"),
            // Still out at the model. Say nothing and look again shortly.
            Ok(None) => {}
            Err(e) => eprintln!("classifier: sweep: {e:#}"),
        }

        if since.elapsed() >= MAX_DRAIN {
            queue.open.store(false, Ordering::Relaxed);
            started = None;
            eprintln!(
                "classifier: drain hit its {}min cap with work still owed -- stopping \
                 anyway, the next window retries. Check scripts/llm-health.sh: a drain \
                 this long usually means the model is failing, not that the day was busy.",
                MAX_DRAIN.as_secs() / 60
            );
            continue;
        }
        std::thread::sleep(DRAIN_TICK);
    }
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
    // Only the devices that are behind THIS batch: a device can be behind the
    // fleet frontier and still have reported past an older batch, and naming
    // it missing there would be a lie the model has to reason around.
    let latest = batch.iter().map(|j| j.ts).max().unwrap_or(span.1);
    let behind = queue
        .stale
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let missing: Vec<(&str, i64)> = behind
        .iter()
        .filter(|(_, through)| *through < latest)
        .map(|(d, through)| (d.as_str(), *through))
        .collect();

    let (labels, usage, raw) = match classify::classify(cfg, key, &items, &prev, &missing) {
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

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .from_local_datetime(
                &chrono::NaiveDate::from_ymd_opt(y, mo, d)
                    .unwrap()
                    .and_hms_opt(h, mi, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap()
    }

    /// The slot identifies which drain is owed, so it must be the most recent
    /// window and it must be stable for the whole gap after it -- the drain
    /// loop opens a window precisely when this value changes, so a slot that
    /// moved on its own would re-fire the drain every tick.
    #[test]
    fn the_slot_is_the_most_recent_window_and_holds_until_the_next() {
        let s = Schedule::At(vec![(7, 0), (13, 0), (19, 0)]);
        let seven = s.slot_at(local(2026, 8, 19, 7, 0)).unwrap();

        assert_eq!(
            seven,
            local(2026, 8, 19, 7, 0).timestamp(),
            "fires on the dot"
        );
        assert_eq!(s.slot_at(local(2026, 8, 19, 12, 59)).unwrap(), seven);
        assert_ne!(s.slot_at(local(2026, 8, 19, 13, 0)).unwrap(), seven);
        assert_eq!(
            s.slot_at(local(2026, 8, 19, 6, 59)).unwrap(),
            local(2026, 8, 18, 19, 0).timestamp(),
            "before the day's first window the owed drain is last night's"
        );
    }

    #[test]
    fn a_schedule_without_times_is_continuous() {
        assert!(Schedule::slot_at(&Schedule::Always, local(2026, 8, 19, 7, 0)).is_none());
    }

    /// The memory argument for gating at ingest: a shut window must not hold
    /// jobs -- and their JPEGs -- for the hours until the next drain.
    #[test]
    fn a_shut_window_defers_instead_of_queueing() {
        let q = Queue::new();
        q.open.store(false, Ordering::Relaxed);

        assert_eq!(q.push(job("pc", 60)), 0);
        assert_eq!(q.push(job("pc", 120)), 0);
        assert_eq!(q.deferred.load(Ordering::Relaxed), 2);
        assert!(
            q.idle(),
            "nothing queued, so a drain would call itself done"
        );

        q.open.store(true, Ordering::Relaxed);
        assert_eq!(
            q.push(job("pc", 180)),
            1,
            "and it takes them again once open"
        );
        assert_eq!(
            q.deferred.swap(0, Ordering::Relaxed),
            2,
            "the count is what the next window reports"
        );
    }

    fn mark(device: &str, through: i64) -> crate::db::Watermark {
        crate::db::Watermark {
            device: device.into(),
            through,
            arrived: None,
        }
    }

    fn waits() -> crate::config::ServerConfig {
        crate::config::ServerConfig {
            device_wait_hours: 24,
            device_active_hours: 72,
            ..Default::default()
        }
    }

    const NOW: i64 = 1_787_000_000;
    const HOUR: i64 = 3600;

    /// The whole point: the slowest in-service device sets where the timeline
    /// stops being trustworthy. The phone reports retrospectively, so the pc
    /// being current says nothing about whether a minute is complete.
    #[test]
    fn the_frontier_waits_for_the_slowest_device() {
        let f = frontier(
            &waits(),
            &[
                mark("pc", NOW),
                mark("framework", NOW - HOUR),
                mark("phone", NOW - 6 * HOUR),
            ],
            NOW,
        );
        assert_eq!(f.through, NOW - 6 * HOUR, "the phone decides, not the pc");
        assert!(
            f.stale.is_empty(),
            "a device that is merely behind is not stale -- it is being waited for"
        );
    }

    /// A real database carries `tagtest` and `phone-test`, last seen weeks ago.
    /// Waiting for those would mean never labelling another minute.
    #[test]
    fn a_retired_device_does_not_hold_the_frontier() {
        let f = frontier(
            &waits(),
            &[
                mark("pc", NOW),
                mark("phone", NOW - 2 * HOUR),
                mark("tagtest", NOW - 454 * HOUR),
                mark("phone-test", NOW - 454 * HOUR),
            ],
            NOW,
        );
        assert_eq!(
            f.through,
            NOW - 2 * HOUR,
            "only devices still in service count"
        );
        assert!(f.stale.is_empty());
    }

    /// A phone that is off for days must not freeze the timeline behind it --
    /// but the minutes it is missing from have to say so, or the model reads
    /// the gap as the person being away.
    #[test]
    fn the_wait_ceiling_releases_a_stuck_device_and_names_it() {
        let f = frontier(
            &waits(),
            &[mark("pc", NOW), mark("phone", NOW - 60 * HOUR)],
            NOW,
        );
        assert_eq!(f.through, NOW - 24 * HOUR, "capped at device_wait_hours");
        assert_eq!(
            f.stale,
            vec![("phone".to_string(), NOW - 60 * HOUR)],
            "and the phone is named, so the prompt can mark it unknown"
        );
    }

    #[test]
    fn zero_wait_restores_the_old_behaviour() {
        let cfg = crate::config::ServerConfig {
            device_wait_hours: 0,
            ..Default::default()
        };
        let f = frontier(&cfg, &[mark("phone", NOW - 60 * HOUR)], NOW);
        assert_eq!(f.through, NOW);
        assert!(f.stale.is_empty());
    }

    /// An empty fleet must not stall on nobody -- a fresh database has no
    /// watermarks at all, and waiting for a set with no members never ends.
    #[test]
    fn an_empty_fleet_does_not_stall() {
        assert_eq!(frontier(&waits(), &[], NOW).through, NOW);
    }

    /// Ingest has to honour the frontier too, or a minute that arrives during
    /// a drain skips the wait the sweep would have imposed on it.
    #[test]
    fn a_minute_past_the_frontier_is_deferred() {
        let q = Queue::new();
        q.complete_through.store(1000, Ordering::Relaxed);

        assert_eq!(q.push(job("pc", 1060)), 0, "newer than the frontier");
        assert_eq!(q.push(job("pc", 1000)), 1, "exactly at it is complete");
        assert_eq!(q.push(job("pc", 940)), 2, "older than it is complete");
        assert_eq!(q.deferred.load(Ordering::Relaxed), 1);
    }
}
