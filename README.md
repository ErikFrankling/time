# time

Track what I actually do, minute by minute, and get a breakdown at the end of
the day.

An agent on each machine screenshots the focused monitor every minute and posts
it to a server. The server asks a multimodal model what's happening and stores
one row — category, project, and one concrete sentence.

The design used to delete every screenshot after labelling ("labels are kept
forever, pixels are not"). That promise is **deliberately reversed** now, by
owner decision: the server keeps the screenshots (`<data dir>/frames/`) and
the raw model replies (the `llm_call` table) forever, precisely so labels can
be recomputed later with better models — see `time reclassify`. What made the
reversal acceptable is that classification moved to a local model on the LAN:
the pixels are archived on my own disk and no longer shown to any third party.
The client-side blocklist is unchanged — a blocked screen is still never
captured at all, so it can't be retained either.

## Shape

```
  agent (pc, laptop)                 server (k3s)
  ─────────────────                  ────────────
  grim → downscale → POST  ───────►  dHash: did the screen change?
                                       no  → carry the label forward, free
  no API key                           yes → store the row, queue it, reply
  no database                                    ↓
  no model calls                     classifier pool → model → UPDATE the row
                                     SQLite on a PVC
                                     serves the UI
```

Ingest answers in milliseconds and never waits on the model. It used to, and a
slow endpoint meant every request thread sat blocked for minutes — including
the ones the health probe needed, which took the pod out of the load balancer
and returned 503 to everybody. The row is written before the job is queued, so
the queue is disposable: a restart re-reads anything still unlabelled.

Both clients post a backlog to `/v1/frames` as one array; `/v1/frame` takes a
single minute, which is what the desktop agent sends whenever it is caught up.

The agent is deliberately dumb. It holds no secret and makes no billable calls,
so a laptop is never more than a capture device. Every decision that costs money
happens server-side, in one place, with one config.

### Offline

Every frame is written to `~/.local/share/time/spool/` before it is sent, and
deleted only once the server has answered 2xx. A server restart, a dropped VPN
or a closed laptop costs nothing: the next tick posts the backlog to
`/v1/frames`, oldest first, and only then the current minute. The spool is
capped at 7 days and 32 MiB, evicts oldest first, and says on stderr exactly
what it dropped.

A spooled frame carries no screenshot. Pixels never touch the *agent's* disk
(retention is a server-side decision, made once, in one place) and metadata is
small enough (a few hundred bytes a minute) that a week-long outage never
reaches the cap.
A stranded minute is classified from its window, apps and domain instead, which
is how every minute the phone sends is classified anyway. Minutes that go out
while the server is up still carry their screenshot.

## Why a model instead of rules

Window titles get you maybe 60% of the way and then fall apart: Electron apps
all look identical, terminals are a black hole, and "a browser is open" tells
you nothing. Worse, "looks active but is actually idle" isn't one edge case —
it's a video playing while you're in the kitchen, a long build, reading a PDF
for six minutes, a meeting where you're listening. Every one of those is a rule,
every rule has exceptions, and the exceptions interact.

A model looking at the screen just sees what's happening. So the split is: code
produces facts (window class, screen-changed-or-not), the model produces
judgments (what is this, whose project is it).

For the same reason there is no package map any more. An earlier shortcut
translated phone package names straight to categories ("com.instagram = twitter")
without a model call. It is gone, deliberately: the model has full authority
over what a minute means, and a lookup table that preempts it is exactly the
rule-stack this section argues against — it froze judgments the model, seeing
every device's timeline at once, is better placed to make (Instagram open while
the pc plays a lecture is not "twitter" the way Instagram at midnight is). The
cost argument that justified it died when classification moved to a free local
model. Old configs that still carry `phone_categories` parse fine; the key is
simply ignored.

The model also sees all devices in **one timeline**: batches interleave every
machine, simultaneous minutes are marked as such, and each device's previous
label rides along. Input counters say where the hands were, so typing on the pc
while the phone plays YouTube reads as work with YouTube in the background —
not as two separate activities — and a video with no input anywhere reads as
watching, or as an empty room, whichever the trajectory suggests.

