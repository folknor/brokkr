//! Schema DDL and connection lifecycle for the corpus runs database.
//!
//! Mirrors `src/db/schema.rs`. The bundled SQLite enforces the FK
//! `REFERENCES` clauses (ingest writes the `run` row before its children, so
//! they hold). Retention deletes child rows before their parent run rows; the
//! foreign keys do not use cascades.
//!
//! The `disposition` table stores each harness line whole, as `raw_json`, and
//! every harness field is a generated column projected from it. The harness
//! adds diagnostics faster than brokkr learns their names, and the run dir
//! holding the NDJSON is deleted after ingest, so a field brokkr had no column
//! for used to be destroyed on arrival. With the record authoritative a new
//! field is stored the day it ships. Naming it is a [`PROJECTIONS`] entry,
//! which reaches fresh stores, plus a migration step for existing ones -
//! `ALTER TABLE disposition ADD COLUMN ... GENERATED ALWAYS AS (...) VIRTUAL`
//! behind a `has_column` guard, because `CREATE TABLE IF NOT EXISTS` never
//! touches a table that already exists. That step reaches every row already
//! stored. It also means there is one copy of each harness fact: a typed
//! column cannot disagree with the record because it *is* the record.
//!
//! What stays physical is what is not the harness's: the row key, brokkr's
//! own gate label `disposition` (derived by `ProbeLine::disposition`, never
//! trusted from the line), the pin's `expected`, the gate's `gate_ok`, the
//! label ratchet's `expected_labels` (the pin's counts as JSON) and
//! `label_gate` (the label deviations as a JSON list, `[]` when checked and
//! clean; NULL when the probe was not label-checked - unselected, an invalid
//! line, or a row from before v14, whose label verdict was never recorded),
//! the same pair for the performance ratchet (v18) and the excursion
//! ratchet (v19), named by `RatchetKind::columns`,
//! `scored` (v17: 1 when the run's reconciliation scored the row - a
//! selected probe, a valid record, no invalid occurrence - and 0 for an
//! invalid record, a tainted probe or a report-only extra; on a row stored
//! before v17, 1 when its run recorded zero protocol violations - so every
//! row of it was scored - and NULL, not recorded, otherwise),
//! `defect` (v17: why an unscored row's record was not usable, the record's
//! own defect or the occurrence that tainted its probe; NULL otherwise),
//! `expected_death` and `expected_attribution` (v20: a pinned abort's death
//! as JSON and its attribution, beside the bare `expected` label; NULL on
//! every other pin and on every older row; `gate_miss` carries the same
//! pair, so an abort probe that emitted no line keeps them too),
//! `expected_cause` (v21: a pinned compile_fail or runtime_fail's cause;
//! NULL on every other pin and on every older row; on `gate_miss` too),
//! and `raw_source`. `raw_source` is `harness` for a parsed harness line and
//! `reconstructed` for a row migrated from the typed schema (v4 and older),
//! whose record was rebuilt from exactly the columns it kept - so on such a
//! row every diagnostic the old schema never had reads NULL, meaning "not
//! retained", not zero.
//!
//! `trade_diff` is built the same way, its physical columns only the row key
//! plus `raw_source`/`raw_json` (v13 rebuilt the typed table, which had been
//! dropping every field the harness added after it).

use std::path::Path;
use std::sync::LazyLock;

use super::CorpusDb;
use super::migrate;
use crate::error::DevError;
use crate::piners::ratchet::RatchetKind;

