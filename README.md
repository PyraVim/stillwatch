# stillwatch

[![ci](https://github.com/PyraVim/stillwatch/actions/workflows/ci.yml/badge.svg)](https://github.com/PyraVim/stillwatch/actions/workflows/ci.yml)

**A watchdog for things that run without you.**

Not "is the process up." That's the least interesting failure, and the one you'd
notice anyway. `stillwatch` watches the loop from inside it. A scraper whose
worker thread died at 3am is still a running process, still answering its health
check, still using memory. It just stopped beating.

---

## What it does today

**Heartbeats.** One HTTP endpoint. Your job posts to it each time round its loop:

```bash
curl -fsS -X POST localhost:9111/beat/nightly-sync
```

If the beats stop for longer than you said they should, you get a message saying
what happened, what *isn't* wrong, and what it probably means.

The beat can carry more, and each part is judged separately:

```bash
curl -fsS -X POST localhost:9111/beat/nightly-sync \
  -H 'content-type: application/json' \
  -d '{"worked":true,"data_ts":1724500000,"counters":{"rows_read":8400,"rows_written":8400}}'
```

* `worked` says whether the loop actually did something, as opposed to merely
  running. Catches the job that's up, looping, and producing nothing.
* `data_ts` says how fresh the data it acted on was. Catches the job reporting in
  punctually about a source that froze nine minutes ago.
* `counters` are yours to name. Any two of them can become a rule, which catches
  the scraper fetching fine and parsing nothing.

**Dependency probes.** stillwatch polls the things your jobs depend on and
watches their latency against their *own* recent normal. A dependency that has
always taken 400ms is fine; one that took 90ms an hour ago is not. This catches
the API that still returns 200 on every request and now takes 1.4s.

**Watching from outside.** If you can't modify the job, point it at a pidfile, a
log that should still be moving, or an output file that should still be
appearing.

You get one message per incident rather than one per evaluation cycle, and an
all-clear with the duration when it ends. Configure an audit trail and every
incident is recorded, along with the times stillwatch itself wasn't running.

Three commands:

```bash
stillwatch --config stillwatch.toml     # watch
stillwatch learn --for 6h               # measure thresholds instead of guessing
stillwatch report --since 7d            # what happened, and what it didn't see
```

---

## Why this exists

I spent fourteen years in FDA-regulated manufacturing as the escalation point for
automated systems. Every one of them had alarm handling, deviation detection, and
an audit trail, because running unattended without those was unthinkable. If a
system ran wrong for six hours, you didn't just fix it. You wrote up why nobody
knew for six hours, and that document had to survive an audit.

Then I started building automation outside that world and found almost none of
that exists. People write the thing that does the work and hope. Where there is
monitoring, it answers "is the process alive," which is the single failure mode
that doesn't need monitoring to detect.

The failures worth catching are the ones where the job is up and wrong:

| Failure | Why ordinary monitoring misses it |
|-|-|
| **Alive but idle-dead** | Process up, loop spinning, stopped doing anything. Looks perfectly healthy. |
| **Acting on stale data** | The feed froze. Everything downstream keeps working confidently on numbers from nine minutes ago. |
| **Working but not landing** | Every attempt fails. Process healthy, dependencies healthy, output zero. |
| **Degraded, not down** | An API that answered in 90ms now takes 1.4s. Still 200 OK. Every check passes. Everything gets slower and nobody is told. |

All four are built.

---

## Alive is not the same as working

This is the distinction most monitoring gets wrong, and it's why generic uptime
tools are useless for anything with irregular work.

A job can legitimately do nothing for six hours because there was nothing to do.
That's healthy. The same silence with a crashed loop is an emergency, and from
the outside the two look identical.

So heartbeats carry two separate signals. `alive` means the loop ran, expected on
a tight interval, and missing means dead. `worked` means it actually did
something, expected irregularly, and a long silence means look into it rather
than wake someone up.

Both are evaluated. stillwatch refuses to confuse them in two ways.

A job that declares no `[job.alive]` block is never judged late, however long it
stays quiet. A nightly sync does not inherit a scraper's cadence.

And while a job's liveness rule is being satisfied, a quiet `worked` signal is
capped at a warning however long it runs. A job that is provably alive and simply
has nothing to do is not a page. The cap applies only when something is actually
vouching for the loop: a job with no liveness rule has nothing standing behind
it, so its `critical_after` means what it says.

There are tests with both those sentences on them.

---

## Quick start

```bash
stillwatch --config stillwatch.toml
```

Then, from your job. Any language, no client library, no dependency:

```bash
curl -fsS -X POST localhost:9111/beat/nightly-sync
```

That's a valid heartbeat. A bare POST with no body and no `content-type` is the
whole protocol. Everything beyond it is optional:

```bash
curl -fsS -X POST localhost:9111/beat/nightly-sync \
  -H 'content-type: application/json' \
  -d '{"worked":true,"data_ts":1724500000,"counters":{"rows_read":8400,"rows_written":8400}}'
```

```python
# python, still no dependency worth naming
requests.post("http://localhost:9111/beat/scraper",
              json={"worked": True, "counters": {"fetched": 120, "parsed": 118}})
```

Only an explicit `worked: true` marks work. A bare beat says the loop ran, not
that it accomplished anything, and that distinction is the whole point.

The endpoint answers `404` when no job by that name is configured. A typo in the
URL must not leave the real job silently unwatched, so it's refused and logged
rather than quietly accepted. It answers `400` when the body isn't valid JSON, or
when a counter is negative or not a finite number: a `NaN` compares false against
everything, so one of them would silently stop a ratio rule firing for good.
Neither counts as a heartbeat.

---

## Configure

```toml
listen = "127.0.0.1:9111"

[notify.telegram]
token   = "${TELEGRAM_TOKEN}"
chat_id = "${TELEGRAM_CHAT}"

# --- a scraper that should run continuously ---
[[job]]
name = "product-scraper"

  [job.alive]
  expect_every   = "60s"     # how often the loop says it ran
  warn_after     = "5m"      # optional, defaults to 5x expect_every
  critical_after = "15m"     # optional, defaults to 15x expect_every

  [job.worked]
  warn_after     = "2h"      # up and looping, but accomplishing nothing
  critical_after = "6h"      # capped at a warning while [job.alive] is satisfied

  [job.freshness]
  warn_after     = "10m"     # measured from data_ts, not from when the beat arrived
  critical_after = "30m"

  [[job.ratio]]
  name        = "parse rate"
  numerator   = "parsed"
  denominator = "fetched"
  window      = "1h"
  min         = 0.9
  min_sample  = 50           # below this the rule reports as unjudged, not passing
  message     = "fetching fine, parsing broken — source markup likely changed"

# --- a nightly job that should be quiet most of the day ---
# No [job.alive] block, so it is never judged late. Nothing vouches for its loop
# either, so its worked thresholds are not capped.
[[job]]
name = "nightly-sync"

  [job.worked]
  warn_after     = "26h"     # missed one run
  critical_after = "50h"     # missed two

# --- a dependency, probed directly ---
[[check]]
name     = "vendor-api"
type     = "http"                          # or "jsonrpc", which also needs `method`
url      = "https://api.vendor.com/health"
interval = "30s"
timeout  = "3s"

  [check.degradation]
  baseline_window   = "1h"
  warn_multiple     = 3.0                  # p90 three times its own recent normal
  critical_multiple = 8.0
  absolute_ceiling  = "2s"                 # unacceptable regardless of the baseline
```

A minimal working config is four lines. Everything but job names has a default:
`listen` falls back to `127.0.0.1:9111`, and both alive thresholds derive from
`expect_every` if you leave them out.

`warn_after` must be longer than `expect_every`, or it would fire in the ordinary
gap between two beats. That's checked at startup, not discovered at 3am.

**Secrets.** `${VAR}` is expanded from the environment in any value, so no secret
has to sit on disk. An unset variable is a startup error naming the variable,
never an empty string that fails quietly later. Interpolation happens over the
parsed TOML rather than the raw text, so a secret containing a quote can't
rewrite the file around it.

**Containers.** Three values can also be set directly, for deploys with no
writable config:

```
STILLWATCH_LISTEN
STILLWATCH_NOTIFY_TELEGRAM_TOKEN
STILLWATCH_NOTIFY_TELEGRAM_CHAT_ID
```

Uppercase the dotted path and replace `.` with `_`. These win over both the file
and any `${VAR}` in it. Per-job values are deliberately not overridable this way.
Array entries have no stable key to name them by, and inventing an indexing
scheme would be a second config language.

`$STILLWATCH_CONFIG` sets the config path; it falls back to `./stillwatch.toml`.

A key stillwatch doesn't recognise parses without error and logs a warning naming
it. A line that does nothing should never do so quietly.

See [`stillwatch.toml.example`](stillwatch.toml.example) for the annotated version.

---

## Not judged yet is not the same as fine

The most comfortable way for a monitoring tool to be wrong is to stay quiet for a
reason that has nothing to do with health. Every rule here can be in a third
state, *not judging anything*, and it is never reported as passing.

* A ratio below its `min_sample` has no verdict. Twenty fetches and zero parses
  is not evidence of a broken parser. It isn't evidence of anything.
* A job that has never sent `data_ts` is not fresh. There's no data age to
  measure.
* A dependency check without enough probes has nothing to compare against.

`min_sample` must be at least 1, which is also what makes a ratio safe to
compute: the rule is skipped as unjudged long before the denominator could reach
zero. A scraper that fetched nothing has no parse rate, and stillwatch says so
rather than reporting 0%.

None of these page. They aren't incidents. They're logged when they change, so
you can see at a glance which of your rules are doing something.

Two cases do get a message, because they never resolve on their own and are
indistinguishable from health from outside. One is a `[job.freshness]` block
where beats keep arriving and none has ever carried `data_ts`. The other is a
ratio naming a counter no beat has ever sent, which is almost always a typo:

```
⚠️  clients-etl — write rate is configured against a counter that never arrives
    412 beats in 6h, not one carrying "rows_reed"
    nothing has failed this rule · it has never been able to run
    → check the counter name against what the job actually sends; as it stands this rule can never fire
```

**Counters are per beat, not running totals.** Each beat reports what happened
that time round the loop, and stillwatch sums across the window. A job sending
cumulative lifetime totals will produce meaningless sums.

---

## One dead job is one message

A job whose loop has stopped will also stop doing work, stop refreshing its data,
and stop moving its counters. Reporting all four would be four messages about one
fact, with the one that explains the rest buried among them.

So while liveness is failing, that job's other rules are suppressed. An incident
suppressed this way is held open rather than resolved. A collapsed parse rate on
a job that has since died did not get better, and saying so would be precisely
the confidently-wrong message this tool exists to avoid.

A job that is alive and healthy is different. There, each finding is genuinely
independent, and each gets its own message and its own all-clear.

---

## Watching something you can't modify

Nobody adds code to a job before they trust the tool, and a consultant can't
modify a client's system before being hired. Passive mode watches from outside.
No heartbeats, nothing to change:

```toml
[[job]]
name = "clients-etl"
mode = "passive"

  [job.process]
  pidfile      = "/var/run/etl.pid"
  absent_after = "60s"                 # a restart briefly removes the pidfile

  [job.log]
  path        = "/var/log/etl.log"
  stale_after = "10m"
  error_regex = "(?i)(traceback|fatal|failed to write)"

  [job.artifact]
  path        = "/data/exports/daily.csv"
  stale_after = "26h"
  min_bytes   = 1024                   # a fresh but empty export is the failure
```

**These signals are weaker than a heartbeat, and the alerts say so inline** rather
than leaving it here, because nobody reads a README at three in the morning. A
pidfile says a process id exists. A log says bytes were written. An artifact says
a file has a size. None of them says the job did its work.

The one exception is a line matching `error_regex`, which outranks everything
else in this section. That's the job's own words rather than an inference about
it, and the alert says that too.

Distinctions the tool keeps, because each sends you somewhere different:

* A log that has never existed means a wrong path in the config. One that existed
  and stopped moving means a stuck job.
* A pidfile absent since stillwatch started might be a dead job, or one not
  started yet. The alert says both rather than guessing.
* An artifact that is fresh but nearly empty is reported as empty rather than
  stale. Its age is beside the point; that's the run that exited zero and wrote
  nothing.

### Log rotation

Tail a log by holding its handle and a rotation leaves you reading a dead inode
forever, so the log looks permanently stale while the job is fine. That's
precisely the failure this tool exists to catch, and shipping it inside the tool
would be embarrassing.

stillwatch stats the path instead of holding a handle, and detects replacement two
independent ways: file identity (the inode on Unix, the file index on Windows)
and a size that went backwards. Either one catches `logrotate`'s
rename-and-recreate. The second independently catches `copytruncate`.

Two things it can't do:

* **PID reuse.** A pidfile whose number has been recycled by an unrelated process
  reads as healthy. There's no portable fix. Treat `[job.process]` as "probably
  up", which is why it's the weakest of the three.
* **A writer that keeps the old handle.** If a log is rotated but the process goes
  on writing to the old file without reopening, the new file at the path looks
  fresh and empty while the real output goes elsewhere. Nothing visible from the
  path can tell.

---

## Don't guess your thresholds

A watchdog with wrong thresholds is worse than none. It either pages you
constantly until you mute it, or never fires at all. Nobody knows their job's real
cadence off the top of their head.

```bash
stillwatch --config stillwatch.toml learn --for 6h > learned.toml
```

Observe-only. No evaluator and no notifier are started at all, so it's safe to
point at production before you trust anything. It records what actually happens
and prints a config block with the evidence behind every number:

```toml
[[job]]
name = "product-scraper"

  [job.alive]
  expect_every   = "60s"    # observed p50 60s, p99 63s, worst 71s over 358 beats
  warn_after     = "5m"     # 4x the worst gap seen
  critical_after = "15m"    # 3x warn_after
```

Never tighter than the worst thing observed, always with the evidence attached so
you can argue with it. What it prints loads: there's a test that runs the output
back through the config parser at every cadence from one second to a day.

### It refuses rather than guessing

An incident inside the observation window is the danger here, and it's worse than
the runtime version of the same problem. A rolling baseline heals as its window
rolls. A learned threshold gets pasted into a file and trusted for months. Forty
minutes of outage during learning becomes a forty-minute "worst gap", which
becomes a threshold the real failure can never cross.

So gaps far out of line with the rest of the window, over 5× the median, are
treated as incidents rather than cadence. Ordinary jitter never reaches 5×. They
are excluded from the derivation and named in the output:

```toml
  [job.alive]
  # NOTE: 1 beat gap(s) up to 40m looked like incidents rather than cadence
  # (over 5x the median) and were EXCLUDED from the numbers below.
  # If those were normal for this job, widen the thresholds by hand.
```

Where excluding isn't enough, it declines outright. More than a quarter of the
window inside suspected incidents leaves no normal to learn from. Fewer than 20
intervals makes a confident-looking number and a meaningless one. A signal that
never arrived at all gets no block: no `worked: true`, no `data_ts`, no probes.

Refusals are still valid TOML, so redirecting to a file leaves an explanation
rather than a mystery.

If the dependency was slow, or the job broken, for the *entire* window, nothing
here can tell. There's no normal to compare against and the median is the broken
value. That's why every number comes with its evidence: the numbers themselves
are the check.

---

## Watch it be right before you trust it

```bash
stillwatch --dry-run --config stillwatch.toml
```

Evaluates everything live and logs what it would have sent, delivering nothing.
Deduplication, escalation and recovery all live above the notifier, so a dry run
logs once per incident. That's exactly what the real thing would deliver, not
what the evaluator produced every five seconds. A dry run that read noisier than
the daemon would be worthless, because nobody would switch the real one on.

---

## A baseline is only worth what it learned

Comparing a dependency against its own history is the right idea, and it has two
failure modes that both end with the tool being confidently wrong.

**A baseline it hasn't got yet.** Until a check has `min_samples` probes, 30 by
default, there's nothing to compare against. It reports *warming up* rather than
*ok*, and startup logs say how long the wait will be. Not being judged yet and
being healthy are different facts, and stillwatch never conflates them. A
`baseline_window` too short to ever hold that many probes at that `interval` is
rejected at startup rather than warming up forever.

**A baseline that learned the wrong thing.** If stillwatch starts while a
dependency is already slow, the baseline learns that slow is normal and the
multiples never fire again. That's the tool quietly failing, which is worse than
not having it.

So `absolute_ceiling` is a floor, not a second opinion. It's evaluated
independently of the baseline and before any baseline exists, so it fires on a
dependency that has been unacceptable since the moment stillwatch started. The
baseline p90 also excludes the most recent stretch, so a slowdown still in
progress doesn't get to teach the baseline that it's fine. And if the baseline
itself ends up at or above the ceiling, that gets its own alert saying the check
has stopped protecting you.

A baseline poisoned to a value *below* the ceiling is still invisible. Learned at
1.4s with a 2s ceiling, nothing catches it. Set `absolute_ceiling` to what you
actually consider unacceptable, not to some extreme.

---

## What the alerts look like

```
⚠️  product-scraper — no heartbeat for 5m12s
    last beat 14:32:07 -04:00, expected every 1m
    stillwatch has been up for the whole gap — this is the job, not the watchdog
    → the loop has stopped; the process has most likely exited or wedged

🔴  clients-etl — no heartbeat since stillwatch started, 15m3s ago
    watching since 09:14:02 -04:00, expected every 1m; nothing has ever arrived
    → either the job was already stopped when the watch began, or it has never been wired up to send beats

🔴  vendor-api — degraded
    p90 1.2s over the last 20s (6 probes), baseline 96ms over the 1m before that (12 probes)
    still responding · this is latency, not an outage
    → 12.5x its own normal; everything downstream is that much slower and nothing else would have said so

⚠️  vendor-api — degraded
    p90 3s over the last 10m (20 probes), baseline 3s over 118 probes — but that is itself past the 2s ceiling, so the baseline has learned that slow is normal and the multiples cannot fire
    still responding · this is latency, not an outage
    → past the 2s you said was unacceptable; everything downstream is waiting that long

⚠️  product-scraper — no work in 3h
    last work 11:02:14 -04:00, expected at least every 2h
    still beating · the loop is running, it just has not reported any work
    → this is the idle-dead case: up, looping, and producing nothing

⚠️  product-scraper — parse rate 40.8% (min 90%)
    last 1h: 1,200 fetched, 490 parsed
    beats arriving ✓ · the loop is running, the work is not landing
    → fetching fine, parsing broken — source markup likely changed

⚠️  price-feed — acting on data 22m old
    data timestamped 14:31:02 -04:00, expected fresher than 10m
    the job itself is reporting in · this is the source, not the job
    → whatever it produced since then was computed from numbers this old

🔴  queue-broker — down for 2m4s
    4 probes in a row failed; the last said: connection refused
    → whatever depends on this is not getting answers, not getting slow answers

✅  product-scraper recovered — no heartbeat for 20m5s
✅  vendor-api recovered — degraded for 18m4s
```

Three rules, all deliberate:

1. Lead with what happened, not with a check ID.
2. Say what isn't wrong. Ruling things out is half the value of being woken up.
3. End with the implication in plain language. Never send a bare "check failed."

Configure no notifier at all and it still runs, says so at startup, and writes the
same alerts to the log instead of delivering them.

---

## Alert fatigue is the real failure mode

A monitoring tool that cries wolf gets muted, and a muted tool is worse than no
tool because you think you're covered.

* **Damped.** A condition must hold for `confirm_after`, 30s by default, before it
  becomes a message at all. Something that fixes itself in four seconds is not an
  incident. This gates the first firing and nothing else: once a condition is
  established as real and then gets worse, the escalation goes out at once.
  Recovery is damped the same way, so a condition that blinks clear and returns
  doesn't produce an all-clear followed by a fresh alert.
* **Deduplicated.** One alert per incident, not one per evaluation cycle.
  Deduplication is per *condition*, so a job that's both missing its heartbeat and
  running a collapsed parse rate reports both.
* **Escalating.** Warn, then critical, then nothing. It doesn't nag, and it doesn't
  walk back down either.
* **Recovering.** Every alert gets an all-clear with the duration of the whole
  incident, measured from when the condition began rather than when it was
  confirmed. An alert with no resolution teaches people to ignore alerts.
* **Loud on failure.** If the notifier is unreachable, alerts queue in order and
  retry with backoff, and nothing is dropped silently. A message the notifier will
  refuse identically forever, a wrong chat id say, is dropped rather than allowed
  to block every alert behind it. That gets logged as an error telling you to fix
  the config.

---

## Incidents, and what it admits it didn't see

Incidents append to a JSONL file. No database. This is opt-in, and with no
`[incidents]` block nothing is recorded, which startup says out loud.

```toml
[incidents]
path      = "/var/lib/stillwatch/incidents.jsonl"
max_bytes = 8388608
```

**If that path can't be opened at startup, stillwatch refuses to start.** A
watchdog running happily with no audit trail is an inert rule pointed at itself.
Everything looks fine, and the record that would have proved otherwise was never
written.

The log rotates at `max_bytes` and keeps exactly one previous generation, so it's
bounded at twice that and cannot creep. Retention is by size rather than by time.
A very busy month can push older incidents out before `report --since 30d` would
have reached them, so raise `max_bytes` if you need longer history.

```bash
stillwatch report --since 7d
```

```
clients-etl       uptime  99.7%   1 incident   longest 33m20s
product-scraper   uptime  99.6%   3 incidents   longest 18m4s
vendor-api        uptime  99.6%   1 incident   longest 41m

watched 6d20h of the last 7d · 3h unaccounted for (stillwatch was not running, or stopped without recording it)
percentages above are of the watched time only
```

That last block is the point. stillwatch records its own start and stop, plus a
`watching` marker every five minutes in between, so `report` can say how much of
the window it was actually there for. Time it can't account for is excluded from
every percentage and reported separately.

Both alternatives were worse. Counting an unwatched gap as uptime is a monitoring
tool overstating its own coverage. Counting it as downtime invents an outage.
Neither is known, so neither is claimed.

Three cases it distinguishes rather than guessing at:

* **A clean shutdown.** A `stopped` record, so the gap after it was deliberate.
* **A silent death.** A `started` with no `stopped`, and the `watching` markers
  simply stopping. Coverage ends at the last thing that run actually recorded, and
  everything after is unknown. The report says "or stopped without recording it".
* **No coverage at all.** `uptime unknown`. Not 0% and not 100%: if nothing was
  watching there's no percentage to give, and a window with no records at all
  reads as `stillwatch has no record of watching any of the last 7d — every
  number above is unknown rather than good`.

Only subjects that had at least one incident appear in the report. A job that
never failed has no records to summarise.

---

## It doesn't lie about itself

Every one of these is the tool's own failure class, pointed at itself.

* **A restart is not an outage.** No history means unknown, not down. It waits a
  full threshold before judging anything.
* **But a job that was already dead is still reported.** With no beats ever seen,
  silence is measured from when stillwatch started, and the alert says exactly
  that rather than inventing a last-beat time it never observed. A job that died
  before the watchdog started is the failure a watchdog most needs to catch.
* **It reports its own gaps.** `report` says how much of the window it was
  actually watching and excludes the rest from every percentage.
* **It refuses to start without its audit trail**, when one is configured.
* **It says which of your rules are doing nothing.** A ratio naming a counter that
  never arrives, or a freshness rule that no beat feeds, gets one alert saying so.
  From outside, a rule that can never fire is indistinguishable from one that is
  passing.
* **One process, no clustering.** If you want the watchdog watched, run a second
  one pointed at the first. `GET /health` exists for that and says nothing more
  than "this process is answering". A watchdog whose own health check claimed more
  would be making the mistake this tool is about.

---

## Install

No published crate yet. Build it:

```bash
git clone https://github.com/PyraVim/stillwatch
cd stillwatch
cargo build --release
./target/release/stillwatch --config stillwatch.toml
```

One binary and no database. It links libc and a TLS stack; there's nothing else
to install.

**systemd** — [`deploy/stillwatch.service`](deploy/stillwatch.service).
`KillSignal=SIGINT` is load-bearing: that's what stillwatch shuts down gracefully
on, and a graceful shutdown is what writes the `stopped` record. Without it every
restart looks to `report` like an unexplained gap. Secrets go in an
`EnvironmentFile` that root can read and nobody else can.

**Docker** — [`deploy/Dockerfile`](deploy/Dockerfile). Distroless, non-root, no
shell. Mount a volume at `/var/lib/stillwatch` or the audit trail goes with the
container, and `report` will honestly say it has no record of the window rather
than claiming everything was fine.

**Watching the watchdog.** There's no clustering, deliberately. Point a second
stillwatch at the first:

```toml
[[check]]
name = "stillwatch-primary"
url  = "http://primary:9111/health"
```

Use `/health` and not a `/beat/` URL. A beat for an unconfigured job answers 404
by design, which a probe reads as an outage.

---

## Roadmap

Three things are genuinely not built. Everything else described above is.

| | |
|-|-|
| **Job → dependency links** | A job alert can't yet say "the scraper is down, not its dependency", because nothing in the config associates a job with the checks it relies on. Needs something like `depends_on = ["vendor-api"]`. |
| **Cumulative counters** | Counters are per beat. A job sending running lifetime totals produces meaningless sums and nothing detects it. Detecting monotonic series and diffing them needs reset handling to be worth having. |
| **On-chain passive checks** | `expect_activity_every` against a contract address. Needs log-range queries, which is a different thing from the JSON-RPC dependency check that already exists. |

---

## What it isn't

Not "not yet". Not ever:

No dashboard. No metrics backend. No Prometheus exporter. No clustering. No
knowledge of what your job actually does.

It watches, it decides, and it tells you. Everything else is somebody else's tool.

---

## License

MIT