## Deploying the server

Manifests live in the homelab repo under `kubernetes/homelab/apps/time.yaml`
and are reconciled by Flux. `deploy/time.yaml` here is the source of truth for
that file. The image is built by GitHub Actions and published to
`ghcr.io/erikfrankling/time:latest`.

**The secret is never in git.** Create it directly on the cluster (`api-key`
is optional now — the local llama-swap endpoint takes no auth, and the server
starts without it; set it only when pointing at a hosted endpoint):

```bash
ssh naiaclaw "sudo kubectl -n homelab create secret generic time-secrets \
  --from-literal=api-key='YOUR_OPENCODE_KEY' \
  --from-literal=ingest-token=\"\$(openssl rand -hex 32)\""
```

Then read the generated ingest token back out — the agents need it:

```bash
ssh naiaclaw "sudo kubectl -n homelab get secret time-secrets \
  -o jsonpath='{.data.ingest-token}' | base64 -d; echo"
```

The route is `time.erikfrankling.duckdns.org` behind the `lan-only` middleware,
so it's reachable from the LAN and the VPN and nowhere else. It receives
screenshots; it must never be internet-facing.

## Running an agent

```bash
nix develop --command cargo run --release -- agent
```

First run writes `~/.config/time/config.toml`. Set the server URL and device
name there, and put the ingest token in the environment:

```bash
export TIME_INGEST_TOKEN=<the token from above>
```

| Command | |
|---|---|
| `time agent` | screenshot and post every minute |
| `time server` | classify incoming frames, store, serve the UI |
| `time once` | post a single minute and print what came back — use this to check labels |
| `time collect` | read commits, diffs and pull requests, post them to the server |
| `time agents` | read what the coding agents did, post it to the server |
| `time reclassify` | re-judge stored minutes from the kept screenshots (see below) |
| `time config` | print config and database paths |

## The Android app

The phone client reports which app is in the foreground. There is no store
behind it, so getting builds onto the phone is its own problem — and it used to
be solved by a human running `kubectl cp`. It isn't any more:

```
push to main (android/**)
  → GitHub Actions builds and signs the APK
  → publishes a release tagged android-v<version>-<versionCode>
  → the server pulls it within the hour and serves it at /app
  → the phone notices at its next sync and offers to install
```

Nothing in that chain needs a person. What it cannot do is install without a
tap: silent installation needs a system or privileged signature, so **every
update ends at one confirmation dialog**. The goal here is zero thinking, not
zero taps.

### Installing it — use Obtainium

[Obtainium][obt] watches a GitHub repo's releases and installs from them. Paste
this into it:

```
https://github.com/ErikFrankling/time
```

This is the recommended route, not a fallback, for a reason that is not
obvious: Obtainium installs through the `PackageInstaller` session API, the
same one app stores use. Android 13+ treats packages installed any other way as
untrusted and **silently greys out restricted permissions** —
`PACKAGE_USAGE_STATS`, which this app cannot work without, is one of them. The
toggle simply refuses to move and gives no reason.

If you install by downloading `/app` in a browser instead, you will hit exactly
that, and the way out is:

> Settings → Apps → time → ⋮ → **Allow restricted settings**

That menu item is hidden until Android has blocked the permission at least
once, so the sequence is: install, try to grant usage access, watch it fail,
then go allow restricted settings. The `/app` landing page says so too.

[obt]: https://github.com/ImranR98/Obtainium

### Updating

Whichever way it was installed, the app checks `GET /app/version` during its
normal 30-minute sync. If the server is serving a higher `versionCode` it
downloads the APK to its cache — on unmetered networks only, since it is ~25 MB
— verifies the sha256, and posts a notification. Tapping it opens the installer.
The settings screen shows installed vs available and has a manual check.

Obtainium does the same thing independently and works even when the app itself
is broken, which is the argument for having both.

