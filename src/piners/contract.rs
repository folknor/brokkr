//! The harness line contract: setup progress, per-probe lifecycle and the
//! terminal records, each line carrying `version` 1
//! (`docs/projects/piners.md`, "Contract lines").
//!
//! [`assess_contract`] validates the contract lines [`crate::piners::report`]
//! recognised, in emission order, against the selection and the process exit.
//! Recognition precedes version validation: a known kind with a missing,
//! malformed or unsupported version is a violation even when no valid line
//! appeared. Strictness starts once any contract line is observed; a stream
//! with none is "no contract observed" and is judged by the rules that
//! predate the contract (exit status and selection coverage).
//!
//! What is enforced once the contract is observed:
//!
//! - Terminal records. `run_end` carries `exit` 0 or 1, appears at most once,
//!   is the last line, and equals the process exit. `run_error` appears at
//!   most once, never with `run_end`, and means process exit 2 (unless brokkr
//!   itself killed the harness: the hang backstop or an interrupt). A process
//!   that exited on its own with neither record broke the contract; one that
//!   died of a signal with neither is an abort, not a malformed stream.
//! - Lifecycle, per probe, in emission order: `probe_start` then `probe_end`,
//!   each once, for selected ids only. A `run_end` claims completion, so it
//!   also requires every selected probe to have a start, a disposition and an
//!   end, in that order.
//!
//! Probes overlap inside the harness, so the lifecycle is evidence of what
//! was in flight, never of what caused anything.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::Value;

use crate::piners::integrity::HarnessEnd;
use crate::piners::report::{ContractLine, HarnessReport};

/// The contract version brokkr speaks.
pub const CONTRACT_VERSION: u64 = 1;

/// Members listed per aggregated violation before eliding to a count.
const MAX_LISTED: usize = 8;

/// A `run_error` record: shared setup or the harness itself failed. Names the
/// failing entry, never a probe. The typed fields drive rendering; `raw` is
/// the harness's line exactly as parsed - `kind`, `version`, explicit nulls
/// and any field brokkr does not model - and is what the run row stores.
#[derive(Debug, Clone, PartialEq)]
pub struct RunErrorRecord {
    pub stage: String,
    pub feed: Option<String>,
    pub role: Option<String>,
    pub path: Option<String>,
    pub field: Option<String>,
    pub error: String,
    pub raw: Value,
}

impl RunErrorRecord {
    fn from_value(value: &Value) -> Result<Self, String> {
        let text = |key: &str| -> Result<Option<String>, String> {
            match value.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(other) => Err(format!("`{key}` is not a string ({other})")),
            }
        };
        Ok(Self {
            stage: text("stage")?.ok_or("no `stage`")?,
            feed: text("feed")?,
            role: text("role")?,
            path: text("path")?,
            field: text("field")?,
            error: text("error")?.ok_or("no `error`")?,
            raw: value.clone(),
        })
    }

    /// `run_error at stage feed_load (feed eth, role base, path /x): msg` -
    /// every locator the harness supplied.
    pub fn render(&self) -> String {
        let locators: Vec<String> = [
            ("feed", &self.feed),
            ("role", &self.role),
            ("path", &self.path),
            ("field", &self.field),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k} {v}")))
        .collect();
        let at = if locators.is_empty() {
            String::new()
        } else {
            format!(" ({})", locators.join(", "))
        };
        format!("run_error at stage {}{at}: {}", self.stage, self.error)
    }

    /// The form stored on the run row: the original record, whole.
    pub fn to_json(&self) -> String {
        self.raw.to_string()
    }

    /// Read a stored record back; `None` when the column holds something
    /// that is not a readable `run_error`.
    pub fn from_json(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        Self::from_value(&value).ok()
    }
}

/// The last `setup_stage` seen: context for an abnormal end, never a cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageInfo {
    pub stage: String,
    pub feed: Option<String>,
    pub path: Option<String>,
}

impl StageInfo {
    pub fn render(&self) -> String {
        let mut out = self.stage.clone();
        if let Some(f) = &self.feed {
            out.push_str(&format!(" (feed {f})"));
        }
        if let Some(p) = &self.path {
            out.push_str(&format!(" (path {p})"));
        }
        out
    }
}

