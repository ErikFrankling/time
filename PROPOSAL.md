# Time v2: design proposal (draft for approval)

Status: revised plan, 2026-10-21. M0 (harness) and M1 (rules classifier) are being built now on the `v2` branch (they do not depend on Erik's answers); nothing goes live before M4.

## What exists, and what is wrong with it

Measured on a copy of the live database (2026-10-20):

| | pc | framework | phone |
|---|---|---|---|
| minutes collected since August | 113,345 | 14,942 | 38,186 |
| minutes ever labelled | 8,234 (7%) | 2,046 (14%) | 13,149 (34%) |
| screenshots kept on the server | 85,291 | 10,054 | none |

- **Nothing has been labelled since 2026-08-21.** Collection kept working; the
  classifier was switched off because of the GPU. For two months the dashboard
  has shown stale labels carried forward.
- **3.8 GB of screenshots are kept forever** on the homelab, against the
  original promise to delete them.
- **The LLM labels are not reliable enough to train on.** The same Netflix tab
  was labelled youtube, other and browsing on different minutes. The same
  terminal session was labelled work_neptune, comms and work_husk. A minute
  with no input for 2.3 days was labelled browsing.
- **A browser bug**: the domain comes from whichever browser's extension
  reported last. A Firefox window showing dn.se is recorded with
  play.hbomax.com from Zen.
- **The phone is already deterministic in practice.** Its package name alone
  predicts the LLM's label 93% of the time; the other 7% is mostly LLM noise.
- **Desktop is where the real question is**, and the question is *which
  project*, not *which app*. Window class alone predicts the old label only
  ~55% of the time, because kitty, Claude, T3 Code and the browser are used
  for every project. Pixels do not answer that well either. The working
  directory, the git repo and the agent session do.
- About 60 distinct window titles per day of present time (1,803 in 30 days).
  That is the whole classification workload. It does not need a GPU.

## Scope

Time measures Erik across every device he owns, in four modalities, and then
categorises and analyses them:

| modality | direction | sources |
|---|---|---|
| **typing** | in | key counts per minute (pc, laptop), typed prompt words (T3 Code) |
| **voice** | in | dictation and voice mode (T3 Code web/Android, local-stt gateway, voxtype hotkey): seconds recorded, words |
| **listening** | out | read-aloud in T3 Code: seconds played, words heard; other audio playing (MPRIS on pc, media sessions on phone) |
| **screen** | out | focused window, title, browser host, screenshot embedding, phone app sessions |

Plus two outcome streams: **tokens** (per provider account, from T3 Code) and
**code** (commits, lines, PRs, reviews: GitHub and local git, as `time collect`
does today).

## Architecture

Everything speaks OpenTelemetry into the homelab observability stack, using
the contract agreed with the observability session (homelab
`docs/observability.md`, "The Time pipeline", c863d8e; planned, not live).
Time is not a separate silo. The stack carries the signals and gives Grafana
its counters, and Time keeps the events and the classified activity long
term.

```
 pc / laptop                                   phone
 ───────────                                   ─────
 T3 Code server ──┐ prompts, tokens, focus,    time android: app spans,
                  │ dictation, read-aloud,     screen on/off, media,
 local-stt ───────┤ voice mode                 heartbeat on every sync
 voxtype hook ────┤                              │ own spool, retries
 time agent ──────┤ screen minute, typing,       │
                  │ audio playing                │
 time collect ────┘ GitHub + local git           │
        │                                        │
        │ local Alloy at 127.0.0.1:4318 when present (own disk queue),
        │ otherwise https://otlp.erikfrankling.duckdns.org
        v                                        v (OTLP/HTTP, lan-only + bearer token)
                gateway Alloy (homelab repo, PVC, persistent queue)
                  ├─► Loki {service_namespace="time"}: events unmodified,
                  │   5 years (the raw copy Time rebuilds from)
                  ├─► Prometheus-compatible store: low-cardinality counters,
                  │   at least a year
                  └─► Time server (OTLP/HTTP /v1/logs, persistent queue)
                        events, kept forever (SQLite, source of truth)
                        -> rules + decision model -> one timeline per minute
                        -> phone-friendly dashboard, reports, inbox
```

- **Endpoints, coded as config and never hard-wired:** the local Alloy at
  `http://127.0.0.1:4318` when it exists, otherwise
  `https://otlp.erikfrankling.duckdns.org` (OTLP/HTTP only, no gRPC, LAN and
  tailnet only, bearer token required; the token lives in the agent and app
  settings, never in git).
  In-cluster: `http://alloy.homelab:4318`. The agent checks for the local
  Alloy at start and uses it if present.
- **Events, not minutes, are the stored fact.** Each event carries a stable
  `event.id`, so a retry or a late phone sync is a no-op. Minutes are derived
  from events and recomputed whenever rules or the model change.
- **Two long-term copies.** Loki keeps the raw events unmodified for 5
  years, embedding included, because pixels are gone and an embedding
  dropped there could never be recomputed. Time's database holds the same
  events plus the classification. `time rebuild --since <date>` replays Loki
  into a fresh Time database after a loss or a schema change.
- **Delivery survives outages.** The device spools (agent, phone), the local
  Alloy's disk queue and the gateway's persistent queue to Time each retry
  until the next hop acknowledges.
- **Grafana gets counters, Time gets detail.** The metrics store holds
  words, seconds and tokens by provider, model, account, input mode, client
  and host. Per-event detail (thread ids, titles, repos) is never a metric
  label. The design depends on a Prometheus-compatible store with at least a
  year of retention, not on a particular product.
- **What leaves a device:** counts, durations, ids, app and window names,
  browser hosts and the screen embedding. **Never** prompt text,
  transcripts, read-aloud text or pixels. Word counts are computed where the
  text is.

### Agreed with the stack, and what Time must provide

(homelab `docs/observability.md`, 1c0c6d8)

- **Health:** `GET /healthz` on port 7373 answers 200 when the HTTP loop is
  up and the database is writable. That is all Gatus probes. A failing check
  starts a repair session, so it must never depend on whether events have
  arrived recently; quiet nights are normal.
- **Metrics:** `GET /metrics` on port 7373 (Prometheus text format), scraped
  by the gateway Alloy:
  - `time_device_last_heartbeat_timestamp_seconds{device}`: the newest
    `time.heartbeat` from each device.
  - `time_device_spool_pending{device}`: the `spool.pending` value of that
    heartbeat.
  - `time_events_ingested_total{event_name}` and
    `time_last_event_timestamp_seconds`, for freshness panels. These are not
    used by health.
- **Heartbeats:** the time agent (pc, laptop) and the Android app send
  `time.heartbeat` on every sync. The phone syncs every 15 minutes on Wi-Fi.
- **Alerts** (defined in the homelab, ntfy only, never a session):
  - pc and laptop: 2 h without a heartbeat while their Alloy is up.
  - phone: 2 h without a heartbeat, only 08:00–23:00 Europe/Stockholm,
    because Android's Doze delays syncs overnight.
  - spool: `spool.pending` rising for 6 h.
- **Backups are a blocker.** Homelab backups are not running yet. **Time v2
  does not go live until the Time database and the Loki volume are backed
  up off the node, and a restore has been tested.**

## Event schema

OTel log records, with names and resource attributes following the stack's
contract. Resource: `service.name` (`t3code`, `local-stt`, `voxtype`,
`time-agent`, `time-android`, `github-collector`), `service.namespace =
"time"`, `host.name` (pc, framework, phone), `service.version`. Common
attributes: `event.id` (UUID, idempotency key), `time.start` / `time.end`
(ms, for spans), `project` (when known).

| event.name | emitted by | attributes |
|---|---|---|
| `t3.prompt.sent` | T3 server | `thread.id`, `project`, `provider`, `model`, `account`, `client` (web, android, electron), `words.typed`, `words.dictated`, `input_mode` (typed, dictated, mixed), `origin` (human, agent, scheduled) |
| `stt.dictation.finished` | local-stt gateway (T3 paths), voxtype hook | `recording.id`, `client` (t3-web, t3-android, voxtype), `audio.seconds`, `words`, `latency.ms`, `outcome` (complete, cancelled, no_speech, failed), `model` |
| `t3.voice.recording` | T3 client via server | `recording.id`, `thread.id`, `seconds`, `voice_mode` (true/false) |
| `t3.voicemode.span` | T3 client via server | `thread.id`, `seconds`, `turns` |
| `t3.readaloud.played` | T3 client via server | `message.id`, `thread.id`, `mode` (summary, full), `voice` (higgs, kokoro), `seconds.played`, `words.heard`, `outcome` (done, stopped, error), `speed` |
| `llm.tokens.used` | T3 server, per provider turn | OTel GenAI names: `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, plus `cached_input_tokens`, `reasoning_tokens`, `account` (provider instance id), `thread.id`, `project`, `origin` |
| `t3.focus.span` | T3 server (from client activity reports) | `client`, `thread.id`, `project`, `visible`, `focused` |
| `time.screen.minute` | time agent | `window.class`, `window.title`, `browser.host`, `fullscreen`, `audio.playing`, `keys`, `mouse`, `idle.seconds`, `embedding` |
| `time.phone.app` | time android | `app.package`, `app.label`, `screen` (on, off), `media.playing` |
| `time.heartbeat` | time android, time agent | `spool.pending` (events not yet sent), `app.version` |
| `time.nudge.sent` / `time.nudge.outcome` | Time server | `window.minutes`, `unproductive.minutes`, `productive.minutes`, `top_sources`, `focus.reason`, `nudges.today`; outcome: `switched` (true/false), `minutes.to.switch` |
| `github.commit` / `github.pr` / `github.review` | github-collector (today's `time collect`) | repo, lines added/removed, files, PR number and state; local-only repos as `git.commit` |

Metrics (counters, low cardinality only: provider, model, account,
input_mode, client, host):
`time_words_total{input_mode=typed|dictated|heard, client}`,
`time_audio_seconds_total{direction=recorded|played, client}`,
`gen_ai.client.token.usage{provider, model, account, type}` (OTel semconv),
`time_dictations_total{client, outcome}`. Nothing per prompt is needed as a
metric; per-prompt analysis runs in Time on the events.
## Instrumentation points (proposed, not edited)

T3 Code fork (`~/projects/personal/t3code`, being edited by other sessions
tonight, so this is a request for them):

- **Emitter:** one new Effect service in `apps/server/src/observability/`.
  The server already exports traces and metrics over OTLP
  (`Observability.ts`, `OTEL_EXPORTER_OTLP_*`); it needs an OTLP *logs*
  exporter added, with the env vars in `hosts/pc/home.nix` pointing at the
  local Alloy (`127.0.0.1:4318`) or, until that exists,
  `https://otlp.erikfrankling.duckdns.org`.
- **Prompts:** at `message.dispatch` (`apps/server/src/ws.ts:1819`) and
  `launchThread` (`ws.ts:1950`), next to the existing
  `analytics.record("client.turn.requested")`. Typed vs dictated: the
  composer sends `[dictation:<id>]` markers, which
  `DictationDeliveryQueue.ts:238` swaps for the transcript. So
  words outside markers are typed and words inside are dictated. Human vs
  agent: `createdBy === "user"` and no `scheduledTaskId`.
- **Dictation:** `DictationDeliveryQueue.ts:203-265` already logs `sent`,
  `no_speech` and `delivery_failed`. Add `stt.dictation.finished` with word count
  there for T3, and pass `X-STT-Client: t3-web|t3-android` to the gateway.
- **Tokens:** where `provider-turn.updated` is stored
  (`orchestration-v2/ProjectionStore.ts:2297`, fed by `ClaudeAdapterV2.ts:3775`
  and `CodexAdapterV2.ts:4344`). Account from the thread's
  `providerInstanceId` and `ServerProviderAuth.email`. This covers every
  account T3 runs. Claude/Codex sessions outside T3 stay covered by
  `time agents`.
- **Read-aloud and voice mode** happen in the client, so the server can't see
  them: `ReadAloudPlayer` (`packages/client-runtime/src/readAloud.ts`: start
  :110, stop :127, done ~:250) and `voiceLoop.ts` phase changes. Add one
  RPC, modelled on `server.reportClientActivity` (`rpc.ts:504`), that the
  web and mobile clients call with finished spans. Played seconds must be
  summed in the `ReadAloudAudio.play` implementations; nothing tracks it now.
- **Focused thread:** `backgroundActivityReporter.ts:95-106` already reports
  `visible`/`focused` with an unused `{type:"thread", threadId}` scope. Fill
  it from the route. **This answers "which project" for the ~45% of desktop
  time spent in T3 Code**, better than inferring it from prompt timestamps.

Dictation service (`~/.dotfiles/tools/local-stt/gateway.py`):

- Emit `stt.dictation.finished` in `finalize()` at the `recording.complete` log
  (:622). It sees every T3 dictation exactly once, retries included.
  Audio seconds = bytes / 32000. Read `X-STT-Client` at start (:662).
- Voxtype never reaches the gateway. Its cleanup script
  (`modules/home-manager/voxtype.nix:21-83`) runs once per dictation with
  the transcript on stdin, so emit `stt.dictation.finished client=voxtype` there.
  Audio length is not known there; take it from the hotkey toggle times.

GitHub: keep `time collect` (local git + GitHub GraphQL) and add per-PR and
per-review events, so the timeline shows when a PR was opened, merged or
reviewed, not just daily totals.
### What each minute is answered with

Every minute gets three typed fields, all deterministic:

- **where**: `pc`, `laptop`, `phone`, `both`, or `away`.
  From input counts, screen state, phone screen-on, and media playing.
- **activity**: a short fixed list (work, study, comms, video, social,
  browsing, music, games, other) plus **project** (neptune, husk, personal
  repo name, kth course) when work.
- **why**: the rule id that decided it, so every label can be explained and
  fixed.

Precedence, highest first:

1. Manual override (a correction from the dashboard).
2. Presence: no input on any device and nothing playing -> `away`.
   A fullscreen video or MPRIS playing with no input is `video`, not away.
3. Project from context: the thread focused in T3 Code (`t3.focus.span`),
   the focused terminal's cwd, and the project of the latest human prompt.
   Repo -> project is a small table (`~/projects/husk/* -> husk`).
4. App and site rules: window class, browser host + title, Android package.
   Regex/glob on typed fields, first match wins, written in TOML and tested.
5. The decision model (screenshot + title + metadata) for everything else (see below).
6. Otherwise `unknown`, shown in an inbox on the dashboard. Labelling one
   item there writes a rule, so it never comes back.

Rules are re-run over the stored raw facts whenever they change, so history
is always consistent with the current rules. That replaces `reclassify`.

### Cross-device merge

One timeline per minute: the device with input wins; phone in hand while the
pc plays a video becomes `phone` + background `video`. The phone's late sync
is handled by recomputing a minute when its facts arrive, which is cheap
because rules are cheap. The "completeness frontier" machinery goes away.

## Classification approach

**A small trained decision model, not an LLM.** It looks at the screenshot
and the metadata, outputs typed enums with a confidence, and gives the same
answer every time for the same input. Rules sit in front of it as hard
overrides. Nothing runs on the GPU.

```
screenshot ──► SigLIP 2 image encoder (pc CPU) ──► 768 numbers ─┐
window title ─► EmbeddingGemma text encoder ────► 256 numbers ─┼─► classifier ─► Where, Activity, Project
metadata ─────► input counts, fullscreen, audio, host, package ─┘   (small head,    + confidence
                                                                     trained on Erik's labels)
```

- **Decision model** = frozen encoders plus a small classifier head trained
  on Erik's own labels (logistic regression or gradient-boosted trees; pick
  whichever scores better on the test set). Fixed weights and argmax make it
  deterministic. The output is a Rust enum, not text, so it cannot invent a
  category. Below a confidence threshold it answers `unknown` and the minute
  goes to the inbox.
