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
             time-collect (git commits, GitHub commits, PRs and reviews)
             time-collect (every agent's tokens per hour, from T3 Code's usage scan and OpenCode)
             time-collect (T3 Code; speak read-aloud and hotkey dictation)   phone: Android app
        │ OTLP/HTTP JSON, one event per fact                │
        └───────────► gateway Alloy (homelab) ◄─────────────┘
                         │
                         ▼
               time-server: events (SQLite) → rules → one label a minute
               website: /  /week  /month  /inbox  /status  /app ; /metrics, /healthz
               api: /api/day/<date>, /api/interactivity/<date> (keyboard and mouse per 10 min),
                    /api/work (commits, lines, tokens and their API-price equivalent)
```

Everything stored is an event with a stable id; minutes are derived and
recomputed whenever the rules (or the labelling code) change.

## Crates

| crate | what |
|---|---|
| `time-core` | event types (their serde names are the OTel attribute keys), the OTLP JSON codec, label enums, the rule table, the cross-device timeline, the nudge rule |
| `time-server` | ingest, store, classifier loop, website, status, `/metrics`, nudges, APK serving, v1 import |
| `time-agent` | the desktop agent: Hyprland IPC, evdev, MPRIS, aw-watcher-web; spool on disk; `--probe` |
| `time-collect` | commits from local clones and GitHub (Erik's own or an agent's), PRs and reviews, T3 Code activity, every agent's tokens per hour priced at API list prices |
| `time-sim` | the simulated world for end-to-end tests: scripted days, fake Hyprland, simulated phone |

`config/rules.toml` is the rule table. Answers in `/inbox` are added to
`rules.local.toml` in the server's data directory.

## Running

```sh
time-server serve server.toml          # data_dir, devices, [nudge]
time-agent agent.toml                  # device, endpoint, spool_dir
time-agent --probe agent.toml          # what the agent can see right now
time-collect collect.toml              # once; a timer runs it every 15 min
time-collect collect.toml --days 4000 # backfill; --dry prints the events instead
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

## Code and AI

Day, Week and Month show what was shipped and what the agents used, per hour
of a day or per day of a week or month, under a line like "312 commits ·
+18k/−6k lines · 42M tokens ≈ $310 this week":

- **Commits and lines** (`github.commit`): Erik's commits in every clone under
  `~/projects` and `~/.dotfiles`, and the ones only GitHub has (commit
  search). A commit is an agent's when a `Co-Authored-By` trailer names one,
  or an agent session was working in the same repository from 5 minutes
  before to 2 minutes after it; else it is Erik's own. One commit across
  clones and rebases (identity: repository, author time and e-mail, subject).
  Lines leave out lockfiles, generated, vendored and data files, files over
  3k lines and commits over 30k lines; renames are not rewrites.
- **Tokens** (`llm.usage.hour`, one per agent session, model and hour):
  input, cache reads and writes, and output, from T3 Code's usage scan of the
  Claude (both accounts), Codex and Grok transcripts (and, for what T3 has not
  scanned yet, the transcripts themselves from where its scan stopped) and from OpenCode's
  database, priced at LiteLLM list prices as T3's usage page does: an
  equivalent at API prices, since most of it is on subscriptions. Per
  account, model, project and T3 thread.

Both are replaced when the collector sends them again (an hour of usage
grows; a commit learns whose it is). `/api/work?period=week&date=YYYY-MM-DD`
has the same as JSON, and `time_summary` has it as `code_and_ai`.

Which activities count as unproductive is `[mcp]` in `config.toml`; everything
not listed is neutral:

```toml
[mcp]
productive = ["work", "study"]
unproductive = ["video", "social"]   # games are neutral, by Erik's choice
```

## Sleep and gaps: what is derived on top of the minutes

The minutes stay as the rules made them (`?raw=1` on any day, week or month
shows them). On top, `crates/time-server/src/derive.rs` adds, every five
minutes:

- **Gaps filled.** A minute with no key, pointer or phone use is away by the
  presence rule, but reading T3 Code for three minutes between two bursts of
  typing is still T3 Code. `time_core::fill` fills a gap shorter than the
  quarter hour of the presence rule when the same window stayed up and Erik
  came back to a device; ambiguous gaps (window changed, or nothing came
  after) go to the decision model two hours at a time, as five-minute
  summaries: "until when was he still doing this, from when the next thing?"
  Stored in `fill`, shown everywhere by default, listed with reasons under
  *Filled gaps* on a day, and as spans at `/api/spans/<date>`.
- **Sleep.** Per night (18:00 to 18:00), `time_core::sleep` finds the longest
  stretch with no human use of any device, bridged over blips under 10
  minutes; three hours after he woke, the decision model confirms or adjusts
  it from ten-minute summaries of every signal (devices, input, idle resets
  without input, phone unlocks and lit-while-locked, steps, network, sound,
  agent prompts, alarms) and answers `{fell_asleep, woke_up, interruptions,
  confidence, reason}`. Stored in `sleep`; on the clock and rings as the
  Sleep track, on Day, Week and Month, at `/api/sleep`, and in
  `time_summary`.
- **Browser tabs.** In a browser Erik is nearly always working or studying, so
  "browsing" is only for what nothing says anything about. The rules know his
  projects and sites (Naia Works, Neptune, NaiaClaw, Teams, Outlook and Azure
  are work; KTH, Canvas, TimeEdit, course codes and arXiv are study; the
  homelab, Time, T3 Code, x-graph and the dotfiles are work tagged with the
  project). A tab no rule settles is caught by an `ask_model = true` rule
  and put to the decision model (`tabs.rs`), 40 tabs at a time, once per
  distinct title, with the `[profile]` and the `[[project]]` descriptions in
  `rules.toml` as its knowledge. Verdicts are in `tab_verdict`, appear as
  rule `model.tab`, and relabel history when they arrive; questions are at
  `/api/decisions?kind=tabs`. To teach it something, edit `[profile]` or a
  project's `about` in `rules.toml` (or add rules and projects in the data
  directory's `rules.local.toml`); a tab it got wrong is fixed with a rule.
- **Not Erik.** An idle timer reset with no key or pointer counted (agents on
  the AGENT-1 seat, the 06:00 update waking a screen) is not presence any
  more; a phone counts only unlocked with the screen on.
- **Alarms.** The phone app reports the next alarm clock
  (`alarm.next_ms` on `time.phone.state`); it moving on after its time came
  is the alarm being turned off.

The decision model is `[decide]` in `config.toml` (default: OpenCode Go,
`deepseek-v4.1-flash`, `reasoning_effort = "low"`, key in
`TIME_DECIDE_KEY`). Every question and answer is in the `decision` table and
at `/api/decisions?kind=sleep|gaps&key=...`; the same question is answered
from there. `time-server derive <config.toml>` runs it once by hand.
