# Time's UI: Day, Week, Month

Erik, 2026-10-23: "Where did my pie chart go? I want a pie chart so I can see
when I'm doing things in parallel on different devices, like watching YouTube
and working at the same time … different days and different weeks and
different months to compare … steps, the kilometres should obviously be more
standard."

The research behind this is in [ui-research.md](ui-research.md) (Timing,
RescueTime, Rize, ActivityWatch, Screen Time, Digital Wellbeing, Toggl,
Clockify, Apple Health, Google Fit, Fitbit). No tracker treats parallel use
across devices as a first-class number. Screen Time adds devices up, which
counts parallel time twice. ActivityWatch picks one device per minute by
priority. Time keeps one "winner" per minute and also records what the other
device was doing.

## What every period shows, in this order

1. **Navigation.** A Day | Week | Month segmented control. Under it,
   ‹ previous · period title · next › (next is disabled for the current
   period). A "Today", "This week" or "This month" pill appears when you are
   looking at another period. Everything is a plain link, so it works with no
   JavaScript and inside the phone app. Weeks start on Monday.
   - URLs: `/` and `/day/YYYY-MM-DD`, `/week[/date]`, `/month[/YYYY-MM]`.
2. **Hero.** The parallel donut, plus four numbers: tracked, work and study,
   on the phone, and in parallel.
   - Each number has its change against the previous period: "−14 min (−2%)
     vs the day before".
   - Weeks and months show daily averages, with the total under them. Their
     deltas compare averages, so a half-finished week is not reported as
     "down 50%".
3. **The parallel donut.**
   - Inner ring: the activity that had Erik's attention, one fixed hue per
     activity, in a fixed order. Slices of 8% or more carry their percentage.
   - Outer ring: for each slice, the part during which another device was
     also busy, coloured by what that device was doing. For example, a blue
     work slice with an amber stretch outside it means YouTube on the phone
     while working.
   - Centre: total tracked time and the time in parallel.
4. **In parallel.** The five biggest pairs in words, e.g. "work on the
   computer + video on the phone, 39 min", each with a two-colour bar.
5. **Steps** (health-app style).
   - A goal ring (default 10 000, `step_goal` in the config) that turns
     green at the goal.
   - Steps written out with a thin-space thousands separator, then km to one
     decimal.
   - The change against the previous period.
   - Bars: per hour for a day; per day for a week or month, with a dashed
     goal line, green on days the goal was reached, and "goal reached on N of
     M days".
6. **When.**
   - Day: hour by hour in two lanes, computers above and the phone below.
     Both lanes filled means two things at once.
   - Week and month: one stacked bar per day (click a bar to open that day),
     with a dashed line for the period's daily average and a dotted line for
     the previous period's.
   - Month only: a calendar. Each day is shaded by work and study, with the
     hours and thousands of steps written in.
7. **By activity.** Each activity's time, share and bar; how it splits
   between computer and phone; the daily average for weeks and months; and
   the change against the previous period.
8. **Day only, unchanged:** places and device signals; typing, voice and
   listening; AI; projects; code.

## Visual rules

- Dark is the default everywhere. Light is a choice: the ☀ button, or
  `?theme=light` on a link. It is remembered on the device.
- Phone width comes first: single-column cards. From 34 rem the hero puts
  the donut beside the numbers.
- One hue per activity in the donut, bars, lanes and lists (the validated
  palette). Grey is "other", dark grey is "unknown".
- Deltas are muted text, never red or green: more time is not good or bad in
  itself. Green only marks a goal reached.
- Durations read "5 h 26 min", with zero units left out.

## Data

- `MinuteLabel.background` was "video or music playing on another device".
  It is now "what another device was doing": media playing elsewhere first,
  otherwise another device in use. `background_device` says which device.
- Migration 4 added the column. `LOGIC_VERSION` 4 relabels all history on
  start, so past months gain parallel time too.
- Period totals come from minute labels and Health Connect step records
  (`views::summarise`). The previous period is summarised the same way for
  the comparison.
