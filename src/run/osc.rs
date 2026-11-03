//! Program status (OSC 7501) for a queued run: a terminal that supports the
//! protocol (Rex) shows "queued" on the tab while `muzzle run` waits, instead
//! of a silent pane. Spec: <https://www.superlogical.com/rex/docs/build/program-status>.
//!
//! One record, id `muzzle`, beside the program's root record (a shell's, or
//! pi's), so it never replaces what the program itself reports. `working`
//! while queued, cleared on admission or giving up. A terminal drops a
//! `working` record by itself when the next prompt starts, so a muzzle that is
//! SIGKILLed leaves nothing behind for long.
//!
//! Where it goes: the controlling terminal (`/dev/tty`), and only when
//! `TERM_PROGRAM` names a terminal known to support it, since a blind write
//! to an unknown terminal is a write of bytes it may show. A harness without
//! a terminal (pi's bash tool has none) gets the same lines through
//! `--status FILE`, rewritten in place, for its own extension to forward.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The record id: one per terminal, so parallel waits in one pane share it.
pub(super) const ID: &str = "muzzle";

/// Terminals that answer the OSC 7501 query. muzzle can't read a reply (its
/// stdin belongs to the job), so it goes by name, as the spec allows.
const SUPPORTING: &[&str] = &["rex"];

/// The body of one report, `OSC 7501 ; … ST`.
pub(super) fn report(state: &str, msg: Option<&str>) -> String {
    let mut s = format!("\x1b]7501;state={state}:id={ID}");
    if state != "clear" {
        s.push_str(":app=muzzle:title=");
        s.push_str(&b64("muzzle"));
        if let Some(m) = msg {
            s.push_str(":msg=");
            s.push_str(&b64(&one_line(m)));
        }
    }
    s.push_str("\x1b\\");
    s
}

/// `queued 2/3 for 6 slots, 14/14 held`: what the record's message says.
pub(super) fn queued_msg(position: usize, of: usize, slots: u32, held: u32, budget: u32) -> String {
    format!("queued {position}/{of} for {slots} slots, {held}/{budget} held")
}

/// The spec refuses a report whose text holds a control character, and caps
/// `msg` at 2048 decoded bytes.
fn one_line(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if out.len() > 2048 {
        let mut end = 2048;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// Standard base64, padded.
fn b64(s: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(s.len().div_ceil(3) * 4);
    for chunk in s.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Where a queued run reports. Every write is best effort: status is a
/// courtesy, and must never fail or slow a run.
pub(super) struct Sink {
    tty: Option<fs::File>,
    file: Option<PathBuf>,
    /// Something was reported, so the end must clear it.
    dirty: bool,
}

impl Sink {
    pub(super) fn open(status_file: Option<&Path>) -> Sink {
        let supported =
            std::env::var("TERM_PROGRAM").is_ok_and(|t| SUPPORTING.contains(&t.as_str()));
        let tty = if supported {
            fs::OpenOptions::new().write(true).open("/dev/tty").ok()
        } else {
            None
        };
        Sink {
            tty,
            file: status_file.map(Path::to_path_buf),
            dirty: false,
        }
    }

    pub(super) fn queued(&mut self, msg: &str) {
        self.dirty = true;
        self.emit(&report("working", Some(msg)));
    }

    /// Admitted or gave up: the record goes.
    pub(super) fn clear(&mut self) {
        if std::mem::take(&mut self.dirty) {
            self.emit(&report("clear", None));
        }
    }

    fn emit(&mut self, seq: &str) {
        if let Some(t) = &mut self.tty {
            let _ = t.write_all(seq.as_bytes());
        }
        if let Some(f) = &self.file {
            // Written whole and renamed, so a reader never sees half a line.
            let tmp = f.with_extension("tmp");
            if fs::write(&tmp, seq).is_ok() {
                let _ = fs::rename(&tmp, f);
            }
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_spec_examples() {
        assert_eq!(b64("Installing updates"), "SW5zdGFsbGluZyB1cGRhdGVz");
        assert_eq!(b64("Photos synced"), "UGhvdG9zIHN5bmNlZA==");
        assert_eq!(b64("ab"), "YWI=");
        assert_eq!(b64(""), "");
    }

    #[test]
    fn a_working_report_names_its_record_and_app() {
        assert_eq!(
            report("working", Some("queued 1/1")),
            "\x1b]7501;state=working:id=muzzle:app=muzzle:title=bXV6emxl:msg=cXVldWVkIDEvMQ==\x1b\\"
        );
    }

    #[test]
    fn clear_carries_only_the_id() {
        assert_eq!(
            report("clear", Some("x")),
            "\x1b]7501;state=clear:id=muzzle\x1b\\"
        );
    }

    #[test]
    fn messages_are_one_line_and_capped() {
        assert_eq!(one_line("a\nb\tc"), "a b c");
        assert_eq!(one_line(&"é".repeat(2000)).len(), 2048);
    }

    #[test]
    fn the_message_shape() {
        assert_eq!(
            queued_msg(2, 3, 6, 14, 14),
            "queued 2/3 for 6 slots, 14/14 held"
        );
    }

    #[test]
    fn a_status_file_holds_the_latest_report_and_ends_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        {
            let mut sink = Sink {
                tty: None,
                file: Some(path.clone()),
                dirty: false,
            };
            sink.queued("queued 1/2");
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                report("working", Some("queued 1/2"))
            );
            sink.queued("queued 1/1");
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                report("working", Some("queued 1/1"))
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), report("clear", None));
    }

    #[test]
    fn a_sink_that_reported_nothing_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        drop(Sink {
            tty: None,
            file: Some(path.clone()),
            dirty: false,
        });
        assert!(!path.exists());
    }
}