- **Rules first.** When the evidence is unambiguous (phone app is Netflix,
  host is youtube.com, no input anywhere and nothing playing), a rule decides
  and the model is not consulted. Rules also produce most of the training
  labels; Erik's corrections in the inbox produce the rest. The old LLM labels
  are too noisy to train on and are not used.
- **Project comes from context, not pixels.** About 45% of present desktop
  time in the last two weeks was T3 Code, whose title never names the
  project. T3 Code knows which thread is open and focused; reporting it
  (`t3.focus.span`) answers that directly. Prompt timestamps with their
  working folder are the fallback for Claude and Codex outside T3.

**Model choice, with benchmarks**

Image encoder (zero-shot ImageNet, a proxy for how much the features know):

| model | params (image side) | ImageNet zero-shot | licence |
|---|---|---|---|
| SigLIP 2 So400m | 400M | 83.4 | Apache-2.0 |
| **SigLIP 2 B/16** | 86M | 78.2–79.1 | Apache-2.0 |
| MobileCLIP2-B | ~86M | 79.4 | Apple sample-code licence |

Pick **SigLIP 2 B/16**: small enough for the CPU, Apache-licensed, and it
recognises screen layouts (video player, editor, chat, game). Encoders like
this read small text poorly, which is why the title and the context signals
are inputs as well.

