/// Validate `--as-snapshot` key: matches `[a-zA-Z0-9_-]+` and is not the
/// reserved sentinel `base`. Delegates to `config::validate_snapshot_key`
/// so the parse-time and CLI-time rules stay in sync.
fn validate_snapshot_key_arg(s: &str) -> Result<String, String> {
    crate::config::validate_snapshot_key(s)?;
    Ok(s.to_owned())
}

/// Validate `--meta key=value`: must contain exactly one `=`. Both sides may
/// be empty (the empty-value case is legitimate for filtering rows where the
/// stored value is the empty string).
fn validate_meta_filter(s: &str) -> Result<String, String> {
    if !s.contains('=') {
        return Err(format!(
            "expected key=value, got '{s}' (use --meta KEY=VALUE)"
        ));
    }
    Ok(s.to_owned())
}

fn validate_compression(s: &str) -> Result<String, String> {
    if s == "none" {
        return Ok(s.to_owned());
    }
    if let Some(level) = s.strip_prefix("zlib:") {
        let n: u8 = level
            .parse()
            .map_err(|_| format!("invalid zlib level '{level}', expected 1-9"))?;
        if (1..=9).contains(&n) {
            return Ok(s.to_owned());
        }
        return Err(format!("zlib level {n} out of range, expected 1-9"));
    }
    if let Some(level) = s.strip_prefix("zstd:") {
        level
            .parse::<u32>()
            .map_err(|_| format!("invalid zstd level '{level}', expected a positive integer"))?;
        return Ok(s.to_owned());
    }
    Err(format!(
        "invalid compression '{s}', expected 'none', 'zlib:N' (N=1-9), or 'zstd:N'"
    ))
}

/// Validate `--osc-range` format: `LO..HI` where both are non-negative integers and LO <= HI.
fn validate_osc_range(s: &str) -> Result<String, String> {
    let (lo_s, hi_s) = s
        .split_once("..")
        .ok_or_else(|| format!("expected LO..HI, got '{s}'"))?;
    let lo: u64 = lo_s
        .parse()
        .map_err(|e| format!("invalid LO '{lo_s}': {e}"))?;
    let hi: u64 = hi_s
        .parse()
        .map_err(|e| format!("invalid HI '{hi_s}': {e}"))?;
    if lo > hi {
        return Err(format!("LO ({lo}) must be <= HI ({hi})"));
    }
    Ok(s.to_owned())
}

/// Validate `--since` format: YYYY-MM-DD or YYYY-MM-DD HH:MM:SS.
fn validate_since(s: &str) -> Result<String, String> {
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
// `impl Command { fn as_pbfhogg(...) }` lives next to the pbfhogg command
// definitions in `src/pbfhogg/cli_adapter.rs` - it's the bridge between
// the CLI shape and the typed `PbfhoggCommand`, and grouping it with the
// target type keeps both surfaces easy to change together.

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

    #[test]
    fn results_compare_requires_two_commits() {
        let parsed = Cli::try_parse_from(["brokkr", "results", "--compare", "abc123"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn check_accepts_profile_flag() {
        let parsed = Cli::try_parse_from(["brokkr", "check", "--profile", "tier1"])
            .expect("parse");
        let Command::Check { profile, .. } = parsed.command else {
            panic!("expected Check");
        };
        assert_eq!(profile.as_deref(), Some("tier1"));
    }

    #[test]
    fn check_profile_conflicts_with_features() {
        let parsed = Cli::try_parse_from([
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
        let parsed = Cli::try_parse_from([
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
        let parsed = Cli::try_parse_from([
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
        assert_eq!(
            pbf.input.as_deref().map(std::path::Path::display).map(|d| d.to_string()),
            Some("fixtures/overlapping.osm.pbf".to_owned())
        );
    }

    #[test]
    fn verify_sort_input_conflicts_with_dataset() {
        let parsed = Cli::try_parse_from([
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
        let parsed = Cli::try_parse_from([
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
        let parsed = Cli::try_parse_from([
            "brokkr",
            "verify",
            "renumber",
            "--input",
            "fixtures/x.pbf",
        ])
        .expect("parse");
        let Command::Verify { verify, .. } = parsed.command else {
            panic!("expected Verify");
        };
        let VerifyCommand::Renumber { input, .. } = verify else {
            panic!("expected Verify::Renumber");
        };
        assert!(input.is_some());
    }

    #[test]
    fn pmtiles_stats_requires_at_least_one_file() {
        let parsed = Cli::try_parse_from(["brokkr", "pmtiles-stats"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn inspect_tags_accepts_mode_flags() {
        let parsed = Cli::try_parse_from([
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

}
