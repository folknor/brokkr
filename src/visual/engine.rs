//! Shared command mechanics for visual subjects.

use std::path::Path;

use crate::error::DevError;
use crate::git::{self, GitInfo};
use crate::resolve;

use super::status::{Status, StatusTally};

pub(crate) trait SubjectItem {
    const KIND: &'static str;

    fn id(&self) -> &str;
}

/// Resolve an exact subject ID or a unique prefix.
pub(crate) fn resolve_prefix<'a, T: SubjectItem>(
    items: &'a [T],
    id: &str,
) -> Result<&'a T, DevError> {
    if let Some(item) = items.iter().find(|item| item.id() == id) {
        return Ok(item);
    }
    let matches: Vec<_> = items
        .iter()
        .filter(|item| item.id().starts_with(id))
        .collect();
    match matches.len() {
        0 => Err(DevError::Config(format!("{} not found: {id}", T::KIND))),
        1 => Ok(matches[0]),
        _ => {
            let ids = matches
                .iter()
                .map(|item| item.id())
                .collect::<Vec<_>>()
                .join(", ");
            Err(DevError::Config(format!(
                "ambiguous {} prefix '{id}', matches: {ids}",
                T::KIND
            )))
        }
    }
}

pub(crate) trait Subject {
    type Item;
    type Outcome;

    /// Render and score one item according to this subject's reference policy.
    fn run_item(&mut self, item: &Self::Item) -> Result<Self::Outcome, DevError>;
    fn status(outcome: &Self::Outcome) -> Status;
    fn print_result(&mut self, item: &Self::Item, outcome: &Self::Outcome);
    fn finish(&mut self, tally: &StatusTally) -> Result<(), DevError>;

    fn is_unexpected_pass(_outcome: &Self::Outcome) -> bool {
        false
    }
}

/// Execute and present a selected set using the common tally and stop policy.
pub(crate) fn run<S: Subject>(subject: &mut S, items: &[&S::Item]) -> Result<(), DevError> {
    let mut tally = StatusTally::default();
    for item in items {
        let outcome = subject.run_item(*item)?;
        subject.print_result(*item, &outcome);
        tally.record(S::status(&outcome), S::is_unexpected_pass(&outcome));
    }
    subject.finish(&tally)
}

pub(crate) fn format_pct(value: Option<f64>, decimals: usize) -> String {
    match value {
        Some(value) => format!("{value:.decimals$}%"),
        None => "\u{2014}".into(),
    }
}

pub(crate) fn print_result_header(
    subject_label: &str,
    has_element_scores: bool,
    mut emit: impl FnMut(&str),
) {
    if has_element_scores {
        emit(&format!(
            "  {:<25} {:<9} {:<11} {}",
            subject_label, "Pixels", "Elements", "Status",
        ));
        emit(&format!("  {}", "\u{2500}".repeat(60)));
    } else {
        emit(&format!(
            "  {:<25} {:<9} {}",
            subject_label, "Pixels", "Status",
        ));
        emit(&format!("  {}", "\u{2500}".repeat(50)));
    }
}

pub(crate) fn format_status_row(
    id: &str,
    approved: &str,
    pixel: &str,
    delta: &str,
    status: &str,
    annotation: &str,
) -> String {
    format!("  {id:<25} {approved:<11} {pixel:<11} {delta:<9} {status}{annotation}",)
}

pub(crate) fn collect_clean_approval(
    subject: &str,
    build_root: &Path,
) -> Result<GitInfo, DevError> {
    let git_info = git::collect(build_root)?;
    if !git_info.is_clean {
        return Err(DevError::Verify(format!(
            "{subject} approve requires a clean git tree"
        )));
    }
    Ok(git_info)
}

/// Open a subject's visual store. The opener is handed the legacy
/// `results.db` path: the store lives beside it under its own file name, and
/// the legacy tables in it are the source of the one-time import.
pub(crate) fn open_results_db<T>(
    project_root: &Path,
    open: impl FnOnce(&Path) -> Result<T, DevError>,
) -> Result<T, DevError> {
    open(&resolve::results_db_path(project_root))
}

pub(crate) struct SummaryOptions<'a> {
    pub(crate) divider_width: usize,
    pub(crate) show_unexpected_passes: bool,
    pub(crate) no_baseline_hint: Option<&'a str>,
}

pub(crate) fn print_run_summary(
    tally: &StatusTally,
    options: &SummaryOptions<'_>,
    mut emit: impl FnMut(&str),
) -> Result<(), DevError> {
    emit(&format!("  {}", "\u{2500}".repeat(options.divider_width)));
    let failed = tally.count(Status::FailThreshold) + tally.count(Status::Regression);
    let mut parts = vec![
        (tally.ordinary_passes(), "passed"),
        (failed, "failed"),
        (tally.count(Status::ExpectedFail), "expected fail"),
        (tally.count(Status::Error), "error"),
        (tally.count(Status::NoBaseline), "awaiting approval"),
    ];
    if options.show_unexpected_passes {
        parts.push((tally.unexpected_passes(), "unexpected pass"));
    }
    let summary = parts
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, label)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(", ");
    emit(&format!("  {summary}"));

    if tally.count(Status::NoBaseline) > 0
        && let Some(hint) = options.no_baseline_hint
    {
        emit(hint);
    }
    if tally.fails_run() {
        return Err(DevError::ExitCode(1));
    }
    Ok(())
}
