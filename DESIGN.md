# `time`

Track what I do each minute. Get a pie chart at the end of the day.

That's the whole thing. Everything below that isn't V1 is in §6, parked.

---

## 1. V1

One Rust binary on `pc`, running as a systemd user service. Every minute:

1. Screenshot the focused monitor (`grim`)
2. Downscale to ~1024px wide, WebP
3. If it's basically identical to last minute's → record `idle`, skip the API call
4. Otherwise send to a cheap multimodal model: the image, the active window
   class/title, and **last minute's label**
5. Get back `{category, project, detail}`
6. Insert a row in SQLite. **Delete the image.**

Then one local web page: today's pie chart, plus a scrollable list of the
minutes so I can drill into what a slice actually was.

No server, no Postgres, no k3s, no Kubernetes manifests. A SQLite file in
`~/.local/share/time/` and a binary. Move it to the cluster later, once it's
proven useful — and it might never need to.

---

## 2. Categories

The value is entirely in this list being short and being *mine*. Starting set:

```
idle
work_neptune
work_husk
work_personal
kth
youtube
netflix
twitter
browsing
comms
other
```

Rules for the list:
- **`other` must exist**, or the model will force bad fits into real categories.
- Keep it under ~12. A pie chart with 30 slices tells you nothing.
- It's a config file, not a constant. Editing it is expected.

Alongside the category, the model returns two free-text fields for drill-down:
- `project` — repo, course, or topic name, or null
- `detail` — one concrete line: "debugging the capture loop in time/src/main.rs"

The category is what the pie chart uses. `project` and `detail` are what make
clicking a slice worth doing.

---

## 3. The model call

- **Model:** `qwen3.6-plus`. The original pick, `mimo-v2-omni`, is still listed
  by the models endpoint but 404s as deprecated on every call — so the plan's
  models had to be probed with a test image to find what actually accepts
  vision. Only `qwen3.6-plus` and `minimax-m3` do; the latter is a reasoning
  model whose think blocks break JSON parsing. Superseded note below:
- ~~**Model:** `opencode-go/mimo-v2-omni`~~ — was the confirmed multimodal model on
  the Go plan, $0.40/$2.00 per M, 262K context.
- **Endpoint:** `https://opencode.ai/zen/go/v1/chat/completions`,
  OpenAI-compatible, Bearer auth.
- **Text model:** `deepseek-v4-flash`, $0.14/$0.28 per M against qwen's
  $0.50/$3.00. It is text-only, so it serves the batches that carry no
  screenshot — every phone batch, and anything the sweep picks up after the
  image is gone. `model_for` routes on the payload, so a batch containing even
  one image goes to the vision model; a picture of my screen cannot reach a
  text-only endpoint by accident.
- **The training rule, corrected.** The model documented as training on
  submitted data is `deepseek-v4-flash-free`, a separate ID that the Go plan
  does not offer at all. The paid `deepseek-v4-flash` is zero-retention like the
  rest of the plan. The old rule here conflated the two and blocked a model that
  was never the risk. The rule that still holds: **never use a `-free` model
  ID** — that suffix, not the vendor, is what marks the training exception.

Prompt is roughly: here are the categories, here's the active window, here's what
you said the previous minute was, here's the screenshot — pick one category,
name the project, describe it in one line. Force it through a JSON schema so
the output is always parseable; make `other` explicitly legal so it doesn't
guess.

Passing the previous label is the cheap trick that makes this work: it's one
string, it costs nothing, and it stops the labels flickering between categories
during one continuous activity.

### Cost

~600 minutes/day, minus whatever the idle skip catches (probably a lot).
Each call is roughly 1400 in / 100 out.

| | $/day | $/month | of the $60 Go cap |
|---|---|---|---|
| Every minute, no skip | ~$0.46 | ~$14 | 23% |
| With idle skip catching ~half | ~$0.23 | ~$7 | 12% |

Fine, but it's real money against the coding allowance, which is why the idle
skip is in V1 and not parked. If it turns out worse than this, the next lever is
batching 5 minutes per call — 5× cheaper — at the cost of some complexity.

**Update: it did turn out worse, and batching shipped** — 20 minutes per call,
not 5. The weekly cap was hit outright, which stopped the user's own coding, so
this stopped being a cost question and became an availability one.

Two things the estimate above got wrong. There is **no Batch API** on the Go
plan — `/v1/batches` and `/v1/files` 404, only `/v1/chat/completions` and
`/v1/responses` exist — so the obvious 50%-off route is not available. And
batching is nowhere near "N× cheaper": only the system prompt amortises. The
screenshot is per-minute and is ~85% of what a batched desktop minute costs, so
20 per call is ~43% off with a screenshot and ~70% off without. The phone,
which sends no image at all, was paying ~925 tokens of system prompt to say
sixty tokens' worth of "Instagram was in the foreground"; that is where the
saving actually landed.

