//! A library call against its CLI counterpart (`docs/commands/library.md`,
//! Isolation scenarios): the `/cli` row's own workload qualifier, and the
//! comparison view the end of a measurement and `brokkr results <uuid>`
//! print. The comparison is this declared library invocation against this
//! declared CLI invocation, after the same dataset page-cache preparation,
//! library first: never a measure of CLI overhead.

use std::collections::BTreeMap;

use super::describe::{Cli, Scenario, Token};
use super::json::{self, Json};

/// The suffix of a `/cli` row's command after its call row's.
pub(crate) const CLI_SUFFIX: &str = "/cli";

/// The build every CLI counterpart runs (`brokkr man library`, CLI
/// equality), as the view names it.
pub(crate) const CLI_BUILD: &str = "release, default features";

/// The `/cli` row's arm qualifier: the CLI workload, not the library's. Its
/// counterpart as declared (argv with placeholders by role, scratch,
/// expectation, overwrite stance), the seed recipe of each seeded output it
/// names, the identity of each dataset role it reads, the page-cache
/// preparation and the interval definition. No executable digest (a
/// trend across commits measures exactly that change) and no plan or
/// context (they are the library side's).
pub(crate) fn qualifier(
    scenario: &Scenario,
    counterpart: &Cli,
    datasets: &BTreeMap<String, String>,
) -> Result<String, String> {
    // By role, each once: an input placeholder may repeat a role, and the
    // repetition is in the argv already.
    let mut inputs: BTreeMap<String, Json> = BTreeMap::new();
    let mut seeds: BTreeMap<String, Json> = BTreeMap::new();
    for token in &counterpart.argv {
        match token {
            Token::Input(role) | Token::FilesIn(role) => {
                if let Some(id) = datasets.get(role) {
                    inputs.insert(role.clone(), Json::Str(id.clone()));
                }
            }
            Token::Output(role) => {
                if let Some(r) = scenario.roles.iter().find(|r| r.id == *role && r.seeded()) {
                    seeds.insert(role.clone(), Json::Str(super::seed::recipe(r.kind)));
                }
            }
            Token::Literal(_) => {}
        }
    }
    let inputs: Vec<(String, Json)> = inputs.into_iter().collect();
    let seeds: Vec<(String, Json)> = seeds.into_iter().collect();
    let value = Json::Object(vec![
        ("v".to_owned(), Json::Int(1)),
        (
            "argv".to_owned(),
            Json::Array(
                counterpart
                    .argv
                    .iter()
                    .map(|t| Json::Str(t.show()))
                    .collect(),
            ),
        ),
        ("scratch".to_owned(), Json::Bool(counterpart.scratch)),
        ("expect".to_owned(), Json::Str(counterpart.expect.show())),
        (
            "overwrite".to_owned(),
            counterpart.overwrite.map_or(Json::Null, Json::Bool),
        ),
        ("seeds".to_owned(), Json::Object(seeds)),
        ("inputs".to_owned(), Json::Object(inputs)),
        (
            "prep".to_owned(),
            Json::Str(super::prep::PROCEDURE.to_owned()),
        ),
        (
            "interval".to_owned(),
            Json::Str(super::interval::DEFINITION.to_owned()),
        ),
    ]);
    json::canonical(&value)
}

/// One call against its CLI counterpart, over the same iterations in order.
pub(crate) struct Comparison {
    pub(crate) call: String,
    /// Each process's `return_us - entry_us`.
    pub(crate) library_us: Vec<i64>,
    /// Each CLI process's command interval.
    pub(crate) cli_us: Vec<i64>,
    /// The harness build, as the view names it.
    pub(crate) library_build: String,
    pub(crate) policy_differences: String,
}

/// The median of `values`, the mean of the middle two for an even count;
/// `None` for none.
fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(sorted[n / 2]),
        _ => Some(f64::midpoint(sorted[n / 2 - 1], sorted[n / 2])),
    }
}

#[allow(clippy::cast_precision_loss)]
fn as_f64(values: &[i64]) -> Vec<f64> {
    values.iter().map(|v| *v as f64).collect()
}

/// A wall in microseconds at the resolution a call's readings have.
pub(crate) fn duration(us: f64) -> String {
    let us = us.max(0.0);
    if us < 1_000.0 {
        format!("{us:.0}us")
    } else if us < 1_000_000.0 {
        format!("{:.3}ms", us / 1_000.0)
    } else {
        format!("{:.3}s", us / 1_000_000.0)
    }
}