/// What the contract lines said, and what they broke.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ContractAssessment {
    /// Any contract line was present. With none, nothing below applies.
    pub observed: bool,
    /// Contract violations, aggregated by kind of breach.
    pub violations: Vec<String>,
    /// The (first, valid) `run_error`.
    pub run_error: Option<RunErrorRecord>,
    /// The (first, valid) `run_end` exit.
    pub run_end: Option<i64>,
    /// The last valid `setup_stage`.
    pub last_stage: Option<StageInfo>,
    pub setup_complete: bool,
    /// Probes with a `probe_start` and no `probe_end`, sorted.
    pub outstanding: Vec<String>,
}

impl ContractAssessment {
    /// The setup context line for an abnormal end, if any: the last stage,
    /// said as context, and never as still running once setup completed.
    pub fn stage_context(&self) -> Option<String> {
        match (&self.last_stage, self.setup_complete) {
            (Some(s), false) => Some(format!("last setup stage seen (context only): {}", s.render())),
            (Some(s), true) => Some(format!(
                "setup completed (last setup stage was {}); probes were running",
                s.render()
            )),
            (None, true) => Some("setup completed; probes were running".to_owned()),
            (None, false) => None,
        }
    }
}

/// Per-probe lifecycle state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Life {
    Started(usize),
    Ended(usize, usize),
}

