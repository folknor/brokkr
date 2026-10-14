//! libtest JSON parsing and reconstruction into the human output shape.

use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use super::read_chunk;
use super::{Chunk, ObsEvent, StreamEnd, StreamTap, TestResult, TestTracker};
use serde_json::Value;

/// Line-buffered drain of the parallel runner's stdout, which is libtest's
/// JSON event stream (plus, in `--json` mode, cargo's own `{"reason":...}`
/// message lines).
///
/// Each line is fed to a [`JsonReconstructor`], which updates the shared
/// [`TestTracker`] (so the watchdog can age in-flight tests) and turns the JSON
/// events back into the human libtest text the downstream cargo parsers expect.
/// The reconstructed text - not the raw JSON - is what lands in `buf`; cargo
/// message lines and any non-JSON output pass through verbatim.
pub(super) fn drain_libtest_json<R, F>(
    mut pipe: R,
    buf: &Mutex<Vec<u8>>,
    tracker: &Mutex<TestTracker>,
    cancel: &AtomicBool,
    tap: Option<StreamTap>,
    mut forward_line: F,
) where
    R: Read + std::os::fd::AsRawFd,
    F: FnMut(&str),
{
    let mut recon = JsonReconstructor {
        tap,
        ..JsonReconstructor::default()
    };
    let emit = |out: &[String], buf: &Mutex<Vec<u8>>, forward_line: &mut F| {
        for text in out {
            if let Ok(mut b) = buf.lock() {
                b.extend_from_slice(text.as_bytes());
                b.push(b'\n');
            }
            forward_line(text);
        }
    };

    let mut read_buf = [0_u8; 4096];
    let mut line = Vec::<u8>::new();
    let end = loop {
        let n = match read_chunk(&mut pipe, &mut read_buf, cancel) {
            Chunk::Data(n) => n,
            Chunk::Eof => break StreamEnd::Eof,
            Chunk::Error => break StreamEnd::ReadError,
            Chunk::Cancelled => break StreamEnd::Cancelled,
        };
        for &byte in &read_buf[..n] {
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let out = recon.observe(&String::from_utf8_lossy(&line), tracker);
                emit(&out, buf, &mut forward_line);
                line.clear();
            } else {
                line.push(byte);
            }
        }
    };
    if !line.is_empty() {
        let out = recon.observe(&String::from_utf8_lossy(&line), tracker);
        emit(&out, buf, &mut forward_line);
    }
    recon.report(ObsEvent::StreamEnded { end });
}

/// Turns libtest's JSON event stream back into the human libtest text the
/// downstream parsers ([`crate::test_runner::parse_test_output`],
/// [`crate::test_runner::filter_test`]) expect, while driving the
/// [`TestTracker`] the watchdog ages.
///
/// libtest (nightly, `--format json`) emits one JSON object per line:
/// - `{"type":"suite","event":"started","test_count":N}`
/// - `{"type":"test","event":"started","name":"..."}`
/// - `{"type":"test","event":"ok"|"failed"|"ignored","name":"...","stdout":"..."}`
/// - `{"type":"suite","event":"ok"|"failed","passed":P,"failed":F,...}`
///
/// Failing tests are buffered until the suite summary, then rendered as
/// libtest's two-part `failures:` block (detail `---- name stdout ----` blocks
/// followed by the name list) so the parser recovers the panic messages exactly
/// as it does from a serial run.
#[derive(Default)]
pub(super) struct JsonReconstructor {
    /// (test name, captured stdout) for failures seen since the last suite
    /// summary, held back so they render as one block in libtest order.
    pub(super) failures: Vec<(String, Option<String>)>,
    /// Where each recognised record is reported as a typed observation, when
    /// the caller asked for them. Only records parsed off the stream are
    /// reported - text this reconstructor synthesizes (a dead suite's closure)
    /// is display, never evidence.
    pub(super) tap: Option<StreamTap>,
}

