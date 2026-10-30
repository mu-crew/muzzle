//! `runs.jsonl` and its monthly gzip archives (spec 0.2 P11, replacing D27's
//! 30-day prune). [`append`] rotates and appends under the caller's state
//! lock; [`read`] gives `report` the lines of a period; [`stats`] gives
//! `doctor` the sizes.
//!
//! Archives are `runs-YYYY-MM.jsonl.gz`, named for the month of their first
//! line, then `-2`, `-3`, … when that name is taken (a crash, a clock jump,
//! a size rotation). They are kept forever: about 2–5 MB of gzip a year at
//! linuxpc's rate.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use serde::Serialize;

use crate::state::RunRecord;

pub const LIVE: &str = "runs.jsonl";
/// P11: a busy month rotates once the live file passes this, so it can't
/// grow without bound. Doctor warns above it: rotation is failing.
pub const LIVE_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// P11: doctor warns once the archives pass this.
pub const ARCHIVES_WARN_BYTES: u64 = 256 * 1024 * 1024;

/// Appends `records` to `runs.jsonl`, rotating it first when its first line
/// is from an earlier UTC month than `now_ms` or it has passed
/// [`LIVE_MAX_BYTES`]. The caller holds the state lock, so nothing appends
/// between the archive and the truncate.
pub fn append(dir: &Path, records: &[RunRecord], now_ms: u64) -> anyhow::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    rotate(dir, now_ms)?;
    let mut buf = Vec::new();
    for r in records {
        serde_json::to_writer(&mut buf, r)?;
        buf.push(b'\n');
    }
    let path = dir.join(LIVE);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(&buf))
        .with_context(|| format!("appending to {}", path.display()))
}

fn rotate(dir: &Path, now_ms: u64) -> anyhow::Result<()> {
    let path = dir.join(LIVE);
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first)?;
    let now_month = &crate::admit::rfc3339(now_ms)[..7];
    // A first line with no readable `end` (torn, hand-edited) never forces
    // a month rotation; the size bound still applies, named for now.
    let month = line_month(&first).unwrap_or_else(|| now_month.to_string());
    if month.as_str() >= now_month && len <= LIVE_MAX_BYTES {
        return Ok(());
    }
    archive(dir, &month)?;
    // Only after the archive is durable: a crash before this leaves the
    // lines in both files, which `read` drops as duplicates.
    File::create(&path).with_context(|| format!("truncating {}", path.display()))?;
    Ok(())
}

/// Gzips `runs.jsonl` into the next free `runs-<month>[-N].jsonl.gz`:
/// temp file, fsync, rename. Leaves `runs.jsonl` as it was.
fn archive(dir: &Path, month: &str) -> anyhow::Result<PathBuf> {
    let tmp = dir.join(format!("runs-{month}.jsonl.gz.tmp"));
    let mut src = File::open(dir.join(LIVE))?;
    let out = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut gz = GzEncoder::new(out, Compression::default());
    io::copy(&mut src, &mut gz)?;
    gz.finish()?
        .sync_all()
        .with_context(|| format!("syncing {}", tmp.display()))?;
    let dest = (1..)
        .map(|n| dir.join(archive_name(month, n)))
        .find(|p| !p.exists())
        .expect("an unbounded range has a free name");
    fs::rename(&tmp, &dest).with_context(|| format!("renaming to {}", dest.display()))?;
    // The rename is durable only once the directory is. Best effort: not
    // every filesystem lets a directory be fsynced.
    let _ = File::open(dir).and_then(|d| d.sync_all());
    Ok(dest)
}

fn archive_name(month: &str, n: u32) -> String {
    if n == 1 {
        format!("runs-{month}.jsonl.gz")
    } else {
        format!("runs-{month}-{n}.jsonl.gz")
    }
}

/// `YYYY-MM` of a line's `end`, which is RFC 3339 UTC.
fn line_month(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct End {
        end: String,
    }
    let end = serde_json::from_str::<End>(line.trim_end()).ok()?.end;
    let m = end.get(..7)?;
    month_start(m).is_some().then(|| m.to_string())
}

/// Unix seconds at the start of month `YYYY-MM`, UTC.
fn month_start(m: &str) -> Option<f64> {
    let (y, mo) = m.split_once('-')?;
    if y.len() != 4 || mo.len() != 2 {
        return None;
    }
    let date = jiff::civil::Date::new(y.parse().ok()?, mo.parse().ok()?, 1).ok()?;
    let ts = date.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp();
    Some(ts.as_second() as f64)
}