/// How one harness field is projected out of `raw_json`.
///
/// Every projection is gated on the JSON type it expects and reads NULL for
/// anything else. The parser no longer type-checks the fields it does not
/// read, so a mistyped value reaches the store, and ungated it would project
/// as garbage: SQLite orders every integer before any text, so the count
/// clamp's `MIN` would turn a string into `i64::MAX`, and a text
/// `runtime_ms` would fail every typed read of the column.
#[derive(Clone, Copy)]
pub(super) enum Projection {
    /// A JSON string.
    Text(&'static str),
    /// A JSON string, with `""` read as NULL - the old ingest's `ne()`.
    NonEmpty(&'static str),
    /// A JSON array, as its JSON text.
    Array(&'static str),
    /// A JSON object, as its JSON text.
    Object(&'static str),
    /// A JSON number, as a real.
    Real(&'static str),
    /// A JSON integer, exact and signed - a timestamp, a delta, a bar index.
    /// A real (`1.5`) or an integer past `i64` reads NULL rather than
    /// truncated, and nothing is clamped: unlike a count, a negative value is
    /// meaningful.
    Int(&'static str),
    /// A non-negative JSON integer, clamped to `i64::MAX` as the old
    /// ingest's `as_i64` did: the harness sends `u64`, and one past
    /// `i64::MAX` comes back from `json_extract` as a REAL no typed read
    /// accepts. NULL when absent.
    Count(&'static str),
    /// [`Projection::Count`], 0 when absent - the parser's serde default.
    CountOr0(&'static str),
    /// A JSON boolean, as 1/0.
    Flag(&'static str),
    /// The acceptance `failing` array, `'[]'` when absent.
    Failing,
    /// `dimension_breaches`, present exactly when a signature object is
    /// (0 when that object omits it), clamped like a count.
    Breaches,
}

impl Projection {
    fn sql(self) -> String {
        let at = |p: &str| format!("json_extract(raw_json, '{p}')");
        let when = |p: &str, types: &str, then: String| {
            format!("CASE WHEN json_type(raw_json, '{p}') IN ({types}) THEN {then} END")
        };
        let count = |p: &str| {
            when(
                p,
                "'integer', 'real'",
                format!("MIN({}, 9223372036854775807)", at(p)),
            )
        };
        match self {
            Projection::Text(p) => when(p, "'text'", at(p)),
            Projection::NonEmpty(p) => when(p, "'text'", format!("NULLIF({}, '')", at(p))),
            Projection::Array(p) => when(p, "'array'", at(p)),
            Projection::Object(p) => when(p, "'object'", at(p)),
            Projection::Real(p) => when(p, "'integer', 'real'", at(p)),
            // JSON calls one past i64::MAX an integer, but `json_extract`
            // returns it as a REAL no typed read accepts: NULL, not cast.
            Projection::Int(p) => format!(
                "CASE WHEN json_type(raw_json, '{p}') = 'integer' \
                 AND typeof({}) = 'integer' THEN {} END",
                at(p),
                at(p)
            ),
            Projection::Flag(p) => when(p, "'true', 'false'", at(p)),
            Projection::Count(p) => count(p),
            Projection::CountOr0(p) => format!("COALESCE({}, 0)", count(p)),
            Projection::Failing => format!(
                "COALESCE({}, '[]')",
                when(
                    "$.acceptance.failing",
                    "'array'",
                    at("$.acceptance.failing")
                )
            ),
            Projection::Breaches => format!(
                "CASE WHEN json_type(raw_json, '$.signature') = 'object' THEN COALESCE({}, 0) END",
                count("$.signature.dimension_breaches")
            ),
        }
    }

    fn sql_type(self) -> &'static str {
        match self {
            Projection::Text(_)
            | Projection::NonEmpty(_)
            | Projection::Array(_)
            | Projection::Object(_)
            | Projection::Failing => "TEXT",
            Projection::Real(_) => "REAL",
            _ => "INTEGER",
        }
    }
}

/// The `disposition` table's columns after the physical ones: every harness
/// field, projected from `raw_json`. The projections reproduce what the typed
/// schema stored, which the v5 migration verifies row by row: counts default
/// to 0 and clamp like the old ingest, an empty acceptance/signature string is
/// NULL, `acc_failing` is `'[]'` when absent, and `sig_breaches` is present
/// exactly when a signature object is. Declared types give the projections
/// their affinity, so an integral p90 reads back REAL as it was stored.
const PROJECTIONS: [(&str, Projection); 41] = [
    ("outcome", Projection::Text("$.outcome")),
    ("matched", Projection::CountOr0("$.matched")),
    ("ours_only", Projection::CountOr0("$.ours_only")),
    ("tv_only", Projection::CountOr0("$.tv_only")),
    ("boundary_ours", Projection::CountOr0("$.boundary_ours")),
    ("boundary_tv", Projection::CountOr0("$.boundary_tv")),
    ("count_tier", Projection::Text("$.count_tier")),
    ("acc_tier", Projection::NonEmpty("$.acceptance.tier")),
    ("acc_profile", Projection::NonEmpty("$.acceptance.profile")),
    ("acc_failing", Projection::Failing),
    ("p90_entry", Projection::Real("$.acceptance.p90.entry")),
    ("p90_exit", Projection::Real("$.acceptance.p90.exit")),
    ("p90_pnl", Projection::Real("$.acceptance.p90.pnl")),
    ("sig_domain", Projection::NonEmpty("$.signature.domain")),
    ("sig_leg", Projection::NonEmpty("$.signature.leg")),
    (
        "sig_dimension",
        Projection::NonEmpty("$.signature.dimension"),
    ),
    ("sig_detail", Projection::NonEmpty("$.signature.detail")),
    ("sig_breaches", Projection::Breaches),
    ("error", Projection::Text("$.error")),
    ("runtime_ms", Projection::Real("$.runtime_ms")),
    ("boundary_anchor", Projection::Text("$.boundary_anchor")),
    (
        "anchor_consumed",
        Projection::Flag("$.boundary_rules.start_anchor_consumed"),
    ),
    (
        "rule_start_ours",
        Projection::Count("$.boundary_rules.start_ours"),
    ),
    (
        "rule_tail_ours",
        Projection::Count("$.boundary_rules.tail_ours"),
    ),
    (
        "rule_start_tv",
        Projection::Count("$.boundary_rules.start_tv"),
    ),
    ("rule_end_tv", Projection::Count("$.boundary_rules.end_tv")),
    ("clipped_ours", Projection::Count("$.clipped_ours")),
    ("clipped_tv", Projection::Count("$.clipped_tv")),
    (
        "history_prefix_bars",
        Projection::Count("$.history_prefix_bars"),
    ),
    ("oracle_trimmed", Projection::Count("$.oracle_trimmed")),
    ("oracle_realtime", Projection::Count("$.oracle_realtime")),
    (
        "ts_entry_considered",
        Projection::Count("$.ts_shift.entry_considered"),
    ),
    (
        "ts_entry_shifted",
        Projection::Count("$.ts_shift.entry_shifted"),
    ),
    (
        "ts_entry_share_pct",
        Projection::Real("$.ts_shift.entry_share_pct"),
    ),
    (
        "ts_exit_considered",
        Projection::Count("$.ts_shift.exit_considered"),
    ),
    (
        "ts_exit_shifted",
        Projection::Count("$.ts_shift.exit_shifted"),
    ),
    (
        "ts_exit_share_pct",
        Projection::Real("$.ts_shift.exit_share_pct"),
    ),
    ("window_sensitive", Projection::Array("$.window_sensitive")),
    // Why an `unevaluated` probe could not be evaluated (v16).
    ("reason", Projection::Text("$.reason")),
    // How an aborted probe died, as the line sent it (v20): its JSON text,
    // NULL on a line that carried none, so a row from before piners sent
    // it reads unknown, never some death.
    ("death", Projection::Object("$.death")),
    // Why a compile_fail or runtime_fail probe stopped, as the line sent it
    // (v21); NULL on a line that carried none, so an older row reads unknown.
    ("cause", Projection::Text("$.cause")),
];

/// One ratchet census projection: its kind, column name, JSON path, and
/// whether it is the text `tolerance` rather than a count.
type RatchetProjection = (RatchetKind, String, String, bool);

/// The ratchet census projections (`ratchet.rs`), one rule for every kind:
/// `<prefix>_<leg>_<count>` for each count a leg carries, then
/// `<prefix>_<leg>_tolerance` where it carries one, each over
/// `$.<key>.<leg>.<field>` and NULL where the line carried no such leg.
/// Performance's six came at v18 (`perf_*`); the label (`label_*`) and
/// excursion (`exc_*`) ones at v19, reading every row already stored.
static RATCHET_PROJECTIONS: LazyLock<Vec<RatchetProjection>> = LazyLock::new(|| {
    let mut out = Vec::new();
    for kind in RatchetKind::ALL {
        for leg in kind.legs() {
            let fields = kind
                .counts()
                .iter()
                .map(|c| (*c, false))
                .chain(kind.has_tolerance().then_some(("tolerance", true)));
            for (field, text) in fields {
                out.push((
                    kind,
                    format!("{}_{leg}_{field}", kind.projection_prefix()),
                    format!("$.{}.{leg}.{field}", kind.key()),
                    text,
                ));
            }
        }
    }
    out
});

/// The ratchet projections as `(name, projection)`, in kind then leg order.
fn ratchet_projections() -> impl Iterator<Item = (&'static str, Projection)> {
    LazyLock::force(&RATCHET_PROJECTIONS)
        .iter()
        .map(|(_, name, path, text)| {
            let p = if *text {
                Projection::Text(path.as_str())
            } else {
                Projection::Count(path.as_str())
            };
            (name.as_str(), p)
        })
}

/// The ratchet projection names of one kind, for the migration step that
/// brings them to a store created before them.
pub(super) fn ratchet_projection_names(kind: RatchetKind) -> Vec<&'static str> {
    LazyLock::force(&RATCHET_PROJECTIONS)
        .iter()
        .filter(|(k, ..)| *k == kind)
        .map(|(_, name, ..)| name.as_str())
        .collect()
}

/// Every `disposition` projection, the harness fields then the ratchet
/// censuses, in column order.
fn disposition_projections() -> Vec<(&'static str, Projection)> {
    PROJECTIONS
        .iter()
        .copied()
        .chain(ratchet_projections())
        .collect()
}

/// Every projected `disposition` column name, in column order.
pub(super) fn disposition_projection_names() -> Vec<&'static str> {
    disposition_projections()
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

/// The `ADD COLUMN` definition of the projection `name`, for a migration
/// step that brings a projection to a store created before it.
pub(super) fn disposition_projection_column(name: &str) -> Option<String> {
    disposition_projections()
        .into_iter()
        .find(|(col, _)| *col == name)
        .map(|(col, p)| column_def(col, p))
}

/// The `disposition` DDL under `name` - `disposition` itself, or the v5
/// migration's staging table, which must be the same definition.
pub(super) fn disposition_ddl(name: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {name} (
    run_id        INTEGER NOT NULL REFERENCES run(run_id),
    probe         TEXT NOT NULL,
    disposition   TEXT NOT NULL,
    expected      TEXT,
    gate_ok       INTEGER NOT NULL,
    raw_source    TEXT NOT NULL,
    raw_json      TEXT NOT NULL,
    expected_labels TEXT,
    label_gate    TEXT,
    scored        INTEGER,
    defect        TEXT,
    expected_performance TEXT,
    performance_gate TEXT,
    expected_excursions TEXT,
    excursion_gate TEXT,
    expected_death TEXT,
    expected_attribution TEXT,
    expected_cause TEXT,
{},
    PRIMARY KEY (run_id, probe)
);
CREATE INDEX IF NOT EXISTS idx_{name}_probe ON {name}(probe, run_id);",
        generated(&disposition_projections())
    )
}

/// The `trade_diff` table's harness fields, projected from `raw_json` as the
/// `disposition` ones are. Timestamps, deltas and bar indices are exact
/// signed integers; ids, signals, labels, sides and `breaches` keep an empty
/// string as stored. The v13 migration verifies the first 23 against the
/// typed columns they replace; the rest arrived after the typed table
/// dropped them, so a reconstructed row reads NULL there - not retained.
/// v19 appended the excursions both sides carry and the held-quantity
/// witness, after the typed prefix the v13 rebuild reads; an excursion is a
/// metric, so one piners spells `inf` or `nan` reads NULL here (the record
/// keeps it).
pub(super) const TRADE_DIFF_PROJECTIONS: [(&str, Projection); 34] = [
    ("our_entry_ts", Projection::Int("$.our_entry_ts")),
    ("our_exit_ts", Projection::Int("$.our_exit_ts")),
    ("our_entry_price", Projection::Real("$.our_entry_price")),
    ("our_exit_price", Projection::Real("$.our_exit_price")),
    ("our_qty", Projection::Real("$.our_qty")),
    ("our_pnl", Projection::Real("$.our_pnl")),
    ("entry_ts_delta", Projection::Int("$.entry_ts_delta")),
    ("exit_ts_delta", Projection::Int("$.exit_ts_delta")),
    ("entry_price_delta", Projection::Real("$.entry_price_delta")),
    ("exit_price_delta", Projection::Real("$.exit_price_delta")),
    ("our_entry_bar", Projection::Int("$.our_entry_bar")),
    ("our_exit_bar", Projection::Int("$.our_exit_bar")),
    ("our_side", Projection::Text("$.our_side")),
    ("our_entry_id", Projection::Text("$.our_entry_id")),
    ("our_exit_id", Projection::Text("$.our_exit_id")),
    ("tv_entry_ts", Projection::Int("$.tv_entry_ts")),
    ("tv_exit_ts", Projection::Int("$.tv_exit_ts")),
    ("tv_entry_price", Projection::Real("$.tv_entry_price")),
    ("tv_exit_price", Projection::Real("$.tv_exit_price")),
    ("tv_entry_qty", Projection::Real("$.tv_entry_qty")),
    ("tv_pnl", Projection::Real("$.tv_pnl")),
    ("tv_entry_signal", Projection::Text("$.tv_entry_signal")),
    ("tv_exit_signal", Projection::Text("$.tv_exit_signal")),
    ("tv_side", Projection::Text("$.tv_side")),
    ("our_entry_label", Projection::Text("$.our_entry_label")),
    ("our_exit_label", Projection::Text("$.our_exit_label")),
    ("breaches", Projection::Text("$.breaches")),
    ("our_run_up", Projection::Real("$.our_run_up")),
    ("our_drawdown", Projection::Real("$.our_drawdown")),
    ("tv_run_up", Projection::Real("$.tv_run_up")),
    ("tv_drawdown", Projection::Real("$.tv_drawdown")),
    ("tv_entry_value", Projection::Real("$.tv_entry_value")),
    ("tv_held_qty_lo", Projection::Real("$.tv_held_qty_lo")),
    ("tv_held_qty_hi", Projection::Real("$.tv_held_qty_hi")),
];

/// The `ADD COLUMN` definition of the `trade_diff` projection `name`.
pub(super) fn trade_diff_projection_column(name: &str) -> Option<String> {
    TRADE_DIFF_PROJECTIONS
        .iter()
        .find(|(col, _)| *col == name)
        .map(|(col, p)| column_def(col, *p))
}

/// The scalars leg's figures, one row per pair (v19), for `corpus-results
/// --scalars` and `--sql`. A view, not a table: it reads the stored record,
/// so it covers every retained row, and retention needs nothing for it.
/// Every step is type-guarded, because the parser held `pairs` to nothing
/// before v19 and the store keeps any record it accepted: only an array is
/// expanded, a field is read only from an object element and only with the
/// JSON type it should have, `figure` as text, `matched` from a boolean
/// (1/0). `ours` and `theirs` are as sent: a JSON number as a REAL (an
/// integer cast, so a numeric filter needs `typeof(ours) = 'real'`), a
/// metric spelling (`inf`, `-inf`, `nan`) as TEXT. `diff` (ours - theirs)
/// and `rel_diff` (|ours - theirs| over the larger magnitude, 0 when both
/// are 0, the measure piners' scalars tolerance is stated in) are NULL
/// unless both are numbers, and an overflowing difference reads as SQLite
/// computes it. `matched` is piners' verdict; nothing here recomputes it.
/// It reads only `run_id`, `probe` and `raw_json`, so a migration dropping
/// another column never meets it.
pub(super) const SCALAR_PAIR_VIEW: &str = "\
CREATE VIEW IF NOT EXISTS scalar_pair AS
SELECT run_id, probe, ordinal, figure, ours, theirs, matched,
    CASE WHEN typeof(ours) = 'real' AND typeof(theirs) = 'real'
        THEN ours - theirs END AS diff,
    CASE WHEN typeof(ours) = 'real' AND typeof(theirs) = 'real' THEN
        CASE WHEN ours = 0 AND theirs = 0 THEN 0.0
            ELSE abs(ours - theirs) / max(abs(ours), abs(theirs)) END
        END AS rel_diff
FROM (
    SELECT d.run_id AS run_id, d.probe AS probe, p.key AS ordinal,
        CASE WHEN p.type = 'object' AND json_type(p.value, '$.figure') = 'text'
            THEN json_extract(p.value, '$.figure') END AS figure,
        CASE WHEN p.type = 'object' THEN
            CASE json_type(p.value, '$.ours')
                WHEN 'integer' THEN CAST(json_extract(p.value, '$.ours') AS REAL)
                WHEN 'real' THEN json_extract(p.value, '$.ours')
                WHEN 'text' THEN json_extract(p.value, '$.ours') END
            END AS ours,
        CASE WHEN p.type = 'object' THEN
            CASE json_type(p.value, '$.theirs')
                WHEN 'integer' THEN CAST(json_extract(p.value, '$.theirs') AS REAL)
                WHEN 'real' THEN json_extract(p.value, '$.theirs')
                WHEN 'text' THEN json_extract(p.value, '$.theirs') END
            END AS theirs,
        CASE WHEN p.type = 'object' THEN
            CASE json_type(p.value, '$.match') WHEN 'true' THEN 1 WHEN 'false' THEN 0 END
            END AS matched
    FROM disposition AS d,
        json_each(CASE WHEN json_type(d.raw_json, '$.performance.scalars.pairs') = 'array'
            THEN json_extract(d.raw_json, '$.performance.scalars.pairs') END) AS p
);
";

/// How many leading [`TRADE_DIFF_PROJECTIONS`] the typed (v12 and older)
/// table kept, which the v13 rebuild reconstructs and verifies.
pub(super) const TRADE_DIFF_TYPED_FIELDS: usize = 23;

fn generated(projections: &[(&str, Projection)]) -> String {
    projections
        .iter()
        .map(|(col, p)| format!("    {}", column_def(col, *p)))
        .collect::<Vec<_>>()
        .join(",\n")
}

fn column_def(col: &str, p: Projection) -> String {
    format!(
        "{col} {} GENERATED ALWAYS AS ({}) VIRTUAL",
        p.sql_type(),
        p.sql()
    )
}

/// The `trade_diff` DDL under `name` - `trade_diff` itself, or the v13
/// migration's staging table. The key stays physical: SQLite refuses a
/// generated column in a primary key, and brokkr validates the key at parse.
pub(super) fn trade_diff_ddl(name: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {name} (
    run_id        INTEGER NOT NULL REFERENCES run(run_id),
    probe         TEXT NOT NULL,
    our_index     INTEGER NOT NULL,
    tv_index      INTEGER NOT NULL,
    raw_source    TEXT NOT NULL,
    raw_json      TEXT NOT NULL,
{},
    PRIMARY KEY (run_id, probe, our_index, tv_index)
);",
        generated(&TRADE_DIFF_PROJECTIONS)
    )
}

pub(super) const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS run (
    run_id            INTEGER PRIMARY KEY,
    started_at        TEXT NOT NULL,
    selector          TEXT NOT NULL,
    gated             INTEGER NOT NULL,
    result            TEXT NOT NULL,
    fail_reason       TEXT,
    harness_exit_code INTEGER,
    probe_count       INTEGER NOT NULL,
    harness_stderr    TEXT,
    wall_ms           REAL,
    commit_sha        TEXT,
    dirty             INTEGER,
    protocol_violations INTEGER,
    diagnosis         TEXT,
    run_error         TEXT,
    debug             INTEGER,
    harness_args      TEXT,
    report_warnings   TEXT NOT NULL DEFAULT '[]',
    cargo_profile     TEXT
);
CREATE INDEX IF NOT EXISTS idx_run_started ON run(started_at);

CREATE TABLE IF NOT EXISTS gate_miss (
    run_id   INTEGER NOT NULL REFERENCES run(run_id),
    probe    TEXT NOT NULL,
    expected TEXT,
    actual   TEXT,
    expected_death TEXT,
    expected_attribution TEXT,
    expected_cause TEXT,
    PRIMARY KEY (run_id, probe)
);

CREATE TABLE IF NOT EXISTS dense_na_site (
    id        INTEGER PRIMARY KEY,
    run_id    INTEGER NOT NULL REFERENCES run(run_id),
    probe     TEXT NOT NULL,
    name      TEXT NOT NULL,
    call_site TEXT NOT NULL,
    na_count  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_dense_na_run ON dense_na_site(run_id);
";

/// Each probe's scheduling estimate per execution context, kept up to date
/// by ingest so scheduling is a keyed read instead of a pass over history.
/// Deliberately outlives run retention: an estimate is advisory and an old
/// one beats none, so `run_id` is provenance only - not a foreign key, and
/// its run may have been pruned.
pub(super) const SCHEDULE_ESTIMATE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS schedule_estimate (
    cargo_profile TEXT NOT NULL,
    harness_args  TEXT NOT NULL,
    probe         TEXT NOT NULL,
    runtime_ms    REAL NOT NULL,
    run_id        INTEGER NOT NULL,
    PRIMARY KEY (cargo_profile, harness_args, probe)
);
";

impl CorpusDb {
    /// Open (or create) the database read-write at `path`, creating the parent
    /// directory, enabling WAL, running migrations, and applying the schema.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn =
            crate::db::store::open_read_write(path, "piners corpus runs", migrate::SCHEMA_VERSION)?;
        migrate::run_migrations(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(SCHEDULE_ESTIMATE_DDL)?;
        conn.execute_batch(&disposition_ddl("disposition"))?;
        conn.execute_batch(&trade_diff_ddl("trade_diff"))?;
        Ok(Self { conn })
    }

    /// Open the database read-only for queries. The read-only flag is the
    /// load-bearing guard behind the `--where`/`--sql` raw-SQL paths: SQLite
    /// rejects any write regardless of what the interpolated SQL asks for.
    ///
    /// A store written by an older brokkr is brought to the current schema
    /// first, through a short-lived read-write [`Self::open`]: the read paths
    /// (`wall_ms`, the `boundary_*` columns) name columns only a migrated
    /// store has, and a read-only connection cannot migrate - so without this
    /// step an old `runs.db` failed every query. The migrations are column adds
    /// plus the v5 `disposition` rebuild; the read-only connection the caller
    /// gets is opened afterwards.
    pub fn open_readonly(path: &Path) -> Result<Self, DevError> {
        let conn = crate::db::store::open_read_only_migrating(
            path,
            "piners corpus runs",
            migrate::SCHEMA_VERSION,
            || {
                drop(Self::open(path)?);
                Ok(())
            },
        )?;
        Ok(Self { conn })
    }

    /// Borrow the underlying connection (crate-internal, for sibling modules).
    pub(super) fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// In-memory database for tests - applies the schema, skips WAL/migrate
    /// (no file, nothing to migrate). Keeps test data out of `/tmp`.
    #[cfg(test)]
    pub(super) fn open_in_memory() -> Result<Self, DevError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(SCHEDULE_ESTIMATE_DDL)?;
        conn.execute_batch(&disposition_ddl("disposition"))?;
        conn.execute_batch(&trade_diff_ddl("trade_diff"))?;
        conn.execute_batch(SCALAR_PAIR_VIEW)?;
        Ok(Self { conn })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The generated ratchet columns keep the exact definitions stores
    /// already hold: a v18 store's `perf_*` columns were written from this
    /// text, and `CREATE TABLE IF NOT EXISTS` never reconciles a store with a
    /// changed rule, so the rule may not drift from them.
    #[test]
    fn the_v18_performance_projections_keep_their_definitions() {
        let expected = [
            (
                "perf_equity_compared",
                "$.performance.equity.compared",
                false,
            ),
            (
                "perf_equity_mismatched",
                "$.performance.equity.mismatched",
                false,
            ),
            (
                "perf_equity_tolerance",
                "$.performance.equity.tolerance",
                true,
            ),
            (
                "perf_scalars_compared",
                "$.performance.scalars.compared",
                false,
            ),
            (
                "perf_scalars_mismatched",
                "$.performance.scalars.mismatched",
                false,
            ),
            (
                "perf_scalars_tolerance",
                "$.performance.scalars.tolerance",
                true,
            ),
        ];
        assert_eq!(
            ratchet_projection_names(RatchetKind::Performance),
            expected.map(|(n, ..)| n)
        );
        for (name, path, text) in expected {
            let p = if text {
                Projection::Text(path)
            } else {
                Projection::Count(path)
            };
            assert_eq!(
                disposition_projection_column(name),
                Some(column_def(name, p)),
                "{name}"
            );
        }
        assert!(disposition_projection_column("exc_run_up_ambiguous").is_some());
        assert!(disposition_projection_column("label_exit_unavailable").is_some());
        assert!(disposition_projection_column("exc_run_up_frob").is_none());
    }

    /// The view reads what the record states and nothing it cannot type.
    #[test]
    fn scalar_pair_reads_each_figure_and_guards_what_it_cannot_type() {
        let db = CorpusDb::open_in_memory().unwrap();
        let line = |probe: &str, pairs: &str| {
            format!(
                r#"{{"probe":"{probe}","performance":{{"scalars":{{"compared":2,"mismatched":1,"tolerance":"t","pairs":{pairs}}}}}}}"#
            )
        };
        db.conn()
            .execute_batch(
                "INSERT INTO run (run_id, started_at, selector, gated, result, probe_count) \
                 VALUES (1, 'then', '{}', 1, 'pass', 3)",
            )
            .unwrap();
        for (probe, raw) in [
            (
                "a",
                line(
                    "a",
                    r#"[{"figure":"max_runup","ours":100,"theirs":101.0,"match":false},{"figure":"max_drawdown","ours":0,"theirs":0,"match":true},{"figure":"f","ours":"nan","theirs":1,"match":0}]"#,
                ),
            ),
            ("b", line("b", r#"["bad", 3]"#)),
            ("c", line("c", r#"{"not":"an array"}"#)),
        ] {
            db.conn()
                .execute(
                    "INSERT INTO disposition (run_id, probe, disposition, gate_ok, raw_source, \
                     raw_json) VALUES (1, ?1, 'accepted', 1, 'harness', ?2)",
                    rusqlite::params![probe, raw],
                )
                .unwrap();
        }
        let t = db
            .raw_sql(
                "SELECT probe, ordinal, figure, ours, theirs, matched, diff, rel_diff, \
                 typeof(ours) FROM scalar_pair ORDER BY probe, ordinal",
            )
            .unwrap();
        let rows: Vec<Vec<&str>> = t
            .rows
            .iter()
            .map(|r| r.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert_eq!(rows[0][..6], ["a", "0", "max_runup", "100", "101", "0"]);
        assert_eq!(rows[0][8], "real");
        assert!(rows[0][7].starts_with("0.0099"), "{rows:?}");
        assert_eq!(rows[1][7], "0", "two zeros are no difference");
        // A metric spelling is shown as sent, with no arithmetic, and a
        // non-boolean match reads NULL.
        assert_eq!(rows[2][3], "nan");
        assert_eq!(rows[2][8], "text");
        assert_eq!(rows[2][6], rows[2][7]);
        // Elements that are no object expand to rows holding nothing; a
        // non-array never expands.
        assert!(rows[3..].iter().all(|r| r[0] == "b" && r[2] == rows[2][6]));
    }

    #[test]
    fn readonly_open_migrates_an_older_store_first() {
        // A pre-v4 store on disk: `run` has no `wall_ms`, and `disposition`
        // is the v1 shape.
        let dir = crate::test_scratch::scratch("piners_corpus_db_schema", "old_store");
        let path = dir.join("runs.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE run (run_id INTEGER PRIMARY KEY, started_at TEXT NOT NULL, \
                     selector TEXT NOT NULL, gated INTEGER NOT NULL, result TEXT NOT NULL, \
                     fail_reason TEXT, harness_exit_code INTEGER, probe_count INTEGER NOT NULL, \
                     harness_stderr TEXT);
                 CREATE TABLE disposition (run_id INTEGER NOT NULL, probe TEXT NOT NULL, \
                     outcome TEXT NOT NULL, disposition TEXT NOT NULL, expected TEXT, \
                     gate_ok INTEGER NOT NULL, matched INTEGER NOT NULL, \
                     ours_only INTEGER NOT NULL, tv_only INTEGER NOT NULL, count_tier TEXT, \
                     acc_tier TEXT, acc_profile TEXT, acc_failing TEXT NOT NULL DEFAULT '[]', \
                     p90_entry REAL, p90_exit REAL, p90_pnl REAL, sig_domain TEXT, \
                     sig_leg TEXT, sig_dimension TEXT, sig_detail TEXT, sig_breaches INTEGER, \
                     error TEXT, PRIMARY KEY (run_id, probe));
                 INSERT INTO run VALUES (1, 'then', '{}', 1, 'pass', NULL, 0, 1, '');
                 INSERT INTO disposition (run_id, probe, outcome, disposition, gate_ok, \
                     matched, ours_only, tv_only) VALUES (1, 'p', 'parity', 'accepted', 1, 3, 0, 0);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let db = CorpusDb::open_readonly(&path).unwrap();
        // The v1 row survived the v5 rebuild and answers the typed queries.
        let d = db.disposition_for_probe(1, "p").unwrap().unwrap();
        assert_eq!((d.matched, d.disposition.as_str()), (3, "accepted"));
        let version: i64 = db
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, migrate::SCHEMA_VERSION);
    }
}