| Route | |
|---|---|
| `GET /app/` | landing page: version, size, signing key, staleness |
| `GET /app` | the APK bytes |
| `GET /app/version` | `{version, versionCode, sha256, url, published, size, signing, stale}` |

The server keeps the APK at `<data dir>/time.apk` with a `.json` sidecar, and
re-downloads only when the release asset actually changes. If GitHub is
unreachable it keeps serving what it has and says so on the landing page.
Setting `TIME_APK_PATH` pins it to a file and turns fetching off entirely,
which is how a locally-built APK gets served; `TIME_APK_VERSION`,
`TIME_APK_VERSION_CODE` and `TIME_APK_SIGNING` describe that file, since
nothing else can. `TIME_APK_REPO` and `TIME_GITHUB_API` move the source.

### Signing — do this before installing anything

An APK signed with a different key than the one already installed **cannot**
upgrade it. Android reports `INSTALL_FAILED_UPDATE_INCOMPATIBLE` and the only
way forward is an uninstall, which takes the app's data with it. So the key has
to exist before the first install, not after.

Until `KEYSTORE_BASE64` is set, CI publishes the **debug** build instead of an
unsigned release — an unsigned APK cannot be installed at all, so releasing one
would be releasing nothing. The debug build works, carries a `.debug`
application ID, and is a dead end: it can never become the release build. The
landing page and the release notes both say which key a given APK carries.

Generate the key and hand it to Actions:

```bash
nix develop --command scripts/setup-signing.sh
```

That makes a 4096-bit key, pushes all four secrets, verifies they landed, and
prints the password once. It refuses to overwrite an existing keystore, because
replacing the key that signed the installed app is how you brick updates for
everyone running it.

**Back up `~/time-release.jks` and the password** somewhere that survives the
laptop. Losing either means every phone reinstalls from scratch. The file is
gitignored (`*.jks`) and must never be committed.

This one step is deliberately not in CI. Everything around a signing key is
ceremony worth scripting, but a workflow that can *mint* a key is a workflow
that can silently make every installed copy unupgradeable — so it is made once,
by a person, who then backs it up.

Afterwards: `gh workflow run android.yml`, uninstall whatever debug build is on
the phone, and install the release-signed one once. Every update after that is a
notification and a tap.

## Time per website

"Firefox — 4h" answers nothing. Two hours on dn.se and two on a bug tracker are
different days, and no window title on Wayland contains a domain.

The browser is the only thing that knows, so it has to volunteer it.
ActivityWatch's browser extension already does exactly that, and the agent
answers the three endpoints it posts to on `127.0.0.1:5600`:

1. Install [ActivityWatch Web Watcher][aw] in every browser you use. It is on
   AMO, so Firefox forks — Zen included — take it as-is.
2. Accept its consent prompt once. To skip the click, grant it up front with a
   managed-storage manifest at
   `~/.mozilla/managed-storage/{ef87d84c-2127-493f-b952-5b4e744245bc}.json`:
   `{"name":"{ef87d84c-2127-493f-b952-5b4e744245bc}","type":"storage","data":{"consentOfflineDataCollection":true}}`
3. Nothing else. The agent is already listening; there is no ActivityWatch
   server to run.

Only the host is ever taken from the URL, and only into memory — `dn.se`, never
which article. The extension keeps reporting its front tab while minimised, so
the domain is only recorded for minutes where the compositor says a browser
(`agent.browsers`) actually had focus.

[aw]: https://addons.mozilla.org/firefox/addon/aw-watcher-web/

## Configuration

`~/.config/time/config.toml` has an `[agent]` and a `[server]` section; each
side reads only its own, so the same file works everywhere.

- **`agent.blocklist`** — substring match on window class and title. Enforced on
  the client, so a matching screen is never even encoded, let alone sent.
  Password managers and Signal are in there by default.
  It matches the browser's front tab too, so a bank listed here suppresses the
  screenshot as well as the domain.
- **`agent.browsers`** — window classes that count as a browser, and so decide
  when the reported tab is what you were actually looking at.