/// Breach kind -> affected ids (or descriptions), aggregated into one line
/// each so a harness bug over 850 probes is one line, not 850.
#[derive(Default)]
struct Breaches(BTreeMap<&'static str, Vec<String>>);

impl Breaches {
    fn add(&mut self, kind: &'static str, what: impl Into<String>) {
        self.0.entry(kind).or_default().push(what.into());
    }

    fn into_lines(self) -> Vec<String> {
        self.0
            .into_iter()
            .map(|(kind, mut items)| {
                items.sort();
                let more = items.len().saturating_sub(MAX_LISTED);
                let shown = items.iter().take(MAX_LISTED).cloned().collect::<Vec<_>>().join(", ");
                let tail = if more > 0 { format!(" (and {more} more)") } else { String::new() };
                format!("{kind}: {shown}{tail}")
            })
            .collect()
    }
}

/// Validate the report's contract lines. `selected` is the selection; `end`
/// how the process ended. Dispositions are read from `report.probes` (already
/// collapsed to one per id) with their stream positions.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // one pass over the stream, then the end-of-stream rules
pub fn assess_contract(report: &HarnessReport, selected: &[String], end: HarnessEnd) -> ContractAssessment {
    let mut a = ContractAssessment { observed: !report.contract.is_empty(), ..Default::default() };
    if !a.observed {
        return a;
    }
    let selected: HashSet<&str> = selected.iter().map(String::as_str).collect();
    let mut b = Breaches::default();
    let mut life: BTreeMap<String, Life> = BTreeMap::new();
    let mut run_end_line: Option<usize> = None;
    let mut run_error_line: Option<usize> = None;

    for c in &report.contract {
        if let Err(why) = check_version(c) {
            b.add("contract line with a bad version", format!("{} at line {} ({why})", c.kind, c.line));
            continue;
        }
        match c.kind.as_str() {
            "setup_stage" => match c.value.get("stage").and_then(Value::as_str) {
                Some(stage) => {
                    a.last_stage = Some(StageInfo {
                        stage: stage.to_owned(),
                        feed: c.value.get("feed").and_then(Value::as_str).map(str::to_owned),
                        path: c.value.get("path").and_then(Value::as_str).map(str::to_owned),
                    });
                }
                None => b.add("malformed setup_stage", format!("line {}", c.line)),
            },
            "setup_complete" => a.setup_complete = true,
            "probe_start" | "probe_end" => {
                let Some(probe) = c.value.get("probe").and_then(Value::as_str) else {
                    b.add("lifecycle line with no probe", format!("{} at line {}", c.kind, c.line));
                    continue;
                };
                if !selected.contains(probe) {
                    b.add("lifecycle line for an unselected probe", probe);
                    continue;
                }
                let state = life.get(probe).copied();
                match (c.kind.as_str(), state) {
                    ("probe_start", None) => {
                        life.insert(probe.to_owned(), Life::Started(c.line));
                    }
                    ("probe_start", Some(Life::Started(_))) => b.add("duplicate probe_start", probe),
                    ("probe_start", Some(Life::Ended(..))) => b.add("probe_start after its probe_end", probe),
                    ("probe_end", None) => b.add("probe_end without probe_start", probe),
                    ("probe_end", Some(Life::Started(s))) => {
                        life.insert(probe.to_owned(), Life::Ended(s, c.line));
                    }
                    _ => b.add("duplicate probe_end", probe),
                }
            }
            "run_end" => {
                let exit = c.value.get("exit").and_then(Value::as_i64).filter(|e| *e == 0 || *e == 1);
                match exit {
                    None => b.add(
                        "run_end exit not integer 0 or 1",
                        format!("line {} ({})", c.line, c.value.get("exit").map_or("absent".to_owned(), Value::to_string)),
                    ),
                    Some(_) if run_end_line.is_some() => b.add("second run_end", format!("line {}", c.line)),
                    Some(e) => {
                        a.run_end = Some(e);
                        run_end_line = Some(c.line);
                    }
                }
            }
            "run_error" => match RunErrorRecord::from_value(&c.value) {
                Err(why) => b.add("malformed run_error", format!("line {} ({why})", c.line)),
                Ok(_) if run_error_line.is_some() => b.add("second run_error", format!("line {}", c.line)),
                Ok(rec) => {
                    a.run_error = Some(rec);
                    run_error_line = Some(c.line);
                }
            },
            _ => {}
        }
    }

    // Terminal order: run_end is last; run_error never comes with run_end.
    let last = report.last_line.unwrap_or(0);
    if let Some(l) = run_end_line
        && last > l
    {
        b.add("records after run_end (it must be last)", format!("{} line(s)", last - l));
    }
    // Nothing follows a run_error either - unknown kinds included, since
    // the position counts every non-blank line.
    if let Some(l) = run_error_line
        && last > l
    {
        b.add("records after run_error (it must be last)", format!("{} line(s)", last - l));
    }
    if run_end_line.is_some() && run_error_line.is_some() {
        b.add("run_end and run_error in one stream", "both present");
    }

    // Terminal records against the process exit.
    let brokkr_killed = matches!(end, HarnessEnd::Backstop | HarnessEnd::Interrupted);
    if let Some(e) = a.run_end
        && end != HarnessEnd::Code(i32::try_from(e).unwrap_or(-1))
    {
        b.add("run_end disagrees with the process exit", format!("run_end {e}, process {}", end.short()));
    }
    if a.run_error.is_some() && end != HarnessEnd::Code(2) && !brokkr_killed {
        b.add("run_error without process exit 2", format!("process {}", end.short()));
    }
    if a.run_end.is_none()
        && a.run_error.is_none()
        && let HarnessEnd::Code(c) = end
    {
        b.add("process exited with neither run_end nor run_error", format!("exit {c}"));
    }

    // A completion claim requires every selected probe's full lifecycle.
    if a.run_end.is_some() && a.run_error.is_none() {
        let disp: BTreeMap<&str, usize> =
            report.probes.iter().map(|p| (p.probe.as_str(), p.line)).collect();
        let mut ids: Vec<&&str> = selected.iter().collect();
        ids.sort();
        for id in ids {
            let id: &str = id;
            match (life.get(id), disp.get(id)) {
                (None, _) => b.add("run_end claims completion, but no probe_start for", id),
                (Some(Life::Started(_)), _) => b.add("run_end claims completion, but no probe_end for", id),
                (Some(Life::Ended(..)), None) => b.add("run_end claims completion, but no disposition for", id),
                (Some(Life::Ended(s, e)), Some(d)) if !(s < d && d < e) => {
                    b.add("disposition outside its probe_start..probe_end", id);
                }
                _ => {}
            }
        }
    }

    a.outstanding = life
        .iter()
        .filter(|(_, l)| matches!(l, Life::Started(_)))
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    a.violations = b.into_lines();
    a
}

/// `version` must be the integer [`CONTRACT_VERSION`].
fn check_version(c: &ContractLine) -> Result<(), String> {
    match c.value.get("version") {
        None => Err("missing".to_owned()),
        Some(v) => match v.as_u64() {
            None => Err(format!("malformed: {v}")),
            Some(CONTRACT_VERSION) => Ok(()),
            Some(n) => Err(format!("unsupported: {n}")),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::report::parse;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn assess(nd: &str, sel: &[&str], end: HarnessEnd) -> ContractAssessment {
        let mut r = parse(nd.as_bytes());
        r.take_duplicates();
        assess_contract(&r, &ids(sel), end)
    }

    fn line(kind: &str, rest: &str) -> String {
        format!("{{\"kind\":\"{kind}\",\"version\":1{rest}}}\n")
    }

    fn disp(probe: &str) -> String {
        format!("{{\"probe\":\"{probe}\",\"outcome\":\"no_tv_data\"}}\n")
    }

    /// A clean completed stream over `probes`, exit `code`.
    fn clean(probes: &[&str], code: i32) -> String {
        let mut s = line("setup_stage", ",\"stage\":\"manifest\"");
        s.push_str(&line("setup_complete", ""));
        for p in probes {
            s.push_str(&line("probe_start", &format!(",\"probe\":\"{p}\"")));
            s.push_str(&disp(p));
            s.push_str(&line("probe_end", &format!(",\"probe\":\"{p}\"")));
        }
        s.push_str(&line("run_end", &format!(",\"exit\":{code}")));
        s
    }

    #[test]
    fn a_clean_stream_has_no_violations() {
        let a = assess(&clean(&["a", "b"], 0), &["a", "b"], HarnessEnd::Code(0));
        assert!(a.observed);
        assert!(a.violations.is_empty(), "{:?}", a.violations);
        assert_eq!(a.run_end, Some(0));
        assert!(a.outstanding.is_empty());
    }

    #[test]
    fn no_contract_lines_is_no_contract_observed() {
        let a = assess(&disp("a"), &["a"], HarnessEnd::Code(0));
        assert!(!a.observed);
        assert!(a.violations.is_empty());
    }

    #[test]
    fn a_bad_version_is_a_violation_even_alone() {
        for v in ["", ",\"version\":\"1\"", ",\"version\":2"] {
            let nd = format!("{{\"kind\":\"setup_complete\"{v}}}\n");
            let a = assess(&nd, &["a"], HarnessEnd::Signal(9));
            assert!(a.observed);
            assert!(
                a.violations.iter().any(|l| l.starts_with("contract line with a bad version")),
                "{v}: {:?}",
                a.violations
            );
        }
    }

    #[test]
    fn lifecycle_breaches_are_named() {
        let mut nd = line("probe_start", ",\"probe\":\"zz\"");
        nd.push_str(&line("probe_start", ",\"probe\":\"a\""));
        nd.push_str(&line("probe_start", ",\"probe\":\"a\""));
        nd.push_str(&line("probe_end", ",\"probe\":\"b\""));
        nd.push_str(&line("probe_end", ",\"probe\":\"a\""));
        nd.push_str(&line("probe_end", ",\"probe\":\"a\""));
        nd.push_str(&line("probe_start", ",\"probe\":\"a\""));
        let a = assess(&nd, &["a", "b"], HarnessEnd::Signal(9));
        let v = a.violations.join("\n");
        for want in [
            "lifecycle line for an unselected probe: zz",
            "duplicate probe_start: a",
            "probe_end without probe_start: b",
            "duplicate probe_end: a",
            "probe_start after its probe_end: a",
        ] {
            assert!(v.contains(want), "{want}\n{v}");
        }
    }

    #[test]
    fn anything_after_run_error_is_a_violation_unknown_kinds_included() {
        let mut nd = line("run_error", ",\"stage\":\"feed_load\",\"error\":\"x\"");
        nd.push_str("{\"kind\":\"future\",\"x\":1}\n");
        let a = assess(&nd, &["a"], HarnessEnd::Code(2));
        assert!(
            a.violations.iter().any(|l| l == "records after run_error (it must be last): 1 line(s)"),
            "{:?}",
            a.violations
        );
        // A run_error that is last is clean.
        let last = line("run_error", ",\"stage\":\"feed_load\",\"error\":\"x\"");
        assert!(assess(&last, &["a"], HarnessEnd::Code(2)).violations.is_empty());
    }

    #[test]
    fn anything_after_run_end_is_a_violation_run_error_included() {
        let mut nd = clean(&["a"], 0);
        nd.push_str(&line("run_error", ",\"stage\":\"supervisor\",\"error\":\"x\""));
        let a = assess(&nd, &["a"], HarnessEnd::Code(2));
        let v = a.violations.join("\n");
        assert!(v.contains("records after run_end"), "{v}");
        assert!(v.contains("run_end and run_error in one stream"), "{v}");
    }

    #[test]
    fn run_end_must_be_0_or_1_once_and_match_the_exit() {
        let a = assess(&clean(&["a"], 1), &["a"], HarnessEnd::Code(0));
        assert!(a.violations.iter().any(|l| l.starts_with("run_end disagrees")), "{:?}", a.violations);
        let bad = clean(&["a"], 0).replace("\"exit\":0", "\"exit\":2");
        let a = assess(&bad, &["a"], HarnessEnd::Code(2));
        assert!(a.violations.iter().any(|l| l.starts_with("run_end exit not integer 0 or 1")));
        let twice = format!("{}{}", clean(&["a"], 0), line("run_end", ",\"exit\":0"));
        let a = assess(&twice, &["a"], HarnessEnd::Code(0));
        assert!(a.violations.iter().any(|l| l.starts_with("second run_end")));
    }

    #[test]
    fn run_error_is_parsed_rendered_and_must_mean_exit_2() {
        let mut nd = line("setup_stage", ",\"stage\":\"feed_load\",\"feed\":\"eth\"");
        nd.push_str(&line(
            "run_error",
            ",\"stage\":\"feed_load\",\"feed\":\"eth\",\"role\":\"base\",\"path\":\"/c/f.csv\",\"error\":\"no such file\"",
        ));
        let a = assess(&nd, &["a"], HarnessEnd::Code(2));
        assert!(a.violations.is_empty(), "{:?}", a.violations);
        let rec = a.run_error.clone().unwrap();
        assert_eq!(
            rec.render(),
            "run_error at stage feed_load (feed eth, role base, path /c/f.csv): no such file"
        );
        assert_eq!(RunErrorRecord::from_json(&rec.to_json()), Some(rec.clone()));
        // Stored whole: kind, version, explicit nulls and unknown fields.
        let odd = line("run_error", ",\"stage\":\"s\",\"feed\":null,\"error\":\"e\",\"hint\":{\"x\":1}");
        let a = assess(&odd, &["a"], HarnessEnd::Code(2));
        let stored: Value = serde_json::from_str(&a.run_error.unwrap().to_json()).unwrap();
        assert_eq!(stored["kind"], "run_error");
        assert_eq!(stored["version"], 1);
        assert!(stored["feed"].is_null() && stored.get("feed").is_some());
        assert_eq!(stored["hint"]["x"], 1);
        // Any exit but 2 is a violation, unless brokkr killed the harness.
        let a = assess(&nd, &["a"], HarnessEnd::Code(0));
        assert!(a.violations.iter().any(|l| l.starts_with("run_error without process exit 2")));
        assert!(assess(&nd, &["a"], HarnessEnd::Backstop).violations.is_empty());
        assert!(assess(&nd, &["a"], HarnessEnd::Interrupted).violations.is_empty());
        // A second run_error is a violation.
        let twice = format!("{nd}{}", line("run_error", ",\"stage\":\"x\",\"error\":\"y\""));
        let a = assess(&twice, &["a"], HarnessEnd::Code(2));
        assert!(a.violations.iter().any(|l| l.starts_with("second run_error")));
    }

    #[test]
    fn an_own_exit_without_a_terminal_breaks_the_contract_a_signal_does_not() {
        let mut nd = line("setup_complete", "");
        nd.push_str(&line("probe_start", ",\"probe\":\"a\""));
        let a = assess(&nd, &["a", "b"], HarnessEnd::Code(0));
        assert!(a.violations.iter().any(|l| l.starts_with("process exited with neither")));
        let a = assess(&nd, &["a", "b"], HarnessEnd::Signal(9));
        assert!(a.violations.is_empty(), "{:?}", a.violations);
        assert_eq!(a.outstanding, ids(&["a"]));
        assert_eq!(a.stage_context().unwrap(), "setup completed; probes were running");
    }

    #[test]
    fn a_completion_claim_requires_every_lifecycle() {
        let mut nd = line("probe_start", ",\"probe\":\"a\"");
        nd.push_str(&line("probe_end", ",\"probe\":\"a\""));
        nd.push_str(&disp("a")); // disposition after its end
        nd.push_str(&line("run_end", ",\"exit\":0"));
        let a = assess(&nd, &["a", "b"], HarnessEnd::Code(0));
        let v = a.violations.join("\n");
        assert!(v.contains("disposition outside its probe_start..probe_end: a"), "{v}");
        assert!(v.contains("run_end claims completion, but no probe_start for: b"), "{v}");
    }

    #[test]
    fn the_stage_is_context_until_setup_completes() {
        let nd = line("setup_stage", ",\"stage\":\"feed_load\",\"feed\":\"eth\"");
        let a = assess(&nd, &["a"], HarnessEnd::Signal(9));
        assert_eq!(a.stage_context().unwrap(), "last setup stage seen (context only): feed_load (feed eth)");
    }
}
