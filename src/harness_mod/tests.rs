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

    // -----------------------------------------------------------------------
    // run progress and sidecar summary lines
    // -----------------------------------------------------------------------

    #[test]
    fn run_progress_line_is_silent_for_a_single_run() {
        assert_eq!(run_progress_line(0, 1), None);
        assert_eq!(run_progress_line(0, 3).as_deref(), Some("run 1/3"));
        assert_eq!(run_progress_line(2, 3).as_deref(), Some("run 3/3"));
    }

    #[test]
    fn sidecar_summary_line_names_only_a_dirty_id() {
        let line = sidecar_summary_line("a1b2c3d4", false, &[], 0);
        assert_eq!(line, "(sidecar.db)");
        let dirty = sidecar_summary_line("a1b2c3d4", true, &[], 0);
        assert_eq!(dirty, "(sidecar.db a1b2c3d4, filed as dirty)");
    }

    // -----------------------------------------------------------------------
    // run_variants
    // -----------------------------------------------------------------------

    #[test]
    fn run_variants_refuses_an_empty_selection() {
        // `brokkr api --query typo --bench` filtered the query list to nothing
        // and used to record no rows and exit 0.
        let mut calls = 0;
        let result = run_variants("query", &[], |_| {
            calls += 1;
            Ok(())
        });
        assert!(matches!(result, Err(DevError::Refused(_))));
        assert_eq!(calls, 0);
    }

    #[test]
    fn run_variants_summary_names_the_variants_and_renders_bare() {
        let result = run_variants("query", &["a", "b", "c"], |v| {
            if v == "c" { Ok(()) } else { Err(DevError::Refused(format!("{v} broke"))) }
        });
        let e = result.unwrap_err();
        assert!(matches!(e, DevError::Reported(_)), "{e:?}");
        assert_eq!(e.to_string(), "2 of 3 variants failed: a, b");
    }

    #[test]
    fn run_variants_stops_at_a_shutdown() {
        let mut calls = 0;
        let result = run_variants("query", &["a", "b"], |_| {
            calls += 1;
            Err(DevError::Interrupted)
        });
        assert!(matches!(result, Err(DevError::Interrupted)));
        assert_eq!(calls, 1);
    }

    // -----------------------------------------------------------------------
    // fractional elapsed_ms
    // -----------------------------------------------------------------------

    #[test]
    fn fractional_elapsed_ms_is_kept_as_microseconds() {
        // The motivating bug: `elapsed_ms=6.847` failed `parse::<i64>()`, so
        // the timing was treated as absent and the run died on the
        // "missing elapsed_ms" check - a target reporting more precision
        // than brokkr asked for was treated as reporting none.
        let (us, _kv) = parse_kv_lines_us(b"elapsed_ms=6.847\n");
        assert_eq!(us, Some(6847));
    }

    #[test]
    fn whole_elapsed_ms_stays_exact() {
        // Integer values must not make a round trip through f64.
        let (us, _kv) = parse_kv_lines_us(b"elapsed_ms=300\n");
        assert_eq!(us, Some(300_000));
    }

    #[test]
    fn total_ms_alias_also_takes_a_fraction() {
        let (us, _kv) = parse_kv_lines_us(b"total_ms=1.5\n");
        assert_eq!(us, Some(1500));
    }

    #[test]
    fn elapsed_ms_rounds_to_nearest_not_down() {
        // 6.847 ms is 7 ms, not 6. The rounded value is what every existing
        // query and historical row sees, so it must not read as faster than
        // the run was.
        let (ms, _kv) = parse_kv_lines(b"elapsed_ms=6.847\n");
        assert_eq!(ms, Some(7));
    }

    #[test]
    fn fractional_timing_is_not_also_a_kv_pair() {
        // The timing line is consumed as timing, never duplicated into kv.
        let (_us, kv) = parse_kv_lines_us(b"elapsed_ms=6.847\ncold_prepare_us=214\n");
        assert!(!kv.iter().any(|p| p.key == "elapsed_ms"));
        assert!(kv.iter().any(|p| p.key == "cold_prepare_us"));
    }

    #[test]
    fn nonsense_timing_is_still_absent() {
        // A garbage value must not silently become 0 ms - the run should
        // still fail the missing-elapsed_ms check.
        let (us, _kv) = parse_kv_lines_us(b"elapsed_ms=NaN\n");
        assert_eq!(us, None);
        let (us, _kv) = parse_kv_lines_us(b"elapsed_ms=fast\n");
        assert_eq!(us, None);
    }

    #[test]
    fn best_of_n_compares_on_microseconds() {
        // Both round to 7 ms; only the microsecond reading can tell them
        // apart, which is the entire point on a sub-10ms workload.
        let slower = BenchResult {
            elapsed_ms: 7,
            elapsed_us: Some(6947),
            kv: Vec::new(),
            iterations: Vec::new(),
            distribution: None,
            hotpath: None,
        };
        let faster = BenchResult {
            elapsed_ms: 7,
            elapsed_us: Some(6847),
            kv: Vec::new(),
            iterations: Vec::new(),
            distribution: None,
            hotpath: None,
        };
        let best = pick_best(Some(slower), faster);
        assert_eq!(best.elapsed_us, Some(6847));
    }

    // -----------------------------------------------------------------------
    // format_result_line
    // -----------------------------------------------------------------------

    #[test]
    fn result_line_prints_the_resolved_mode() {
        // Every writer leaves `BenchConfig.mode` at `None`; the line must
        // print the harness-resolved mode it is handed, not that field.
        let config = BenchConfig {
            command: "cat".into(),
            mode: None,
            input_file: None,
            input_mb: None,
            cargo_features: None,
            cargo_profile: CargoProfile::Release,
            runs: 1,
            cli_args: None,
            brokkr_args: None,
            metadata: Vec::new(),
        };
        let result = BenchResult {
            elapsed_ms: 12,
            elapsed_us: None,
            kv: Vec::new(),
            iterations: Vec::new(),
            distribution: None,
            hotpath: None,
        };
        let git = GitInfo {
            commit: "abc".into(),
            subject: String::new(),
            is_clean: true,
        };
        let line = format_result_line(&config, Some("bench"), &result, &git);
        assert!(line.contains("mode=bench"), "{line}");
        let line = format_result_line(&config, None, &result, &git);
        assert!(!line.contains("mode="), "{line}");
    }

    // -----------------------------------------------------------------------
    // run_distribution's microsecond summary
    // -----------------------------------------------------------------------

    #[test]
    fn distribution_keeps_sub_millisecond_samples() {
        // The motivating bug: nidhogg API queries under half a millisecond
        // recorded 0 at every percentile, because samples were rounded to
        // whole milliseconds before the summary was taken.
        let (dist, min_us) = summarize_distribution(&[312, 450, 298, 305]).unwrap();
        assert_eq!(min_us, 298);
        assert_eq!(dist.samples, 4);
        assert_eq!(
            dist.us,
            Some(DistributionUs {
                min: 298,
                // Sorted [298, 305, 312, 450]; pos 1.5 -> 308.5, rounded.
                p50: 309,
                // pos 2.85 -> 312 + 0.85 * 138 = 429.3.
                p95: 429,
                max: 450,
            })
        );
        // The millisecond columns are the microseconds rounded - still 0 here,
        // which is exactly why they cannot be the only record.
        assert_eq!((dist.min_ms, dist.p50_ms, dist.p95_ms, dist.max_ms), (0, 0, 0, 0));
    }

    #[test]
    fn distribution_ms_fields_round_to_nearest() {
        let (dist, _) = summarize_distribution(&[1_499, 1_500, 2_600]).unwrap();
        assert_eq!(dist.min_ms, 1);
        assert_eq!(dist.p50_ms, 2);
        assert_eq!(dist.max_ms, 3);
    }

    #[test]
    fn distribution_of_nothing_is_refused() {
        assert!(summarize_distribution(&[]).is_none());
    }

    #[test]
    fn result_line_prints_fractional_distribution() {
        let config = BenchConfig {
            command: "api-bbox-small".into(),
            mode: None,
            input_file: None,
            input_mb: None,
            cargo_features: None,
            cargo_profile: CargoProfile::Release,
            runs: 3,
            cli_args: None,
            brokkr_args: None,
            metadata: Vec::new(),
        };
        let (dist, min_us) = summarize_distribution(&[312, 298, 450]).unwrap();
        let result = BenchResult {
            elapsed_ms: dist.min_ms,
            elapsed_us: Some(min_us),
            kv: Vec::new(),
            iterations: vec![0, 0, 0],
            distribution: Some(dist),
            hotpath: None,
        };
        let git = GitInfo {
            commit: "abc".into(),
            subject: String::new(),
            is_clean: true,
        };
        let line = format_result_line(&config, Some("bench"), &result, &git);
        assert!(line.contains("elapsed_ms=0.298"), "{line}");
        assert!(line.contains("min_ms=0.298"), "{line}");
        assert!(line.contains("max_ms=0.450"), "{line}");
    }

    #[test]
    fn iteration_microseconds_are_all_or_none() {
        assert_eq!(all_or_none(vec![Some(1), Some(2)]), vec![1, 2]);
        assert!(all_or_none(vec![Some(1), None]).is_empty());
        assert!(all_or_none(Vec::new()).is_empty());
    }

    #[test]
    fn us_to_ms_saturates_instead_of_overflowing() {
        // `elapsed_to_us` saturates at i64::MAX; rounding that must not panic.
        assert_eq!(us_to_ms(i64::MAX), i64::MAX / 1000);
    }

    // -----------------------------------------------------------------------
    // percentile
    // -----------------------------------------------------------------------

    #[test]
    fn percentile_empty_returns_zero() {
        assert_eq!(percentile(&[], 50), 0, "empty slice should return 0");
    }

    #[test]
    fn percentile_single_element_ignores_pct() {
        assert_eq!(percentile(&[42], 0), 42);
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 100), 42);
    }

    #[test]
    fn percentile_two_elements_interpolates() {
        // [100, 200]: p0=100, p50=150, p100=200
        let data = vec![100, 200];
        assert_eq!(percentile(&data, 0), 100);
        assert_eq!(
            percentile(&data, 50),
            150,
            "midpoint should interpolate to 150"
        );
        assert_eq!(percentile(&data, 100), 200);
        // p25 = 100 + 0.25*(200-100) = 125
        assert_eq!(percentile(&data, 25), 125);
        // p75 = 100 + 0.75*(200-100) = 175
        assert_eq!(percentile(&data, 75), 175);
    }

    #[test]
    fn percentile_five_elements_at_boundaries() {
        let data = vec![10, 20, 30, 40, 50];
        assert_eq!(percentile(&data, 0), 10);
        assert_eq!(
            percentile(&data, 25),
            20,
            "p25 of [10,20,30,40,50] should be 20"
        );
        assert_eq!(percentile(&data, 50), 30, "p50 should be median");
        assert_eq!(percentile(&data, 75), 40);
        assert_eq!(percentile(&data, 100), 50);
    }

    #[test]
    fn percentile_interpolation_beats_nearest_rank() {
        // With 3 samples [0, 100, 1000], nearest-rank p95 would pick index 2 = 1000.
        // Linear interpolation: pos = 0.95 * 2 = 1.9, lo=1(100), hi=2(1000)
        // result = 100 + 0.9 * 900 = 910
        let data = vec![0, 100, 1000];
        let p95 = percentile(&data, 95);
        assert_eq!(
            p95, 910,
            "linear interpolation should yield 910, not nearest-rank 1000"
        );
        assert!(
            p95 < 1000,
            "interpolated p95 must be less than max for non-degenerate data"
        );
    }

    #[test]
    fn percentile_identical_values() {
        let data = vec![7, 7, 7, 7];
        assert_eq!(percentile(&data, 0), 7);
        assert_eq!(percentile(&data, 50), 7);
        assert_eq!(percentile(&data, 100), 7);
    }

    // -----------------------------------------------------------------------
    // parse_kv_stderr
    // -----------------------------------------------------------------------

    #[test]
    fn parse_kv_stderr_basic_elapsed_ms() {
        let stderr = b"elapsed_ms=1234\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 1234);
        assert!(
            result.kv.is_empty(),
            "no extra fields => kv should be empty"
        );
    }

    #[test]
    fn parse_kv_stderr_total_ms_alias() {
        let stderr = b"total_ms=999\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(
            result.elapsed_ms, 999,
            "total_ms should be accepted as elapsed_ms alias"
        );
    }

    #[test]
    fn parse_kv_stderr_elapsed_ms_takes_precedence_over_total_ms() {
        // Both present: last one wins (due to overwrite semantics)
        let stderr = b"total_ms=100\nelapsed_ms=200\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(
            result.elapsed_ms, 200,
            "elapsed_ms should overwrite earlier total_ms"
        );

        // Reverse order: total_ms overwrites elapsed_ms
        let stderr2 = b"elapsed_ms=200\ntotal_ms=100\n";
        let result2 = parse_kv_stderr(stderr2).unwrap();
        assert_eq!(result2.elapsed_ms, 100, "last key=value wins");
    }

    #[test]
    fn parse_kv_stderr_extra_int_float_string_fields() {
        let stderr = b"elapsed_ms=500\nrows=42\nrate=3.14\nlabel=fast\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 500);

        assert_eq!(result.kv.len(), 3);
        // Check that we have the expected keys and values
        let find = |k: &str| result.kv.iter().find(|p| p.key == k).unwrap();
        assert!(matches!(find("rows").value, KvValue::Int(42)));
        assert!(matches!(find("rate").value, KvValue::Real(r) if (r - 3.14).abs() < 0.001));
        assert!(matches!(&find("label").value, KvValue::Text(s) if s == "fast"));
    }

    #[test]
    fn parse_kv_stderr_missing_elapsed_ms_error() {
        let stderr = b"rows=100\nlabel=test\n";
        match parse_kv_stderr(stderr) {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("missing elapsed_ms"),
                    "error should mention missing elapsed_ms, got: {msg}"
                );
            }
            Ok(_) => panic!("expected error for missing elapsed_ms, got Ok"),
        }
    }

    #[test]
    fn parse_kv_stderr_mixed_garbage_lines() {
        let stderr = b"some random log output\nwarning: something\nelapsed_ms=777\nmore junk\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(
            result.elapsed_ms, 777,
            "should find elapsed_ms among garbage lines"
        );
    }

    #[test]
    fn parse_kv_stderr_empty_value_treated_as_string() {
        // "tag=" has empty value - not parseable as i64 or f64, so becomes a string
        let stderr = b"elapsed_ms=100\ntag=\n";
        let result = parse_kv_stderr(stderr).unwrap();
        let find = |k: &str| result.kv.iter().find(|p| p.key == k).unwrap();
        assert!(
            matches!(&find("tag").value, KvValue::Text(s) if s.is_empty()),
            "empty value should become empty string"
        );
    }

    #[test]
    fn parse_kv_stderr_whitespace_trimming() {
        let stderr = b"  elapsed_ms  =  300  \n  count  =  5  \n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 300, "keys and values should be trimmed");
        let find = |k: &str| result.kv.iter().find(|p| p.key == k).unwrap();
        assert!(matches!(find("count").value, KvValue::Int(5)));
    }

    #[test]
    fn parse_kv_stderr_invalid_elapsed_ms_value() {
        let stderr = b"elapsed_ms=not_a_number\n";
        match parse_kv_stderr(stderr) {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("missing elapsed_ms"),
                    "should report missing elapsed_ms for unparseable value, got: {msg}"
                );
            }
            Ok(_) => panic!("expected error for invalid elapsed_ms value, got Ok"),
        }
    }

    #[test]
    fn parse_kv_stderr_nan_float_becomes_string() {
        // NaN is not representable in JSON numbers
        let stderr = b"elapsed_ms=100\nweird=NaN\n";
        let result = parse_kv_stderr(stderr).unwrap();
        let find = |k: &str| result.kv.iter().find(|p| p.key == k).unwrap();
        assert!(
            matches!(&find("weird").value, KvValue::Text(s) if s == "NaN"),
            "NaN should fall through to string"
        );
    }

    // -----------------------------------------------------------------------
    // format_cli_args / maybe_quote
    // -----------------------------------------------------------------------

    #[test]
    fn format_cli_args_no_args() {
        assert_eq!(format_cli_args("./bench", &[]), "./bench");
    }

    #[test]
    fn format_cli_args_simple_args() {
        assert_eq!(
            format_cli_args("./bench", &["--fast", "-n", "10"]),
            "./bench --fast -n 10"
        );
    }

    #[test]
    fn format_cli_args_args_with_spaces_get_quoted() {
        assert_eq!(
            format_cli_args("./my tool", &["--input", "path with spaces", "--verbose"]),
            "\"./my tool\" --input \"path with spaces\" --verbose"
        );
    }

    #[test]
    fn maybe_quote_no_spaces() {
        assert_eq!(maybe_quote("simple"), "simple");
    }

    #[test]
    fn maybe_quote_with_spaces() {
        assert_eq!(maybe_quote("has space"), "\"has space\"");
    }

    #[test]
    fn maybe_quote_empty_string() {
        assert_eq!(
            maybe_quote(""),
            "",
            "empty string has no spaces, should not be quoted"
        );
    }

    // -----------------------------------------------------------------------
    // pick_best / pick_best_ms
    // -----------------------------------------------------------------------

    #[test]
    fn pick_best_none_vs_candidate() {
        let candidate = BenchResult {
            elapsed_ms: 500,
            elapsed_us: None,
            kv: vec![],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let result = pick_best(None, candidate);
        assert_eq!(
            result.elapsed_ms, 500,
            "None current should always take candidate"
        );
    }

    #[test]
    fn pick_best_keeps_better() {
        let current = BenchResult {
            elapsed_ms: 100,
            elapsed_us: None,
            kv: vec![],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 200,
            elapsed_us: None,
            kv: vec![],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let result = pick_best(Some(current), candidate);
        assert_eq!(result.elapsed_ms, 100, "should keep the lower value");
    }

    #[test]
    fn pick_best_replaces_with_better() {
        let current = BenchResult {
            elapsed_ms: 300,
            elapsed_us: None,
            kv: vec![],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 150,
            elapsed_us: None,
            kv: vec![],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let result = pick_best(Some(current), candidate);
        assert_eq!(result.elapsed_ms, 150, "should replace with lower value");
    }

    #[test]
    fn pick_best_equal_keeps_current() {
        // Tie-breaking: current wins (<=)
        let current = BenchResult {
            elapsed_ms: 100,
            elapsed_us: None,
            kv: vec![KvPair::text("tag", "first")],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 100,
            elapsed_us: None,
            kv: vec![KvPair::text("tag", "second")],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let result = pick_best(Some(current), candidate);
        let tag = result.kv.iter().find(|p| p.key == "tag").unwrap();
        assert!(
            matches!(&tag.value, KvValue::Text(s) if s == "first"),
            "on tie, current (first seen) should be kept"
        );
    }

    // -----------------------------------------------------------------------
    // elapsed_to_ms
    // -----------------------------------------------------------------------

    #[test]
    fn elapsed_to_ms_normal() {
        let d = Duration::from_millis(1234);
        assert_eq!(elapsed_to_ms(&d), 1234);
    }

    #[test]
    fn elapsed_to_ms_zero() {
        let d = Duration::ZERO;
        assert_eq!(elapsed_to_ms(&d), 0);
    }

    #[test]
    fn elapsed_to_ms_overflow_saturates() {
        // Duration can hold values larger than i64::MAX milliseconds.
        // u64::MAX seconds = ~584 billion years worth of milliseconds, way beyond i64::MAX.
        let d = Duration::from_secs(u64::MAX);
        assert_eq!(
            elapsed_to_ms(&d),
            i64::MAX,
            "overflow should saturate to i64::MAX"
        );
    }

    #[test]
    fn elapsed_to_ms_rounds_to_nearest_not_down() {
        // Flooring 999us to 0ms read the run as faster than it was - the
        // one error `us_to_ms` documents as worth avoiding.
        assert_eq!(elapsed_to_ms(&Duration::from_micros(999)), 1);
        assert_eq!(elapsed_to_ms(&Duration::from_micros(1499)), 1);
        assert_eq!(elapsed_to_ms(&Duration::from_micros(1500)), 2);
        assert_eq!(elapsed_to_ms(&Duration::from_micros(499)), 0);
    }

    #[test]
    fn elapsed_to_us_is_exact_and_agrees_with_us_to_ms() {
        let d = Duration::from_nanos(6_847_900);
        assert_eq!(elapsed_to_us(&d), 6847);
        assert_eq!(us_to_ms(elapsed_to_us(&d)), elapsed_to_ms(&d));
        assert_eq!(elapsed_to_us(&Duration::from_secs(u64::MAX)), i64::MAX);
    }

    // -----------------------------------------------------------------------
    // hotpath_feature
    // -----------------------------------------------------------------------

    #[test]
    fn hotpath_feature_without_alloc() {
        assert_eq!(hotpath_feature(false), "hotpath");
    }

    #[test]
    fn hotpath_feature_with_alloc() {
        assert_eq!(hotpath_feature(true), "hotpath-alloc");
    }

    // -------------------------------------------------------------------
    // backup_sidecar rotation
    // -------------------------------------------------------------------

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brokkr-harness-test-{}-{}",
            std::process::id(),
            suffix,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Create a minimal sidecar DB at the given path.
    fn create_sidecar(path: &Path) {
        let db = crate::db::sidecar::SidecarDb::open(path).unwrap();
        db.conn().execute(
            "INSERT INTO sidecar_markers (result_uuid, run_idx, marker_idx, \
             timestamp_us, name) VALUES ('test', 0, 0, 1000, 'marker')",
            [],
        ).unwrap();
    }

    #[test]
    fn backup_sidecar_creates_and_rotates() {
        let dir = temp_dir("rotate");
        let sidecar_path = dir.join("sidecar.db");
        create_sidecar(&sidecar_path);

        let backup_dir = dir.join("backups");

        // Run backup 4 times to exercise rotation.
        for _ in 0..4 {
            backup_sidecar_to(
                &sidecar_path,
                crate::project::Project::Pbfhogg,
                Some(&backup_dir),
            )
            .unwrap();
        }

        let base = backup_dir.join("pbfhogg-sidecar.db");
        assert!(base.exists(), "newest backup should exist");
        assert!(
            base.with_extension("db.1").exists(),
            "second backup should exist"
        );
        assert!(
            base.with_extension("db.2").exists(),
            "third backup should exist"
        );
        // Only 3 copies kept.
        assert!(
            !base.with_extension("db.3").exists(),
            "fourth backup should not exist"
        );

        // Verify the backup is a valid SQLite DB.
        let conn = rusqlite::Connection::open_with_flags(
            &base,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sidecar_markers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_failure_does_not_displace_good_backup() {
        let dir = temp_dir("nodisplace");
        let sidecar_path = dir.join("sidecar.db");
        create_sidecar(&sidecar_path);

        let backup_dir = dir.join("backups");

        // Create a good initial backup.
        backup_sidecar_to(
            &sidecar_path,
            crate::project::Project::Pbfhogg,
            Some(&backup_dir),
        )
        .unwrap();

        let base = backup_dir.join("pbfhogg-sidecar.db");
        assert!(base.exists());
        let good_size = std::fs::metadata(&base).unwrap().len();

        // Attempt backup from a non-SQLite source - should fail.
        let bad_source = dir.join("not-a-database.db");
        std::fs::write(&bad_source, b"this is not sqlite").unwrap();

        let result = backup_sidecar_to(
            &bad_source,
            crate::project::Project::Pbfhogg,
            Some(&backup_dir),
        );
        assert!(result.is_err());

        // The good backup should still be intact.
        assert!(base.exists(), "good backup should still exist");
        let after_size = std::fs::metadata(&base).unwrap().len();
        assert_eq!(good_size, after_size, "good backup should be unchanged");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_sidecar_nonexistent_source_is_noop() {
        let dir = temp_dir("noop");
        let sidecar_path = dir.join("does-not-exist.db");

        let result = backup_sidecar_to(
            &sidecar_path,
            crate::project::Project::Pbfhogg,
            Some(&dir),
        );
        assert!(result.is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }
}
