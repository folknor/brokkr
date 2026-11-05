//! Parse-time validation of command-line values.

/// Validate `--as-snapshot` key: matches `[a-zA-Z0-9_-]+` and is not the
/// reserved sentinel `base`. Delegates to `config::validate_snapshot_key`
/// so the parse-time and CLI-time rules stay in sync.
pub(super) fn validate_snapshot_key_arg(s: &str) -> Result<String, String> {
    crate::config::validate_snapshot_key(s)?;
    Ok(s.to_owned())
}

/// Validate `--meta key=value`: must contain exactly one `=`. Both sides may
/// be empty (the empty-value case is legitimate for filtering rows where the
/// stored value is the empty string).
pub(super) fn validate_meta_filter(s: &str) -> Result<String, String> {
    if s.matches('=').count() != 1 {
        return Err(format!(
            "expected exactly one '=' in key=value, got '{s}' (use --meta KEY=VALUE)"
        ));
    }
    Ok(s.to_owned())
}

/// Validate `results --param NAME=VALUE`: a non-empty NAME before the first
/// `=` (a param name holds none), then VALUE, which may hold `=` itself
/// since a plain param's key is any string.
pub(super) fn validate_param_filter(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((name, _)) if !name.is_empty() => Ok(s.to_owned()),
        _ => Err(format!(
            "expected NAME=VALUE with a non-empty NAME, got '{s}' (use --param NAME=VALUE)"
        )),
    }
}

/// Validate `--env KEY=VALUE` for `brokkr clippy`: exactly one `=`, a
/// **non-empty** KEY (an env var must be named - unlike `results --meta`, whose
/// validator deliberately allows an empty key for row filtering), and any VALUE
/// including empty (`KEY=` legitimately sets an empty variable).
pub(super) fn validate_env_kv(s: &str) -> Result<String, String> {
    if s.matches('=').count() != 1 {
        return Err(format!(
            "expected exactly one '=' in KEY=VALUE, got '{s}' (use --env KEY=VALUE)"
        ));
    }
    match s.split_once('=') {
        Some((k, _)) if !k.is_empty() => Ok(s.to_owned()),
        Some(_) => Err(format!("--env KEY must be non-empty, got '{s}'")),
        None => Err(format!(
            "expected KEY=VALUE, got '{s}' (use --env KEY=VALUE)"
        )),
    }
}

/// Validate `brokkr test <NAME>`: a blank name is refused. libtest treats the
/// name as a substring filter, so `""` matches every test in the package -
/// a whole-suite run under `--nocapture`, which is `brokkr check`'s job.
pub(super) fn validate_test_name(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        return Err(
            "test name must not be blank: an empty filter matches every test \
             (run the whole suite with `brokkr check -p <pkg>`)"
                .to_owned(),
        );
    }
    Ok(s.to_owned())
}

/// Validate `--since`/`--until` format: YYYY-MM-DD or YYYY-MM-DD HH:MM:SS, with
/// every field in range (month 1-12, a day that exists in that month, hour
/// 0-23, minute and second 0-59). The value is compared as a string against
/// the stored UTC timestamps, so an out-of-range field would not error there -
/// it would silently select the wrong rows.
pub(super) fn validate_since(s: &str) -> Result<String, String> {
    let shape = validate_since_shape(s)?;
    let num = |range: std::ops::Range<usize>| -> u32 { s[range].parse().unwrap_or(u32::MAX) };
    let (year, month, day) = (num(0..4), num(5..7), num(8..10));
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(format!("invalid month in '{s}', expected 01-12")),
    };
    if day == 0 || day > days_in_month {
        return Err(format!(
            "invalid day in '{s}', month {month:02} has {days_in_month} days"
        ));
    }
    if s.len() == 19 && (num(11..13) > 23 || num(14..16) > 59 || num(17..19) > 59) {
        return Err(format!(
            "invalid time in '{s}', expected HH 00-23, MM and SS 00-59"
        ));
    }
    Ok(shape)
}

