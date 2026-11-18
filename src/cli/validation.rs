//! Parse-time validation of command-line values.

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
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn parse(argv: impl IntoIterator<Item = &'static str>) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(argv)
    }

    /// `brokkr bench --dry-run` reads only, so it takes no lock and writes no
    /// run log, and `--dry-run` after NAME is no option of brokkr's; so do
    /// listing, `NAME --help` and a refused NAME or param. A run keeps the
    /// command's lock.
    #[test]
    fn a_bench_dry_run_takes_no_lock() {
        use crate::cli::{CommandId, CommandSpec, LockPolicy};
        let dir = crate::test_scratch::scratch("cli_validation", "bench_reads_only");
        std::fs::write(
            dir.join("brokkr.toml"),
            "project = \"p\"\n[bench.cat]\nscript = \"function bench() end\"\n\
             params.n = { free = true }\n",
        )
        .unwrap();
        let (_, config, _, _) = crate::config::load_with_sources(&dir).unwrap();
        let benches = Some(&config.benches);
        let bench = CommandId::Bench.spec();
        let reads = |argv: Vec<&'static str>| {
            CommandSpec::reads_only(&parse(argv).unwrap().command, benches)
        };
        let dry = parse(["brokkr", "bench", "--dry-run", "cat", "--row", "x"]).unwrap();
        assert!(matches!(
            dry.command,
            Command::Bench(crate::cli::BenchCmd { dry_run: true, .. })
        ));
        assert!(CommandSpec::reads_only(&dry.command, benches));
        assert!(bench.lock_for(true) == LockPolicy::None);
        assert_eq!(bench.run_log_for(true), None);
        assert!(bench.lock_for(false) == LockPolicy::Blocking);
        assert!(bench.run_log_for(false).is_some());
        for argv in [
            vec!["brokkr", "bench"],
            vec!["brokkr", "bench", "cat", "--help"],
            vec!["brokkr", "bench", "dog"],
            vec!["brokkr", "bench", "cat", "--row", "x"],
            vec!["brokkr", "bench", "--test"],
        ] {
            assert!(reads(argv.clone()), "{argv:?} only prints");
        }
        assert!(
            parse(["brokkr", "bench", "--test", "--dry-run"]).is_err(),
            "--test runs the tests alone"
        );
        for argv in [
            vec!["brokkr", "bench", "cat"],
            vec!["brokkr", "bench", "cat", "--n", "x"],
            vec!["brokkr", "bench", "cat", "--n", "--help"],
        ] {
            let run = parse(argv.clone()).unwrap();
            assert!(matches!(
                run.command,
                Command::Bench(crate::cli::BenchCmd { dry_run: false, .. })
            ));
            assert!(
                !CommandSpec::reads_only(&run.command, benches),
                "{argv:?} runs"
            );
        }
    }

    /// `brokkr bench`: brokkr's options before NAME, and nothing after NAME
    /// read by clap, `--help` and brokkr's own option names included.
    #[test]
    fn bench_options_stop_at_the_bench_name() {
        let bench = |cli: Cli| match cli.command {
            Command::Bench(crate::cli::BenchCmd {
                commit,
                iterations,
                bench,
                ..
            }) => (commit, iterations.map(std::num::NonZeroU32::get), bench),
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
        assert_eq!((commit.as_deref(), iterations), (Some("abc"), Some(5)));
        assert_eq!(rest, ["cat", "--help", "--commit", "x"]);
        let (_, iterations, rest) = bench(parse(["brokkr", "bench"]).unwrap());
        assert_eq!((iterations, rest.len()), (None, 0));
        assert!(parse(["brokkr", "bench", "--iterations", "0", "cat"]).is_err());
    }

    /// The positional tail the arm key keeps verbatim, from a real parse.
    fn tail(argv: &'static [&'static str]) -> usize {
        let mut cmd = <Cli as clap::CommandFactory>::command();
        let matches = cmd.try_get_matches_from_mut(argv).expect("parse");
        let argv: Vec<String> = argv.iter().map(|&a| a.to_owned()).collect();
        crate::run_invocation::positional_tail(&cmd, &matches, &argv).expect("tail")
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
        assert_eq!(tail(&["brokkr", "fmt", "a", "--b"]), 2);
        assert_eq!(tail(&["brokkr", "env"]), 0);
    }

    #[test]
    fn results_compare_requires_two_commits() {
        let parsed = parse(["brokkr", "results", "--compare", "abc123"]);
        assert!(parsed.is_err());
    }

    /// A mark is one write per command: retire needs a reason, a reason needs
    /// retire, and no query option rides along, though the defaulted ones
    /// left unset do not count.
    #[test]
    fn results_marks_stand_alone() {
        assert!(parse(["brokkr", "results", "--retire", "x", "--reason", "r"]).is_ok());
        assert!(parse(["brokkr", "results", "--unretire", "x"]).is_ok());
        assert!(parse(["brokkr", "results", "--hidden"]).is_ok());
        assert!(parse(["brokkr", "results", "--invalidate", "ab", "--reason", "r"]).is_ok());
        assert!(parse(["brokkr", "results", "--revalidate", "ab"]).is_ok());
        for bad in [
            &["brokkr", "results", "--invalidate", "ab"][..],
            &["brokkr", "results", "--revalidate", "ab", "--reason", "r"],
            &[
                "brokkr",
                "results",
                "--invalidate",
                "ab",
                "--reason",
                "r",
                "--retire",
                "x",
            ],
            &[
                "brokkr",
                "results",
                "--invalidate",
                "ab",
                "--reason",
                "r",
                "--hidden",
            ],
            &["brokkr", "results", "--revalidate", "ab", "--unretire", "x"],
            &["brokkr", "results", "abcd", "--revalidate", "ab"],
        ] {
            assert!(parse(bad.iter().copied()).is_err(), "{bad:?}");
        }
        for bad in [
            &["brokkr", "results", "--retire", "x"][..],
            &["brokkr", "results", "--reason", "r"],
            &["brokkr", "results", "--unretire", "x", "--reason", "r"],
            &[
                "brokkr",
                "results",
                "--retire",
                "x",
                "--reason",
                "r",
                "--unretire",
                "y",
            ],
            &["brokkr", "results", "--unretire", "x", "--hidden"],
            &["brokkr", "results", "--unretire", "x", "--limit", "20"],
            &["brokkr", "results", "--unretire", "x", "--command", "c"],
            &["brokkr", "results", "abcd", "--unretire", "x"],
        ] {
            assert!(parse(bad.iter().copied()).is_err(), "{bad:?}");
        }
        assert!(parse(["brokkr", "results", "--rename", "a", "b"]).is_ok());
        for bad in [
            &["brokkr", "results", "--rename", "a"][..],
            &["brokkr", "results", "--rename", "a", "b", "--reason", "r"],
            &["brokkr", "results", "--rename", "a", "b", "--unretire", "x"],
            &["brokkr", "results", "--rename", "a", "b", "--hidden"],
            &["brokkr", "results", "--rename", "a", "b", "--command", "c"],
        ] {
            assert!(parse(bad.iter().copied()).is_err(), "{bad:?}");
        }
    }

    /// `test` needs a name; its selection flags parse beside it.
    #[test]
    fn test_requires_a_name() {
        assert!(parse(["brokkr", "test"]).is_err());
        let plain = parse(["brokkr", "test", "some_test", "-p", "pkg", "-N", "3"]).expect("parse");
        let Command::Test(crate::cli::TestCmd { name, repeat, .. }) = plain.command else {
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
        let Command::Check(crate::cli::CheckCmd { profile, .. }) = parsed.command else {
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
        let Command::Clippy(crate::cli::ClippyCmd {
            package,
            all_features,
            env,
            ..
        }) = parsed.command
        else {
            panic!("expected clippy command");
        };
        assert_eq!(package, vec!["a", "b"]);
        assert!(all_features);
        assert_eq!(env, vec!["K=V"]);
    }
}