**Reasoning is not the place to save money — measured, not assumed.**
`deepseek-v4-flash` emits ~945 output tokens per minute against qwen's ~204,
almost all of it reasoning, and output is the larger half of the bill. The
endpoint honours `reasoning_effort: "none"` (and `thinking: {type: "disabled"}`),
which halves output tokens with the JSON still complete — so the saving is real
and available.

It was still rejected. On ten varied minutes, eight labels were identical with
and without. The two that differed were both `cargo build` running with 65s and
125s of idle time and zero input: with reasoning, `idle`; without,
`work_personal`. That is precisely the "what was THE PERSON doing" distinction
§3 calls the whole job, and getting it wrong inflates the working day with time
spent away from the desk. The reasoning tokens buy presence judgement, so they
stay.

(One trap for whoever re-tests this: an example schema written as `{"ts": 0}`
makes the model return positional indices 0..n instead of carrying the real
timestamps back. Keep the example a realistic-looking unix timestamp. The first
run of this experiment "found" a correctness bug that was entirely the probe's.)

The unplanned benefit is quality: the model now sees a run rather than twenty
unrelated frames, which is the context it needed to tell "reading" from "left
the room" — the distinction §3 says is the whole job.

---

## 4. Data

One SQLite table. Roughly:

```sql
CREATE TABLE minute (
  ts        INTEGER PRIMARY KEY,   -- unix minute
  category  TEXT NOT NULL,
  project   TEXT,
  detail    TEXT,
  window    TEXT,                  -- active window class/title
  phash     INTEGER,               -- for the idle check
  model     TEXT                   -- which model labeled it
);
```

~220k rows/year, a few tens of MB. Labels kept forever, screenshots never
written to disk longer than the moment between capture and API call.

---

## 5. Two things not to skip

**Blocklist.** If the active window is a password manager, banking, or Signal,
don't capture at all — record the minute as `other` with no image. This is
sending screenshots to a third party; the deny list is the only thing standing
between that and a genuine problem. Cheap to add now, annoying to retrofit.

**Kill switch.** A command that pauses capture for N minutes. If pausing is
inconvenient I'll end up disabling the whole thing permanently instead.

---

## 6. Parked

Not V1. Roughly in the order they'd become worth it:

- **Framework laptop** — second client, needs sync and an offline buffer.
- **A collector server** — Postgres plus an ingest API on a home server, reachable
  on the LAN only, so multiple machines land in one place. Only worth it once
  there's a second machine.
- **A local vision model** — pointing the classifier at a self-hosted VLM
  (llama.cpp / llama-swap with a Qwen-VL model on a machine that has a GPU)
  makes labeling free, private, and off any usage allowance entirely. The
  strongest argument for doing this eventually.
- **Typing/mouse metrics** — WPM, click counts. Requires reading
  `/dev/input/event*` via evdev (Wayland gives no global input API). If done:
  count keystrokes into buckets, never record which key.
- **Speech** — PipeWire exposes `application.name` on the capture stream node,
  so you can tell the mic is open *and which app has it* with no per-app
  integration. Silero VAD for speech-vs-silence. Never retain audio.
- **Better idle/presence detection** — `ext-idle-notify-v1`, plus a dormancy
  state machine that stops capturing entirely after long inactivity. The pHash
  skip in V1 covers most of the value for a fraction of the work.
- **Git harvest** — nightly `git log --numstat` across `~/projects` for a
  lines-of-code number on the daily summary.
- **Daily PDF report** — a nightly job that renders the day's pie chart,
  timeline, and top projects to a PDF so it can be sent somewhere or just kept.
  Easiest path on NixOS is **Typst**: a single static binary, takes a template
  plus JSON data, emits a PDF, no headless browser to babysit. Charts either as
  SVG generated from the query or drawn natively in Typst. A cron/timer runs it
  at ~04:00 over yesterday's rows and drops it in a folder — mailing or pushing
  it via Hermes is then trivial. Genuinely small once the data exists, and it's
  the artifact you actually want out of this, so probably the first parked item
  to build.
- **Daily summary** — LLM narrative over the day's labels, to go on the PDF and
  get pushed via Hermes.
- **Correction UI** — click a wrong label, fix it, feed corrections back as
  few-shot examples. This is the thing that takes accuracy from okay to
  trustworthy; worth building as soon as V1 proves the concept.

### Android — researched, blocked

Worth recording so it doesn't get re-investigated: **a silent minute-by-minute
screenshotter is not possible on stock modern Android.** On 15+, MediaProjection
requires explicit user consent *every session*, the token can't be cached across
restarts, and the service can't start from `BOOT_COMPLETED`. Root or Shizuku is
the only way around it.

What a phone app *can* do cleanly: `UsageStatsManager` (foreground app +
duration), `NotificationListenerService`, screen on/off and unlock counts,
batched upload via WorkManager. That's enough for "how much of my day went to
the phone and to what" — just without screenshots.

---

## 7. First step

Get the capture loop working end to end on `pc`: screenshot → model → SQLite
row → deleted image. Run it for a day and look at whether the labels are any
good. Everything else depends on that answer.
