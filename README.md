# stillwatch

**A watchdog for things that run without you.**

Not "is the process up." That's the least interesting failure, and it's the one
you'd notice anyway. `stillwatch` watches the *loop*, from inside it: a scraper
whose worker thread died at 3am is still a running process, still answering its
health check, still using memory. It just stopped beating.

---

## What it does today

**Heartbeats.** One HTTP endpoint. Your job posts to it each time round its loop:

```bash
curl -fsS -X POST localhost:9111/beat/nightly-sync
```

If the beats stop for longer than you said they should, you get a message that
tells you what happened, what *isn't* wrong, and what it probably means.

**Dependency probes.** stillwatch polls the things your jobs depend on and
watches their latency against their *own* recent normal — because a dependency
that has always taken 400ms is fine, and one that took 90ms an hour ago is not.
It catches the API that still returns 200 on every request and now takes 1.4s.

Either way you get one message per incident, not one per evaluation cycle, and
an all-clear with the duration when it ends.

That's version 0.2. The rest of the plan is in [Roadmap](#roadmap), and none of
it is built yet.

---

## Why this exists

I spent fourteen years in FDA-regulated manufacturing as the escalation point for
automated systems. Every one of them had alarm handling, deviation detection, and
an audit trail, because running unattended without those was unthinkable. If a
system ran wrong for six hours, you didn't just fix it — you wrote up why nobody
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

Version 0.1 catches the first — the loop stopping entirely — and the last, where
a dependency gets slower and slower while every health check keeps passing. The
middle two are what the roadmap is for.

---

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

Today `stillwatch` evaluates `alive`. What matters is that it already refuses to
confuse the two: a job that declares no `[job.alive]` block is **never** judged
late, however long it stays quiet. A nightly sync does not inherit a scraper's
cadence, and there is a test with that name on it.

---

## Quick start

```bash
stillwatch --config stillwatch.toml
```

Then, from your job — any language, no client library, no dependency:

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

`worked`, `data_ts` and `counters` are accepted and logged today. Nothing
evaluates them yet — they're part of the wire protocol so that jobs can start
sending them now and not need changing later.

Two responses that aren't `200`:

* **`404`** — no job by that name is configured. A typo in the URL must not
  leave the real job silently unwatched, so it's refused and logged rather than
  quietly accepted.
* **`400`** — there was a body and it wasn't valid JSON. It does not count as a
  heartbeat.

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

# --- a nightly job that should be quiet most of the day ---
# No [job.alive] block, so it is never judged late.
[[job]]
name = "nightly-sync"

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

A minimal working config is four lines — everything but job names has a default.
`listen` defaults to `127.0.0.1:9111`, and both thresholds derive from
`expect_every` if you leave them out.

`warn_after` must be longer than `expect_every`, or it would fire in the ordinary
gap between two beats. That's checked at startup, not discovered at 3am.

**Secrets.** `${VAR}` is expanded from the environment in any value, so no secret
has to sit on disk. An unset variable is a startup error naming the variable —
never an empty string that fails quietly later. Interpolation happens over the
parsed TOML, not the raw text, so a secret containing a quote can't rewrite the
file around it.

**Containers.** Three values can also be set directly, for deploys with no
writable config:

```
STILLWATCH_LISTEN
STILLWATCH_NOTIFY_TELEGRAM_TOKEN
STILLWATCH_NOTIFY_TELEGRAM_CHAT_ID
```

Uppercase the dotted path, replace `.` with `_`. These win over both the file and
any `${VAR}` in it. Per-job values are deliberately *not* overridable this way:
array entries have no stable key to name them by, and inventing an indexing
scheme would be a second config language.

`$STILLWATCH_CONFIG` sets the config path; it falls back to `./stillwatch.toml`.

Config for features that aren't built yet parses without error and logs a warning
naming every key it ignored. A line that does nothing should never do so quietly.

See [`stillwatch.toml.example`](stillwatch.toml.example) for the annotated version.

---

## A baseline is only worth what it learned

Comparing a dependency against its own history is the right idea and it has two
failure modes, both of which end with the tool being confidently wrong. Both are
handled deliberately.

**A baseline it hasn't got yet.** Until a check has `min_samples` probes (30 by
default), there is nothing to compare against. It reports **warming up** — not
*ok* — and startup logs say how long the wait will be. "Not being judged yet" and
"healthy" are different facts and stillwatch never conflates them. A
`baseline_window` too short to ever hold that many probes at that `interval` is
rejected at startup rather than warming up forever.

**A baseline that learned the wrong thing.** If stillwatch starts while a
dependency is *already* slow, the baseline learns that slow is normal and the
multiples never fire again. That's the tool quietly failing, which is worse than
not having it.

So `absolute_ceiling` is not a second opinion — it's a floor. It's evaluated
independently of the baseline, and before any baseline exists, so it fires on a
dependency that has been unacceptable since the moment stillwatch started. The
baseline p90 also deliberately excludes the most recent stretch, so a slowdown
still in progress doesn't get to teach the baseline that it's fine. And if the
baseline itself ends up at or above the ceiling, that gets its own alert saying
the check has stopped protecting you.

One limit worth stating plainly: a baseline poisoned to a value *below* the
ceiling is still invisible. Learned at 1.4s with a 2s ceiling, nothing catches
it. Set `absolute_ceiling` to what you actually consider unacceptable, not to
some extreme.

