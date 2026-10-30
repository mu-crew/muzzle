//! `runs.jsonl` parsing, `--since`/`--until`/`--compare` periods and
//! percentiles: the inputs every figure in the report is computed from.

use anyhow::{Context, bail};

use crate::state::RunRecord;

/// A half-open period `[since, until)` in Unix seconds; `None` is open.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Period {
    pub since: Option<f64>,
    pub until: Option<f64>,
}

/// A parsed record with its times resolved, so nothing re-parses `end`.
#[derive(Debug, Clone)]
pub struct Run {
    pub rec: RunRecord,
    /// Unix seconds.
    pub end: f64,
}

impl Run {
    pub(super) fn run_start(&self) -> f64 {
        self.end - self.rec.ran_s
    }

    pub(super) fn wait_start(&self) -> f64 {
        self.run_start() - self.rec.waited_s
    }

    /// D30: exit 0 and not killed. OOM runs are never clean (D30a).
    pub fn is_clean(&self) -> bool {
        self.rec.exit == Some(0) && self.rec.killed.is_none()
    }

    /// Spec §report: cores used = `cpu_s / ran_s`.
    pub fn cores(&self) -> Option<f64> {
        let cpu = self.rec.cpu_s?;
        (self.rec.ran_s > 0.0).then(|| cpu / self.rec.ran_s)
    }
}

/// Parses `runs.jsonl` text. Lines that don't parse or have a newer schema
/// are skipped and counted, so one torn line doesn't hide a month of runs.
pub fn parse(text: &str) -> (Vec<Run>, usize) {
    let mut runs = Vec::new();
    let mut skipped = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let parsed = serde_json::from_str::<RunRecord>(line)
            .ok()
            .filter(|r| r.v == 1)
            .and_then(|rec| Some((parse_ts(&rec.end)?, rec)));
        match parsed {
            Some((end, rec)) => runs.push(Run { rec, end }),
            None => skipped += 1,
        }
    }
    runs.sort_by(|a, b| a.end.total_cmp(&b.end));
    (runs, skipped)
}

pub(super) fn parse_ts(s: &str) -> Option<f64> {
    let ts: jiff::Timestamp = s.parse().ok()?;
    Some(ts.as_millisecond() as f64 / 1000.0)
}

pub(super) fn fmt_ts(secs: f64) -> String {
    jiff::Timestamp::from_second(secs.floor() as i64)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|_| format!("{secs}"))
}

/// A `D` for `--since`/`--until` or one side of `A..B`: `now`, a relative
/// `30m`/`12h`/`7d` back from now, a `YYYY-MM-DD` UTC date, or an RFC 3339
/// timestamp. A date as an upper bound includes that whole day.
pub fn parse_point(s: &str, now: f64, upper: bool) -> anyhow::Result<f64> {
    let s = s.trim();
    if s == "now" {
        return Ok(now);
    }
    if let Some(unit) = s.chars().last().and_then(|c| match c {
        'm' => Some(60.0),
        'h' => Some(3600.0),
        'd' => Some(86400.0),
        _ => None,
    }) && let Ok(n) = s[..s.len() - 1].parse::<f64>()
    {
        return Ok(now - n * unit);
    }
    if let Ok(date) = s.parse::<jiff::civil::Date>() {
        let start = date
            .to_zoned(jiff::tz::TimeZone::UTC)
            .context("date out of range")?
            .timestamp()
            .as_second() as f64;
        return Ok(if upper { start + 86400.0 } else { start });
    }
    parse_ts(s).with_context(|| {
        format!("bad date {s:?}: use now, 7d, 12h, 30m, YYYY-MM-DD or an RFC 3339 time")
    })
}

/// `A..B`, either side may be empty.
pub fn parse_range(s: &str, now: f64) -> anyhow::Result<Period> {
    let Some((a, b)) = s.split_once("..") else {
        bail!("bad period {s:?}: expected A..B");
    };
    let side = |x: &str, upper| {
        (!x.trim().is_empty())
            .then(|| parse_point(x, now, upper))
            .transpose()
    };
    Ok(Period {
        since: side(a, false)?,
        until: side(b, true)?,
    })
}

