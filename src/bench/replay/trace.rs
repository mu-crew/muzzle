//! Trace loading: the header and calls, and the `--window` choice.

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;

pub const LIGHT: &str = "light";
pub const UNMATCHED: &str = "unmatched-heavy";

#[derive(Debug, Clone, Deserialize)]
pub struct Header {
    pub host: String,
    pub cores: u32,
    pub ram_gb: u32,
    pub date: String,
    pub window_s: f64,
    pub calls_kept: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    pub t: f64,
    pub dur_s: f64,
    pub rule: String,
    pub role: String,
    pub agent: String,
}

impl Call {
    /// Has a rule label: claims slots under muzzle.
    pub fn matched(&self) -> bool {
        self.rule != LIGHT && self.rule != UNMATCHED
    }
}

pub fn parse_trace(text: &str) -> anyhow::Result<(Header, Vec<Call>)> {
    let mut header = None;
    let mut calls = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).with_context(|| format!("line {}", i + 1))?;
        if row["v"] != 1 {
            bail!("line {}: unknown trace version", i + 1);
        }
        match (row["kind"].as_str(), &header) {
            (Some("header"), None) => header = Some(serde_json::from_value(row)?),
            (Some("call"), Some(_)) => calls.push(
                serde_json::from_value::<Call>(row).with_context(|| format!("line {}", i + 1))?,
            ),
            _ => bail!("line {}: expected one header, then calls", i + 1),
        }
    }
    let header = header.context("the trace has no header")?;
    calls.sort_by(|a, b| a.t.total_cmp(&b.t));
    Ok((header, calls))
}

/// `busiest:3h` → 10800 s. Units s, m, h; a bare number is seconds.
pub fn parse_window(s: &str) -> anyhow::Result<f64> {
    let len = s
        .strip_prefix("busiest:")
        .with_context(|| format!("--window {s:?}: expected busiest:<duration>, e.g. busiest:3h"))?;
    let (num, mult) = match len.char_indices().last() {
        Some((i, 'h')) => (&len[..i], 3600.0),
        Some((i, 'm')) => (&len[..i], 60.0),
        Some((i, 's')) => (&len[..i], 1.0),
        _ => (len, 1.0),
    };
    match num.parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => Ok(n * mult),
        _ => bail!("--window {s:?}: bad duration"),
    }
}

/// Whether the window `[start, start + len)` replays `call`: it starts in
/// the window. Scoring and replay share this, so the window chosen as
/// busiest is busiest among the calls actually replayed
/// (bench_window_straddle).
pub fn in_window(call: &Call, start: f64, len: f64) -> bool {
    call.t >= start && call.t < start + len
}

/// The matched call-seconds a window replays: whole `dur_s` of the calls
/// starting in it, not clipped.
pub fn window_load(calls: &[Call], start: f64, len: f64) -> f64 {
    calls
        .iter()
        .filter(|c| c.matched() && in_window(c, start, len))
        .map(|c| c.dur_s)
        .sum()
}

/// The earliest call start that begins a `len`-second window with the most
/// matched call-seconds. Some best window starts at a call: sliding any
/// window right until its first call sits on its start loses nothing.
pub fn busiest_start(calls: &[Call], len: f64) -> f64 {
    let mut best = (0.0, window_load(calls, 0.0, len));
    for c in calls.iter().filter(|c| c.matched()) {
        let v = window_load(calls, c.t, len);
        if v > best.1 || (v == best.1 && c.t < best.0) {
            best = (c.t, v);
        }
    }
    best.0
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in super::super) fn call(t: f64, dur_s: f64, rule: &str) -> Call {
        Call {
            t,
            dur_s,
            rule: rule.into(),
            role: "worker".into(),
            agent: "a1".into(),
        }
    }

    #[test]
    fn window_parses_units() {
        assert_eq!(parse_window("busiest:3h").unwrap(), 10800.0);
        assert_eq!(parse_window("busiest:90m").unwrap(), 5400.0);
        assert_eq!(parse_window("busiest:30s").unwrap(), 30.0);
        assert_eq!(parse_window("busiest:45").unwrap(), 45.0);
        for bad in ["3h", "busiest:", "busiest:0h", "busiest:xh", "busiest:-1"] {
            assert!(parse_window(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn busiest_window_counts_matched_call_seconds_only() {
        // A long light call and an unmatched-heavy one early must not pull
        // the window: the busiest 600 s hold both late gates whole.
        let calls = [
            call(0.0, 5000.0, LIGHT),
            call(10.0, 900.0, UNMATCHED),
            call(100.0, 50.0, "gate"),
            call(1000.0, 300.0, "gate"),
            call(1100.0, 300.0, "gate"),
        ];
        let s = busiest_start(&calls, 600.0);
        assert_eq!(window_load(&calls, s, 600.0), 600.0);
        assert_eq!(s, 1000.0, "the earliest call start of the tied windows");
    }

    /// bench_window_straddle: a call that started before the window is
    /// not replayed, so it must not count towards choosing the window.
    #[test]
    fn busiest_window_is_scored_by_the_calls_it_replays() {
        let calls = [call(0.0, 1000.0, "gate"), call(900.0, 100.0, "gate")];
        let replayed = |s: f64| -> f64 {
            calls
                .iter()
                .filter(|c| in_window(c, s, 600.0))
                .map(|c| c.dur_s)
                .sum()
        };
        let s = busiest_start(&calls, 600.0);
        // Clipped scoring picked 400 (700 call-seconds, 600 of them from
        // the t = 0 call) and then replayed 100.
        assert_eq!(s, 0.0);
        assert_eq!(window_load(&calls, s, 600.0), replayed(s));
        assert_eq!(replayed(s), 1000.0);
    }

    #[test]
    fn busiest_window_beats_a_brute_force_scan() {
        let calls = [
            call(0.0, 100.0, "gate"),
            call(250.0, 40.0, "gate"),
            call(1000.0, 1000.0, "gate"),
            call(1900.0, 500.0, "gate"),
            call(2600.0, 30.0, "gate"),
        ];
        for len in [50.0, 300.0, 600.0, 1500.0] {
            let best = (0..3000)
                .map(|s| window_load(&calls, f64::from(s), len))
                .fold(0.0, f64::max);
            let s = busiest_start(&calls, len);
            assert_eq!(window_load(&calls, s, len), best, "len {len}");
        }
    }

    #[test]
    fn trace_needs_a_header_first() {
        let header = r#"{"v":1,"kind":"header","host":"linuxpc","cores":16,"ram_gb":31,"date":"2026-10-08","window_s":86400,"source":"museum","calls_total":1,"calls_kept":1,"calls_dropped":0}"#;
        let c = r#"{"v":1,"kind":"call","t":5.0,"dur_s":1,"dur_src":"durationMs","bg":false,"rule":"gate","role":"worker","agent":"a1","project":"p1"}"#;
        let (h, calls) = parse_trace(&format!("{header}\n{c}\n")).unwrap();
        assert_eq!((h.cores, calls.len()), (16, 1));
        assert!(parse_trace(c).is_err());
        assert!(parse_trace(&format!("{header}\n{header}")).is_err());
        assert!(parse_trace(&header.replace("\"v\":1", "\"v\":2")).is_err());
    }
}
