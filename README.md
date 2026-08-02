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
  no API key                           yes → model call → category/project/detail
  no database                        SQLite on a PVC
  no model calls                     serves the UI
```

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
| `time config` | print config and database paths |

## Configuration

`~/.config/time/config.toml` has an `[agent]` and a `[server]` section; each
side reads only its own, so the same file works everywhere.

- **`agent.blocklist`** — substring match on window class and title. Enforced on
  the client, so a matching screen is never even encoded, let alone sent.
  Password managers and Signal are in there by default.
- **`agent.width`** — downscale before sending. The cost dial.
- **`server.categories`** — the list that becomes the chart. `idle` and `other`
  always render grey; the rest get one of 8 colourblind-checked colours in list
  order. Keep it to 8 real categories. Off-list answers from the model fold into
  `other` rather than inventing a slice.
- **`server.idle_distance`** — how similar two consecutive screens must be to
  count as unchanged.

Secrets come from the environment only, never the config file: `TIME_API_KEY`
(server) and `TIME_INGEST_TOKEN` (both).

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
