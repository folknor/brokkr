//! The pbfhogg CLI's command interval (`docs/commands/library.md`, Isolation
//! scenarios): the untagged process-stream markers `CLI_COMMAND_START` and
//! `CLI_COMMAND_END` a CLI process writes around its dispatched command, on
//! its own process-start clock. A process writes at most one pair, and the
//! FIFO is best effort, so exactly one of each is an interval and anything
//! else is no valid timing evidence: the process wall is never substituted.

use crate::sidecar::SidecarData;

/// The marker opening the CLI's command interval.
pub(crate) const START: &str = "CLI_COMMAND_START";

/// The marker closing it.
pub(crate) const END: &str = "CLI_COMMAND_END";

/// The interval definition this brokkr reads, as stored rows name it.
pub(crate) const DEFINITION: &str = "cli-interval-v1";

/// One CLI process's command interval, in microseconds since its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interval {
    pub(crate) start_us: i64,
    pub(crate) end_us: i64,
}

impl Interval {
    pub(crate) fn wall_us(self) -> i64 {
        self.end_us - self.start_us
    }
}

/// The interval in `data`, a CLI process that ran for `elapsed` (spawn to
/// the ending brokkr observed): exactly one untagged `START` and one `END`,
/// `0 <= START <= END`, `END` at most the observed wall plus one second, and
/// no capture loss that may have dropped a FIFO record. Otherwise why there
/// is no valid interval.
pub(crate) fn read(data: &SidecarData, elapsed: std::time::Duration) -> Result<Interval, String> {
    let loss = data.summary.loss;
    if loss.stream_lost() {
        return Err(format!(
            "the capture may have lost a FIFO record ({})",
            loss.stream_causes().join("; ")
        ));
    }
    let stamps = |name: &str| -> Vec<i64> {
        data.markers()
            .filter(|m| m.call.is_none() && m.name == name)
            .map(|m| m.timestamp_us)
            .collect()
    };
    let (starts, ends) = (stamps(START), stamps(END));
    let (start_us, end_us) = match (starts.as_slice(), ends.as_slice()) {
        ([start], [end]) => (*start, *end),
        ([], []) => return Err(format!("the CLI wrote neither {START} nor {END}")),
        _ => {
            return Err(format!(
                "the CLI wrote {} and {}, not exactly one of each",
                crate::output::count(starts.len(), START),
                crate::output::count(ends.len(), END)
            ));
        }
    };
    if start_us < 0 || end_us < start_us {
        return Err(format!(
            "{START} at {start_us} us and {END} at {end_us} us are not in order from 0"
        ));
    }
    let limit_us = i64::try_from(elapsed.as_micros())
        .unwrap_or(i64::MAX)
        .saturating_add(1_000_000);
    if end_us > limit_us {
        return Err(format!(
            "{END} at {end_us} us is past the process wall brokkr observed ({} us, plus 1 s)",
            limit_us - 1_000_000
        ));
    }
    Ok(Interval { start_us, end_us })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::sidecar::{Marker, Record};

    fn data(markers: &[(Option<&str>, &str, i64)]) -> SidecarData {
        let records = markers
            .iter()
            .enumerate()
            .map(|(i, (call, name, ts))| {
                Record::Marker(Marker {
                    marker_idx: i32::try_from(i).unwrap(),
                    timestamp_us: *ts,
                    name: (*name).to_owned(),
                    run_idx: 0,
                    call: call.map(str::to_owned),
                })
            })
            .collect();
        SidecarData::for_test(records)
    }

    const WALL: std::time::Duration = std::time::Duration::from_millis(50);

    #[test]
    fn one_pair_in_order_is_the_interval_and_other_markers_are_ignored() {
        let d = data(&[
            (None, START, 1_000),
            (None, "SORT_START", 1_500),
            (Some("a"), END, 1_600),
            (None, END, 9_000),
        ]);
        let interval = read(&d, WALL).unwrap();
        assert_eq!(interval.wall_us(), 8_000);
    }

    #[test]
    fn anything_but_one_valid_pair_is_no_interval() {
        for (markers, why) in [
            (vec![], "neither"),
            (vec![(None, START, 1)], "not exactly one of each"),
            (
                vec![(None, START, 1), (None, END, 2), (None, END, 3)],
                "2 CLI_COMMAND_ENDs",
            ),
            (vec![(None, START, 9), (None, END, 2)], "not in order"),
            (vec![(None, START, -1), (None, END, 2)], "not in order"),
            (
                vec![(None, START, 1), (None, END, 2_000_000)],
                "past the process wall",
            ),
        ] {
            let e = read(&data(&markers), WALL).unwrap_err();
            assert!(e.contains(why), "{markers:?}: {e}");
        }
    }

    #[test]
    fn a_lossy_capture_is_no_interval() {
        let mut d = data(&[(None, START, 1), (None, END, 2)]);
        d.summary.loss.rejected_lines = 1;
        assert!(read(&d, WALL).unwrap_err().contains("may have lost"));
    }
}