/// `[start, end)` of month `YYYY-MM` in Unix seconds.
fn month_span(m: &str) -> Option<(f64, f64)> {
    let start = month_start(m)?;
    let (y, mo) = m.split_once('-')?;
    let (y, mo): (i32, u32) = (y.parse().ok()?, mo.parse().ok()?);
    let next = if mo == 12 {
        format!("{:04}-01", y + 1)
    } else {
        format!("{y:04}-{:02}", mo + 1)
    };
    Some((start, month_start(&next)?))
}

/// The archives in `dir` as `(month, suffix, path)`, oldest first.
fn archives(dir: &Path) -> anyhow::Result<Vec<(String, u32, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name
            .to_str()
            .and_then(|n| n.strip_prefix("runs-")?.strip_suffix(".jsonl.gz"))
        else {
            continue;
        };
        let (month, n) = match stem.get(7..) {
            Some("") => (&stem[..7], 1),
            Some(rest) => match rest.strip_prefix('-').and_then(|n| n.parse().ok()) {
                Some(n) => (&stem[..7], n),
                None => continue,
            },
            None => continue,
        };
        if month_start(month).is_some() {
            out.push((month.to_string(), n, entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// The run lines of `period` (`[since, until)` in Unix seconds; `None` is
/// everything): every archive whose month overlaps it, then `runs.jsonl`,
/// with exact duplicate lines dropped (a crash between archive and
/// truncate). Archives outside the period are never opened. Lines inside
/// a read archive are not filtered: `report` filters by period itself.
pub fn read(dir: &Path, period: Option<(f64, f64)>) -> anyhow::Result<String> {
    let mut texts = Vec::new();
    for (month, _, path) in archives(dir)? {
        let overlaps = match (period, month_span(&month)) {
            (Some((since, until)), Some((start, end))) => start < until && since < end,
            _ => true,
        };
        if !overlaps {
            continue;
        }
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|f| MultiGzDecoder::new(f).read_to_end(&mut bytes))
            .with_context(|| format!("reading {}", path.display()))?;
        texts.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    let live = dir.join(LIVE);
    match fs::read(&live) {
        Ok(bytes) => texts.push(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", live.display())),
    }
    let mut seen = HashSet::new();
    let mut out = String::new();
    for line in texts.iter().flat_map(|t| t.lines()) {
        if seen.insert(line) {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// What doctor shows of the run log (P11).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Stats {
    pub live_bytes: u64,
    pub archives: usize,
    pub archive_bytes: u64,
    /// `YYYY-MM` of the oldest archive.
    pub oldest_month: Option<String>,
}

pub fn stats(dir: &Path) -> anyhow::Result<Stats> {
    let live_bytes = match fs::metadata(dir.join(LIVE)) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    let list = archives(dir)?;
    let mut archive_bytes = 0;
    for (_, _, p) in &list {
        archive_bytes += fs::metadata(p)?.len();
    }
    Ok(Stats {
        live_bytes,
        archives: list.len(),
        archive_bytes,
        oldest_month: list.first().map(|(m, _, _)| m.clone()),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const DAY: u64 = 86_400_000;

    /// 2026-11-15T00:00:00Z in ms.
    fn nov15() -> u64 {
        "2026-11-15T00:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond() as u64
    }

    fn rec(end_ms: u64) -> RunRecord {
        let mut r: RunRecord = serde_json::from_value(json!({
            "v":1,"end":"x","rule":null,"slots":1,"class":"high","cfg":null,"agent":null,
            "cwd":"/","waited_s":0.0,"ran_s":0.0,"exit":0,"killed":null,"cpu_s":null,
            "mem_peak_mb":null,"enforced":false,"nested":false,"kept":false,"budget":4
        }))
        .unwrap();
        r.end = crate::admit::rfc3339(end_ms);
        r
    }

    fn gunzip(p: &Path) -> String {
        let mut s = String::new();
        MultiGzDecoder::new(File::open(p).unwrap())
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn month_rollover_archives_the_previous_month() {
        let dir = tempfile::tempdir().unwrap();
        let oct = nov15() - 20 * DAY;
        append(dir.path(), &[rec(oct), rec(oct + DAY)], oct + DAY).unwrap();
        // Same month: no rotation.
        assert_eq!(names(dir.path()), ["runs.jsonl"]);
        append(dir.path(), &[rec(nov15())], nov15()).unwrap();
        assert_eq!(names(dir.path()), ["runs-2026-10.jsonl.gz", "runs.jsonl"]);
        let old = gunzip(&dir.path().join("runs-2026-10.jsonl.gz"));
        assert_eq!(old.lines().count(), 2);
        assert!(old.contains("2026-10-26T"), "{old}");
        let live = fs::read_to_string(dir.path().join(LIVE)).unwrap();
        assert_eq!(live.lines().count(), 1);
        assert!(live.contains("2026-11-15T"));
        // Everything is still readable as one log.
        assert_eq!(read(dir.path(), None).unwrap().lines().count(), 3);
    }

    #[test]
    fn size_rollover_and_taken_names_get_a_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join(LIVE);
        let line = serde_json::to_string(&rec(nov15())).unwrap() + "\n";
        // Under the bound: nothing rotates.
        fs::write(&live, line.repeat((LIVE_MAX_BYTES as usize) / line.len())).unwrap();
        append(dir.path(), &[rec(nov15())], nov15()).unwrap();
        assert_eq!(names(dir.path()), ["runs.jsonl"]);
        // Past it: same month, so the name is free and then -2.
        append(dir.path(), &[rec(nov15())], nov15()).unwrap();
        assert_eq!(names(dir.path()), ["runs-2026-11.jsonl.gz", "runs.jsonl"]);
        assert!(fs::metadata(&live).unwrap().len() < 1000);
        fs::write(
            &live,
            line.repeat((LIVE_MAX_BYTES as usize) / line.len() + 2),
        )
        .unwrap();
        append(dir.path(), &[rec(nov15())], nov15()).unwrap();
        assert_eq!(
            names(dir.path()),
            [
                "runs-2026-11-2.jsonl.gz",
                "runs-2026-11.jsonl.gz",
                "runs.jsonl"
            ]
        );
        let s = stats(dir.path()).unwrap();
        assert_eq!(
            (s.archives, s.oldest_month.as_deref()),
            (2, Some("2026-11"))
        );
        assert!(s.archive_bytes > 0 && s.live_bytes < 1000, "{s:?}");
    }

    #[test]
    fn crash_between_rename_and_truncate_leaves_duplicates_read_drops() {
        let dir = tempfile::tempdir().unwrap();
        let oct = nov15() - 20 * DAY;
        append(dir.path(), &[rec(oct), rec(oct + 1000)], oct).unwrap();
        // The crash: archived, never truncated.
        archive(dir.path(), "2026-10").unwrap();
        assert_eq!(read(dir.path(), None).unwrap().lines().count(), 2);
        // The next append archives the same lines again under -2.
        append(dir.path(), &[rec(nov15())], nov15()).unwrap();
        assert!(dir.path().join("runs-2026-10-2.jsonl.gz").exists());
        let text = read(dir.path(), None).unwrap();
        assert_eq!(text.lines().count(), 3, "{text}");
    }

    #[test]
    fn read_skips_archives_outside_the_period_unopened() {
        let dir = tempfile::tempdir().unwrap();
        // Not gzip: opening it is an error, so a pass proves it was skipped.
        fs::write(dir.path().join("runs-2026-09.jsonl.gz"), "not gzip").unwrap();
        let oct = nov15() - 20 * DAY;
        append(dir.path(), &[rec(oct)], oct).unwrap();
        append(dir.path(), &[rec(nov15())], nov15()).unwrap();
        let since = month_start("2026-10").unwrap();
        let text = read(dir.path(), Some((since, f64::INFINITY))).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(read(dir.path(), Some((since - 1.0, f64::INFINITY))).is_err());
        assert!(read(dir.path(), None).is_err());
        // November only: the October archive is skipped too.
        let nov = month_start("2026-11").unwrap();
        let text = read(dir.path(), Some((nov, f64::INFINITY))).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
    }

    #[test]
    fn months_and_names() {
        assert_eq!(
            month_span("2026-12").unwrap().1,
            month_start("2027-01").unwrap()
        );
        assert_eq!(month_start("2026-13"), None);
        assert_eq!(
            line_month("{\"end\":\"2026-10-05T00:00:00Z\"}\n").as_deref(),
            Some("2026-10")
        );
        assert_eq!(line_month("torn"), None);
        let dir = tempfile::tempdir().unwrap();
        for n in [
            "runs-2026-10.jsonl.gz",
            "runs-2026-10-3.jsonl.gz",
            "runs-2026-10.jsonl.gz.tmp",
            "runs-2026-1x.jsonl.gz",
            "runs-2026-10-x.jsonl.gz",
        ] {
            fs::write(dir.path().join(n), "").unwrap();
        }
        let a: Vec<(String, u32)> = archives(dir.path())
            .unwrap()
            .into_iter()
            .map(|(m, n, _)| (m, n))
            .collect();
        assert_eq!(a, [("2026-10".into(), 1), ("2026-10".into(), 3)]);
    }

    #[test]
    fn a_torn_first_line_never_forces_a_month_rotation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(LIVE), "torn\n").unwrap();
        append(dir.path(), &[rec(nov15())], nov15()).unwrap();
        assert_eq!(names(dir.path()), ["runs.jsonl"]);
    }
}
