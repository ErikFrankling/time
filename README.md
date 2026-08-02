# time

Track what I actually do, minute by minute, and get a breakdown at the end of
the day.

An agent on each machine screenshots the focused monitor every minute and posts
it to a server. The server asks a cheap multimodal model what's happening,
stores one row — category, project, and one concrete sentence — and **deletes
the screenshot**. Labels are kept forever, pixels are not.

The point is the chart, not the archive. This is deliberately not a screen
recorder.

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

The phone posts its whole backlog to `/v1/frames` as one array; `/v1/frame`
takes a single minute, which is all the desktop agent ever has.

The agent is deliberately dumb. It holds no secret and makes no billable calls,
so a laptop is never more than a capture device. Every decision that costs money
happens server-side, in one place, with one config.

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

## Deploying the server

Manifests live in the homelab repo under `kubernetes/homelab/apps/time.yaml`
and are reconciled by Flux. `deploy/time.yaml` here is the source of truth for
that file. The image is built by GitHub Actions and published to
`ghcr.io/erikfrankling/time:latest`.

**The secret is never in git.** Create it directly on the cluster:

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
  `other` rather than inventing a slice.
- **`server.idle_distance`** — how similar two consecutive screens must be to
  count as unchanged.

Secrets come from the environment only, never the config file: `TIME_API_KEY`
(server) and `TIME_INGEST_TOKEN` (both).

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

Run it nightly — commit timestamps are retrospective, so there is nothing to
gain from running it more often. The home-manager module has a timer for it:

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

## Cost

`qwen3.6-plus` on the OpenCode Go plan ($0.50/$3.00 per M), roughly 1400 tokens
in / 100 out per call, so about $0.001 a call. That lands around
**$9–18/month** depending on how much the idle skip catches — a
minute whose screen is unchanged from the last one is recorded without an API
call at all, which on a normal day is a large fraction of them.

## Data

One SQLite table on the server's PVC, keyed by `(device, ts)` so several
machines can report the same minute without overwriting each other. ~220k
rows/year, a few tens of megabytes.

```sql
SELECT category, COUNT(DISTINCT ts)/60.0 AS hours FROM minute
WHERE ts > strftime('%s','now','-7 days')
GROUP BY category ORDER BY 2 DESC;
```

## Privacy

This screenshots your screen and sends it to a third party. Worth being clear
about:

- The blocklist is a hard gate, enforced client-side — blocked windows are never
  captured, and the window title isn't sent either.
- Browsing is recorded as a host and nothing more. Paths and query strings are
  the half of a URL that leaks, they never leave the agent's memory, and a
  blocklisted domain blocks the whole minute rather than just the domain field.
- The model is pinned. Notably it is **not** DeepSeek V4 Flash, the one model on
  the Go plan documented as training on submitted data.
- No screenshot is ever written to disk, on either side. It exists in memory
  between capture and the model call, and that's it.
- The ingest route is LAN/VPN only.
- No key is in this repo. The database and any key file are gitignored.

## Not built yet

A nightly PDF report, a local vision model (free, private, no third party),
typing/mouse metrics, speech detection, git line counts, and a correction UI.
See [DESIGN.md](DESIGN.md) — including why a phone client can't do screenshots
at all on modern Android.
