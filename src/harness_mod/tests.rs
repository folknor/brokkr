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
            kv: vec![],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 200,
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
            kv: vec![],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 150,
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
            kv: vec![KvPair::text("tag", "first")],
            iterations: vec![],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 100,
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
    fn elapsed_to_ms_sub_millisecond_truncates() {
        let d = Duration::from_micros(999);
        assert_eq!(elapsed_to_ms(&d), 0, "sub-millisecond should truncate to 0");
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