Text encoder (MTEB multilingual v2, mean / classification):

| model | params | score | licence |
|---|---|---|---|
| Qwen3-Embedding-0.6B | 595M | 64.3 / 66.8 | Apache-2.0 |
| **EmbeddingGemma-300M** | 308M | 61.2 / 60.9 | Gemma terms |
| multilingual-e5-small | 118M | 55.8 / 56.2 | MIT |

Pick **EmbeddingGemma-300M**: it handles Swedish, needs under 200 MB of RAM,
and is half the size of Qwen3-0.6B. Titles are cached, so it runs only on
titles it has not seen (~60 a day). Qwen3-0.6B is the swap-in if it scores
better on our test set.

Rejected:

- **Screen-reading VLMs** (Qwen3-VL-2B: DocVQA 92.7). They are generative
  LLMs in practice: tens of seconds per image on a CPU, or the GPU again.
- **Cloudflare Workers AI** for images. It offers the same kind of models
  cheaply (text embeddings at ~$0.012 per million tokens, inside the free
  daily tier at our volume, and Cloudflare states it does not train on
  inputs), but it would mean sending screenshots off the machine. Local CPU is
  free and private. It stays an option for text only.

**Evidence still to collect.** No one publishes accuracy for this task.
ActivityWatch uses regex rules only, with no ML [1]. The first build step is
a test set: ~500 minutes from the live database, hand-labelled by Erik in the
inbox UI (about 15 minutes of clicking). Rules alone, rules plus the model,
and the model with and without the screenshot are all scored on it. If the
screenshot does not measurably help, it is switched off and stops costing
CPU. No published CPU speed exists for SigLIP 2 B/16 either. I estimate
50–150 ms per image on the 7800X3D and will measure it before committing.

