# stillwatch

**A watchdog for things that run without you.**

Not "is the process up." That's the least interesting failure, and it's the one
you'd notice anyway. The expensive failures are the ones where it's up and wrong:

* the scraper that's been returning zero rows since Tuesday
* the sync job authenticating fine against a token that quietly stopped granting access
* the aggregator that lost one of its seven sources and just started publishing thinner data
* the trading bot submitting orders that never fill
* the nightly ETL that ran, exited zero, and wrote nothing

Every one of those passes a health check.

\---

## Why this exists

I spent fourteen years in FDA-regulated manufacturing as the escalation point for
automated systems. Every one of them had alarm handling, deviation detection, and
an audit trail, because running unattended without those was unthinkable. If a
system ran wrong for six hours, you didn't just fix it — you wrote up why nobody
knew for six hours, and that document had to survive an audit.

Then I started building automation outside that world and found almost none of that
exists. People write the thing that does the work and hope. Where there is
monitoring, it answers "is the process alive," which is the single failure mode
that doesn't need monitoring to detect.

`stillwatch` is the part nobody builds for themselves.

\---

## What it catches

|Failure|Why ordinary monitoring misses it|
|-|-|
|**Alive but idle-dead**|Process up, loop spinning, stopped doing anything. Looks perfectly healthy.|
|**Acting on stale data**|The feed froze. Everything downstream keeps working confidently on numbers from nine minutes ago.|
|**Working but not landing**|Every attempt fails. Process healthy, dependencies healthy, output zero.|
|**Degraded, not down**|An API that answered in 90ms now takes 1.4s. Still 200 OK. Every check passes. Everything gets slower and nobody is told.|

That last one is the one that costs the most and gets monitored the least.

\---

## Alive is not the same as working

This is the distinction most monitoring gets wrong, and it's why generic uptime
tools are useless for anything with irregular work.

A job can legitimately do nothing for six hours because there was nothing to do.
That's healthy. The same silence with a crashed loop is an emergency. From the
outside they look identical.

So heartbeats carry two separate signals:

* **`alive`** — my loop ran. Expected on a tight interval. Missing means dead.
* **`worked`** — I actually did something. Expected irregularly. A long silence
means *look into it*, not *wake someone up*.

Different thresholds, different severities.

\---

## Quick start

```bash
stillwatch --config stillwatch.toml
```

Then, from your job — any language, no client library, no dependency:

```bash
curl -fsS -X POST localhost:9111/beat/nightly-sync
```

That's a valid heartbeat. Everything beyond it is optional:

```bash
curl -fsS -X POST localhost:9111/beat/nightly-sync \\
  -H 'content-type: application/json' \\
  -d '{"worked":true,"data\_ts":1724500000,"counters":{"rows\_read":8400,"rows\_written":8400}}'
```

```python
# python, still no dependency worth naming
requests.post("http://localhost:9111/beat/scraper",
              json={"worked": True, "counters": {"fetched": 120, "parsed": 118}})
```

\---

## Configure

```toml
listen = "127.0.0.1:9111"

\[notify.telegram]
token   = "${TELEGRAM\_TOKEN}"
chat\_id = "${TELEGRAM\_CHAT}"

# --- a scraper that should run continuously ---
\[\[job]]
name = "product-scraper"

  \[job.alive]
  expect\_every   = "60s"
  warn\_after     = "5m"
  critical\_after = "15m"

  \[\[job.ratio]]
  name        = "parse rate"
  numerator   = "parsed"
  denominator = "fetched"
  window      = "1h"
  min         = 0.9
  min\_sample  = 50
  message     = "pages fetching but not parsing — the site's markup probably changed"

# --- a nightly job that should be quiet most of the day ---
\[\[job]]
name = "nightly-sync"

  \[job.worked]
  warn\_after     = "26h"      # missed one night
  critical\_after = "50h"      # missed two

  \[job.freshness]
  warn\_after     = "26h"

# --- a dependency, watched directly ---
\[\[check]]
name     = "vendor-api"
type     = "http"
url      = "https://api.vendor.com/health"
interval = "30s"
timeout  = "3s"

  \[check.degradation]
  baseline\_window   = "1h"
  warn\_multiple     = 3.0     # p90 three times its own recent normal
  critical\_multiple = 8.0
  absolute\_ceiling  = "2s"
```