---

## What the alerts look like

```
⚠️  product-scraper — no heartbeat for 5m12s
    last beat 14:32:07 -04:00, expected every 1m
    stillwatch has been up for the whole gap — this is the job, not the watchdog
    → the loop has stopped; the process has most likely exited or wedged

🔴  clients-etl — no heartbeat since stillwatch started, 15m3s ago
    watching since 09:14:02 -04:00, expected every 1m; nothing has ever arrived
    → either the job was already stopped when the watch began, or it has never
      been wired up to send beats

🔴  vendor-api — degraded
    p90 1.2s over the last 20s (6 probes), baseline 96ms over the 1m before that (12 probes)
    still responding · this is latency, not an outage
    → 12.5x its own normal; everything downstream is that much slower and nothing
      else would have said so

⚠️  vendor-api — degraded
    p90 3s over the last 10m (20 probes), baseline 3s over 118 probes — but that is
    itself past the 2s ceiling, so the baseline has learned that slow is normal and
    the multiples cannot fire
    still responding · this is latency, not an outage
    → past the 2s you said was unacceptable; everything downstream is waiting that long

🔴  queue-broker — down for 2m4s
    4 probes in a row failed; the last said: connection refused
    → whatever depends on this is not getting answers, not getting slow answers

✅  product-scraper recovered — no heartbeat for 20m5s
✅  vendor-api recovered — degraded for 18m4s
```

Three rules, and they're deliberate:

1. **Lead with what happened**, not with a check ID.
2. **Say what isn't wrong.** Ruling things out is half the value of being woken up.
3. **End with the implication in plain language.** Never send a bare "check failed."

Configure no notifier at all and it still runs, says so at startup, and writes
the same alerts to the log instead of delivering them.

---

## Alert fatigue is the real failure mode

A monitoring tool that cries wolf gets muted, and a muted tool is worse than no
tool because you think you're covered.

* **Deduplicated** — one alert per incident, not one per evaluation cycle
* **Escalating** — warn, then critical. Then nothing. It doesn't nag, and it
  doesn't walk back down either.
* **Recovering** — every alert gets an all-clear with the duration of the whole
  incident, measured from the first warning rather than the escalation. An alert
  with no resolution teaches people to ignore alerts.
* **Fails loudly** — if the notifier is unreachable, alerts queue in order and
  retry with backoff. Nothing is dropped silently. A message the notifier will
  refuse identically forever — a wrong chat id, say — is dropped rather than
  allowed to block every alert behind it, and it's logged as an error telling
  you to fix the config.

---

## It doesn't lie about itself

* **A restart is not an outage.** No history means unknown, not down. It waits a
  full threshold before judging anything.
* **But a job that was already dead is still reported.** With no beats ever seen,
  silence is measured from when `stillwatch` started — and the alert says exactly
  that rather than inventing a last-beat time it never observed. A job that died
  before the watchdog started is the failure a watchdog most needs to catch.
* **One process, no clustering.** If you want the watchdog watched, run a second
  one pointed at the first. That's the whole answer.

---

## Install

No published crate yet. Build it:

```bash
git clone https://github.com/PyraVim/stillwatch
cd stillwatch
cargo build --release
./target/release/stillwatch --config stillwatch.toml
```

Single static binary, no runtime dependencies, no database.

---

## Roadmap

None of this is built. It's here so you can see where it's going and decide
whether today's version is worth adopting anyway.

| | |
|-|-|
| **Freshness, `worked`, counter ratios** | Evaluate the other half of the heartbeat: how stale the data is, whether real work is happening, and generic named-counter rules (`parsed / fetched`, `rows_written / rows_read`, `landed / submitted` — same machinery, no domain knowledge). |
| **Job → dependency links** | Today a job alert can't say "the scraper is down, not its dependency", because nothing in the config associates a job with the checks it relies on. Needs something like `depends_on = ["vendor-api"]`. |
| **`learn` mode** | `stillwatch learn --job x --for 6h` observes without alerting, then emits a config block with the evidence behind every number. Nobody knows their job's real cadence, and a watchdog with wrong thresholds either pages constantly until muted or never fires. |
| **`--dry-run`** | Evaluate live and log what it *would* have sent. People need to watch it be right for a day before trusting it with their phone. |
| **Passive mode** | Watch something you can't modify: a pidfile, a log that should still be moving, an output file that should still be appearing. Weaker signals than a heartbeat, and the docs will say so — but "I can watch this today without touching your code" is what makes it usable on day one. |
| **Flap damping** | Require a condition to hold through a confirmation window. Something that fixes itself in four seconds is not an incident. |
| **Incident log and `report`** | Append every incident to JSONL; `stillwatch report --since 7d` prints per-subject uptime, incident count and longest outage — including gaps when `stillwatch` itself wasn't watching, rather than claiming 100% for a window it missed. |
| **Deploy** | Dockerfile and a systemd unit. |

---

## What it isn't

Not "not yet" — not ever:

No dashboard. No metrics backend. No Prometheus exporter. No clustering. No
knowledge of what your job actually does.

It watches, it decides, and it tells you. Everything else is somebody else's tool.

---

## License

MIT