[1] https://docs.activitywatch.net/en/latest/features/categorization.html
[2] EmbeddingGemma paper, MTEB tables: https://arxiv.org/html/2509.20354
[3] SigLIP 2: https://arxiv.org/html/2502.14786
[4] Workers AI: https://developers.cloudflare.com/workers-ai/platform/pricing/ ,
    https://developers.cloudflare.com/workers-ai/platform/privacy/
## Power and noise budget

- **The GPU is never used.** llama-swap is not involved, so there is no fan
  noise.
- pc agent: the screenshot is encoded on the CPU, only when the screen has
  changed and someone is at it (about 350 minutes a day on the pc). At an
  estimated 50–150 ms each, that is under a minute of CPU a day, on a
  single core, inaudible.
- Server: rules, the title encoder for new titles (~60 a day, cached) and
  the classifier head. Seconds of CPU a day on the homelab.
- Laptop: same agent, same cost, and it can turn screenshots off on battery.
- For comparison, the old design: a VLM call every minute on a ~300 W GPU,
  with the model kept loaded. That is an estimated 0.5–2 kWh a day plus hours
  of fan noise. The new design uses roughly a thousandth of that energy.

## Privacy

- **Screenshots never leave the pc.** The agent encodes each one into 768
  numbers in memory and throws the pixels away. Only those numbers go to the
  homelab. They are not a picture, but they are not nothing either: they
  roughly describe what kind of screen it was. Stating that plainly.
