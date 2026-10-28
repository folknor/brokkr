//! The holder report: what `brokkr lock` shows beyond the legacy status
//! fields, its wire extension, and the one renderer both the socket path and
//! the reader-side fallback use.
//!
//! # Wire
//!
//! The extension rides the existing `status` reply as `x1.*` lines after the
//! legacy keys, which keep their exact names and meanings (`cpu_secs` is the
//! holder's own CPU, `child` a yes/no, `progress` run/total), so an older
//! client decodes a newer holder unchanged: it ignores unknown keys. The
//! extension's encoded size is bounded well under the 64 KiB an older client
//! accepts. Fields within a line are tab-separated; every text value is
//! sanitized of control characters first, so no escaping is needed. A newer
//! client that cannot parse the extension drops it and renders the legacy
//! status alone.
//!
//! # Meaning
//!
//! Every figure is an observation, labelled as one:
//!
//! - process rows are the holder's descendants, every process carrying this
//!   acquisition's `BROKKR_EXEC` token, and members of the hold's cgroup;
//!   "observed", never "all" - a process that cleared its environment, made it
//!   unreadable, or runs in a daemon the hold merely talks to is not in them;
//! - a row's CPU rate comes from two scans about a quarter second apart and is
//!   given only when the same process carried the same token in both;
//! - the summed RSS is the sum of per-process RSS, shared pages counted once
//!   per process; IO counters include children the process has waited for;
//! - "unreadable" counts environments that could not be inspected and whose
//!   process could still belong to the hold: not already a row, and not older
//!   than the hold itself - those may or may not belong to it;
//! - rows are a forest: a process precedes the ones it started, which are
//!   indented beneath it; a row whose parent is not among the rows is a root;
//! - the cgroup line is everything charged to the hold's cgroup since entry.

use super::observe::sanitize;

/// `write!` into a `String`, which cannot fail.
macro_rules! put {
    ($dst:expr, $($arg:tt)*) => {
        $dst.push_str(&format!($($arg)*))
    };
}

/// `writeln!` into a `String`, which cannot fail.
macro_rules! putln {
    ($dst:expr, $($arg:tt)*) => {{
        $dst.push_str(&format!($($arg)*));
        $dst.push('\n');
    }};
}