impl Period {
    /// Rule rows count a run in the period its `end` falls in.
    pub(super) fn contains(&self, t: f64) -> bool {
        self.since.is_none_or(|s| t >= s) && self.until.is_none_or(|u| t < u)
    }

    /// Closes open ends with the span of the runs, from the first wait to
    /// the last end.
    pub(super) fn resolve(&self, runs: &[Run]) -> Option<(f64, f64)> {
        let lo = self
            .since
            .or_else(|| runs.iter().map(Run::wait_start).min_by(f64::total_cmp))?;
        let hi = self
            .until
            .or_else(|| runs.iter().map(|r| r.end).max_by(f64::total_cmp))?;
        (hi > lo).then_some((lo, hi))
    }
}

/// Nearest-rank percentile: the smallest value with at least `p`% of the
/// sample at or below it. `None` for an empty sample.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.clamp(1, v.len()) - 1])
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct P5090 {
    pub p50: f64,
    pub p90: f64,
}

pub(super) fn p5090(values: &[f64]) -> Option<P5090> {
    Some(P5090 {
        p50: percentile(values, 50.0)?,
        p90: percentile(values, 90.0)?,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{T0, run};
    use super::*;
    use crate::state::Killed;

    #[test]
    fn percentiles_are_nearest_rank() {
        let v: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), Some(5.0));
        assert_eq!(percentile(&v, 90.0), Some(9.0));
        assert_eq!(percentile(&v, 100.0), Some(10.0));
        assert_eq!(percentile(&[7.0], 90.0), Some(7.0));
        assert_eq!(percentile(&[], 50.0), None);
        // Unsorted input.
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 50.0), Some(2.0));
    }

    #[test]
    fn cores_used_is_cpu_over_ran() {
        let mut r = run(T0, 200.0);
        r.rec.cpu_s = Some(500.0);
        assert_eq!(r.cores(), Some(2.5));
        r.rec.cpu_s = None;
        assert_eq!(r.cores(), None);
        let mut z = run(T0, 0.0);
        z.rec.cpu_s = Some(1.0);
        assert_eq!(z.cores(), None);
    }

    #[test]
    fn other_kills_are_not_clean() {
        for k in [Killed::Timeout, Killed::Signal, Killed::Holder] {
            let mut r = run(T0, 1.0);
            r.rec.killed = Some(k);
            assert!(!r.is_clean(), "{k:?}");
        }
        let mut r = run(T0, 1.0);
        r.rec.exit = None;
        assert!(!r.is_clean());
        assert!(run(T0, 1.0).is_clean());
    }

    #[test]
    fn parse_skips_torn_and_newer_lines() {
        let good = serde_json::to_string(&run(T0, 5.0).rec).unwrap();
        let newer = good.replacen("\"v\":1", "\"v\":2", 1);
        let text = format!("{good}\n{{\"v\":1,\"end\":\n\n{newer}\n{good}\n");
        let (runs, skipped) = parse(&text);
        assert_eq!((runs.len(), skipped), (2, 2));
        assert_eq!(runs[0].end, T0);
    }

    #[test]
    fn points_and_ranges() {
        let now = T0;
        assert_eq!(parse_point("now", now, false).unwrap(), now);
        assert_eq!(parse_point("7d", now, false).unwrap(), now - 7.0 * 86400.0);
        assert_eq!(parse_point("12h", now, false).unwrap(), now - 12.0 * 3600.0);
        let day = parse_point("2026-11-03", now, false).unwrap();
        assert_eq!(fmt_ts(day), "2026-11-03T00:00:00Z");
        assert_eq!(parse_point("2026-11-03", now, true).unwrap(), day + 86400.0);
        assert_eq!(
            parse_point("2026-11-03T10:12:44Z", now, false).unwrap(),
            day + 36764.0
        );
        assert!(parse_point("yesterday", now, false).is_err());
        let p = parse_range("2026-11-01..2026-11-03", now).unwrap();
        assert_eq!(p.until, Some(day + 86400.0));
        assert_eq!(parse_range("..now", now).unwrap().since, None);
        assert!(parse_range("2026-11-01", now).is_err());
    }
}
