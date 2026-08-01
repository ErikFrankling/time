# time

Track what I actually do, minute by minute, and get a breakdown at the end of
the day.

Every minute it screenshots the focused monitor, sends it to a cheap multimodal
model along with the active window and the previous minute's label, and stores
one row: a category, a project, and one concrete sentence about what was
happening. **The screenshot is deleted immediately** — it never touches disk.
Labels are kept forever, pixels are not.

The point is the chart, not the archive. This is deliberately not a screen
recorder.

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

## Running it

```bash
nix develop --command cargo run --release -- run
```

First run writes `~/.config/time/config.toml`. Put an
[OpenCode Zen](https://opencode.ai/docs/zen/) key in `~/.config/time/api-key`
(or set `TIME_API_KEY`), then open <http://127.0.0.1:7373>.

| Command | |
|---|---|
| `time run` | capture + classify every minute, serve the UI |
| `time serve` | UI only |
| `time once` | classify a single minute and print it — use this to check labels |
| `time config` | print config, key, and database paths |

## Configuration

`~/.config/time/config.toml`:

- **`categories`** — the list that becomes the chart. `idle` and `other` always
  render grey; the rest get one of 8 colourblind-checked colours in list order.
  Keep it to 8 real categories. Off-list answers from the model fold into
  `other` rather than inventing a slice.
- **`blocklist`** — substring match on window class and title. Matching windows
  are never screenshotted and nothing leaves the machine. Password managers and
  Signal are in there by default; add anything else you'd rather not send to a
  third party.
- **`width`** — downscale before sending. The cost dial.
- **`idle_distance`** — how similar two consecutive screens must be to count as
  unchanged.

## Cost

`mimo-v2-omni` on the OpenCode Go plan, roughly 1400 tokens in / 100 out per
call. About **$7–14/month** depending on how much the idle skip catches — a
minute whose screen is unchanged from the last one is recorded without an API
call at all, which on a normal day is a large fraction of them.

## Data

One SQLite table at `~/.local/share/time/time.db`. ~220k rows/year, a few tens
of megabytes. `phash` is an 8-byte difference hash used for the idle check.

```sql
SELECT category, COUNT(*)/60.0 AS hours FROM minute
WHERE ts > strftime('%s','now','-7 days')
GROUP BY category ORDER BY 2 DESC;
```

## Privacy

This screenshots your screen and sends it to a third party. Worth being clear
about:

- The blocklist is a hard gate — blocked windows are never captured at all.
- The model is pinned. Notably it is **not** DeepSeek V4 Flash, the one model on
  the Go plan documented as training on submitted data.
- No screenshot is ever written to disk. It exists in memory between capture and
  the API call, and that's it.
- No key is stored in this repo, and the database and any key file are
  gitignored.

## Not built yet

Second machine + a server to collect them, a nightly PDF report, a local vision
model on llama-swap (free, private, no third party), typing/mouse metrics,
speech detection, git line counts, and a correction UI. See
[DESIGN.md](DESIGN.md) — including why a phone client can't do screenshots at
all on modern Android.