- **`agent.width`** — downscale before sending. The cost dial.
- **`server.categories`** — the list that becomes the chart. `idle` and `other`
  always render grey; the rest get one of 8 colourblind-checked colours in list
  order. Keep it to 8 real categories. Off-list answers from the model fold into
  `other` rather than inventing a slice — but the name the model would have
  used is kept as the minute's first tag, so
  `SELECT tags, COUNT(*) FROM minute WHERE category='other' GROUP BY 1` is how
  this list gets grown from data instead of guessed.
- **`server.idle_distance`** — how similar two consecutive screens must be to
  count as unchanged.
- **`server.batch_minutes`** / **`server.batch_wait_secs`** — how many minutes
  share one model call, and how long the first of them waits for the rest.
  One batch spans every device — a single (ts, device) timeline with
  simultaneous minutes marked — which is what lets the model correlate
  machines instead of judging each screen in isolation. A label appears up to
  `batch_wait_secs` after the minute it describes; the row itself is written
  immediately.
- **`server.endpoint`** — any OpenAI-compatible chat-completions URL. The
  intended setup is a local llama-swap (llama.cpp) instance on a machine with
  a GPU; the hosted OpenCode endpoint is the works-out-of-the-box fallback.
- **`server.max_tokens_per_minute`** — explicit output-token budget per minute
  in a batch. Unset, the budget is guessed from the model name (reasoning
  models get more) — a guess with nothing to go on when the "name" is a
  llama-swap alias like `time-vision`.
- **`server.json_schema`** — ask the endpoint to enforce the label schema via
  `response_format: {"type": "json_schema", ...}`. llama.cpp's llama-server
  compiles it to a grammar; hosted gateways mostly ignore it. Off by default,
  and the prompt describes the same shape in words either way.

Secrets come from the environment only, never the config file:
`TIME_INGEST_TOKEN` (both sides), and `TIME_API_KEY` (server) — now optional,
since a local llama-swap endpoint has no key; when unset, no Authorization
header is sent at all.

## Reclassifying history

Keeping the raw data is only worth anything if it can be re-read. `time
reclassify` re-runs the classifier over stored minutes — screenshots re-read
from `frames/`, same batching, same prompt — and compares the new answers
against the live labels:

```bash
time reclassify 7                                  # dry run, last 7 days
time reclassify 7 --device pc --limit 40           # a cheap first taste
time reclassify 30 --model qwen3-vl-32b --endpoint http://192.168.50.232:8000/v1/chat/completions
time reclassify 30 --run-id qwen32b-test --apply   # believe it: rewrite labels
```

By default nothing is touched: results land in the `minute_trial` table under
a run id (default `<model>-<YYYYMMDD-HHMM>`), and the run prints minutes sent,
agreement %, and the top category changes (`old → new: count`). `--apply` is
the destructive version that updates the live labels instead. `--model`
overrides both `model` and `model_text` — a backtest asks what one model would
have said. It runs where the server's data dir is (set `TIME_DATA_DIR` to
point at it), because that is where the database and the frames live.

## What you actually shipped

The minute table answers *where the day went*. It cannot answer *whether any of
it was worth anything* — an editor full of code looks the same whether you
shipped a feature or stared at it. So `time collect` reads the other half:

```bash
time collect            # last 30 days, posted to the server
time collect 7 --dry-run   # print it instead, to check the numbers
```

Two sources, kept apart in the `code_day` table because neither can replace the
other:

- **local git** (`git2`/libgit2) — commits, lines added and removed, files
  touched, per repository per day. The only place diffs exist, and the only
  place private and client repositories show up at all.
- **GitHub** (one GraphQL call) — pull requests opened and merged, issues
  opened and closed, reviews given. None of those leave a trace in a clone.

GitHub reports private work as a bare `restrictedContributionsCount` with no
repository, no day and no diff — 819 of a recent month's contributions here.
That number is exactly the gap the local scan exists to fill, so the two commit
counts are never added together.

Lines are counted per file, and a file that changed by more than 10k lines in
one commit is skipped along with lock files and vendored trees: one refresh of a
checked-in data dump is 650k lines, which is more than a month of real writing
and would flatten every other day in the chart to a single pixel.

