#!/usr/bin/env bash
# Is the classifier producing usable labels, or burning the GPU on garbage?
#
# Written after 2026-08-17, when the model spent every batch's whole token
# budget inside one label's `tags` and nothing noticed for six hours: the pod
# logged "no JSON in model output", the rows stayed pending, and the only
# outward sign was a hot room. The pod log says a batch failed; it does not say
# the answers have quietly turned to mush. These three numbers do.
#
#   usage: scripts/llm-health.sh [hours]      (default 12)
set -euo pipefail

HOURS="${1:-12}"
NS="${TIME_NAMESPACE:-homelab}"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

POD="$(kubectl -n "$NS" get pods -o name | grep -oE 'time-[0-9a-f]+-[a-z0-9]+' | head -1)"
[ -n "$POD" ] || { echo "no time pod in namespace $NS" >&2; exit 1; }
# Read-only copy: sqlite3 is not in the image, and querying the live file from
# outside the pod would race the classifier's writes.
kubectl -n "$NS" cp "$POD:/data/time.db" "$TMP/t.db" >/dev/null 2>&1

sqlite3 -header -column "$TMP/t.db" <<SQL
.print '== calls (tok_per_min is the one to watch: healthy is under ~120) =='
select substr(datetime(created,'unixepoch','localtime'),1,13)||'h' hour,
       count(*) calls, sum(error is not null) failed, sum(n) minutes,
       sum(completion_tokens) out_tokens,
       round(1.0*sum(completion_tokens)/nullif(sum(n),0)) tok_per_min
from llm_call where created > strftime('%s','now')-${HOURS}*3600
group by 1 order by 1;

.print ''
.print '== babble: tags per labelled minute (healthy 1-4; >10 means runaway) =='
select substr(datetime(ts,'unixepoch','localtime'),1,13)||'h' hour,
       count(*) labelled,
       round(avg(length(tags)-length(replace(tags,',',''))+1),1) avg_tags,
       max(length(tags)-length(replace(tags,',',''))+1) max_tags
from minute where classified=1 and tags is not null and tags<>''
  and ts > strftime('%s','now')-${HOURS}*3600
group by 1 order by 1;

.print ''
.print '== backlog (given_up = hit MAX_ATTEMPTS, will never self-heal) =='
select date(ts,'unixepoch','localtime') day, device, count(*) pending,
       sum(attempts>=3) given_up
from minute where pending=1 group by 1,2 order by 1,2;
SQL

# One line a person can act on, so this can be cron'd and only read when it
# complains. The tag average is the earliest signal -- it degrades a full batch
# before the first "no JSON" line appears.
sqlite3 -noheader -separator ' ' "$TMP/t.db" <<SQL | while read -r avg fails; do
select coalesce(round(avg(length(tags)-length(replace(tags,',',''))+1),1),0),
       (select count(*) from llm_call
         where error is not null and created > strftime('%s','now')-${HOURS}*3600)
from minute where classified=1 and tags is not null and tags<>''
  and ts > strftime('%s','now')-${HOURS}*3600;
SQL
  echo
  if   awk "BEGIN{exit !($avg > 10)}"; then echo "VERDICT: BROKEN - avg $avg tags/minute, the model is babbling"
  elif [ "$fails" -gt 3 ];             then echo "VERDICT: DEGRADED - $fails failed calls in ${HOURS}h"
  else                                      echo "VERDICT: OK - avg $avg tags/minute, $fails failed calls"
  fi
done

echo
rocm-smi --showuse --showpower --showtemp 2>/dev/null \
  | grep -E 'GPU\[0\].*(use \(%\)|Power \(W\)|junction)' || true