- Optional, off by default: keep the screenshots of `unknown` minutes on the
  pc's own disk for 7 days, so Erik can see what he is labelling in the
  inbox. They are deleted after 7 days.
- The 3.8 GB archive on the homelab is deleted after Erik confirms. An
  export can be made first if he wants it.
- Window titles and browser hosts (never full URLs) go to the homelab over
  the LAN/VPN, as today. They are kept in Time and in the 5-year Loki copy
  (the rebuild source, on the same homelab). Whether titles stay in that
  Loki copy is a decision for Erik; the default is to keep them.
  Nothing goes to any third party.
- No prompt text, transcripts or read-aloud text are ever sent; only their
  word counts.
- The blocklist stays on the agent. A blocked window is not captured at all
  and sends only "blocked".
## Phone side

The phone app becomes **a thin native shell around the website**, plus the
native collectors only an Android app can run. The interface is the
website, loaded in a WebView, so the app and the browser are the same
screens by construction. They are not two UIs kept in sync.

Native collectors, which all run inside the periodic sync. There is no
foreground GPS service and no location permission at all:

- **App usage:** the existing UsageStats session builder (the most correct
  part of the old system, kept with its tests). It now sends spans
  (`pkg, start, end`) and the server fans them out to minutes.
- **Screen and media:** screen on/off and keyguard events from the same
  event log, and media playback from MediaSession, so "video on the phone in
  bed" is distinguishable from "phone on the table".
- **Health (new):** steps and distance from **Health Connect**
  (`READ_STEPS`, `READ_DISTANCE`), summed per hour. Health Connect gets this
  from Google Fit or the phone's own pedometer, which already run, so this
  costs no battery. Reading during a background sync needs
  `READ_HEALTH_DATA_IN_BACKGROUND`. Data older than 30 days needs
  `READ_HEALTH_DATA_HISTORY`, and both depend on the installed Health
  Connect version, so the app checks feature status and says what it got
  [5].
- **Walked routes:** only what the phone already records.
  - *Health Connect exercise routes* exist only for workouts recorded by
    another app (Fit, Strava). Erik must grant route access in Settings,
    since the normal permission dialog cannot grant it [5]. If granted,
    workout routes appear on the day view.
  - *Google Maps Timeline* has no API. Since 2024 it is stored on the device,
    and the only way out is a manual JSON export from the phone's location
    settings [6]. Time can import that file (upload on the website) and draw
    the walks, but it cannot fetch it automatically.
  - **No GPS tracking in the app**, by design.