/// The extension's version key.
const VERSION_KEY: &str = "x1";
/// Most bytes the extension may add to a reply.
const EXT_BUDGET: usize = 24 * 1024;
/// Most activity rows sent.
const MAX_ACTIVITY_ROWS: usize = 64;
/// Bytes kept free for the fixed sample header, cgroup line and omission
/// counts. Every text field is clipped to 200 chars (at most 800 bytes), so
/// these lines stay well inside it.
const RESERVE: usize = 2048;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Extension {
    /// `None` when the holder's activity table was busy at the instant asked.
    pub(crate) activities: Option<Vec<ActivityRow>>,
    /// Open activities left out to keep the reply bounded.
    pub(crate) activities_omitted: usize,
    pub(crate) narration: Option<(String, u64)>,
    pub(crate) marker: Option<(String, u64)>,
    pub(crate) sample: Option<SampleView>,
    /// A fresh process sample was being collected when the reply was built.
    pub(crate) collecting: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivityRow {
    pub(crate) id: u64,
    pub(crate) parent: Option<u64>,
    pub(crate) elapsed_ms: u64,
    pub(crate) progress: Option<(u64, u64)>,
    pub(crate) label: String,
    pub(crate) narration: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SampleView {
    pub(crate) age_ms: u64,
    pub(crate) interval_ms: u64,
    /// The PIDs mean the same thing to the holder's `/proc` as to the
    /// holder itself (its procfs numbers its own namespace).
    pub(crate) pids_meaningful: bool,
    pub(crate) observed: usize,
    pub(crate) omitted: usize,
    pub(crate) unreadable: usize,
    /// Sum of the rows' sampled CPU rates, in thousandths of a core, over every
    /// observed process (omitted rows included).
    pub(crate) cpu_milli_total: u64,
    pub(crate) rss_kb_total: u64,
    pub(crate) rows: Vec<ProcRow>,
    pub(crate) cgroup: Option<CgroupRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProcRow {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    /// How it was found: `T` token, `D` descendant, `C` cgroup member.
    pub(crate) via: String,
    pub(crate) cpu_milli: Option<u64>,
    pub(crate) cpu_ms: u64,
    pub(crate) rss_kb: u64,
    pub(crate) age_ms: u64,
    pub(crate) state: char,
    pub(crate) read_bytes: Option<u64>,
    pub(crate) write_bytes: Option<u64>,
    pub(crate) launched: Option<String>,
    pub(crate) argv: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CgroupRow {
    pub(crate) name: String,
    pub(crate) since_entry_ms: u64,
    pub(crate) cpu_usec: u64,
    pub(crate) cpu_milli: Option<u64>,
    pub(crate) memory_bytes: Option<u64>,
    pub(crate) members: usize,
}

fn opt(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

fn text(value: &str) -> String {
    sanitize(value)
}

impl Extension {
    /// Encode as `x1.*` lines, bounded by [`EXT_BUDGET`].
    pub(crate) fn encode(&self) -> String {
        let mut out = format!("{VERSION_KEY}=1\n");
        if self.collecting {
            out.push_str("x1.collecting=1\n");
        }
        // Every variable line is admitted against the budget, keeping
        // [`RESERVE`] free for the sample header and cgroup line.
        let fits = |out: &str, line: &str| out.len() + line.len() + RESERVE <= EXT_BUDGET;
        if let Some((n, age)) = &self.narration {
            let line = format!("x1.narration={age}\t{}\n", text(n));
            if fits(&out, &line) {
                out.push_str(&line);
            }
        }
        if let Some((m, age)) = &self.marker {
            let line = format!("x1.marker={age}\t{}\n", text(m));
            if fits(&out, &line) {
                out.push_str(&line);
            }
        }
        match &self.activities {
            None => out.push_str("x1.acts=busy\n"),
            Some(rows) => {
                let mut omitted = rows.len().saturating_sub(MAX_ACTIVITY_ROWS);
                for a in rows.iter().take(MAX_ACTIVITY_ROWS) {
                    let (done, total) =
                        a.progress.map_or((None, None), |(d, t)| (Some(d), Some(t)));
                    let line = format!(
                        "x1.act={}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                        a.id,
                        opt(a.parent),
                        a.elapsed_ms,
                        opt(done),
                        opt(total),
                        text(&a.label),
                        text(a.narration.as_deref().unwrap_or("")),
                    );
                    if fits(&out, &line) {
                        out.push_str(&line);
                    } else {
                        omitted += 1;
                    }
                }
                if omitted > 0 {
                    putln!(out, "x1.acts_omitted={omitted}");
                }
            }
        }
        if let Some(s) = &self.sample {
            Self::encode_sample(&mut out, s);
        }
        out
    }

    /// Append the sample header, as many process rows as the budget admits,
    /// and the cgroup line. Rows are selected against the output so far plus
    /// the rows already accepted, with [`RESERVE`] held for the header and
    /// cgroup.
    fn encode_sample(out: &mut String, s: &SampleView) {
        let mut rows = String::new();
        let mut sent = 0usize;
        for r in &s.rows {
            let line = format!(
                "x1.proc={}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                r.pid,
                r.ppid,
                text(&r.via),
                opt(r.cpu_milli),
                r.cpu_ms,
                r.rss_kb,
                r.age_ms,
                if r.state.is_control() || r.state == '\t' {
                    '?'
                } else {
                    r.state
                },
                opt(r.read_bytes),
                opt(r.write_bytes),
                text(r.launched.as_deref().unwrap_or("")),
                text(&r.argv),
            );
            if out.len() + rows.len() + line.len() + RESERVE > EXT_BUDGET {
                break;
            }
            rows.push_str(&line);
            sent += 1;
        }
        let omitted = s.omitted + (s.rows.len() - sent);
        putln!(
            out,
            "x1.sample={}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            s.age_ms,
            s.interval_ms,
            u8::from(s.pids_meaningful),
            s.observed,
            omitted,
            s.unreadable,
            s.cpu_milli_total,
            s.rss_kb_total,
        );
        out.push_str(&rows);
        if let Some(c) = &s.cgroup {
            putln!(
                out,
                "x1.cgroup={}\t{}\t{}\t{}\t{}\t{}",
                text(&c.name),
                c.since_entry_ms,
                c.cpu_usec,
                opt(c.cpu_milli),
                opt(c.memory_bytes),
                c.members,
            );
        }
    }

    /// Decode the `x1.*` lines of a reply. `None` when there is no extension
    /// or any extension line is malformed - the caller then renders the legacy
    /// status alone.
    pub(crate) fn decode(lines: &[&str]) -> Option<Self> {
        let mut ext = Self::default();
        let mut seen_version = false;
        let mut activities = Vec::new();
        let mut activities_busy = false;
        let mut rows = Vec::new();
        let mut cgroup = None;
        for line in lines {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key == VERSION_KEY {
                seen_version = value == "1";
                continue;
            }
            let Some(field) = key.strip_prefix("x1.") else {
                continue;
            };
            let parts: Vec<&str> = value.split('\t').collect();
            match field {
                "acts" => activities_busy = true,
                "acts_omitted" => ext.activities_omitted = num(value)?,
                "act" => activities.push(decode_activity(&parts)?),
                "narration" => ext.narration = Some(decode_aged(&parts)?),
                "marker" => ext.marker = Some(decode_aged(&parts)?),
                "collecting" => ext.collecting = value == "1",
                "sample" => ext.sample = Some(decode_sample(&parts)?),
                "proc" => rows.push(decode_proc(&parts)?),
                "cgroup" => cgroup = Some(decode_cgroup(&parts)?),
                _ => {}
            }
        }
        if !seen_version {
            return None;
        }
        ext.activities = (!activities_busy).then_some(activities);
        if let Some(sample) = ext.sample.as_mut() {
            sample.rows = rows;
            sample.cgroup = cgroup;
        } else if !rows.is_empty() || cgroup.is_some() {
            return None;
        }
        Some(ext)
    }
}

fn num<T: std::str::FromStr>(value: &str) -> Option<T> {
    value.parse().ok()
}

fn opt_num(value: &str) -> Option<Option<u64>> {
    if value.is_empty() {
        Some(None)
    } else {
        value.parse().ok().map(Some)
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn decode_activity(p: &[&str]) -> Option<ActivityRow> {
    if p.len() != 7 {
        return None;
    }
    let done = opt_num(p[3])?;
    let total = opt_num(p[4])?;
    Some(ActivityRow {
        id: num(p[0])?,
        parent: opt_num(p[1])?,
        elapsed_ms: num(p[2])?,
        progress: done.zip(total),
        label: p[5].to_owned(),
        narration: non_empty(p[6]),
    })
}

fn decode_aged(p: &[&str]) -> Option<(String, u64)> {
    if p.len() != 2 {
        return None;
    }
    Some((p[1].to_owned(), num(p[0])?))
}

fn decode_sample(p: &[&str]) -> Option<SampleView> {
    if p.len() != 8 {
        return None;
    }
    Some(SampleView {
        age_ms: num(p[0])?,
        interval_ms: num(p[1])?,
        pids_meaningful: p[2] == "1",
        observed: num(p[3])?,
        omitted: num(p[4])?,
        unreadable: num(p[5])?,
        cpu_milli_total: num(p[6])?,
        rss_kb_total: num(p[7])?,
        rows: Vec::new(),
        cgroup: None,
    })
}

fn decode_proc(p: &[&str]) -> Option<ProcRow> {
    if p.len() != 12 {
        return None;
    }
    Some(ProcRow {
        pid: num(p[0])?,
        ppid: num(p[1])?,
        via: p[2].to_owned(),
        cpu_milli: opt_num(p[3])?,
        cpu_ms: num(p[4])?,
        rss_kb: num(p[5])?,
        age_ms: num(p[6])?,
        state: p[7].chars().next().unwrap_or('?'),
        read_bytes: opt_num(p[8])?,
        write_bytes: opt_num(p[9])?,
        launched: non_empty(p[10]),
        argv: p[11].to_owned(),
    })
}

fn decode_cgroup(p: &[&str]) -> Option<CgroupRow> {
    if p.len() != 6 {
        return None;
    }
    Some(CgroupRow {
        name: p[0].to_owned(),
        since_entry_ms: num(p[1])?,
        cpu_usec: num(p[2])?,
        cpu_milli: opt_num(p[3])?,
        memory_bytes: opt_num(p[4])?,
        members: num(p[5])?,
    })
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

fn dur_ms(ms: u64) -> String {
    crate::output::format_duration(std::time::Duration::from_millis(ms))
}

/// Thousandths of a core as `"1.25 cores"` / `"35% of a core"`.
pub(crate) fn cores(milli: u64) -> String {
    if milli >= 1000 {
        format!("{:.2} cores", milli as f64 / 1000.0)
    } else {
        format!("{}% of a core", milli / 10)
    }
}

pub(crate) fn bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if f >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", f / (1024.0 * 1024.0 * 1024.0))
    } else if f >= 1024.0 * 1024.0 {
        format!("{:.0} MB", f / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", f / 1024.0)
    }
}

/// The lines describing activities, processes, cgroup and marker. Shared by
/// the socket path and the reader-side fallback.
pub(crate) fn render(ext: &Extension, show_pids: bool) -> Vec<String> {
    let mut out = Vec::new();
    match &ext.activities {
        None => out.push("activities: the holder was busy updating them; ask again".into()),
        Some(rows) if rows.is_empty() => {}
        Some(rows) => render_activities(rows, &mut out),
    }
    if ext.activities_omitted > 0 {
        out.push(format!(
            "       ... and {} more activities not shown",
            ext.activities_omitted
        ));
    }
    if let Some((text, age)) = &ext.narration
        && ext
            .activities
            .as_ref()
            .is_none_or(|r| r.iter().all(|a| a.narration.is_none()))
    {
        out.push(format!("last status: {text} ({} ago)", dur_ms(*age)));
    }
    match &ext.sample {
        Some(sample) => render_sample(sample, show_pids, &mut out),
        None if ext.collecting => {
            out.push("processes: the first sample is still being collected; ask again".into());
        }
        None => {}
    }
    if let Some((name, age)) = &ext.marker {
        out.push(format!("last marker: {name} ({} ago)", dur_ms(*age)));
    }
    out
}

fn render_activities(rows: &[ActivityRow], out: &mut Vec<String>) {
    // Depth-first from the roots, in id order (opening order).
    let mut sorted: Vec<&ActivityRow> = rows.iter().collect();
    sorted.sort_by_key(|a| a.id);
    let ids: std::collections::HashSet<u64> = sorted.iter().map(|a| a.id).collect();
    fn walk(
        parent: Option<u64>,
        depth: usize,
        rows: &[&ActivityRow],
        ids: &std::collections::HashSet<u64>,
        out: &mut Vec<String>,
    ) {
        for a in rows.iter().filter(|a| {
            a.parent == parent || (parent.is_none() && a.parent.is_some_and(|p| !ids.contains(&p)))
        }) {
            if depth > 12 {
                return;
            }
            let mut line = format!(
                "{}{} ({})",
                "  ".repeat(depth),
                a.label,
                dur_ms(a.elapsed_ms)
            );
            if let Some((done, total)) = a.progress {
                put!(line, ", {done}/{total}");
            }
            if let Some(n) = a.narration.as_ref().filter(|n| **n != a.label) {
                put!(line, " - {n}");
            }
            out.push(if depth == 0 {
                format!("doing: {line}")
            } else {
                format!("       {line}")
            });
            walk(Some(a.id), depth + 1, rows, ids, out);
        }
    }
    walk(None, 0, &sorted, &ids, out);
}

/// Rows as a forest, a process before the processes it started. Rows come in
/// the sender's order (busiest first) and keep it among siblings and roots, so
/// the result is deterministic. A row whose parent is not among the rows -
/// reparented, left out by the size bound, or never observed - is a root. The
/// PID and PPID are both from the holder's one procfs, so the relation holds
/// even when the numbers are not shown.
fn tree_order<'a>(rows: &[&'a ProcRow]) -> Vec<(&'a ProcRow, usize)> {
    let pids: std::collections::HashSet<u32> = rows.iter().map(|r| r.pid).collect();
    let mut kids: std::collections::HashMap<u32, Vec<usize>> = std::collections::HashMap::new();
    let mut roots = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if r.ppid != r.pid && pids.contains(&r.ppid) {
            kids.entry(r.ppid).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut seen = vec![false; rows.len()];
    let mut stack: Vec<(usize, usize)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
    while let Some((i, depth)) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        out.push((rows[i], depth));
        if let Some(k) = kids.get(&rows[i].pid) {
            stack.extend(k.iter().rev().map(|c| (*c, depth + 1)));
        }
    }
    // A parent cycle (PID reuse between the two scans) leaves rows unreached
    // from any root; show them rather than drop them.
    for (i, r) in rows.iter().enumerate() {
        if !seen[i] {
            out.push((r, 0));
        }
    }
    out
}

fn render_sample(s: &SampleView, show_pids: bool, out: &mut Vec<String>) {
    let mut head = format!(
        "processes: {} observed (sampled {} ago over {}): {}, summed RSS {}",
        s.observed,
        dur_ms(s.age_ms),
        dur_ms(s.interval_ms),
        cores(s.cpu_milli_total),
        bytes(s.rss_kb_total * 1024),
    );
    // An exited child its parent has not reaped yet has no memory, no CPU
    // rate and no command line: a row for it is noise, a count is enough.
    let zombies = s.rows.iter().filter(|r| r.state == 'Z').count();
    if zombies > 0 {
        put!(head, ", {zombies} exited awaiting reap");
    }
    if s.unreadable > 0 {
        put!(head, ", {} environment(s) not inspectable", s.unreadable);
    }
    out.push(head);
    let pids = show_pids && s.pids_meaningful;
    let shown: Vec<&ProcRow> = s.rows.iter().filter(|r| r.state != 'Z').collect();
    for (r, depth) in tree_order(&shown) {
        let mut line = "  ".repeat(depth + 1);
        if pids {
            put!(line, "PID {} ", r.pid);
        }
        put!(line, "{}", r.argv);
        let mut facts = Vec::new();
        facts.push(match r.cpu_milli {
            Some(m) => cores(m),
            None => "CPU rate n/a".into(),
        });
        facts.push(format!("CPU {}", dur_ms(r.cpu_ms)));
        facts.push(format!("RSS {}", bytes(r.rss_kb * 1024)));
        facts.push(format!("up {}", dur_ms(r.age_ms)));
        if r.state != 'R' && r.state != 'S' {
            facts.push(format!("state {}", r.state));
        }
        if let (Some(rd), Some(wr)) = (r.read_bytes, r.write_bytes)
            && (rd > 0 || wr > 0)
        {
            facts.push(format!("io r {} w {}", bytes(rd), bytes(wr)));
        }
        put!(line, " - {}", facts.join(", "));
        if let Some(l) = &r.launched {
            put!(line, " [{l}]");
        }
        out.push(line);
    }
    if s.omitted > 0 {
        out.push(format!("  ... and {} more not shown", s.omitted));
    }
    if let Some(c) = &s.cgroup {
        let mut line = format!(
            "cgroup {}: CPU {} since entry {} ago",
            c.name,
            dur_ms(c.cpu_usec / 1000),
            dur_ms(c.since_entry_ms)
        );
        if let Some(m) = c.cpu_milli {
            put!(line, ", now {}", cores(m));
        }
        if let Some(mem) = c.memory_bytes {
            put!(line, ", charged memory {}", bytes(mem));
        }
        put!(line, ", {} member process(es)", c.members);
        out.push(line);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn sample_ext() -> Extension {
        Extension {
            activities: Some(vec![
                ActivityRow {
                    id: 1,
                    parent: None,
                    elapsed_ms: 60_000,
                    progress: None,
                    label: "verify merge".into(),
                    narration: None,
                },
                ActivityRow {
                    id: 2,
                    parent: Some(1),
                    elapsed_ms: 5_000,
                    progress: Some((3, 7)),
                    label: "osmium apply-changes".into(),
                    narration: Some("step\twith tab".into()),
                },
            ]),
            activities_omitted: 0,
            narration: Some(("plan".into(), 10)),
            marker: Some(("phase2".into(), 20)),
            sample: Some(SampleView {
                age_ms: 100,
                interval_ms: 250,
                pids_meaningful: true,
                observed: 1,
                omitted: 0,
                unreadable: 2,
                cpu_milli_total: 980,
                rss_kb_total: 4096,
                rows: vec![ProcRow {
                    pid: 42,
                    ppid: 41,
                    via: "TD".into(),
                    cpu_milli: Some(980),
                    cpu_ms: 12_000,
                    rss_kb: 4096,
                    age_ms: 5_000,
                    state: 'R',
                    read_bytes: Some(10),
                    write_bytes: None,
                    launched: Some("osmium [verify merge]".into()),
                    argv: "osmium apply-changes a b".into(),
                }],
                cgroup: Some(CgroupRow {
                    name: "brokkr-x".into(),
                    since_entry_ms: 9,
                    cpu_usec: 7,
                    cpu_milli: None,
                    memory_bytes: Some(1),
                    members: 3,
                }),
            }),
            collecting: true,
        }
    }

    #[test]
    fn extension_round_trips() {
        let ext = sample_ext();
        let encoded = ext.encode();
        let lines: Vec<&str> = encoded.lines().collect();
        let back = Extension::decode(&lines).unwrap();
        let mut want = ext;
        want.activities.as_mut().unwrap()[1].narration = Some("step with tab".into());
        assert_eq!(back, want);
    }

    #[test]
    fn rows_read_as_a_tree_parent_first() {
        let mut ext = sample_ext();
        let base = ext.sample.as_ref().unwrap().rows[0].clone();
        let row = |pid: u32, ppid: u32, argv: &str| ProcRow {
            pid,
            ppid,
            argv: argv.into(),
            ..base.clone()
        };
        // Busiest first, as the sender orders them: child, then parent, then
        // an unrelated root whose parent is not in the sample.
        ext.sample.as_mut().unwrap().rows = vec![
            row(30, 20, "child"),
            row(20, 10, "parent"),
            row(50, 1, "other"),
        ];
        let text = render(&ext, true).join("\n");
        let at = |s: &str| text.find(s).unwrap();
        assert!(at("PID 20 parent") < at("PID 30 child"), "{text}");
        assert!(text.contains("\n    PID 30 child"), "{text}");
        assert!(text.contains("\n  PID 20 parent"), "{text}");
        assert!(text.contains("\n  PID 50 other"), "{text}");
    }

    #[test]
    fn no_version_line_means_no_extension() {
        assert!(Extension::decode(&["x1.act=1\t\t0\t\t\tl\t"]).is_none());
        assert!(Extension::decode(&["cpu_secs=3"]).is_none());
    }

    #[test]
    fn a_malformed_line_drops_the_whole_extension() {
        assert!(Extension::decode(&["x1=1", "x1.act=nope"]).is_none());
    }

    #[test]
    fn the_extension_is_bounded() {
        let mut ext = sample_ext();
        let row = ext.sample.as_ref().unwrap().rows[0].clone();
        let mut big = row;
        big.argv = "x".repeat(250);
        ext.sample.as_mut().unwrap().rows = vec![big; 1000];
        let encoded = ext.encode();
        assert!(encoded.len() <= EXT_BUDGET, "{}", encoded.len());
        let back = Extension::decode(&encoded.lines().collect::<Vec<_>>()).unwrap();
        let s = back.sample.unwrap();
        assert_eq!(s.rows.len() + s.omitted, 1000);
    }

    /// Wide activity labels and narrations (four-byte chars, at the clip
    /// length) cannot push the reply past the budget either; what is left out
    /// is counted.
    #[test]
    fn activities_are_bounded_too() {
        let mut ext = sample_ext();
        let wide = "\u{1F600}".repeat(200);
        let row = ActivityRow {
            id: 1,
            parent: None,
            elapsed_ms: 1,
            progress: None,
            label: wide.clone(),
            narration: Some(wide.clone()),
        };
        ext.activities = Some(vec![row; 64]);
        ext.narration = Some((wide.clone(), 1));
        ext.marker = Some((wide, 1));
        let encoded = ext.encode();
        assert!(encoded.len() <= EXT_BUDGET, "{}", encoded.len());
        let back = Extension::decode(&encoded.lines().collect::<Vec<_>>()).unwrap();
        assert_eq!(back.activities.unwrap().len() + back.activities_omitted, 64);
    }

    #[test]
    fn render_nests_activities_and_names_the_measures() {
        let lines = render(&sample_ext(), true);
        let all = lines.join("\n");
        assert!(all.contains("doing: verify merge (1m"), "{all}");
        assert!(all.contains("  osmium apply-changes (5"), "{all}");
        assert!(all.contains("3/7"), "{all}");
        assert!(all.contains("PID 42 osmium apply-changes a b"), "{all}");
        assert!(all.contains("summed RSS"), "{all}");
        assert!(all.contains("CPU 0ms since entry"), "{all}");
        assert!(all.contains("last marker: phase2"), "{all}");
        assert!(!render(&sample_ext(), false).join("\n").contains("PID 42"));
    }
}
