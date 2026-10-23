# time

Measures what Erik does on his devices: typing, voice, listening and what is
on screen, plus tokens and code. It classifies every minute with rules (no
LLM, no GPU), shows the day, the week and the system's own health on a
website that is also the phone app, and nudges him when an hour slips away.

This is v2, on branch `v2`. v1 (an LLM classifier, switched off since August)
is in `legacy/` and is what `main` still deploys. Design, decisions, open
questions and the go-live runbook: [PROPOSAL.md](PROPOSAL.md).

## Shape

```
pc / laptop: time-agent (screen, input, audio, browser tab)
             time-collect (git commits, GitHub PRs and reviews)
             T3 Code, dictation (planned, M7)            phone: Android app
        │ OTLP/HTTP JSON, one event per fact                │
        └───────────► gateway Alloy (homelab) ◄─────────────┘
                         │
                         ▼
               time-server: events (SQLite) → rules → one label a minute
               website: /  /week  /inbox  /status  /app ; /metrics, /healthz
```

Everything stored is an event with a stable id; minutes are derived and
recomputed whenever the rules (or the labelling code) change.

## Crates

| crate | what |
|---|---|
| `time-core` | event types (their serde names are the OTel attribute keys), the OTLP JSON codec, label enums, the rule table, the cross-device timeline, the nudge rule |
| `time-server` | ingest, store, classifier loop, website, status, `/metrics`, nudges, APK serving, v1 import |
| `time-agent` | the desktop agent: Hyprland IPC, evdev, MPRIS, aw-watcher-web; spool on disk; `--probe` |
| `time-collect` | commits from local clones, PRs and reviews from GitHub |
| `time-sim` | the simulated world for end-to-end tests: scripted days, fake Hyprland, simulated phone |

`config/rules.toml` is the rule table. Answers in `/inbox` are added to
`rules.local.toml` in the server's data directory.

## Running

```sh
time-server serve server.toml          # data_dir, devices, [nudge]
time-agent agent.toml                  # device, endpoint, spool_dir
time-agent --probe agent.toml          # what the agent can see right now
time-collect collect.toml              # once; a timer runs it every 15 min
time-sim serve crates/time-sim/scenarios/morning.toml   # a scripted day to look at
time-server import-v1 server.toml /path/to/v1/time.db   # bring v1's history in
```

Nix: `packages.time-v2` builds all binaries; `homeManagerModules.agent-v2`
runs the agent and the collector on a desktop.

Secrets only from the environment: `TIME_OTLP_TOKEN` (devices to the
gateway), `TIME_INGEST_TOKEN` (optional, guards `/v1/logs` and `/v1/focus`),
`NOTIFY_URL`/`NOTIFY_TOKEN` (nudges to the phone through the homelab notify-gateway, secret `time-notify`),
`TIME_AGENT_WEBHOOK` (nudges to the personal agent), `TIME_GITHUB_TOKEN`
(else the `gh` login).

## Testing

```sh
scripts/remote-cargo.sh test --workspace    # on the homelab builder, not the pc
```

- Unit tests per crate; the scripted days in `crates/time-sim/scenarios/` run
  the real server and agent on a virtual clock (`crates/time-sim/tests/e2e.rs`).
- `fixtures/otlp/phone-batch.json` is the contract between the Android app
  and the server: the Kotlin encoder must produce it, the Rust decoder must
  read it.
- CI (`.github/workflows/ci.yml`, every branch except `main`): fmt, clippy,
  tests; Android unit tests and build; an emulator that runs the real app
  against the real server and checks its heartbeat arrives.

## For agents: MCP and the hourly line

`POST /mcp` is an MCP server (Streamable HTTP, stateless, read-only) with three
tools for Erik's personal agent: `time_now` (what he is doing right now and in
the last 5 minutes), `time_summary` (a window like `today` or `3h`: minutes per
category, top apps and sites, productive / unproductive / neutral totals) and
`time_timeline` (the window as blocks). Each answers with one paragraph and the
same facts as compact JSON. With `TIME_MCP_TOKEN` set in the server's
environment, `/mcp` requires `Authorization: Bearer <token>`; the route is
lan-only on top of that. Live at `https://time.erikfrankling.duckdns.org/mcp`.

The same data as plain HTTP for scripts (open, like `/api/day`):
`/api/now`, `/api/summary?window=3h`, `/api/timeline?window=3h`, and
`/api/summary/line`, the last hour in one text line with what he is doing now, for the hourly review.

Which activities count as unproductive is `[mcp]` in `config.toml`; everything
not listed is neutral:

```toml
[mcp]
productive = ["work", "study"]
unproductive = ["video", "social"]   # games are neutral, by Erik's choice
```