/// The shape half of [`validate_since`]: digits and separators in place.
pub(super) fn validate_since_shape(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let date_ok = b.len() >= 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit);

    let shape_ok = match b.len() {
        10 => date_ok,
        19 => {
            date_ok
                && b[10] == b' '
                && b[13] == b':'
                && b[16] == b':'
                && b[11..13].iter().all(u8::is_ascii_digit)
                && b[14..16].iter().all(u8::is_ascii_digit)
                && b[17..19].iter().all(u8::is_ascii_digit)
        }
        _ => false,
    };

    if shape_ok {
        Ok(s.to_owned())
    } else {
        Err(format!(
            "invalid date format '{s}', expected YYYY-MM-DD or YYYY-MM-DD HH:MM:SS"
        ))
    }
}

// ---------------------------------------------------------------------------
// Pbfhogg command extraction
// ---------------------------------------------------------------------------
//
// `Command::into_route` (`src/command_input.rs`) consumes a pbfhogg command
// variant into its typed `PbfhoggCommand` and parameters - the one bridge
// between the CLI shape and the dispatch layer.

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;
    use crate::cli::{Cli, Command, VerifyCommand};
    use clap::Parser;

    /// `Cli::try_parse_from` on a thread with the main thread's stack. An
    /// unoptimized parse of the whole `Command` tree needs more than the 2 MiB
    /// a libtest thread gets, and overflows it - which aborts the harness
    /// rather than failing the one test.
    fn parse(argv: impl IntoIterator<Item = &'static str>) -> Result<Cli, clap::Error> {
        let argv: Vec<&'static str> = argv.into_iter().collect();
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || Cli::try_parse_from(argv))
            .expect("spawn parser thread")
            .join()
            .expect("parser thread")
    }

    /// `brokkr bench`: brokkr's options before NAME, and nothing after NAME
    /// read by clap, `--help` and brokkr's own option names included.
    #[test]
    fn bench_options_stop_at_the_bench_name() {
        let bench = |cli: Cli| match cli.command {
            Command::Bench {
                commit,
                iterations,
                bench,
                ..
            } => (commit, iterations.get(), bench),
            _ => panic!("unexpected command"),
        };
        let (commit, iterations, rest) = bench(
            parse([
                "brokkr",
                "bench",
                "--commit",
                "abc",
                "--iterations",
                "5",
                "cat",
                "--help",
                "--commit",
                "x",
            ])
            .unwrap(),
        );
        assert_eq!((commit.as_deref(), iterations), (Some("abc"), 5));
        assert_eq!(rest, ["cat", "--help", "--commit", "x"]);
        let (_, iterations, rest) = bench(parse(["brokkr", "bench"]).unwrap());
        assert_eq!((iterations, rest.len()), (3, 0));
        assert!(parse(["brokkr", "bench", "--iterations", "0", "cat"]).is_err());
    }

    /// check-locations alone defaults to `altw-keep-untagged`, or `altw` with
    /// `--agreement`; an explicit `--variant` still wins, and a sibling check
    /// keeps `indexed`.
    #[test]
    fn check_locations_defaults_to_an_altw_variant() {
        let variant = |cli: Cli| match cli.command {
            Command::CheckLocations { pbf, .. } | Command::CheckIndex { pbf, .. } => pbf.variant,
            _ => panic!("unexpected command"),
        };
        let omitted = parse(["brokkr", "check-locations"]).unwrap();
        assert_eq!(variant(omitted), "altw-keep-untagged");
        let agreement = parse(["brokkr", "check-locations", "--agreement"]).unwrap();
        assert_eq!(variant(agreement), "altw");
        let explicit = parse(["brokkr", "check-locations", "--variant", "raw"]).unwrap();
        assert_eq!(variant(explicit), "raw");
        let both = parse([
            "brokkr",
            "check-locations",
            "--agreement",
            "--variant",
            "altw-keep-untagged",
        ])
        .unwrap();
        assert_eq!(variant(both), "altw-keep-untagged");
        let sibling = parse(["brokkr", "check-index"]).unwrap();
        assert_eq!(variant(sibling), "indexed");
    }

    /// The positional tail the arm key keeps verbatim, from a real parse.
    fn tail(argv: &'static [&'static str]) -> usize {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                let mut cmd = <Cli as clap::CommandFactory>::command();
                let matches = cmd.try_get_matches_from_mut(argv).expect("parse");
                let argv: Vec<String> = argv.iter().map(|&a| a.to_owned()).collect();
                crate::run_invocation::positional_tail(&cmd, &matches, &argv).expect("tail")
            })
            .expect("spawn parser thread")
            .join()
            .expect("parser thread")
    }

    /// Every trailing positional's values are the argv's suffix, `--`
    /// tokens inside them included, so the arm key's boundary is one count.
    #[test]
    fn trailing_positionals_are_the_argv_suffix() {
        assert_eq!(tail(&["brokkr", "mogwai", "stage", "quick", "-v"]), 2);
        assert_eq!(tail(&["brokkr", "mogwai", "stage", "quick", "--", "x"]), 3);
        assert_eq!(
            tail(&["brokkr", "mogwai", "stage", "--", "quick", "--", "x"]),
            3
        );
        assert_eq!(
            tail(&[
                "brokkr", "mogwai", "stage", "--bench", "5", "--", "--bench", "3", "-v"
            ]),
            3
        );
        assert_eq!(
            tail(&["brokkr", "mogwai", "-v", "--bench", "5", "stage"]),
            0
        );
        assert_eq!(tail(&["brokkr", "mogwai", "--bench"]), 0);
        assert_eq!(tail(&["brokkr", "run", "name", "a", "-v"]), 2);
        assert_eq!(tail(&["brokkr", "crit", "--", "--sample-size", "10"]), 2);
        assert_eq!(tail(&["brokkr", "fmt", "--", "--check"]), 1);
        assert_eq!(tail(&["brokkr", "update", "a", "--b"]), 2);
        assert_eq!(tail(&["brokkr", "read", "--bench", "3"]), 0);
    }

    #[test]
    fn results_compare_requires_two_commits() {
        let parsed = parse(["brokkr", "results", "--compare", "abc123"]);
        assert!(parsed.is_err());
    }

    /// `test` needs a name; its selection flags parse beside it.
    #[test]
    fn test_requires_a_name() {
        assert!(parse(["brokkr", "test"]).is_err());
        let plain = parse(["brokkr", "test", "some_test", "-p", "pkg", "-N", "3"]).expect("parse");
        let Command::Test { name, repeat, .. } = plain.command else {
            panic!("expected Test")
        };
        assert_eq!(name, "some_test");
        assert_eq!(repeat.get(), 3);
    }

    /// A parity run builds the harness, so it takes `--commit`; the corpus
    /// operations that build nothing refuse it.
    #[test]
    fn corpus_commit_needs_a_build() {
        assert!(parse(["brokkr", "corpus", "--all", "--commit", "abc123"]).is_ok());
        assert!(parse(["brokkr", "corpus", "--all", "--bless", "--commit", "abc123"]).is_ok());
        assert!(
            parse([
                "brokkr",
                "corpus",
                "--all",
                "--verify-only",
                "--commit",
                "abc123"
            ])
            .is_err()
        );
        assert!(
            parse([
                "brokkr", "corpus", "--all", "--reseed", "--commit", "abc123"
            ])
            .is_err()
        );
    }

    /// Corpus is not measurable: piners' corpus binary carries no profiler
    /// and no timing contract, so the measurement and measured-run flags
    /// are refused at parse instead of building something that cannot
    /// report, and the shared flags a parity run never read are gone too.
    #[test]
    fn corpus_takes_no_measurement_flags() {
        for flag in [
            "--bench",
            "--hotpath",
            "--alloc",
            "--force",
            "--dry-run",
            "--verbose",
        ] {
            assert!(
                parse(["brokkr", "corpus", "--all", flag]).is_err(),
                "{flag}"
            );
        }
        for (flag, value) in [("--stop", "m"), ("--features", "x")] {
            assert!(
                parse(["brokkr", "corpus", "--all", flag, value]).is_err(),
                "{flag}"
            );
        }
    }

    #[test]
    fn check_accepts_profile_flag() {
        let parsed = parse(["brokkr", "check", "--profile", "tier1"]).expect("parse");
        let Command::Check { profile, .. } = parsed.command else {
            panic!("expected Check");
        };
        assert_eq!(profile.as_deref(), Some("tier1"));
    }

    #[test]
    fn check_profile_conflicts_with_features() {
        let parsed = parse([
            "brokkr",
            "check",
            "--profile",
            "tier1",
            "--features",
            "commands",
        ]);
        assert!(parsed.is_err(), "--profile + --features should conflict");
    }

    #[test]
    fn check_profile_conflicts_with_no_default_features() {
        let parsed = parse([
            "brokkr",
            "check",
            "--profile",
            "tier1",
            "--no-default-features",
        ]);
        assert!(
            parsed.is_err(),
            "--profile + --no-default-features should conflict"
        );
    }

    #[test]
    fn verify_sort_accepts_input_path() {
        let parsed = parse([
            "brokkr",
            "verify",
            "sort",
            "--input",
            "fixtures/overlapping.osm.pbf",
        ])
        .expect("parse");
        let Command::Verify { verify, .. } = parsed.command else {
            panic!("expected Verify");
        };
        let VerifyCommand::Sort { pbf } = verify else {
            panic!("expected Verify::Sort");
        };
        assert_eq!(pbf.source.dataset, crate::cli::DEFAULT_DATASET);
        assert!(!pbf.direct_io);
        assert_eq!(
            pbf.source
                .input
                .as_deref()
                .map(std::path::Path::display)
                .map(|d| d.to_string()),
            Some("fixtures/overlapping.osm.pbf".to_owned())
        );
    }

    #[test]
    fn verify_sort_input_conflicts_with_dataset() {
        let parsed = parse([
            "brokkr",
            "verify",
            "sort",
            "--input",
            "x.pbf",
            "--dataset",
            "denmark",
        ]);
        assert!(parsed.is_err(), "--input + --dataset should conflict");
    }

    #[test]
    fn verify_sort_input_conflicts_with_variant() {
        let parsed = parse([
            "brokkr",
            "verify",
            "sort",
            "--input",
            "x.pbf",
            "--variant",
            "raw",
        ]);
        assert!(parsed.is_err(), "--input + --variant should conflict");
    }

    #[test]
    fn verify_renumber_accepts_input_path() {
        let parsed =
            parse(["brokkr", "verify", "renumber", "--input", "fixtures/x.pbf"]).expect("parse");
        let Command::Verify { verify, .. } = parsed.command else {
            panic!("expected Verify");
        };
        let VerifyCommand::Renumber { pbf, .. } = verify else {
            panic!("expected Verify::Renumber");
        };
        assert!(pbf.source.input.is_some());
    }

    /// `--input` conflicts with an explicit `--snapshot` through the nested
    /// flatten too, and on the subcommands that take the input alone.
    #[test]
    fn verify_input_conflicts_hold_through_every_flatten() {
        for argv in [
            [
                "brokkr",
                "verify",
                "sort",
                "--input",
                "x.pbf",
                "--snapshot",
                "base",
            ],
            [
                "brokkr",
                "verify",
                "all",
                "--input",
                "x.pbf",
                "--variant",
                "raw",
            ],
            [
                "brokkr",
                "verify",
                "check-refs",
                "--input",
                "x.pbf",
                "--dataset",
                "d",
            ],
            [
                "brokkr",
                "verify",
                "renumber",
                "--input",
                "x.pbf",
                "--snapshot",
                "base",
            ],
            [
                "brokkr",
                "verify",
                "diff",
                "--input",
                "x.pbf",
                "--variant",
                "raw",
            ],
        ] {
            let rendered = argv.join(" ");
            let Err(err) = parse(argv) else {
                panic!("{rendered} should conflict");
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{rendered}"
            );
        }
    }

    /// `--direct-io` is on the verify subcommands whose command under test
    /// takes it, `all` included, and absent from check-refs alone (pbfhogg
    /// refuses it under check --refs alone).
    #[test]
    fn verify_direct_io_is_offered_only_where_forwarded() {
        for sub in [
            "sort",
            "all",
            "derive-changes",
            "renumber",
            "diff",
            "check-ids",
            "getparents",
            "repack",
            "degrade",
        ] {
            let parsed = parse(["brokkr", "verify", sub, "--direct-io"]).expect(sub);
            let Command::Verify { verify, .. } = parsed.command else {
                panic!("expected Verify");
            };
            let pbf = match verify {
                VerifyCommand::Sort { pbf }
                | VerifyCommand::All { pbf, .. }
                | VerifyCommand::DeriveChanges { pbf, .. }
                | VerifyCommand::Renumber { pbf, .. }
                | VerifyCommand::Diff { pbf, .. }
                | VerifyCommand::CheckIds { pbf }
                | VerifyCommand::Getparents { pbf }
                | VerifyCommand::Repack { pbf }
                | VerifyCommand::Degrade { pbf } => pbf,
                _ => panic!("unexpected verify subcommand for {sub}"),
            };
            assert!(pbf.direct_io, "{sub}");
        }
        for sub in ["check-refs", "merge-changes", "tags-filter-osc"] {
            let Err(err) = parse(["brokkr", "verify", sub, "--direct-io"]) else {
                panic!("verify {sub} --direct-io should be refused");
            };
            assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument, "{sub}");
        }
    }

    #[test]
    fn verify_merge_changes_takes_one_source_of_change_files() {
        let parsed = parse([
            "brokkr",
            "verify",
            "merge-changes",
            "--osc",
            "a.osc",
            "--osc",
            "b.osc.gz",
        ])
        .expect("repeated --osc");
        let Command::Verify { verify, .. } = parsed.command else {
            panic!("expected Verify");
        };
        let VerifyCommand::MergeChanges { osc, osc_range, .. } = verify else {
            panic!("expected merge-changes");
        };
        assert_eq!(osc.len(), 2);
        assert!(osc_range.is_none());
        let parsed = parse([
            "brokkr",
            "verify",
            "merge-changes",
            "--osc-range",
            "4705..4712",
        ])
        .expect("--osc-range");
        let Command::Verify { verify, .. } = parsed.command else {
            panic!("expected Verify");
        };
        assert!(matches!(
            verify,
            VerifyCommand::MergeChanges {
                osc_range: Some(_),
                ..
            }
        ));
        for argv in [
            &["--osc-seq", "4705", "--osc-range", "4705..4706"][..],
            &["--osc-seq", "4705", "--osc", "a.osc"],
            &["--osc-range", "4705..4706", "--osc", "a.osc"],
        ] {
            let mut full = vec!["brokkr", "verify", "merge-changes"];
            full.extend_from_slice(argv);
            assert!(parse(full).is_err(), "{argv:?} should conflict");
        }
        assert!(
            parse([
                "brokkr",
                "verify",
                "tags-filter-osc",
                "--osc-seq",
                "4705",
                "--osc",
                "a.osc"
            ])
            .is_err()
        );
    }

    #[test]
    fn cat_dedupe_conflicts_with_type_and_clean() {
        for extra in [["--clean", ""], ["--type", "way"]] {
            let mut argv = vec!["brokkr", "cat", "--dedupe"];
            argv.extend(extra.into_iter().filter(|s| !s.is_empty()));
            let rendered = argv.join(" ");
            assert!(parse(argv).is_err(), "{rendered} should conflict");
        }
    }

    #[test]
    fn cat_accepts_type_with_clean() {
        let parsed = parse(["brokkr", "cat", "--type", "way", "--clean"]).expect("parse");
        let Command::Cat {
            type_filter, clean, ..
        } = parsed.command
        else {
            panic!("expected Cat");
        };
        assert_eq!(type_filter.as_deref(), Some("way"));
        assert!(clean);
    }

    #[test]
    fn pmtiles_stats_requires_at_least_one_file() {
        let parsed = parse(["brokkr", "pmtiles-stats"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn inspect_tags_accepts_mode_flags() {
        let parsed = parse([
            "brokkr",
            "inspect",
            "--tags",
            "--hotpath",
            "--dataset",
            "japan",
        ])
        .expect("parse");

        let Command::Inspect {
            mode,
            pbf,
            tags,
            type_filter,
            ..
        } = parsed.command
        else {
            panic!("expected inspect command");
        };
        assert!(mode.hotpath.is_some());
        assert_eq!(pbf.dataset, "japan");
        assert!(tags);
        assert_eq!(type_filter, None);
    }

    #[test]
    fn inspect_expr_repeats_in_order_and_requires_tags() {
        let parsed = parse([
            "brokkr", "inspect", "--tags", "--expr", "*=*", "--expr", "highway",
        ])
        .expect("parse");
        let Command::Inspect { expr, .. } = parsed.command else {
            panic!("expected inspect command");
        };
        assert_eq!(expr, ["*=*", "highway"]);

        assert!(parse(["brokkr", "inspect", "--expr", "highway"]).is_err());
        assert!(parse(["brokkr", "inspect", "--nodes", "--expr", "highway"]).is_err());
        assert!(parse(["brokkr", "inspect", "--extended", "--expr", "highway"]).is_err());
        // The same hole was open for `--type`, dropped silently in routing.
        assert!(parse(["brokkr", "inspect", "--nodes", "--type", "way"]).is_err());
        assert!(parse(["brokkr", "inspect", "--extended", "--type", "way"]).is_err());
    }

    #[test]
    fn validate_since_checks_field_ranges() {
        for good in [
            "2026-03-01",
            "2024-02-29",
            "2026-12-31 23:59:59",
            "2026-01-01 00:00:00",
        ] {
            assert!(
                validate_since(good).is_ok(),
                "expected {good:?} to be accepted"
            );
        }
        for bad in [
            "2026-13-01",
            "2026-00-10",
            "2026-02-29",
            "2026-04-31",
            "2026-03-00",
            "2026-03-01 24:00:00",
            "2026-03-01 12:60:00",
            "2026-03-01 12:00:60",
            "2026/03/01",
        ] {
            assert!(
                validate_since(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_env_kv_accepts_key_value() {
        assert!(validate_env_kv("K=V").is_ok());
        assert!(validate_env_kv("K=").is_ok(), "empty value is legal");
        assert!(validate_env_kv("HIGH_PRECISION=1").is_ok());
    }

    #[test]
    fn validate_env_kv_rejects_empty_key_and_missing_eq() {
        assert!(validate_env_kv("=oops").is_err(), "empty key rejected");
        assert!(validate_env_kv("noeq").is_err(), "missing = rejected");
    }

    #[test]
    fn clippy_sweep_conflicts_with_ad_hoc_flags() {
        for extra in [
            vec!["-p", "x"],
            vec!["--all-features"],
            vec!["--features", "a"],
            vec!["--no-default-features"],
        ] {
            let mut argv = vec!["brokkr", "clippy", "--sweep", "s"];
            argv.extend(extra.iter().copied());
            assert!(
                parse(argv.clone()).is_err(),
                "--sweep must conflict with {extra:?}"
            );
        }
    }

    #[test]
    fn clippy_all_features_conflicts_with_feature_flags() {
        assert!(parse(["brokkr", "clippy", "--all-features", "--features", "z"]).is_err());
        assert!(
            parse([
                "brokkr",
                "clippy",
                "--all-features",
                "--no-default-features"
            ])
            .is_err()
        );
    }

    #[test]
    fn test_rejects_blank_name() {
        for blank in ["", " ", "\t"] {
            assert!(
                parse(["brokkr", "test", blank]).is_err(),
                "blank name {blank:?} must be refused"
            );
        }
        assert!(parse(["brokkr", "test", "some_test"]).is_ok());
    }

    #[test]
    fn clippy_rejects_empty_env_key() {
        assert!(parse(["brokkr", "clippy", "--env", "=oops"]).is_err());
    }

    #[test]
    fn clippy_accepts_repeatable_package_and_env() {
        let parsed = parse([
            "brokkr",
            "clippy",
            "-p",
            "a",
            "-p",
            "b",
            "--env",
            "K=V",
            "--all-features",
        ])
        .expect("parse");
        let Command::Clippy {
            package,
            all_features,
            env,
            ..
        } = parsed.command
        else {
            panic!("expected clippy command");
        };
        assert_eq!(package, vec!["a", "b"]);
        assert!(all_features);
        assert_eq!(env, vec!["K=V"]);
    }
}