/// Split a stdout line into any leading test output and a trailing libtest
/// event, or `None` when the line carries no event.
///
/// # Why a line can carry both
///
/// libtest writes each JSON record, newline included, as one write, and reserves
/// no boundary against whatever preceded it. A test that prints without a
/// trailing newline - `print!("breadcrumb")` - therefore produces
/// `breadcrumb{"type":"test","event":"ok",...}` on a single line. Requiring the
/// line to *start* with `{` dropped that event on the floor: the test stayed in
/// the tracker as in-flight, its duration inflated to the next transition, and
/// the per-test clock aged the wrong thing. This is reachable without
/// `--nocapture` and without malice, because libtest's capture installs Rust's
/// capture mechanism rather than redirecting the process's stdout descriptor, so
/// a `write!(std::io::stdout(), ..)` or a subprocess that inherited the
/// descriptor writes straight past it.
///
/// # Why right to left
///
/// Candidate boundaries are tried from the last `{` backwards. If test output
/// itself ends in JSON immediately before the real record, only the real
/// record's `{` parses cleanly through end-of-line, so the later boundary is the
/// correct one. Scanning left to right would hand the earlier, wrong object back.
///
/// # What is deliberately not attempted
///
/// A candidate is accepted only if the suffix parses whole as one JSON value and
/// carries a *recognised* libtest `type`/`event` pair. Arbitrary JSON is not an
/// event merely because it has a `type` field, and cargo's own
/// `--message-format=json` records (`reason`, no `type`) pass through untouched.
///
/// This cannot be made exact. A test can print a byte-for-byte valid libtest
/// record on its own line, and no amount of scanning distinguishes that from the
/// real thing - the limitation is in sharing one stream between a protocol and
/// arbitrary output, not in this function. It is tolerable precisely because
/// these events no longer decide when anything is killed: being wrong costs a
/// wrong name in a report, not a terminated run.
pub(super) fn split_trailing_event(line: &str) -> Option<(&str, Value)> {
    if !line.contains('{') {
        return None;
    }
    let mut candidates: Vec<usize> = line.match_indices('{').map(|(i, _)| i).collect();
    candidates.reverse();
    for start in candidates {
        let Ok(val) = serde_json::from_str::<Value>(line[start..].trim_end()) else {
            continue;
        };
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        let event = val.get("event").and_then(Value::as_str).unwrap_or("");
        if !is_known_libtest_event(kind, event) {
            // Parsed, but not one of ours. Do not keep scanning left for a
            // "better" object: a valid non-event JSON tail means the line is not
            // an event line.
            return None;
        }
        // Verbatim, deliberately not trimmed: trailing spaces before the record
        // are the test's own output, and downstream status-line filtering
        // classifies a prefix by its exact text.
        return Some((&line[..start], val));
    }
    None
}

/// The `type`/`event` pairs libtest actually emits. Anything else is not an
/// event, however JSON-shaped.
fn is_known_libtest_event(kind: &str, event: &str) -> bool {
    matches!(
        (kind, event),
        ("suite", "started" | "ok" | "failed")
            | ("test", "started" | "ok" | "failed" | "ignored" | "timeout")
    )
}

impl JsonReconstructor {
    /// Consume one raw stdout line, updating `tracker` and returning the human
    /// libtest lines to emit for it (possibly none, or several).
    pub(super) fn observe(&mut self, line: &str, tracker: &Mutex<TestTracker>) -> Vec<String> {
        let Some((prefix, val)) = split_trailing_event(line) else {
            // No libtest event on this line: test output, a stray print, or
            // cargo's own `--message-format=json` record (which has `reason`
            // and no `type`). Pass through untouched.
            return vec![line.to_owned()];
        };
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        let event = val.get("event").and_then(Value::as_str).unwrap_or("");
        // Output that ran into the event keeps its own line, verbatim and first.
        let mut out = if prefix.is_empty() {
            Vec::new()
        } else {
            vec![prefix.to_owned()]
        };
        out.extend(self.observe_event(kind, event, &val, tracker));
        out
    }