- **Heartbeat** on every sync, with spool size and permission state (usage
  access, Health Connect, notifications, battery optimisation). The status
  screen can then say exactly why the phone isn't reporting.
- **Sync:** every 15 minutes (WorkManager's minimum), on any network. The
  spool survives reboots, and events are acknowledged per batch.

[5] https://developer.android.com/health-and-fitness/health-connect/read-data
[6] https://www.androidauthority.com/android-google-maps-location-history-3491255/

## Status screen

A page on the website (and so in the app), always reachable from the top
bar. It answers "is it working?" without a terminal:

- **Pipeline:**
  - Events received in the last hour per source.
  - Minutes classified versus waiting, with the age of the oldest waiting
    minute.
  - When the classifier last ran and when it runs next.
  - The share of minutes decided by rules, by the model, or left `unknown`.
  - Any minute the classifier failed on, with the error.
- **Devices:** for pc, laptop and phone:
  - last heartbeat;
  - the newest minute covered, and how far behind it is;
  - spool size;
  - app version;
  - for the phone, which permissions are missing.

  Each device is shown green, amber or red. "Connected" means a heartbeat
  within 20 minutes.
- **Channels recording right now:** one row per signal (screen, typing,
  browser tab, audio, T3 prompts, dictation, read-aloud, tokens, phone apps,
  health, GitHub), showing its last event and whether it is live, quiet or
  missing. A channel that is switched off by config says so in words, so
  the old failure (classification switched off, with nothing on screen
  saying it) cannot happen silently again.
- **System:**
  - the Time server's own health (database size, write latency);
  - the classifier's model and rules version;
  - whether the notification tool and the OTLP gateway are reachable;
  - the last backup time, once backups exist.

The same numbers go out on `/metrics`, so Grafana and the alerts read
exactly what the screen shows.
## Focus nudges

When Erik has spent a long stretch only on unproductive things, Time pings
his phone and tells his personal agent, so the agent can talk to him about it
and keep the day on track.

**The rule is deterministic and runs on the server every 15 minutes:**

- Look at the last 60 minutes of the merged timeline (all devices).
- **Unproductive minutes:** activity `video` or `social` from a configured
  list of leisure sources: YouTube, X, Instagram, TikTok, Reddit, Netflix,
  HBO Max and so on. These are matched by rules on host and package, so the
  nudge does not depend on the model at all.
- **Productive signals:** any `work` or `study` minute, a human prompt sent
  in T3 Code, a dictation, a commit, or typing in an editor or terminal.
  YouTube playing next to real work does not count against him.
- **Nudge when** unproductive minutes ≥ the threshold (default 30; Erik
  picks 30–60) **and** productive minutes < 5 in the same window.
- **Never spam:** at most one nudge per 45 minutes, and at most 4 a day.
  The next one says how long it has been ("1 h 20 min on YouTube and X").
  No nudges inside quiet hours.
- **Focus days:** the personal agent knows about deadlines and urgent work;
  Time doesn't. The agent can call `POST /v1/focus {until, reason,
  threshold}` to tighten the rule for the day (e.g. 20 minutes, and outside
  normal hours too). The reason is included in the nudge text.

**Where the nudge goes:**

1. **Phone:** through the shared phone-notification tool being built in
   [Phone notifications tool](t3-thread://v1/mcp:582e0de7-096b-49b3-8f9a-8dd721ddf803),
   at normal (not urgent) priority. Tapping it opens Time's day view.
2. **Personal agent:** a POST of the same facts to a webhook the personal
   agent owns (a T3 `schedule_task` webhook bound to its thread). The URL is
   a secret in Time's config. The payload is facts only: window, minutes per
   source, productive minutes, nudges today and the focus reason. The agent
   decides what to say.
3. **Record:** a `time.nudge.sent` event, and later `time.nudge.outcome`
   (did activity change within 10 minutes?). Over time this shows whether
   the nudges help, and lets Erik tune the threshold from data.

**Freshness and GPU:** a 30–60 minute rule only needs data that is minutes
old. Desktop events arrive within a minute. The phone sends every 15
minutes, which is good enough. Classification in this design never uses the
GPU (rules plus a CPU model), so it can never evict the dictation models.
If a GPU model is ever added, it runs as a batch at most once per nudge
cycle and is unloaded afterwards; it is never kept resident.

## What Erik gets out of it

- Today: a timeline (pc / laptop / phone lanes) and totals per activity and
  project, each total explained by its rules.
- Week view: hours per project, focus blocks (longest uninterrupted work),
  phone pickups and phone time during work blocks.
- Unknown inbox: the handful of unclassified things, one tap to fix.
- Modalities, per day and per week: hours typing, words typed vs dictated
  (in prompts and overall), minutes recorded and words dictated, minutes
  and words listened to (read-aloud vs other audio), screen time per
  device.
- Movement: steps and kilometres per day and per hour, next to screen time.
  Workout routes and imported Timeline walks appear on a map.
- Tokens per provider account, model and project, next to hours spent.
- Code: commits, lines, PRs opened, merged and reviewed, per project.
- The status screen above.
- Grafana gets the same counters from Prometheus for alerts and quick panels;
  the phone view is Time's own page, since Grafana is poor on a phone.

## Reliability and technology

Reliability is the main goal. The old code failed in ways nobody could see:
classification switched off for two months with nothing on screen saying
so, a browser domain attributed to the wrong browser, and labels that
changed from minute to minute. The rewrite is built around making failure
visible and testable.

**Full rewrite, same core languages.** The old code is 12k lines of Rust and
1.8k of Kotlin, and about half of it is the LLM pipeline being removed. What
survives (the Android session builder and its tests, the spool idea, git and
transcript parsing) is small enough to port by hand into the new structure.

| part | old | new | why |
|---|---|---|---|
| server | Rust, `tiny_http` thread pool | Rust, `axum` + `tokio` | The old pool starved its own health probe. axum is the standard async stack, and it serves OTLP/HTTP, the web UI and `/metrics` on one port. |
| storage | SQLite, ad-hoc migrations | SQLite (WAL) with numbered migrations, one writer task | Still the right size: a few million small rows. Migrations are tested from an empty database and from a copy of the old one. |
| shared types | strings | a `time-core` crate: `Where`, `Activity`, `Project` enums, event structs, rules, the timeline merge | One definition of every type, used by the server, agent and simulator. A label that does not exist cannot be stored. |
| agent | Rust, shells out to `hyprctl` and `grim` | Rust, Hyprland IPC socket directly, with every system call behind a trait | The traits are what the simulator replaces, and talking to the socket avoids forking a process every minute. |
| model | none (LLM over HTTP) | ONNX Runtime (`ort` crate) on CPU: SigLIP 2 B/16 and EmbeddingGemma-300M, with a logistic-regression head in Rust | Runs in-process with no Python and no GPU, and gives the same output for the same input. |
| web UI | server-rendered HTML with inline JS | server-rendered HTML (templates) with a small amount of JS, mobile-first | It works in the phone WebView, in a desktop browser and with scripting off, and needs no build step to break. |
| phone | Kotlin with a Compose UI | Kotlin collectors with a WebView of the website | App and website are the same screens (item 3 of Erik's request). |

Not chosen: a cross-platform app framework (Flutter, React Native) for the
phone. A WebView of the website gives the same screens with less code. The
collectors need native Android APIs whatever framework is used.

## End-to-end testing and emulation

Every feature is tested against a simulated world, on the desktop and in CI,
without Erik's devices.

```
 scenario file (a scripted day)
   │
   ├─► sim-desktop:  fake Hyprland IPC socket (active window, clients,
   │                 workspaces), fake screenshots (rendered images per
   │                 scene), fake input counts, fake MPRIS/audio,
   │                 a fake aw-watcher-web tab feed
   │                 + the REAL time agent binary pointed at them
   ├─► sim-phone:    sends exactly the OTLP batches the Android app sends
   │                 (shared fixtures with the Kotlin tests), including
   │                 late syncs, duplicates and a 2-day outage
   ├─► sim-t3, sim-stt, sim-github: emit their documented events
   ├─► fake notify endpoint and fake personal-agent webhook (record calls)
   │
   └─► the REAL Time server, on a temporary database, with a virtual clock
         │
         └─► assertions: classified timeline, report totals, nudges sent,
             status screen, /metrics, /healthz, plus Playwright checks of
             the website at phone and desktop sizes
