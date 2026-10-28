//! Bounded-memory readers over a verify step's stdout file.
//!
//! A listing step (a diff of two PBFs, an osmium diff) writes its stdout to a
//! file under the check's directory, because its size scales with the data:
//! at planet scale a listing can run to gigabytes. Everything here reads such
//! a file through fixed buffers, so no reader holds more than a bounded
//! amount of it, whatever its length and however long its lines are.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use crate::error::DevError;

/// Lines and bytes per line a listing preview replays into the check detail.
pub(super) const PREVIEW_LINES: usize = 50;
pub(super) const PREVIEW_LINE_BYTES: usize = 240;

fn open(path: &Path) -> Result<BufReader<File>, DevError> {
    File::open(path)
        .map(BufReader::new)
        .map_err(|e| DevError::Verify(format!("cannot open {}: {e}", path.display())))
}

fn read_error(path: &Path, e: &io::Error) -> DevError {
    DevError::Verify(format!("cannot read {}: {e}", path.display()))
}

/// Call `f` once per line with at most the first `head` bytes of the line
/// (newline excluded), however long the line is. Lines may be split across
/// buffer refills; the head is assembled across them.
pub(super) fn for_each_line_head(
    path: &Path,
    head: usize,
    f: impl FnMut(&[u8]),
) -> Result<(), DevError> {
    line_heads(open(path)?, head, f).map_err(|e| read_error(path, &e))
}

fn line_heads(mut reader: impl BufRead, head: usize, mut f: impl FnMut(&[u8])) -> io::Result<()> {
    let mut current: Vec<u8> = Vec::with_capacity(head);
    let mut in_line = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            if in_line {
                f(&current);
            }
            return Ok(());
        }
        let len = buf.len();
        let mut start = 0;
        while start < len {
            in_line = true;
            let rest = &buf[start..];
            let (segment, ended) = match rest.iter().position(|&b| b == b'\n') {
                Some(i) => (&rest[..i], true),
                None => (rest, false),
            };
            let room = head.saturating_sub(current.len());
            current.extend_from_slice(&segment[..segment.len().min(room)]);
            if ended {
                f(&current);
                current.clear();
                in_line = false;
                start += segment.len() + 1;
            } else {
                start = len;
            }
        }
        reader.consume(len);
    }
}

/// The first `max_lines` lines, each cut to `max_line_bytes` (lossy UTF-8),
/// and whether the file held more lines than that.
pub(super) fn preview(
    path: &Path,
    max_lines: usize,
    max_line_bytes: usize,
) -> Result<(Vec<String>, bool), DevError> {
    let mut lines = Vec::new();
    let mut more = false;
    for_each_line_head(path, max_line_bytes, |line| {
        if lines.len() < max_lines {
            lines.push(String::from_utf8_lossy(line).into_owned());
        } else {
            more = true;
        }
    })?;
    Ok((lines, more))
}

/// Whether the file holds any byte that is not ASCII whitespace: a listing
/// is "empty" exactly when this is false.
pub(super) fn has_content(path: &Path) -> Result<bool, DevError> {
    let mut reader = open(path)?;
    loop {
        let buf = reader.fill_buf().map_err(|e| read_error(path, &e))?;
        if buf.is_empty() {
            return Ok(false);
        }
        if buf.iter().any(|b| !b.is_ascii_whitespace()) {
            return Ok(true);
        }
        let len = buf.len();
        reader.consume(len);
    }
}

/// Newline-terminated lines plus a final unterminated one.
pub(super) fn count_lines(path: &Path) -> Result<u64, DevError> {
    let mut count = 0u64;
    for_each_line_head(path, 0, |_| count += 1)?;
    Ok(count)
}

/// The byte offset at which two files first differ (a length difference
/// counts at the shorter one's end), or `None` when they are identical.
pub(super) fn first_difference(a: &Path, b: &Path) -> Result<Option<u64>, DevError> {
    let fa = File::open(a).map_err(|e| read_error(a, &e))?;
    let fb = File::open(b).map_err(|e| read_error(b, &e))?;
    crate::osc::first_stream_difference(fa, fb).map_err(|e| {
        DevError::Verify(format!(
            "cannot compare {} with {}: {e}",
            a.display(),
            b.display()
        ))
    })
}

/// The whole file as text when it is at most `cap` bytes; larger is an
/// error naming the file, for a report whose size does not scale with the
/// data (a reference tool's counts) and so is never legitimately large.
pub(super) fn read_bounded(path: &Path, cap: u64) -> Result<String, DevError> {
    let len = std::fs::metadata(path)
        .map_err(|e| read_error(path, &e))?
        .len();
    if len > cap {
        return Err(DevError::Verify(format!(
            "{} is {len} bytes, more than the {cap} a report of counts can need",
            path.display()
        )));
    }
    let bytes = std::fs::read(path).map_err(|e| read_error(path, &e))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::line_heads;

    fn heads(data: &[u8], head: usize, capacity: usize) -> Vec<Vec<u8>> {
        let reader = std::io::BufReader::with_capacity(capacity, data);
        let mut out = Vec::new();
        line_heads(reader, head, |l| out.push(l.to_vec())).unwrap();
        out
    }

    #[test]
    fn line_heads_are_bounded_and_survive_buffer_splits() {
        let data = b"*n1 abc\n*w-22 x\n\nlong line without end";
        for capacity in [1, 2, 3, 7, 64] {
            let got = heads(data, 4, capacity);
            assert_eq!(
                got,
                vec![
                    b"*n1 ".to_vec(),
                    b"*w-2".to_vec(),
                    Vec::new(),
                    b"long".to_vec()
                ],
                "capacity {capacity}"
            );
        }
    }

    #[test]
    fn line_heads_count_terminated_and_trailing_lines() {
        assert_eq!(heads(b"", 3, 8).len(), 0);
        assert_eq!(heads(b"a\n", 3, 8).len(), 1);
        assert_eq!(heads(b"a\nb", 3, 8).len(), 2);
    }

    #[test]
    fn file_helpers_read_through_fixed_buffers() {
        let dir = crate::test_scratch::scratch("pbfhogg-verify-stream", "helpers");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let blank = dir.join("blank.txt");
        std::fs::write(&a, "one\ntwo\nthree\n").unwrap();
        std::fs::write(&b, "one\ntwX\nthree\n").unwrap();
        std::fs::write(&blank, " \n\n").unwrap();
        assert_eq!(super::count_lines(&a).unwrap(), 3);
        assert_eq!(super::first_difference(&a, &b).unwrap(), Some(6));
        assert_eq!(super::first_difference(&a, &a).unwrap(), None);
        assert!(super::has_content(&a).unwrap());
        assert!(!super::has_content(&blank).unwrap());
        let (lines, more) = super::preview(&a, 2, 2).unwrap();
        assert_eq!(lines, vec!["on".to_owned(), "tw".to_owned()]);
        assert!(more);
        assert!(super::read_bounded(&a, 4).is_err());
        assert_eq!(super::read_bounded(&a, 64).unwrap(), "one\ntwo\nthree\n");
    }
}