impl Comparison {
    /// The view, one line each: the claim, each arm's min and median, the
    /// median of the paired ratios (or why there is none), both builds, and
    /// the declared policy differences.
    pub(crate) fn lines(&self) -> Vec<String> {
        let arm = |name: &str, us: &[i64]| -> String {
            let values = as_f64(us);
            let min = values.iter().copied().fold(f64::INFINITY, f64::min);
            match median(&values) {
                Some(med) => format!(
                    "  {name:<8} min {}, median {} over {}",
                    duration(min),
                    duration(med),
                    crate::output::count(values.len(), "process")
                ),
                None => format!("  {name:<8} no sample"),
            }
        };
        let mut lines = vec![
            format!(
                "library vs CLI, call {}: this declared library invocation against this declared \
                 CLI invocation, library first, each after the {} preparation; the CLI interval \
                 ({}) also holds the CLI's option handling, output and scratch setup, process \
                 policy, publication and printed summary",
                self.call,
                super::prep::PROCEDURE,
                super::interval::DEFINITION
            ),
            arm("library", &self.library_us),
            arm("CLI", &self.cli_us),
        ];
        lines.push(match self.paired_ratio() {
            Ok(ratio) => format!("  CLI/library, median of the paired ratios: {ratio:.3}"),
            Err(why) => format!("  CLI/library, median of the paired ratios: unavailable, {why}"),
        });
        let differ = if self.library_build == CLI_BUILD {
            ""
        } else {
            " (the builds differ)"
        };
        lines.push(format!(
            "  builds   harness {}, CLI {CLI_BUILD}{differ}",
            self.library_build
        ));
        if !self.policy_differences.is_empty() {
            lines.push(format!(
                "  declared policy differences: {}",
                self.policy_differences
            ));
        }
        lines
    }

    /// The median of `cli_i / library_i` over the pairs, or why there is
    /// none: unequal counts, or a pair with a zero reading (never dropped
    /// silently).
    pub(crate) fn paired_ratio(&self) -> Result<f64, String> {
        if self.library_us.len() != self.cli_us.len() || self.library_us.is_empty() {
            return Err(format!(
                "the arms have {} and {} samples",
                self.library_us.len(),
                self.cli_us.len()
            ));
        }
        let mut ratios = Vec::new();
        for (i, (l, c)) in self.library_us.iter().zip(&self.cli_us).enumerate() {
            if *l <= 0 || *c <= 0 {
                return Err(format!("pair {i} has a zero reading"));
            }
            #[allow(clippy::cast_precision_loss)]
            ratios.push(*c as f64 / *l as f64);
        }
        median(&ratios).ok_or_else(|| "no pair".to_owned())
    }
}

/// The metadata key only a `/cli` row carries, without its namespace.
pub(crate) const INTERVAL_KEY: &str = "library.cli_interval";

fn meta(row: &crate::db::StoredRow, key: &str) -> Option<String> {
    row.kv
        .iter()
        .find(|p| p.class() == crate::db::KvClass::Metadata && p.key.name() == key)
        .map(|p| p.value.to_string())
}

