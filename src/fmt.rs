//! Human-facing formatting for durations and timestamps.
//!
//! Alerts are read on a phone at three in the morning. Everything here optimises
//! for being unambiguous to a tired person, not for being machine-parseable.

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};

/// Renders a duration the way an alert should read it: the two most significant
/// non-zero units, no spaces.
///
/// `5m12s`, `18m4s`, `27h`, `0s`.
///
/// Hours are the largest unit on purpose. A nightly job that missed a run is
/// "27h late", not "1d3h late" — days force the reader to do arithmetic to
/// compare the number against a threshold they wrote in hours.
pub fn duration(d: Duration) -> String {
    let total = d.as_secs();
    let units = [
        (total / 3_600, 'h'),
        ((total % 3_600) / 60, 'm'),
        (total % 60, 's'),
    ];

    let Some(first) = units.iter().position(|(value, _)| *value > 0) else {
        return "0s".to_string();
    };

    let mut out = String::new();
    for (value, unit) in units.iter().skip(first).take(2) {
        if *value > 0 {
            out.push_str(&format!("{value}{unit}"));
        }
    }
    out
}

/// Renders a latency at the precision a person compares latencies at.
///
/// `90ms`, `1.4s`, `2s`, `18m4s`. Separate from [`duration`] because that one
/// counts in whole seconds and would render every healthy response time as
/// `0s`, which is exactly the number a degradation alert must not print.
pub fn latency(d: Duration) -> String {
    let millis = d.as_millis();

    if millis == 0 {
        // A sub-millisecond response is real; claiming 0ms is not.
        return "<1ms".to_string();
    }
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    if d.as_secs() < 60 {
        let seconds = format!("{:.1}", d.as_secs_f64());
        return format!("{}s", seconds.strip_suffix(".0").unwrap_or(&seconds));
    }

    duration(d)
}

/// Renders a wall-clock timestamp in local time, always with a UTC offset.
///
/// Same day as `now` gives `14:32:07 -04:00`; anything older carries the date
/// too, because "02:14" on its own is a guess about which night it was.
///
/// The offset is spelled numerically rather than as an abbreviation because
/// `%Z` resolves to a name on Unix and to an offset on Windows, and an alert
/// should not read differently depending on where the watchdog runs.
pub fn timestamp(ts: SystemTime, now: SystemTime) -> String {
    let ts: DateTime<Local> = ts.into();
    let now: DateTime<Local> = now.into();

    if ts.date_naive() == now.date_naive() {
        ts.format("%H:%M:%S %:z").to_string()
    } else {
        ts.format("%Y-%m-%d %H:%M:%S %:z").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_uses_two_most_significant_units() {
        assert_eq!(duration(Duration::from_secs(312)), "5m12s");
        assert_eq!(duration(Duration::from_secs(1084)), "18m4s");
        assert_eq!(duration(Duration::from_secs(3_930)), "1h5m");
    }

    #[test]
    fn duration_drops_trailing_zero_units() {
        assert_eq!(duration(Duration::from_secs(27 * 3600)), "27h");
        assert_eq!(duration(Duration::from_secs(300)), "5m");
    }

    /// Days would make a 27h threshold render as "1d3h", which the reader then
    /// has to convert back to compare against what they configured.
    #[test]
    fn duration_counts_past_a_day_in_hours() {
        assert_eq!(duration(Duration::from_secs(50 * 3600)), "50h");
        assert_eq!(duration(Duration::from_secs(90_061)), "25h1m");
    }

    #[test]
    fn duration_of_less_than_a_second_is_zero() {
        assert_eq!(duration(Duration::ZERO), "0s");
        assert_eq!(duration(Duration::from_millis(400)), "0s");
    }

    #[test]
    fn latency_keeps_the_precision_people_compare_at() {
        assert_eq!(latency(Duration::from_millis(90)), "90ms");
        assert_eq!(latency(Duration::from_millis(140)), "140ms");
        assert_eq!(latency(Duration::from_millis(1_400)), "1.4s");
        assert_eq!(latency(Duration::from_millis(2_000)), "2s");
    }

    /// `duration` counts whole seconds, so every healthy response time would
    /// render as `0s` — the one number a degradation alert must never print.
    #[test]
    fn latency_never_renders_a_real_response_as_zero() {
        assert_eq!(duration(Duration::from_millis(90)), "0s");
        assert_eq!(latency(Duration::from_millis(90)), "90ms");
        assert_eq!(latency(Duration::from_micros(400)), "<1ms");
    }

    #[test]
    fn latency_falls_back_to_duration_past_a_minute() {
        assert_eq!(latency(Duration::from_secs(90)), "1m30s");
    }

    #[test]
    fn timestamp_includes_date_only_when_not_today() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_755_000_000);
        let earlier_same_day = now - Duration::from_secs(60);
        let days_ago = now - Duration::from_secs(3 * 86_400);

        // "14:32:07 -04:00" and "2025-08-09 14:32:07 -04:00": the offset is
        // always six characters, so both widths are fixed.
        let today = timestamp(earlier_same_day, now);
        let old = timestamp(days_ago, now);

        assert_eq!(today.len(), 15, "expected HH:MM:SS +oo:oo, got {today}");
        assert_eq!(old.len(), 26, "expected a dated timestamp, got {old}");
        assert!(old.starts_with("2025-08-"), "{old}");
        assert!(
            old.ends_with(&today[8..]),
            "offsets should match: {old} / {today}"
        );
    }
}