    /// Handle one recognised libtest event.
    fn observe_event(
        &mut self,
        kind: &str,
        event: &str,
        val: &Value,
        tracker: &Mutex<TestTracker>,
    ) -> Vec<String> {
        let count = |key: &str| val.get(key).and_then(Value::as_u64).unwrap_or(0);
        match (kind, event) {
            ("suite", "started") => {
                self.failures.clear();
                // Execution has begun: the per-test cap now bounds the whole run,
                // so a lost start record cannot buy a test the idle ceiling.
                if let Ok(mut t) = tracker.lock() {
                    t.observe_suite_start();
                }
                let test_count = count("test_count");
                self.report(ObsEvent::SuiteStarted { test_count });
                vec![String::new(), format!("running {test_count} tests")]
            }
            ("suite", _) => {
                // A summary closes the suite, so the next suite-started record is
                // a real boundary rather than a replay.
                if let Ok(mut t) = tracker.lock() {
                    t.observe_suite_end();
                }
                self.report(ObsEvent::SuiteFinished {
                    passed: count("passed"),
                    failed: count("failed"),
                    ignored: count("ignored"),
                });
                self.render_suite_summary(val, event)
            }
            ("test", "started") => {
                if let Some(name) = val.get("name").and_then(Value::as_str) {
                    if let Ok(mut t) = tracker.lock() {
                        t.observe_start(name.to_owned());
                    }
                    self.report(ObsEvent::Started {
                        name: name.to_owned(),
                    });
                }
                Vec::new()
            }
            ("test", "ok") => {
                let name = self.finish(val, tracker, TestResult::Ok);
                name.map(|n| vec![format!("test {n} ... ok")])
                    .unwrap_or_default()
            }
            ("test", "ignored") => {
                // Not a run: see `TestTracker::observe_result`.
                let name = self.finish(val, tracker, TestResult::Ignored);
                name.map(|n| vec![format!("test {n} ... ignored")])
                    .unwrap_or_default()
            }
            ("test", "failed") => {
                let Some(name) = self.finish(val, tracker, TestResult::Failed) else {
                    return Vec::new();
                };
                let captured = val.get("stdout").and_then(Value::as_str).map(str::to_owned);
                self.failures.push((name.clone(), captured));
                vec![format!("test {name} ... FAILED")]
            }
            // libtest's slow-test warning. The test is still running, so it
            // changes nothing in the tracker - but it is reported rather than
            // dropped: under brokkr's 20s cap a test can only get there by a
            // budget that was not enforced, which accounting must see.
            ("test", "timeout") => {
                if let Some(name) = val.get("name").and_then(Value::as_str) {
                    self.report(ObsEvent::SlowWarning {
                        name: name.to_owned(),
                    });
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn report(&self, event: ObsEvent) {
        if let Some(tap) = &self.tap {
            tap.emit(event);
        }
    }

    /// Clear a completed test from the tracker, report its terminal record and
    /// return its name. An ignored result does not count as completed.
    fn finish(
        &self,
        val: &Value,
        tracker: &Mutex<TestTracker>,
        result: TestResult,
    ) -> Option<String> {
        let name = val.get("name").and_then(Value::as_str)?.to_owned();
        if let Ok(mut t) = tracker.lock() {
            t.observe_result(&name, result != TestResult::Ignored);
        }
        self.report(ObsEvent::Finished {
            name: name.clone(),
            result,
        });
        Some(name)
    }

    /// Render the end-of-suite `failures:` block (if any) plus the
    /// `test result:` summary line from a `suite` `ok`/`failed` event.
    pub(super) fn render_suite_summary(&mut self, val: &Value, event: &str) -> Vec<String> {
        let count = |key: &str| val.get(key).and_then(Value::as_u64).unwrap_or(0);
        let (passed, failed) = (count("passed"), count("failed"));
        let (ignored, measured) = (count("ignored"), count("measured"));
        let filtered_out = count("filtered_out");
        let exec_time = val.get("exec_time").and_then(Value::as_f64).unwrap_or(0.0);

        let mut out = Vec::new();
        if !self.failures.is_empty() {
            out.push(String::new());
            out.push("failures:".to_owned());
            for (name, captured) in &self.failures {
                if let Some(cap) = captured
                    && !cap.is_empty()
                {
                    out.push(String::new());
                    out.push(format!("---- {name} stdout ----"));
                    for l in cap.lines() {
                        out.push(l.to_owned());
                    }
                }
            }
            out.push(String::new());
            out.push("failures:".to_owned());
            for (name, _) in &self.failures {
                out.push(format!("    {name}"));
            }
            out.push(String::new());
        }
        self.failures.clear();

        let verb = if failed > 0 || event == "failed" {
            "FAILED"
        } else {
            "ok"
        };
        out.push(format!(
            "test result: {verb}. {passed} passed; {failed} failed; \
             {ignored} ignored; {measured} measured; {filtered_out} filtered out; \
             finished in {exec_time:.2}s"
        ));
        out
    }
}
