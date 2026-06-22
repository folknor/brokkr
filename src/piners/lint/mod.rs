//! `brokkr lint-corpus`: the piners differential-lint corpus.
//!
//! Runs a keyword-selected slice of `.pine` snippets through two offline
//! validators - **piners** (this dirty tree, brokkr-compiled) and
//! **pine-lint** (pre-installed) - diffs their diagnostics on a
//! `(line, col, severity)` grain, and gates on a pinned agreement
//! disposition per snippet. A periodic `--reanchor` mode consults
//! TradingView (`pine-lint --tv`) to re-ground the corpus.
//!
//! The trade-parity sibling is `brokkr corpus` (`src/piners/`); this
//! mirrors its structure minus the OHLCV feeds, trade oracle, manifest
//! harness, and runtime ceiling. See `docs/commands/lint-corpus.md`.

pub mod cmd;
pub mod db;
pub mod diff;
pub mod lints_write;
pub mod query;
pub mod registry;
pub mod select;
pub mod validators;

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// The two gated diagnostic severities. piners also emits `hint`, but
/// pine-lint has no counterpart, so hints are dropped before the diff
/// (informational only - never a `piners_only` divergence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One normalized diagnostic, reduced to the gated diff grain: position
/// plus severity. Message text is deliberately excluded - wording can drift
/// in either tool without changing the disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiagKey {
    pub line: usize,
    /// Column as the tool reported it (1-based). `None` when a tool omits it.
    pub col: Option<usize>,
    pub severity: Severity,
}

/// A validator's normalized output for one snippet: the deduplicated set of
/// diagnostic keys (error+warning only). An ordered set so the diff and the
/// rendering are deterministic.
pub type DiagSet = BTreeSet<DiagKey>;

/// The canonical per-probe disposition labels, the unit the gate compares.
/// `divergent` is gated coarsely; its [`Signature`] is diagnostic detail
/// (rendered + stored, never gated) the same way `corpus`'s `count_tier`
/// stays out of the gate.
pub const DISPOSITION_LABELS: [&str; 5] = [
    "agree_clean",
    "agree_flagged",
    "divergent",
    "piners_error",
    "lint_error",
];

/// True if `label` is one of [`DISPOSITION_LABELS`].
pub fn is_disposition(label: &str) -> bool {
    DISPOSITION_LABELS.contains(&label)
}

/// One probe's fully-resolved result for a run: the classified disposition,
/// the gate verdict against its pin, the raw diagnostic counts, and the TV
/// anchor relationship. The single unit `cmd` builds, the run store
/// ([`db`](crate::piners::lint::db)) ingests, and `lint-results` renders.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub probe: String,
    /// Gated disposition label (one of [`DISPOSITION_LABELS`]).
    pub disposition: String,
    /// Divergence breakdown (`piners_only`/`lint_only`/`severity_mismatch`/
    /// `mixed`); `None` unless `disposition == "divergent"`.
    pub signature: Option<String>,
    /// The pinned `expected` at run time; `None` = never blessed.
    pub expected: Option<String>,
    /// Whether the actual disposition satisfied the pin.
    pub gate_ok: bool,
    /// Count of piners diagnostics (error+warning).
    pub piners_count: usize,
    /// Count of pine-lint diagnostics (error+warning).
    pub lint_count: usize,
    /// Failure reason for a `piners_error`/`lint_error` disposition.
    pub error: Option<String>,
    /// When the probe's TV anchor was last refreshed; `None` = never anchored.
    pub tv_anchored_at: Option<String>,
    /// Whether piners' output diverges from a present TV anchor (informational;
    /// `None` when the probe has no anchor). The shared-but-wrong signal.
    pub tv_divergent: Option<bool>,
}

/// Current UTC time as an RFC3339 string (`YYYY-MM-DDThh:mm:ssZ`), the format
/// `tv_anchored_at` and the run store's `started_at` carry. Computed from the
/// epoch with the civil-date algorithm so brokkr needs no date crate.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Howard Hinnant's days-from-civil inverse: epoch-day count -> (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 { y + 1 } else { y },
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}