Runs are idempotent — rows are replaced, not added — so it can run as often as
you like; the nightly timer below is the baseline, and the metrics timer
further down keeps today's numbers fresh. The home-manager module:

```nix
services.time-agent.collect = {
  enable = true;
  roots = [ "~/projects" ];
  githubUser = "your-login";
};
```

The scan runs where the repositories are, not where the server is, and posts to
`/v1/code` the same way the agent posts frames. The GitHub token comes from
`TIME_GITHUB_TOKEN`, or from `gh`'s own credentials if it is logged in — never
from the config file.

## How much of it was the agents

A screenshot of an editor looks the same whether you wrote the line or watched
one appear, so the minute table cannot separate the two. `time agents` counts
the other side from the tools' own records:

```bash
time agents             # last 14 days, posted to the server
time agents 3 --dry-run    # print it instead, to check the numbers
```

Three sources into `agent_day` and `agent_minute`, per tool per project per day:
**opencode** from its own SQLite database, **claude** and **codex** from the
JSONL transcripts under `~/.claude/projects` and `~/.codex/sessions`. Those two
are private on-disk formats with no stability promise — `~/.claude/history.jsonl`
silently stopped being written mid-2026 — so each parser is best-effort and a
tool whose format has moved can be dropped from `agent_tools` without a rebuild.

Token counts are never summed across tools: the three of them count cached input
three different ways. The dashboard reports elapsed agent time as *distinct
minutes*, not summed session minutes — several sessions overlap constantly, and
adding them up produces days longer than a day.

## Near-live metrics

`collect` and `agents` are retrospective; run only nightly, they leave the
dashboard saying "no commits recorded for this day" until 3am. The
home-manager module has a second timer that runs `time collect 2` and
`time agents 2` on a short interval instead:

```nix
services.time-agent.metrics = {
  enable = true;
  interval = "1m";   # the default; systemd OnUnitActiveSec, so runs never overlap
};
```

A one-minute cadence is affordable because both commands keep a scan cache in
the data dir (`code-scan-cache.json`, `agents-scan-cache.json`), keyed by
window length: `time collect` records each clone's `.git` mtime and skips a
repository group nobody has committed to; `time agents` records
`(mtime, size)` for every transcript file in the window and skips a tool whose
files are all unchanged — so the common run is a stat walk and two empty
posts. Skips are all-or-nothing per repository group / per tool, because day
totals are recomputed from every file and upserted whole: a skipped source
posts nothing and the server keeps its previous rows, which is correct, while
a half-scanned one would replace correct rows with undercounts. The cache is
committed only after the server accepts the rows — a failed post is rescanned,
not forgotten — and anything doubtful (no cache, a changed window, an
unreadable file, a failed parse) falls back to a full rescan.

## Cost

**Classification is now local and free.** The deployed config points at
llama-swap on the pc's GPU (a local Qwen VL model behind the `time-vision` /
`time-text` aliases), so a labelled minute costs electricity and nothing else,
counts against no allowance, and sends no pixels to any third party. The rest
of this section is what the hosted setup cost and why it is shaped the way it
is — still true whenever `endpoint` points back at a paid gateway, and the
batching it motivated is kept regardless (a run of minutes is also a better
prompt than an isolated screenshot).

`qwen3.6-plus` on the OpenCode Go plan ($0.50/$3.00 per M). The plan has **no
Batch API** — `/v1/batches` and `/v1/files` are 404 on that endpoint, so the
half-price 24-hour turnaround that OpenAI sells does not exist here. Batching
had to be done in the prompt instead.

Minutes go out in runs of `batch_minutes` rather than one at a time, which
matters because the system prompt is ~3.4k characters and was previously
re-sent with every single minute:

| per minute | one call each | 20 per call |
| --- | --- | --- |
| system prompt | ~925 tok | ~60 tok |
| the minute itself | ~60 tok | ~60 tok |
| 1024px screenshot | ~780 tok | ~780 tok |
| **input** | **~1765 tok** | **~900 tok** |