`${VAR}` interpolation everywhere, so no secret is ever in the file.

\---

## Don't guess your thresholds

A watchdog with wrong thresholds is worse than none. It either pages you constantly
until you mute it, or it never fires at all. Nobody knows their job's real cadence
off the top of their head.

```bash
stillwatch learn --job product-scraper --for 6h
```

Observe-only. Records what actually happens — beat intervals, gaps between real
work, dependency latency distributions, counter ratios — then prints a config block
with the evidence behind every number:

```toml
# learned from 6h0m, 358 beats
\[\[job]]
name = "product-scraper"

  \[job.alive]
  expect\_every   = "60s"    # observed p50 60.2s, p99 63s, worst gap 71s
  warn\_after     = "5m"     # 4x the worst gap seen
  critical\_after = "15m"

# vendor-api: p50 88ms, p90 140ms, p99 410ms over 719 samples
```

Never tighter than the worst thing observed. Always with the evidence attached, so
you can argue with it.

And before you trust it with your phone:

```bash
stillwatch --dry-run     # evaluates and logs what it would have sent. sends nothing.
```

\---

## What the alerts look like

```
⚠️  product-scraper — no heartbeat for 5m12s
    last beat 14:32:07, expected every 60s
    vendor-api OK
    → the scraper is down, not its dependency

🔴  vendor-api — degraded
    p90 1.4s, baseline 140ms over the last hour
    still responding · 0 errors · every health check passing
    → everything downstream is now three times slower and nothing said so

⚠️  product-scraper — parse rate 41% (was 99%)
    last hour: 1,204 fetched, 494 parsed
    alive ✓ · vendor-api healthy ✓
    → fetching fine, parsing broken. the markup probably changed.

✅  vendor-api recovered — degraded for 18m4s
```

Three rules, and they're deliberate:

1. **Lead with what happened**, not with a check ID.
2. **Say what isn't wrong.** Ruling things out is half the value of being woken up.
3. **End with the implication in plain language.** Never send a bare "check failed."

\---

## Alert fatigue is the real failure mode

A monitoring tool that cries wolf gets muted, and a muted tool is worse than no
tool because you think you're covered.

* **Deduplicated** — one alert per incident, not one per evaluation cycle
* **Escalating** — warn, then critical. Then nothing. It doesn't nag.
* **Recovering** — every alert gets an all-clear with the duration. An alert with
no resolution teaches people to ignore alerts.
* **Flap-damped** — a condition has to hold through a confirmation window. Something
that fixes itself in four seconds is not an incident.
* **Fails loudly** — if the notifier is unreachable, alerts queue and retry. Nothing
is dropped silently.

\---

## It doesn't lie about itself

* **A restart is not an outage.** No history means unknown, not down. It waits a
full interval before judging anything.
* **It reports its own gaps.** If `stillwatch` was down for twenty minutes, the
report says so rather than showing 100% uptime for a window it wasn't watching.
* **One process, no clustering.** If you want the watchdog watched, run a second one
pointed at the first. That's the whole answer.

\---

## Incidents

Everything appends to a JSONL log. No database.

```bash
stillwatch report --since 7d
```

```
product-scraper   uptime 99.2%   3 incidents   longest 18m4s
nightly-sync      uptime  100%   0 incidents
vendor-api        uptime 97.8%   1 degradation 41m
```

\---

## What it isn't

No dashboard. No metrics backend. No Prometheus exporter. No clustering. No
knowledge of what your job actually does.

It watches, it decides, and it tells you. Everything else is somebody else's tool.

\---

## Install

```bash
cargo install stillwatch
```

Single static binary. `Dockerfile` and a systemd unit are in `deploy/`.

## License

MIT

