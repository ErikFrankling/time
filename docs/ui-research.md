# UI research: how time trackers and health apps present data

Scope: design input for the Time redesign (server-rendered, dark, phone-width first).
Method: web search of official help pages, product pages and reviews. Items marked
(unverified) came from weak or old sources, or my knowledge of the apps, and were not
confirmed on a current official page. Several apps (Apple Health, Digital Wellbeing)
have no current official page describing the chart layout, so those rows lean on
secondary sources.

## 1. Time trackers

### RescueTime
- Navigation: Dashboard shows Day / Week / Month / Year as a period switch, with arrows
  beside the date to step one period back or forward, plus a date picker. The selected date
  persists across reports. A "Work Hours" vs "All Time" toggle filters the data.
  Sources: https://help.rescuetime.com/article/455-navigating-the-rescuetime-website ,
  https://help.rescuetime.com/article/461-understanding-your-data
- Score: Productivity Pulse, 0-100, a weighted average. Focus Work 100, Other Work 75,
  Neutral 50, Personal 25, Distracting 0. Help docs call it a trend to watch, not a number to max out.
  Source: https://help.rescuetime.com/article/457-your-first-insights
- Comparison: the daily dashboard shows a delta to the previous day (example: score 81, "16% down
  from the day before"). Whether week/month compare to the prior period is not documented in what I found.
- Charts: category/productivity-level bars, hour-by-hour bars for the day, most-used list.

### Rize
- Navigation: Day / Week / Month toggle buttons top left. Each view splits hours into focus, break and
  meeting time, plus projects and categories. Sources: https://rize.io/changelog/new-calendar-home-views ,
  https://www.rize.io/blog/weekly-and-monthly-dashboard-updates
- Dashboard redesign: categories are "the most important information"; tracked time is the common denominator
  for categories, focus time and projects. Source: https://rize.io/changelog/dashboard-redesign
- Score: focus score out of 100. Sessions (focus / meeting / break) are auto-created and open to a category
  breakdown and a rating. Source: https://docs.rize.io/sessions/overview
- Timeline: daily calendar view with toggleable columns. Week view compares scheduled vs actual time.
- Desktop only (no mobile), so no multi-device overlap view. Review: https://dhruvirzala.com/rize-review/

### Timing (macOS)
- Home is an Overview dashboard with a selectable date range you can scroll through; Review/Details were merged
  into one Activities screen in 2021.6.1, with a timeline of app icons and hover details.
  Sources: https://tidbits.com/2020/04/08/timing-2-helps-track-and-manage-your-working-hours/ ,
  https://www.macworld.com/article/230097/timing-2-0-review-mac-software-for-professionals-to-track-billable-time.html
- Productivity score from per-project ratings (Macworld).
- Multi-device: imports iPhone/iPad usage from Screen Time and shows it in the same timeline, but because
  the data comes from Screen Time exact cross-device overlap is not guaranteed. Timing Sync merges several Macs.
  Sources: https://timingapp.com/help/sync , https://www.imore.com/timing-app-get-major-update-multi-mac-sync
  I found no documented overlap-resolution UI (unverified either way).

### ActivityWatch
- Web UI: timeline of buckets, most-used apps per day, a bucket overview. Maintainers say richer
  visualisation is not a core priority. Source: https://docs.activitywatch.net/en/latest/features/user-interface.html
- Data model: one bucket per watcher, events with UTC timestamp and duration; buckets can overlap in time.
  Source: https://docs.activitywatch.net/en/latest/buckets-and-events.html
- Multi-device: the "canonical events" query merges devices with priority (one source wins where they overlap)
  rather than showing overlap. Source: https://docs.activitywatch.net/en/latest/examples/working-with-data.html
- Closest to a multi-lane view: the timeline shows one row per bucket, so concurrent activity is visible as stacked
  lanes. It is a developer view, not a polished one.

### Toggl Track (reports)
- Summary report: bar chart on top with one bar per day, week or month depending on range length; options
  Time / Billable % / Revenue; stacking by billable, member, client, project, task, tag (stacking is Premium).
  A pie/donut and a breakdown table sit beside or below.
  Sources: https://support.toggl.com/summary-report , https://support.toggl.com/category/reporting/
- Navigation: date range picker with left/right arrows (Left on "This Week" goes to "Last Week").
  No built-in vs-previous-period delta found. Timeline view is separate (calendar-style day/week).

### Clockify reports
- Summary report: bar chart with one bar per day in the range, billable (dark green) vs non-billable
  (light green) or per project colour; hover for exact numbers. Pie chart grouped by project, client, user,
  tag or date with sub-grouping. Source: https://clockify.me/help/reports/summary-report
- No previous-period comparison found. Weekly/monthly bar grouping requested on the forum, not shipped (as of what I found):
  https://forum.clockify.me/t/more-detailed-summary-report-charts/1755

## 2. Device wellbeing

### Apple Screen Time
- iPhone: Settings > Screen Time > See All Activity. A Week / Day segmented control. Week shows the last 7 days
  as bars with a dashed average line and a header "Daily Average" with up/down vs last week; Day shows hourly bars
  across the day. Below: Most Used, then Pickups (count and first app used) and Notifications.
  Sources: https://support.apple.com/en-lamr/108806 , https://www.igeeksblog.com/how-to-use-screen-time-on-iphone-ipad/
- Bars are stacked by app category (colour per category) with a category legend (my knowledge, unverified here).
- Mac: pop-up menu plus arrows to pick a day or week, a Devices filter; Pickups and Notifications are separate
  sidebar items. Source: https://support.apple.com/en-mz/guide/mac-help/mchlf2c0c770/mac
- Multi-device: the Devices selector lets you see one device or all devices together. Totals are summed, so
  parallel use on two devices counts twice; there is no overlap visualisation.
- Pickups and notifications are the only "frequency" charts; they are what people use for interruption analysis.

### Android Digital Wellbeing
- Dashboard: a ring/donut of today's app time at the top (Android 9 era), tap for details: screen time, times
  opened, notifications. Weekly bars with a Screen time / Notifications / Times opened dropdown.
  Sources: https://support.google.com/pixelphone/answer/9137850 , https://androidauthority.com/a-guide-to-googles-digital-wellbeing-3225195
  (3rd-party coverage from 2018; current layout not confirmed from an official page.)
- Day-by-day bars, today highlighted, tap a bar to drill into that day. No delta vs last week in the main chart.

## 3. Steps and distance (health apps)

### Apple Health / Fitness
- Steps page: D / W / M / 6M / Y segmented control (secondary source mentions Daily, Weekly, Monthly, Yearly).
  Beyond Day, the header shows the average per day; tap a bar to see that bar's value; "Show All Data" lists days.
  Health weeks run Sunday-Saturday, Fitness Monday-Sunday. Source: https://leancrew.com/all-this/2024/11/apple-health-trends/ ,
  https://discussions.apple.com/thread/251103164
- Fitness app has concentric Move / Exercise / Stand rings; goal ring is the iconic pattern (my knowledge).
- Highlights (unverified): "Compared to last week" style text cards and a Trends section with arrows.

### Google Fit
- Home had two rings: Heart Points (outer, green) and steps or Move Minutes (inner, blue); weekly Heart Points
  target of 150 shown below. A chart of the last 7 days of steps follows; default daily goal 10 000 steps, editable.
  Sources: https://9to5google.com/2018/08/22/hands-on-google-fit-redesign/ ,
  https://gadgetsandwearables.com/2020/04/08/google-fit-heart-points/ ,
  https://www.engadget.com/2018-08-21-google-fit-redesign-move-heart-points.html
- Journal view: day cards listing steps, distance, calories.

### Samsung Health
- Step tile on home with a progress ring; Trends screen with Days / Weeks / Months dropdown, showing the
  average daily steps for the period; scroll the graph sideways for earlier days; tap the date for a day;
  "Set target" under the three-dot menu. Distance is derived from steps times stride length.
  Source: https://www.samsung.com/us/support/answer/ANS00080925/
- Default target: older sources say 10 000 (Wikipedia); I could not confirm the current default from Samsung.

### Fitbit
- Default goal 10 000 steps per day. Dashboard tile shows count and progress ring; tapping it opens bars by day
  with goal line; the "weekly steps" figure is a rolling 7-day total (today plus previous 6).
  Sources: https://support.google.com/fitbit/answer/14236914 ,
  https://community.fitbit.com/t5/Get-Moving/What-is-the-average-steps-per-day/m-p/81625

### Steps patterns across the four apps
- Hero number = count with unit label, distance as a smaller companion (km or mi, one decimal).
- Goal = ring on day view, dashed goal line and filled/hollow bars in week view.
- Week view: 7 bars, today highlighted, bars that met goal in the accent colour, a dashed average line, header
  "Average 8 412" for the range.
- Month view: either 30 bars or a calendar heatmap (Fitbit/Samsung have calendar-style month browsing; unverified).
- Year/6M: bars aggregated to weeks or months.

## 4. Parallel / overlapping activity (multi-device)

Findings:
- Apple Screen Time: sums devices, no overlap handling. Timing: puts iPhone usage into the same timeline as
  Mac usage but does not clearly mark concurrent use. ActivityWatch: canonical query resolves overlap by priority (one
  source wins); raw buckets are separate timeline lanes. RescueTime, Rize, Toggl, Clockify: single-device or single
  running timer model; Toggl and Clockify block or discourage overlapping entries; both show overlaps only in a
  calendar view as side-by-side blocks (Toggl calendar, unverified).
- No tracker I found presents "phone used while computer active" as a first-class metric. The best existing analog is
  ActivityWatch's lane-per-bucket timeline, and calendar apps' side-by-side column layout for overlapping events.
- Conclusion: this is a gap. Time can be the first to do it well; nothing to copy directly, copy primitives
  (lane timelines, Venn-style exclusive/shared split).

## 5. What Time should copy (prioritised)

1. Navigation: a segmented control Day | Week | Month at the top (Apple Screen Time / Rize / RescueTime), with
   prev / next arrows flanking a centred period label ("Thu 23 Oct", "Week 43", "October 2026") and a "Today"
   button. Make it plain links (`?view=week&date=...`) so it stays server-rendered; add swipe left/right on
   the chart area as progressive enhancement. Arrows cannot go past today; disable the next arrow.
   Weeks start Monday.
2. Compare to previous period: one line under the hero number, "+12% vs last week" (day view: "vs yesterday",
   month: "vs September"). Green/red are not good/bad for time, so use neutral up/down arrows and
   muted text; only colour when a goal exists. On Week and Month charts draw the previous-period average as a
   dashed line, and the period's own daily average as a second labelled line (Screen Time pattern).
3. Chart set per view (exact types):
   - Day: hero total; donut by category (centre label = total); hour-by-hour bar chart (24 bars, stacked by
     category, as Screen Time Day); two-lane overlap timeline (see 4); category list with min and % bars.
   - Week: 7 stacked bars by category (Screen Time / Toggl), today highlighted, dashed average line, tap bar
     for day; category totals list with delta chip each; steps as 7 bars with goal line.
   - Month: calendar heatmap (one cell per day, single hue, 5 steps, legend) for total or steps; stacked weekly bars
     or per-day bars below; donut for the month; "best day / average / total" mini-stats.
4. Parallel activity: show the split three ways.
   - Hero: "Total 7 h 40 min" with "of which 2 h 05 min in parallel" (wall-clock vs summed device time is
     the key number; define it once).
   - Donut: outer ring = exclusive minutes by category; inner or sibling ring = parallel minutes; or an extra
     hatched "In parallel" slice. Prefer the two-ring donut (outer exclusive, inner parallel) with a legend; on narrow
     phones fall back to a single donut with a hatched "In parallel" slice and a table.
   - Timeline: two lanes (Computer, Phone) across the day, one block per session coloured by category, with
     the overlapping span highlighted by a shaded band across both lanes (ActivityWatch lane idea, calendar overlap idea).
     Tap a block for details. Provide a text table fallback for accessibility.
5. Steps: ring for the day, goal default 10 000 steps (Fitbit / Google Fit default), editable per user. Hero shows
   "7 412 steps" with "5.4 km" underneath (one decimal), ring turns the accent colour at 100% and shows a check; the
   ring may overflow past 100% as a second lap (Apple). Week view: 7 bars + goal line + "Average 8 412 / day";
   goal-met bars in accent, others muted. Month: calendar heatmap by percent of goal.
6. Number formatting: durations "5 h 26 min" (non-breaking spaces; omit zero units: "26 min", "2 h"); under a
   minute "<1 min"; hero uses large tabular numerals, units small and muted. Steps with a thin space
   thousands separator "10 000"; distance "5.4 km" (one decimal, comma or dot per locale). Percent deltas
   whole numbers, "+12% vs last week"; for durations in tight spaces "+38 min". Dates "Thu 23 Oct", 24 h clock.
7. Colour for categories: one hue per category, consistent across donut, bars, timeline and list; max 6-8
   categories plus grey "Other"; never rely on colour alone (label + pattern for the parallel slice).
   Dark mode (default): lower-saturation, lighter tones (about L 65-72%) on a near-black surface (#0f1115),
   chart grid at 8-12% white, text 90% / 60% white. Light mode: same hues darker (about L 40-50%) on white,
   grid at 8% black. Keep the same hue order in both modes. Use a single-hue ramp for heatmaps. Reserve red/green
   for goal state. Check 3:1 contrast against the surface for marks and 4.5:1 for text.
8. Productivity score (optional, later): RescueTime-style 0-100 weighted by category (productive 100 ... distracting 0),
   shown as a small number with a delta, not as a primary hero. Only after categories are reliable.
9. Phone-width layout: single column cards; sticky segmented control and period header; hero + delta first,
   then charts, then lists. Tap to drill (bar to day, category to its entries). Pickups-style counts (switches
   between devices) are a worthwhile extra card, borrowed from Screen Time.

## Source index
- https://help.rescuetime.com/article/455-navigating-the-rescuetime-website
- https://help.rescuetime.com/article/457-your-first-insights
- https://help.rescuetime.com/article/461-understanding-your-data
- https://rize.io/changelog/new-calendar-home-views
- https://rize.io/changelog/dashboard-redesign
- https://www.rize.io/blog/weekly-and-monthly-dashboard-updates
- https://docs.rize.io/sessions/overview
- https://tidbits.com/2020/04/08/timing-2-helps-track-and-manage-your-working-hours/
- https://www.macworld.com/article/230097/timing-2-0-review-mac-software-for-professionals-to-track-billable-time.html
- https://timingapp.com/help/sync
- https://docs.activitywatch.net/en/latest/features/user-interface.html
- https://docs.activitywatch.net/en/latest/buckets-and-events.html
- https://support.toggl.com/summary-report
- https://clockify.me/help/reports/summary-report
- https://support.apple.com/en-lamr/108806
- https://support.apple.com/en-mz/guide/mac-help/mchlf2c0c770/mac
- https://support.google.com/pixelphone/answer/9137850
- https://www.samsung.com/us/support/answer/ANS00080925/
- https://support.google.com/fitbit/answer/14236914
- https://9to5google.com/2018/08/22/hands-on-google-fit-redesign/
- https://leancrew.com/all-this/2024/11/apple-health-trends/