/// The comparison a stored group holds: its one `/cli` row (a command
/// ending `/cli` that carries the interval definition, so a call merely
/// named `cli` is never one) and the call row whose command it extends,
/// each read as every iteration in order. `None` for a group without
/// exactly one of each.
pub(crate) fn from_rows(rows: &[crate::db::StoredRow]) -> Option<Comparison> {
    let mut clis = rows
        .iter()
        .filter(|r| r.command.ends_with(CLI_SUFFIX) && meta(r, INTERVAL_KEY).is_some());
    let cli = clis.next()?;
    if clis.next().is_some() {
        return None;
    }
    let call_command = cli.command.strip_suffix(CLI_SUFFIX)?;
    let mut calls = rows.iter().filter(|r| r.command == call_command);
    let call = calls.next()?;
    if calls.next().is_some() {
        return None;
    }
    let us = |row: &crate::db::StoredRow| -> Vec<i64> {
        row.iterations
            .iter()
            .map(|w| w.micros().unwrap_or_else(|| w.ms().saturating_mul(1_000)))
            .collect()
    };
    let meta = |row: &crate::db::StoredRow, key: &str| meta(row, key).unwrap_or_default();
    Some(Comparison {
        call: call_command
            .rsplit_once('/')
            .map_or(call_command, |(_, c)| c)
            .to_owned(),
        library_us: us(call),
        cli_us: us(cli),
        library_build: meta(cli, "library.build"),
        policy_differences: meta(cli, "library.policy_differences"),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn comparison(library_us: Vec<i64>, cli_us: Vec<i64>) -> Comparison {
        Comparison {
            call: "a".to_owned(),
            library_us,
            cli_us,
            library_build: CLI_BUILD.to_owned(),
            policy_differences: "the CLI raises NOFILE".to_owned(),
        }
    }

    #[test]
    fn the_paired_ratio_is_the_median_of_each_pair_s_own() {
        // Pairs 2.0, 1.0, 1.5: median 1.5, while the medians' ratio is 1.0.
        let c = comparison(vec![100, 200, 200], vec![200, 200, 300]);
        assert!((c.paired_ratio().unwrap() - 1.5).abs() < 1e-9);
        let even = comparison(vec![100, 100], vec![100, 300]);
        assert!((even.paired_ratio().unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_zero_reading_or_unequal_arms_leave_no_paired_ratio() {
        let zero = comparison(vec![100, 0], vec![100, 100]);
        assert!(zero.paired_ratio().unwrap_err().contains("pair 1 has a zero reading"));
        let unequal = comparison(vec![100], vec![100, 100]);
        assert!(unequal.paired_ratio().unwrap_err().contains("1 and 2 samples"));
        let lines = zero.lines();
        assert!(lines.iter().any(|l| l.contains("unavailable, pair 1")), "{lines:?}");
    }

    fn stored(command: &str, iterations_us: &[i64], cli: bool) -> crate::db::StoredRow {
        let mut row = crate::db::fixture_row(command, "bench", "in.osm.pbf", 0);
        row.iterations = iterations_us
            .iter()
            .map(|u| crate::db::Wall::from_micros(*u))
            .collect();
        if cli {
            row.kv.push(crate::db::KvPair::text(
                crate::db::KvKey::metadata(INTERVAL_KEY),
                super::super::interval::DEFINITION,
            ));
        }
        row
    }

    #[test]
    fn a_stored_group_pairs_its_cli_row_with_the_call_it_extends() {
        let rows = [
            stored("library:s", &[50, 60], false),
            stored("library:s/a", &[10, 20], false),
            stored("library:s/a/cli", &[30, 40], true),
        ];
        let c = from_rows(&rows).unwrap();
        assert_eq!((c.call.as_str(), c.library_us.as_slice()), ("a", &[10, 20][..]));
        assert_eq!(c.cli_us, [30, 40]);
        // An isolation call named `cli` is still told apart.
        let rows = [
            stored("library:s", &[50], false),
            stored("library:s/cli", &[10], false),
            stored("library:s/cli/cli", &[30], true),
        ];
        let c = from_rows(&rows).unwrap();
        assert_eq!((c.call.as_str(), c.cli_us.as_slice()), ("cli", &[30][..]));
    }

    #[test]
    fn a_call_merely_named_cli_is_no_comparison() {
        let rows = [
            stored("library:s", &[50], false),
            stored("library:s/cli", &[10], false),
        ];
        assert!(from_rows(&rows).is_none());
    }

    #[test]
    fn a_repeated_input_keeps_the_workload_qualifier_and_it_changes_with_the_workload() {
        let scenario = super::super::describe::parse(
            br#"{"protocol": 1, "scenarios": [{"id": "s", "description": "d", "deadline_s": 60,
              "roles": [{"id": "input", "kind": "pbf", "source": "dataset", "variant": "v"},
                        {"id": "out", "kind": "pbf", "source": "output", "owner": "a"}],
              "calls": [{"id": "a", "label": "l",
                         "cli": {"argv": ["merge", {"input": "input"}, {"input": "input"},
                                          "-o", {"output": "out"}]}}],
              "assertions": []}]}"#,
        )
        .unwrap()
        .scenarios
        .remove(0);
        let cli = scenario.calls[0].cli.as_ref().unwrap();
        let datasets = BTreeMap::from([("input".to_owned(), "xxh128:1".to_owned())]);
        let q = qualifier(&scenario, cli, &datasets).unwrap();
        assert!(q.contains(r#""inputs":{"input":"xxh128:1"}"#), "{q}");
        let other = BTreeMap::from([("input".to_owned(), "xxh128:2".to_owned())]);
        assert_ne!(q, qualifier(&scenario, cli, &other).unwrap());
        let mut scratch = cli.clone();
        scratch.scratch = true;
        assert_ne!(q, qualifier(&scenario, &scratch, &datasets).unwrap());
    }

    #[test]
    fn the_view_states_its_claim_both_builds_and_the_declarations() {
        let mut c = comparison(vec![100, 300, 200], vec![150, 450, 300]);
        let lines = c.lines();
        assert!(lines[0].contains("this declared library invocation"), "{lines:?}");
        assert!(lines[1].contains("min 100us, median 200us"), "{lines:?}");
        assert!(lines[2].contains("min 150us, median 300us"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("over 3 processes")));
        assert!(lines.iter().any(|l| l.contains("declared policy differences: the CLI raises NOFILE")));
        assert!(!lines.iter().any(|l| l.contains("the builds differ")));
        c.library_build = "debug, features x".to_owned();
        assert!(c.lines().iter().any(|l| l.contains("the builds differ")));
    }
}