So roughly **43% off a minute with a screenshot** and **~70% off one without**
— a phone minute is nothing but the system prompt, which is why the phone was
the expensive device. Output is unchanged either way: the same fields have to
come back per minute.

Every call logs what it actually cost (`N minute(s), X in / Y out`), so the
real numbers are in `kubectl logs`, not in this table.

The remaining levers, in order of size:

- **`agent.width`.** The screenshot is ~85% of a batched minute's input.
  768px instead of 1024px is ~45% fewer image tokens. Not changed by default,
  because nobody has checked yet what it does to label quality.
- **The vision model.** `minimax-m3` is the other vision model on the plan at
  $0.30/$1.20, so ~63% cheaper than `qwen3.6-plus` on the same tokens. It emits
  `<think>` blocks; the parser now strips those, so it is a one-line config
  change — but again, unverified on real screens.
- **`server.model_text`**, already there: batches with no screenshot go to
  `deepseek-v4-flash` at $0.14/$0.28 rather than qwen's $0.50/$3.00 — 3.6x
  cheaper in, 10.7x cheaper out, on the majority of minutes.
- **The idle skip**, which is free and already there: a minute whose screen is
  unchanged from the last one is recorded without an API call at all.

## Data

SQLite on the server's PVC. The `minute` table is keyed by `(device, ts)` so
several machines can report the same minute without overwriting each other —
~220k rows/year, a few tens of megabytes. Around it, the raw-data archive:

- **`frames/<device>/<ts>.jpg`** in the data dir — every screenshot as
  ingested, referenced from `minute.image_path`. Kept forever by design; at
  50–100 KiB a minute this is the thing the 200Gi PVC is sized for.
- **`llm_call`** — one row per model call: which minutes went out, model,
  endpoint, token counts, and the raw reply text (or the error). The audit
  trail that `time reclassify` exists to exploit.
- **`minute_trial`** — dry-run reclassification results, per run id, kept
  apart from the live labels until a run earns `--apply`.

```sql
SELECT category, COUNT(DISTINCT ts)/60.0 AS hours FROM minute
WHERE ts > strftime('%s','now','-7 days')
GROUP BY category ORDER BY 2 DESC;
```

## Privacy

This screenshots your screen every minute and now **keeps those screenshots
forever** on the server. Worth being clear about:

- Retention is reversed from the original design, deliberately: screenshots
  (`frames/`) and raw model replies (`llm_call`) are kept so labels can be
  recomputed later with better models. There is no pruning. The server's disk
  is a minute-by-minute visual archive of your screens — treat access to it
  accordingly.
- What made this acceptable is that the model moved home: with the llama-swap
  endpoint on the LAN, no screenshot is shown to any third party. Pointing
  `endpoint` back at a hosted gateway re-introduces the third party for new
  minutes — the archive itself still never leaves the server either way.
- The blocklist is a hard gate, enforced client-side — blocked windows are
  never captured, so there is nothing to retain, and the window title isn't
  sent either.
- Browsing is recorded as a host and nothing more. Paths and query strings are
  the half of a URL that leaks, they never leave the agent's memory, and a
  blocklisted domain blocks the whole minute rather than just the domain field.
- If a hosted endpoint is used: never a `-free` model ID. That suffix is what
  marks the OpenCode plan's training exception; the paid IDs are
  zero-retention.
- Only the vision model can ever receive a screenshot. The text model is chosen
  from the payload, and any batch holding an image routes to the vision model.
- The agent's side is unchanged: pixels never touch the agent's disk, and the
  spool stores metadata only.
- The ingest route is LAN/VPN only.
- No key is in this repo. The database and any key file are gitignored.

## Not built yet

A nightly PDF report, typing/mouse metrics, speech detection, git line counts,
and a correction UI. (The local vision model, long the top of this list, is
built — it is the llama-swap setup above.) See [DESIGN.md](DESIGN.md) —
including why a phone client can't do screenshots at all on modern Android.