```

- **Virtual clock.** Server, agent and simulators share a `Clock` trait, so a
  whole scripted day runs in seconds. Production uses the system clock.
- **Scenarios are data.** For example: "09:00–09:40 YouTube in Firefox on
  pc, phone idle; 09:40 a typed prompt of 40 words in a husk thread; phone
  offline 12:00–14:00, then syncs late." Each bug found later becomes a
  scenario.
- **Layers:**
  - Unit tests: rules, timeline merge, nudge rule, session builder.
  - Golden tests: real anonymised minutes from the old database, labelled
    by Erik, used for classifier accuracy.
  - Scenario end-to-end tests (above).
  - Android: JVM tests for the collectors (as today), plus one instrumented
    smoke test on an emulator in CI (GitHub Actions). It installs the APK,
    grants permissions with `adb`, feeds Health Connect test data, and
    checks that the app's events reach a fake gateway and the WebView
    renders.
- `nix flake check` runs everything except the Android emulator test, which
  runs in CI. The test harness never touches the real Hyprland, the real
  phone or the live server.

## Milestones

Each milestone is merged and pushed with its tests green. The live server
switches to v2 only at M4, after backups exist.

Progress (branch `v2`, 2026-10-21):

- **M0 done.** Workspace, OTLP codec, virtual clock, scripted days, fake
  Hyprland, simulated phone; end-to-end tests run the real server and agent.
- **M1 done.** 36 rules, cross-device timeline, v1 backfill: 116,034 minutes
  labelled in about 6 s on CPU; 88% agreement with v1's LLM labels; 0.6% of
  present minutes left unknown.
- **M2 mostly done.** OTLP ingest, `/healthz`, `/metrics`, the status screen,
  real desktop probes (evdev, MPRIS, browser tabs matched by title). Not yet:
  protobuf OTLP (Alloy can send JSON), the unknown inbox.
- **M3 in progress.** Day view done (hour-by-hour, totals, projects, steps;
  checked on real data at phone width). Android rewritten as a WebView of the
  website plus collectors (usage spans, media, Health Connect, heartbeat), with
  a contract test against the server and an emulator smoke test in CI. Not yet:
  week view, inbox.

| | milestone | contents | depends on Erik? |
|---|---|---|---|
| M0 | Skeleton and harness | Cargo workspace (`time-core`, `time-server`, `time-agent`, `time-sim`); virtual clock; scenario format; fake Hyprland socket, fake phone, fake notify; first end-to-end scenario green | no |
| M1 | Rules classifier and timeline | typed enums, TOML rules, cross-device merge, the `unknown` inbox; a backfill that labels the two-month backlog offline from a copy of the old database | defaults used for categories |
| M2 | Ingest and status | OTLP/HTTP receiver, events table, idempotency, `/healthz`, `/metrics`, the status screen; the agent rewritten on traits | no |
| M3 | Website and phone shell | day and week views, inbox, status, mobile-first; Android WebView shell with collectors (usage, screen, media, Health Connect, heartbeat); emulator smoke test in CI | no |
| M4 | Go live | backups in place, migrate the old database, deploy v2 next to v1, compare for a day, then switch | **yes**: backups, screenshot archive |
| M5 | Decision model | labelling UI, test set, SigLIP 2 and EmbeddingGemma on CPU, the screenshot ablation, then on by default only if it measurably helps | **yes**: 15 minutes of labelling |
| M6 | Focus nudges | nudge rule, focus days, notify tool, personal-agent webhook | thresholds use defaults until answered; needs the notify tool to exist |
| M7 | T3 Code and dictation instrumentation | the changes listed above, in the T3 fork and dotfiles, in their own session | **yes**: who edits the T3 fork, and when |
| M8 | Health extras | Health Connect routes; Timeline JSON import | no |

## Open questions for Erik, with the defaults in use

| # | question | default until answered |
|---|---|---|
| 1 | Rules plus a small CPU model (screenshot + title + metadata), no LLM, no GPU | building rules first (M1); the model waits for M5 either way |
| 2 | All data through the shared OTel stack, with a 5-year raw copy in Loki | yes |
| 3 | Window titles kept in the 5-year Loki copy | yes |
| 4 | Backups before go-live | yes, and a blocker for M4 |
| 5 | Delete the 3.8 GB screenshot archive, or export it first | keep it untouched until he answers |
| 6 | Activity list and project mapping | work, study, comms, video, social, browsing, music, games, other; neptune, husk, personal repo name, kth course |
| 7 | 15 minutes of labelling for the test set | needed at M5 |
| 8 | Who edits the T3 fork for instrumentation, and when | a separate session after he approves |
| 9 | Nudge threshold | 30 minutes |
| 10 | Nudge hours | 09:00–18:00 on weekdays, plus any focus day |
| 11 | Unproductive list | YouTube, X, Instagram, TikTok, Reddit, Netflix, HBO Max; LinkedIn and news count as browsing (neutral) |

## Removed

LLM classifier, prompt, batching, llama-swap config, `reclassify`,
`llm_call`/`minute_trial` tables, the screenshot archive on the server (on
Erik's word), completeness frontier, dHash, the Compose UI.
