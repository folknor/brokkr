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

    fn make_config(hosts: HashMap<String, HostConfig>) -> DevConfig {
        DevConfig {
            hosts,
            litehtml: None,
            sluggrs: None,
            ratatoskr: None,
            piners: None,
            dellingr: None,
            mogwai: None,
            dependency_rules: Vec::new(),
            check: Vec::new(),
            test: None,
            quarantine: Vec::new(),
            capture_env: Vec::new(),
            gremlins: None,
            header: None,
            textlint: Vec::new(),
            script_checks: Vec::new(),
            manifest: None,
            deps: None,
            lints: None,
            bin: None,
            disable_toolchain: false,
        }
    }

    fn textlint_of(src: &str) -> Result<Vec<TextlintRule>, DevError> {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(src).unwrap();
        parse_textlint(&table)
    }

    fn script_checks_of(src: &str) -> Result<Vec<ScriptCheck>, DevError> {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(src).unwrap();
        parse_script_checks(&table)
    }

    #[test]
    fn clippy_allow_parses() {
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[clippy]\nallow = [\"clippy::unused_async\", \"dead_code\"]").unwrap();
        let cfg = parse_lints(&table).unwrap().unwrap();
        assert_eq!(cfg.allow, vec!["clippy::unused_async", "dead_code"]);
    }

    #[test]
    fn clippy_allow_rejects_non_lint_entries() {
        // The list feeds `-A <lint>` argv slots: a leading dash or embedded
        // whitespace is flag smuggling, not a lint name.
        for bad in ["[clippy]\nallow = [\"-W clippy::pedantic\"]", "[clippy]\nallow = [\" \"]"] {
            let table: toml::map::Map<String, toml::Value> = toml::from_str(bad).unwrap();
            assert!(parse_lints(&table).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn clippy_allow_exact_parses() {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[clippy]\nallow_exact = [\"clippy::unused_async@src/lib.rs\", \"dead_code@src/a.rs\"]",
        )
        .unwrap();
        let cfg = parse_lints(&table).unwrap().unwrap();
        assert_eq!(cfg.allow_exact.len(), 2);
        assert_eq!(cfg.allow_exact[0].lint, "clippy::unused_async");
        assert_eq!(cfg.allow_exact[0].path, "src/lib.rs");
        assert_eq!(cfg.allow_exact[1].to_string(), "dead_code@src/a.rs");
    }

    #[test]
    fn clippy_allow_exact_rejects_malformed_entries() {
        // No '@' (a blanket allow belongs in `allow`), empty halves, flag
        // smuggling, whitespace, and a second '@' are all parse errors.
        for bad in [
            "clippy::unused_async",
            "@src/lib.rs",
            "clippy::unused_async@",
            "-W clippy::pedantic@src/lib.rs",
            "dead_code@src/a b.rs",
            "dead_code@src@lib.rs",
        ] {
            let src = format!("[clippy]\nallow_exact = [\"{bad}\"]");
            let table: toml::map::Map<String, toml::Value> = toml::from_str(&src).unwrap();
            assert!(parse_lints(&table).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn lints_section_parses_under_its_own_name() {
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[lints]\nallow = [\"deprecated\"]").unwrap();
        let cfg = parse_lints(&table).unwrap().unwrap();
        assert_eq!(cfg.allow, vec!["deprecated"]);
    }

    #[test]
    fn lints_and_clippy_alias_union_rather_than_shadow() {
        // A project mid-rename must not have one section silently swallow the
        // other - that would drop a live suppression on a schema change.
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[lints]\nallow = [\"deprecated\"]\n\
             [clippy]\nallow = [\"dead_code\"]\nallow_exact = [\"unused@src/a.rs\"]",
        )
        .unwrap();
        let cfg = parse_lints(&table).unwrap().unwrap();
        assert_eq!(cfg.allow, vec!["deprecated", "dead_code"]);
        assert_eq!(cfg.allow_exact.len(), 1);
    }

    #[test]
    fn lints_validation_names_the_section_it_read() {
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[lints]\nallow = [\"-Wpedantic\"]").unwrap();
        let err = parse_lints(&table).unwrap_err().to_string();
        assert!(err.contains("[lints]"), "got: {err}");
    }

    #[test]
    fn test_phase_flags_fold_allow_exact_in_and_dedupe() {
        // The test phase cannot scope by file, so an allow_exact contributes
        // its lint name; a name present in both lists appears once.
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[lints]\nallow = [\"deprecated\"]\n\
             allow_exact = [\"deprecated@src/a.rs\", \"dead_code@src/b.rs\"]",
        )
        .unwrap();
        let cfg = parse_lints(&table).unwrap().unwrap();
        assert_eq!(
            test_phase_allow_flags(&cfg.allow, &cfg.allow_exact),
            vec!["-A", "deprecated", "-A", "dead_code"]
        );
    }

    #[test]
    fn test_phase_flags_empty_without_config() {
        assert!(test_phase_allow_flags(&[], &[]).is_empty());
    }

    #[test]
    fn script_check_stage_defaults_to_pre_clippy() {
        // An entry written before `stage` existed keeps running where it always
        // did - the key's absence must not move a gate.
        let checks = script_checks_of(
            r#"
[[script_check]]
name = "docs"
command = "bash check_docs.sh"
expect = "ok"
"#,
        )
        .unwrap();
        assert_eq!(checks[0].stage, Stage::PreClippy);
    }

    #[test]
    fn script_check_stage_parses_each_slot() {
        let checks = script_checks_of(
            r#"
[[script_check]]
name = "early"
command = "a"
expect = "ok"
stage = "pre-clippy"

[[script_check]]
name = "middle"
command = "b"
expect = "ok"
stage = "pre-test"

[[script_check]]
name = "late"
command = "c"
expect = "ok"
stage = "post-test"
"#,
        )
        .unwrap();
        let stages: Vec<Stage> = checks.iter().map(|c| c.stage).collect();
        assert_eq!(stages, vec![Stage::PreClippy, Stage::PreTest, Stage::PostTest]);
    }

    #[test]
    fn script_check_rejects_unknown_stage() {
        // deny_unknown_fields covers typo'd keys; this covers a typo'd *value*,
        // which would otherwise silently fall back to the default slot.
        let err = script_checks_of(
            r#"
[[script_check]]
name = "late"
command = "c"
expect = "ok"
stage = "after-tests"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, DevError::Config(_)), "unexpected: {err:?}");
    }

    #[test]
    fn textlint_preset_supplies_scope_and_rule_overrides_scalars() {
        let rules = textlint_of(
            r#"
[textlint_preset.dst-scope]
paths = ["crates/*/src/**/*.rs"]
exclude = ["crates/adapters/**"]
region = "code"
allow_marker = "dst-ok"
join_wrapped_use = true

[[textlint]]
name = "inherits"
pattern = "Instant::now"
message = "m"
preset = "dst-scope"

[[textlint]]
name = "overrides"
pattern = "Utc::now"
message = "m"
preset = "dst-scope"
allow_marker = "clock-ok"
join_wrapped_use = false
"#,
        )
        .unwrap();

        assert_eq!(rules[0].paths, vec!["crates/*/src/**/*.rs".to_owned()]);
        assert_eq!(rules[0].region.as_deref(), Some("code"));
        assert_eq!(rules[0].allow_marker.as_deref(), Some("dst-ok"));
        assert!(rules[0].join_wrapped_use);

        // Nearest value wins - including one explicitly set back to the field's
        // own default, which a post-deserialization merge could not distinguish
        // from "unset".
        assert_eq!(rules[1].allow_marker.as_deref(), Some("clock-ok"));
        assert!(!rules[1].join_wrapped_use);
    }

    #[test]
    fn textlint_preset_lists_concatenate_preset_first() {
        let rules = textlint_of(
            r#"
[textlint_preset.scope]
exclude = ["crates/adapters/**", "**/tests/**"]
except = ["^\\s*use\\s"]

[[textlint]]
name = "adds"
pattern = "p"
message = "m"
paths = ["crates/**/*.rs"]
preset = "scope"
exclude = ["crates/network/src/net.rs"]
"#,
        )
        .unwrap();

        assert_eq!(
            rules[0].exclude,
            vec![
                "crates/adapters/**".to_owned(),
                "**/tests/**".to_owned(),
                "crates/network/src/net.rs".to_owned(),
            ]
        );
        assert_eq!(rules[0].except, vec!["^\\s*use\\s".to_owned()]);
    }

    #[test]
    fn textlint_multiple_presets_scalars_first_lists_declaration_order() {
        let rules = textlint_of(
            r#"
[textlint_preset.a]
paths = ["a/**"]
region = "code"

[textlint_preset.b]
paths = ["b/**"]
region = "comment"
skip_after = "cfg\\(test"

[[textlint]]
name = "both"
pattern = "p"
message = "m"
preset = ["a", "b"]
"#,
        )
        .unwrap();

        // S3-25: the earlier-listed preset wins for scalars *and* its list
        // entries come first, so lists and scalars agree on `preset = ["a",
        // "b"]` meaning "a before b" (before the fix, lists came out reversed:
        // ["b/**", "a/**"]).
        assert_eq!(rules[0].region.as_deref(), Some("code"));
        assert_eq!(rules[0].skip_after.as_deref(), Some("cfg\\(test"));
        assert_eq!(
            rules[0].paths,
            vec!["a/**".to_owned(), "b/**".to_owned()]
        );
    }

    #[test]
    fn textlint_multiple_presets_rule_list_comes_last() {
        // A rule adding its own path keeps the full order: preset a, preset b,
        // then the rule's own entry - the rule *adds* to the shared lists.
        let rules = textlint_of(
            r#"
[textlint_preset.a]
paths = ["a/**"]

[textlint_preset.b]
paths = ["b/**"]

[[textlint]]
name = "both"
pattern = "p"
message = "m"
paths = ["own/**"]
preset = ["a", "b"]
"#,
        )
        .unwrap();
        assert_eq!(
            rules[0].paths,
            vec!["a/**".to_owned(), "b/**".to_owned(), "own/**".to_owned()]
        );
    }

    #[test]
    fn textlint_unreferenced_preset_rejected() {
        // S3-24: a preset no rule draws on is dead config - reject it, whether
        // or not any rules exist.
        assert!(textlint_of(
            "[textlint_preset.unused]\npaths = [\"**\"]\n\n[[textlint]]\n\
             name = \"r\"\npattern = \"p\"\nmessage = \"m\"\npaths = [\"src/**\"]\n"
        )
        .is_err());
        // A defined preset with no `[[textlint]]` at all is also unreferenced.
        assert!(textlint_of("[textlint_preset.lonely]\npaths = [\"**\"]\n").is_err());
        // A referenced preset stays fine.
        assert!(textlint_of(
            "[textlint_preset.used]\npaths = [\"**\"]\n\n[[textlint]]\n\
             name = \"r\"\npattern = \"p\"\nmessage = \"m\"\npreset = \"used\"\n"
        )
        .is_ok());
    }

    #[test]
    fn textlint_preset_errors() {
        // Unknown preset name.
        assert!(textlint_of(
            "[[textlint]]\nname = \"r\"\npattern = \"p\"\nmessage = \"m\"\n\
             paths = [\"**\"]\npreset = \"nope\"\n"
        )
        .is_err());

        // A preset may not carry a rule's identity.
        assert!(textlint_of(
            "[textlint_preset.p]\npattern = \"x\"\n\n[[textlint]]\nname = \"r\"\n\
             pattern = \"p\"\nmessage = \"m\"\npaths = [\"**\"]\npreset = \"p\"\n"
        )
        .is_err());

        // Typo in a preset field is caught even though the rule looks fine.
        assert!(textlint_of("[textlint_preset.p]\nexcludes = [\"x\"]\n").is_err());

        // `preset` must be a string or list of strings.
        assert!(textlint_of(
            "[textlint_preset.p]\npaths = [\"**\"]\n\n[[textlint]]\nname = \"r\"\n\
             pattern = \"p\"\nmessage = \"m\"\npreset = 3\n"
        )
        .is_err());

        // A rule drawing no `paths` from anywhere is still rejected.
        assert!(textlint_of(
            "[textlint_preset.p]\nregion = \"code\"\n\n[[textlint]]\nname = \"r\"\n\
             pattern = \"p\"\nmessage = \"m\"\npreset = \"p\"\n"
        )
        .is_err());

        // `preset` itself never reaches the rule struct's deny_unknown_fields.
        assert!(textlint_of(
            "[textlint_preset.p]\npaths = [\"**\"]\n\n[[textlint]]\nname = \"r\"\n\
             pattern = \"p\"\nmessage = \"m\"\npreset = \"p\"\n"
        )
        .is_ok());
    }

    #[test]
    fn gremlins_exclude_matches_dir_prefix() {
        let cfg = GremlinsConfig {
            exclude: vec!["docs/manual".to_owned(), "vendor/".to_owned()],
            ..Default::default()
        };
        // The directory itself and anything beneath it are excluded.
        assert!(cfg.is_excluded(Path::new("docs/manual")));
        assert!(cfg.is_excluded(Path::new("docs/manual/ch1.md")));
        assert!(cfg.is_excluded(Path::new("vendor/lib/foo.rs")));
        // A sibling sharing a textual prefix is not.
        assert!(!cfg.is_excluded(Path::new("docs/manual-extra/x.md")));
        assert!(!cfg.is_excluded(Path::new("src/main.rs")));
    }

    #[test]
    fn parse_gremlins_rejects_empty_and_absolute() {
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nexclude = [\"\"]\n").unwrap();
        assert!(parse_gremlins(&table).is_err());

        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nexclude = [\"/abs/path\"]\n").unwrap();
        assert!(parse_gremlins(&table).is_err());

        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nexclude = [\"docs/manual\"]\n").unwrap();
        let cfg = parse_gremlins(&table).unwrap().unwrap();
        assert_eq!(cfg.exclude, vec!["docs/manual".to_owned()]);
    }

    #[test]
    fn parse_gremlins_disable_allow_ban() {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[gremlins]\ndisable = true\nallow = [\"U+2019\"]\nban = [\"u+2011\"]\n",
        )
        .unwrap();
        let cfg = parse_gremlins(&table).unwrap().unwrap();
        assert!(cfg.disable);
        assert!(cfg.allow.contains('\u{2019}'));
        // Case-insensitive `u+` prefix accepted.
        assert!(cfg.ban.contains('\u{2011}'));
    }

    #[test]
    fn parse_gremlins_ban_range() {
        // The whole Cyrillic block, inclusive of both ends.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nban = [\"U+0400..U+04FF\"]\n").unwrap();
        let cfg = parse_gremlins(&table).unwrap().unwrap();
        assert!(cfg.ban.contains('\u{0400}'));
        assert!(cfg.ban.contains('\u{04FF}'));
        assert!(cfg.ban.contains('\u{0450}'));
        assert!(!cfg.ban.contains('\u{03FF}'));
        assert!(!cfg.ban.contains('\u{0500}'));
    }

    #[test]
    fn parse_gremlins_rejects_reversed_range_but_allows_range_singleton_overlap() {
        // Reversed bounds are still a hard error.
        let bad: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nban = [\"U+04FF..U+0400\"]\n").unwrap();
        assert!(parse_gremlins(&bad).is_err());
        // A singleton in `allow` that falls inside a `ban` range is now VALID:
        // `allow` wins over `ban` at scan time, so config load must accept it
        // (this is the canonical "ban a block, allow-list exceptions" config).
        let overlap: toml::map::Map<String, toml::Value> = toml::from_str(
            "[gremlins]\nallow = [\"U+0450\"]\nban = [\"U+0400..U+04FF\"]\n",
        )
        .unwrap();
        let cfg = parse_gremlins(&overlap).unwrap().unwrap();
        assert!(cfg.allow.contains('\u{0450}'));
        assert!(cfg.ban.contains('\u{0450}'));
    }

    #[test]
    fn parse_gremlins_rejects_bad_codepoint_but_allows_overlap() {
        // Not U+XXXX form.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nban = [\"2011\"]\n").unwrap();
        assert!(parse_gremlins(&table).is_err());

        // Non-hex.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nallow = [\"U+ZZZZ\"]\n").unwrap();
        assert!(parse_gremlins(&table).is_err());

        // A codepoint in both lists is accepted: `allow` wins at scan time,
        // so this is not a contradiction the parser should reject.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nallow = [\"U+2011\"]\nban = [\"U+2011\"]\n").unwrap();
        let cfg = parse_gremlins(&table).unwrap().unwrap();
        assert!(cfg.allow.contains('\u{2011}'));
        assert!(cfg.ban.contains('\u{2011}'));
    }

    #[test]
    fn parse_check_rejects_packages_and_test_exclude_together() {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[[check]]\nname = \"x\"\npackages = [\"a\"]\ntest_exclude_packages = [\"b\"]\n",
        )
        .unwrap();
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_unfiltered_curated_entry() {
        // Curated means "a hand-picked subset"; without its own filters the
        // entry is a full sweep that would silently drop out of the coverage
        // universe.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[[check]]\nname = \"sim-live\"\ncurated = true\n").unwrap();
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("curated"), "got: {err}");

        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[[check]]\nname = \"sim-live\"\ncurated = true\nonly = [\"targeted\"]\n",
        )
        .unwrap();
        assert!(parse_check(&table).unwrap()[0].curated);
    }

    #[test]
    fn parse_check_rejects_package_unification_without_packages() {
        // Package mode resolves one cargo invocation per selected package, so
        // an empty package list gives it nothing to resolve. The refusal is
        // also what keeps this pin and the parallel lane's workspace promotion
        // from ever being live at once: promotion requires an empty
        // `packages`, so a non-empty one makes them exclusive by construction.
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[[check]]\nname = \"daemon\"\nfeature_unification = \"package\"\n",
        )
        .unwrap();
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("no `packages`"), "got: {err}");

        // With a package list it parses, and the other three values are
        // accepted unconditionally - only `package` carries this requirement.
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[[check]]\nname = \"daemon\"\npackages = [\"d\"]\nfeature_unification = \"package\"\n",
        )
        .unwrap();
        assert_eq!(
            parse_check(&table).unwrap()[0].feature_unification,
            FeatureUnification::Package
        );
        for value in ["auto", "selected", "workspace"] {
            let src = format!("[[check]]\nname = \"x\"\nfeature_unification = \"{value}\"\n");
            let table: toml::map::Map<String, toml::Value> = toml::from_str(&src).unwrap();
            assert!(parse_check(&table).is_ok(), "{value} must parse bare");
        }
    }

    #[test]
    fn check_entry_defaults_to_auto_unification() {
        // Absence and `auto` mean the same thing, so a config rewrite that
        // materializes the default cannot change behaviour.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[[check]]\nname = \"plain\"\n").unwrap();
        assert_eq!(
            parse_check(&table).unwrap()[0].feature_unification,
            FeatureUnification::Auto
        );
    }

    #[test]
    fn parse_check_rejects_a_degenerate_filter_substring() {
        // A three-character substring is a substring of nearly every test
        // name: it suppresses tests nobody chose while still matching
        // something, so the coverage phase's alive-check can never see it.
        for field in ["skip", "only"] {
            let table: toml::map::Map<String, toml::Value> =
                toml::from_str(&format!("[[check]]\nname = \"x\"\n{field} = [\"ser\"]\n"))
                    .unwrap();
            let err = parse_check(&table).unwrap_err().to_string();
            assert!(err.contains("shorter than 4"), "{field}: {err}");
            assert!(err.contains(field), "{field}: {err}");
        }
        // Four characters is the floor, not past it.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[[check]]\nname = \"x\"\nskip = [\"seri\"]\n").unwrap();
        assert!(parse_check(&table).is_ok());
    }

    #[test]
    fn parse_check_rejects_rustflags_with_env_rustflags() {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[[check]]\nname = \"sim\"\nrustflags = [\"--cfg\", \"madsim\"]\n\
             env = { RUSTFLAGS = \"--cfg madsim\" }\n",
        )
        .unwrap();
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("rustflags") && err.contains("RUSTFLAGS"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_rustflags_with_env_target_dir() {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[[check]]\nname = \"sim\"\nrustflags = [\"--cfg\", \"madsim\"]\n\
             env = { CARGO_TARGET_DIR = \"target/madsim\" }\n",
        )
        .unwrap();
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("CARGO_TARGET_DIR"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_blank_rustflag() {
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[[check]]\nname = \"sim\"\nrustflags = [\"--cfg\", \"\"]\n").unwrap();
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("blank") && err.contains("rustflags"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_env_target_dir_without_rustflags() {
        // S3-19: brokkr owns CARGO_TARGET_DIR for the sweep's isolation
        // unconditionally - the old guard only fired when `rustflags` was also
        // set, so a plain entry hand-setting it slipped through.
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[[check]]\nname = \"x\"\nenv = { CARGO_TARGET_DIR = \"target/foo\" }\n",
        )
        .unwrap();
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("CARGO_TARGET_DIR"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_env_encoded_rustflags() {
        // S3-19: CARGO_ENCODED_RUSTFLAGS wins over a plain RUSTFLAGS, so a
        // hand-set one would silently defeat the composed flags too.
        let table: toml::map::Map<String, toml::Value> = toml::from_str(
            "[[check]]\nname = \"x\"\nenv = { CARGO_ENCODED_RUSTFLAGS = \"x\" }\n",
        )
        .unwrap();
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("CARGO_ENCODED_RUSTFLAGS"), "got: {err}");
    }

    /// Parse `[[check]]`/`[test]`/`[[quarantine]]` from a fragment and run the
    /// cross-check that guards profile-level invariants.
    fn validate_fragment(src: &str) -> Result<(), DevError> {
        let table: toml::map::Map<String, toml::Value> = toml::from_str(src).unwrap();
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let quarantine = parse_quarantine(&table).unwrap();
        validate_check_against_test(&check, test.as_ref(), &quarantine)
    }

    #[test]
    fn profile_env_rejects_reserved_target_dir() {
        // S3-19: a profile's env reaches the sweep (build_resolved_sweep merges
        // it in) and would win over the composed isolation, so the reserved
        // keys are rejected on a profile too, not only on a `[[check]]` entry.
        let err = validate_fragment(
            "[[check]]\nname = \"all\"\n\n[test.profiles.p]\nsweeps = [\"all\"]\n\
             env = { CARGO_TARGET_DIR = \"target/foo\" }\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("CARGO_TARGET_DIR") && err.contains("test.profiles.p"),
            "got: {err}"
        );
    }

    #[test]
    fn skip_phases_coverage_rejected() {
        // S3-23: coverage is a valid `failed_phase` but not skippable - it runs
        // only under a complete claim, so skipping it under the required partial
        // claim could never do anything.
        let err = validate_fragment(
            "[[check]]\nname = \"all\"\n\n[test.profiles.p]\nsweeps = [\"all\"]\n\
             certifies = \"partial\"\nskip_phases = [\"coverage\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("coverage") && err.contains("cannot be skipped"),
            "got: {err}"
        );
    }

    #[test]
    fn skip_phases_real_phase_accepted_under_partial() {
        assert!(validate_fragment(
            "[[check]]\nname = \"all\"\n\n[test.profiles.p]\nsweeps = [\"all\"]\n\
             certifies = \"partial\"\nskip_phases = [\"clippy\", \"test\"]\n",
        )
        .is_ok());
    }

    #[test]
    fn non_skippable_phases_are_real_phases() {
        // S3-23: the non-skippable set must be a subset of the real phase set -
        // `coverage` stays a valid `failed_phase` value even though it can't be
        // skipped.
        for p in NON_SKIPPABLE_PHASES {
            assert!(PHASE_NAMES.contains(&p), "{p} missing from PHASE_NAMES");
        }
        assert!(NON_SKIPPABLE_PHASES.contains(&"coverage"));
    }

    #[test]
    fn rustflags_target_key_is_stable_and_content_keyed() {
        // Deterministic across calls and shared by identical flag lists;
        // different flags key to a different dir; empty -> no isolation.
        let a = rustflags_target_key(&["--cfg".into(), "madsim".into()]).unwrap();
        let b = rustflags_target_key(&["--cfg".into(), "madsim".into()]).unwrap();
        assert_eq!(a, b);
        let c = rustflags_target_key(&["--cfg".into(), "turmoil".into()]).unwrap();
        assert_ne!(a, c);
        assert!(rustflags_target_key(&[]).is_none());
    }

    #[test]
    fn capture_env_matcher() {
        let patterns = vec!["PBFHOGG*".to_owned(), "MALLOC_CONF".to_owned()];
        assert!(matches_capture("PBFHOGG_USE_NEW_PATH", &patterns));
        assert!(matches_capture("PBFHOGG", &patterns));
        assert!(matches_capture("MALLOC_CONF", &patterns));
        assert!(!matches_capture("MALLOC_ARENA_MAX", &patterns));
        assert!(!matches_capture("PATH", &patterns));
        assert!(!matches_capture("XPBFHOGG", &patterns));
    }

    #[test]
    fn capture_env_parse_array() {
        let text = r#"
project = "pbfhogg"
capture_env = ["PBFHOGG*", "MALLOC_CONF"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        let got = parse_capture_env(table).unwrap();
        assert_eq!(got, vec!["PBFHOGG*", "MALLOC_CONF"]);
    }

    #[test]
    fn capture_env_absent_ok() {
        let text = r#"project = "pbfhogg""#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).unwrap().is_empty());
    }

    #[test]
    fn capture_env_rejects_non_array() {
        let text = r#"
project = "pbfhogg"
capture_env = "oops"
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_rejects_bare_star() {
        // `"*"` would capture every env var into results.db, including
        // PATH, SSH_AUTH_SOCK, and any API tokens. Validation is the
        // safety net.
        let text = r#"
project = "pbfhogg"
capture_env = ["*"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        let err = parse_capture_env(table).unwrap_err();
        assert!(matches!(err, DevError::Config(_)));
    }

    #[test]
    fn capture_env_rejects_middle_star() {
        // `"FOO*BAR"` would today be treated as an exact name (matches
        // nothing) - reject it loudly rather than silently no-op.
        let text = r#"
project = "pbfhogg"
capture_env = ["FOO*BAR"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_rejects_leading_star() {
        let text = r#"
project = "pbfhogg"
capture_env = ["*FOO"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_rejects_empty_string() {
        let text = r#"
project = "pbfhogg"
capture_env = [""]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_rejects_multiple_stars() {
        let text = r#"
project = "pbfhogg"
capture_env = ["FOO**"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        assert!(parse_capture_env(table).is_err());
    }

    #[test]
    fn capture_env_trims_whitespace() {
        // Leading/trailing whitespace used to be accepted literally,
        // so " PBFHOGG*" silently never matched. Trim eagerly.
        let text = r#"
project = "pbfhogg"
capture_env = ["  PBFHOGG*  ", "MALLOC_CONF"]
"#;
        let root: toml::Value = toml::from_str(text).unwrap();
        let table = root.as_table().unwrap();
        let got = parse_capture_env(table).unwrap();
        assert_eq!(got, vec!["PBFHOGG*", "MALLOC_CONF"]);
    }

    fn empty_dataset() -> Dataset {
        Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            snapshot: HashMap::new(),
            path: None,
            xxh128: None,
        }
    }

    // -------------------------------------------------------------------
    // resolve_paths
    // -------------------------------------------------------------------

    #[test]
    fn host_datasets_resolved() {
        let mut pbf = HashMap::new();
        pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "dk-indexed.osm.pbf".into(),
                seq: Some(4704),
                ..Default::default()
            },
        );
        let mut host_ds = HashMap::new();
        host_ds.insert(
            "dk".into(),
            Dataset {
                bbox: Some("1,2,3,4".into()),
                pbf,
                ..empty_dataset()
            },
        );
        let mut hosts = HashMap::new();
        hosts.insert(
            "myhost".into(),
            HostConfig {
                data: None,
                scratch: None,
                output: None,
                target: None,
                port: None,
                drives: None,
                features: Vec::new(),
                datasets: host_ds,
                tilegen: HashMap::new(),
                worktree_keep: None,
            },
        );
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.pbf.get("indexed").unwrap().file, "dk-indexed.osm.pbf");
        assert_eq!(dk.bbox.as_deref(), Some("1,2,3,4"));
    }

    #[test]
    fn unknown_host_gets_empty_datasets() {
        let config = make_config(HashMap::new());
        let resolved = resolve_paths(&config, "unknown", Path::new("/proj"), Path::new("/target"));
        assert!(resolved.datasets.is_empty());
    }

    #[test]
    fn multiple_pbf_variants() {
        let mut pbf = HashMap::new();
        pbf.insert(
            "raw".into(),
            PbfEntry {
                file: "dk-raw.osm.pbf".into(),
                xxhash: Some("aaa".into()),
                seq: Some(4704),
                ..Default::default()
            },
        );
        pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "dk-indexed.osm.pbf".into(),
                xxhash: Some("bbb".into()),
                ..Default::default()
            },
        );
        pbf.insert(
            "locations".into(),
            PbfEntry {
                file: "dk-locations.osm.pbf".into(),
                ..Default::default()
            },
        );
        let mut host_ds = HashMap::new();
        host_ds.insert(
            "dk".into(),
            Dataset {
                pbf,
                ..empty_dataset()
            },
        );
        let mut hosts = HashMap::new();
        hosts.insert(
            "myhost".into(),
            HostConfig {
                data: None,
                scratch: None,
                output: None,
                target: None,
                port: None,
                drives: None,
                features: Vec::new(),
                datasets: host_ds,
                tilegen: HashMap::new(),
                worktree_keep: None,
            },
        );
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.pbf.len(), 3);
        assert_eq!(dk.pbf.get("raw").unwrap().xxhash.as_deref(), Some("aaa"));
        assert_eq!(
            dk.pbf.get("indexed").unwrap().xxhash.as_deref(),
            Some("bbb")
        );
    }

    #[test]
    fn multiple_osc_entries() {
        let mut osc = HashMap::new();
        osc.insert(
            "4705".into(),
            OscEntry {
                file: "dk-4705.osc.gz".into(),
                xxhash: Some("ccc".into()),
            },
        );
        osc.insert(
            "4706".into(),
            OscEntry {
                file: "dk-4706.osc.gz".into(),
                xxhash: None,
            },
        );
        let mut host_ds = HashMap::new();
        host_ds.insert(
            "dk".into(),
            Dataset {
                osc,
                ..empty_dataset()
            },
        );
        let mut hosts = HashMap::new();
        hosts.insert(
            "myhost".into(),
            HostConfig {
                data: None,
                scratch: None,
                output: None,
                target: None,
                port: None,
                drives: None,
                features: Vec::new(),
                datasets: host_ds,
                tilegen: HashMap::new(),
                worktree_keep: None,
            },
        );
        let config = make_config(hosts);
        let resolved = resolve_paths(&config, "myhost", Path::new("/proj"), Path::new("/target"));
        let dk = resolved.datasets.get("dk").unwrap();
        assert_eq!(dk.osc.len(), 2);
        assert_eq!(dk.osc.get("4705").unwrap().file, "dk-4705.osc.gz");
    }

    // -------------------------------------------------------------------
    // TOML parsing
    // -------------------------------------------------------------------

    #[test]
    fn parse_nested_dataset_from_toml() {
        let toml_str = r#"
project = "pbfhogg"

[myhost.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "8.0,54.5,13.0,58.0"

[myhost.datasets.denmark.pbf.raw]
file = "dk-raw.osm.pbf"
sha256 = "aaa"
seq = 4704

[myhost.datasets.denmark.pbf.indexed]
file = "dk-indexed.osm.pbf"
sha256 = "bbb"

[myhost.datasets.denmark.osc.4705]
file = "dk-4705.osc.gz"
sha256 = "ccc"
"#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        let host = hosts.get("myhost").unwrap();
        let dk = host.datasets.get("denmark").unwrap();
        assert_eq!(dk.origin.as_deref(), Some("Geofabrik"));
        assert_eq!(dk.download_date.as_deref(), Some("2026-02-20"));
        assert_eq!(dk.bbox.as_deref(), Some("8.0,54.5,13.0,58.0"));
        assert_eq!(dk.pbf.get("raw").unwrap().file, "dk-raw.osm.pbf");
        assert_eq!(dk.pbf.get("raw").unwrap().seq, Some(4704));
        assert_eq!(
            dk.pbf.get("indexed").unwrap().xxhash.as_deref(),
            Some("bbb")
        );
        assert_eq!(dk.osc.get("4705").unwrap().file, "dk-4705.osc.gz");
    }

    #[test]
    fn parse_dataset_with_snapshot_table() {
        let toml_str = r#"
project = "pbfhogg"

[myhost.datasets.planet]
origin = "planet.openstreetmap.org"

[myhost.datasets.planet.pbf.raw]
file = "planet-base.osm.pbf"

[myhost.datasets.planet.snapshot.20260411]
download_date = "2026-04-11"
seq = 4969

[myhost.datasets.planet.snapshot.20260411.pbf.raw]
file = "planet-20260411.osm.pbf"
xxhash = "deadbeef"

[myhost.datasets.planet.snapshot.20260411.pbf.indexed]
file = "planet-20260411-with-indexdata.osm.pbf"
xxhash = "feedface"
"#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        let host = hosts.get("myhost").unwrap();
        let planet = host.datasets.get("planet").unwrap();
        assert_eq!(planet.pbf.get("raw").unwrap().file, "planet-base.osm.pbf");
        let snap = planet.snapshot.get("20260411").unwrap();
        assert_eq!(snap.download_date.as_deref(), Some("2026-04-11"));
        assert_eq!(snap.seq, Some(4969));
        assert_eq!(snap.pbf.get("raw").unwrap().file, "planet-20260411.osm.pbf");
        assert_eq!(snap.pbf.get("raw").unwrap().xxhash.as_deref(), Some("deadbeef"));
        assert_eq!(
            snap.pbf.get("indexed").unwrap().file,
            "planet-20260411-with-indexdata.osm.pbf"
        );
    }

    #[test]
    fn snapshot_named_base_is_rejected() {
        let mut hc = HostConfig {
            data: None,
            scratch: None,
            output: None,
            target: None,
            port: None,
            drives: None,
            features: Vec::new(),
            datasets: HashMap::new(),
            tilegen: HashMap::new(),
                worktree_keep: None,
        };
        let mut ds = Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            snapshot: HashMap::new(),
            path: None,
            xxh128: None,
        };
        ds.snapshot.insert(
            "base".into(),
            Snapshot {
                download_date: None,
                seq: None,
                pbf: HashMap::new(),
                osc: HashMap::new(),
            },
        );
        hc.datasets.insert("planet".into(), ds);
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), hc);

        let err = validate_datasets(&hosts).unwrap_err().to_string();
        assert!(err.contains("'base' is a reserved snapshot name"), "got: {err}");
    }

    #[test]
    fn snapshot_key_with_invalid_chars_rejected() {
        let mut hc = HostConfig {
            data: None,
            scratch: None,
            output: None,
            target: None,
            port: None,
            drives: None,
            features: Vec::new(),
            datasets: HashMap::new(),
            tilegen: HashMap::new(),
                worktree_keep: None,
        };
        let mut ds = Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            snapshot: HashMap::new(),
            path: None,
            xxh128: None,
        };
        ds.snapshot.insert(
            "bad key".into(),
            Snapshot {
                download_date: None,
                seq: None,
                pbf: HashMap::new(),
                osc: HashMap::new(),
            },
        );
        hc.datasets.insert("planet".into(), ds);
        let mut hosts = HashMap::new();
        hosts.insert("myhost".into(), hc);

        let err = validate_datasets(&hosts).unwrap_err().to_string();
        assert!(err.contains("[a-zA-Z0-9_-]+"), "got: {err}");
    }

    #[test]
    fn both_sha256_and_xxhash_is_rejected() {
        let toml_str = r#"
project = "pbfhogg"

[myhost.datasets.dk.pbf.raw]
file = "test.pbf"
sha256 = "aaa"
xxhash = "bbb"
"#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let result = parse_hosts(table);
        assert!(
            result.is_err(),
            "should reject entry with both sha256 and xxhash"
        );
    }

    #[test]
    fn parse_no_host_section() {
        let toml_str = r#"project = "pbfhogg""#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        assert!(hosts.is_empty());
    }

    #[test]
    fn parse_pmtiles_entries() {
        let toml_str = r#"
project = "nidhogg"

[myhost.datasets.denmark.pmtiles.elivagar]
file = "denmark-elivagar.pmtiles"
sha256 = "ddd"
"#;
        let root: toml::Value = toml::from_str(toml_str).unwrap();
        let table = root.as_table().unwrap();
        let hosts = parse_hosts(table).unwrap();
        let host = hosts.get("myhost").unwrap();
        let dk = host.datasets.get("denmark").unwrap();
        assert_eq!(dk.pmtiles.len(), 1);
        assert_eq!(
            dk.pmtiles.get("elivagar").unwrap().file,
            "denmark-elivagar.pmtiles"
        );
        assert_eq!(
            dk.pmtiles.get("elivagar").unwrap().xxhash.as_deref(),
            Some("ddd")
        );
    }

    // -------------------------------------------------------------------
    // [[check]] parsing
    // -------------------------------------------------------------------

    fn root_table(text: &str) -> toml::map::Map<String, toml::Value> {
        let v: toml::Value = toml::from_str(text).unwrap();
        v.as_table().unwrap().clone()
    }

    #[test]
    fn parse_check_returns_empty_when_absent() {
        let table = root_table(r#"project = "pbfhogg""#);
        let check = parse_check(&table).unwrap();
        assert!(check.is_empty());
    }

    #[test]
    fn parse_check_array_of_tables() {
        let table = root_table(
            r#"
project = "pbfhogg"

[[check]]
name = "all"
features = ["test-hooks", "linux-direct-io"]

[[check]]
name = "consumer"
no_default_features = true
features = ["commands"]
build_packages = ["pbfhogg-cli"]
"#,
        );
        let check = parse_check(&table).unwrap();
        assert_eq!(check.len(), 2);
        assert_eq!(check[0].name, "all");
        assert_eq!(check[0].features, vec!["test-hooks", "linux-direct-io"]);
        assert!(!check[0].no_default_features);
        assert!(check[0].build_packages.is_empty());

        assert_eq!(check[1].name, "consumer");
        assert_eq!(check[1].features, vec!["commands"]);
        assert!(check[1].no_default_features);
        assert_eq!(check[1].build_packages, vec!["pbfhogg-cli"]);
    }

    #[test]
    fn parse_check_rejects_legacy_table_form() {
        // The previous shape was `[check]\nconsumer_features = [...]`.
        // Detect the singular table and error loudly so a stale config
        // doesn't silently fall through.
        let table = root_table(
            r#"
project = "pbfhogg"
[check]
consumer_features = ["commands"]
"#,
        );
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("[[check]]"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_duplicate_names() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
features = ["a"]
[[check]]
name = "all"
features = ["b"]
"#,
        );
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("duplicate name 'all'"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_empty_name() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = ""
features = ["a"]
"#,
        );
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("empty `name`"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_blank_build_packages() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
build_packages = [""]
"#,
        );
        let err = parse_check(&table).unwrap_err().to_string();
        assert!(err.contains("build_packages"), "got: {err}");
    }

    #[test]
    fn parse_manifest_rejects_empty_adapter_marker() {
        let table = root_table(
            r#"
project = "ratatoskr"
[manifest]
[manifest.adapter_group]
marker = ""
forbidden_in = ["core"]
"#,
        );
        let err = parse_manifest(&table).unwrap_err().to_string();
        assert!(err.contains("empty `marker`"), "got: {err}");
    }

    #[test]
    fn parse_manifest_rejects_empty_forbidden_in() {
        let table = root_table(
            r#"
project = "ratatoskr"
[manifest]
[manifest.adapter_group]
marker = "Adapter dependencies"
forbidden_in = []
"#,
        );
        let err = parse_manifest(&table).unwrap_err().to_string();
        assert!(err.contains("forbidden_in"), "got: {err}");
    }

    #[test]
    fn parse_check_rejects_features_all_sentinel() {
        // The `features = "all"` shorthand is gone - explicit lists only.
        // serde rejects with a type-mismatch error, which is loud enough
        // (the user sees "expected sequence" pointing at the offending line).
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "everything"
features = "all"
"#,
        );
        assert!(parse_check(&table).is_err());
    }

    #[test]
    fn parse_dependency_rules_accepts_single_or_array_values() {
        let table = root_table(
            r#"
project = "ratatoskr"

[[dependency_rule]]
name = "app-db"
from = "app"
forbid = ["db", "service-state"]
"#,
        );
        let rules = parse_dependency_rules(&table).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name.as_deref(), Some("app-db"));
        assert_eq!(rules[0].from, vec!["app"]);
        assert_eq!(rules[0].forbid, vec!["db", "service-state"]);
    }

    #[test]
    fn parse_dependency_rules_rejects_empty_lists() {
        let table = root_table(
            r#"
project = "ratatoskr"

[[dependency_rule]]
from = []
forbid = "db"
"#,
        );
        let err = parse_dependency_rules(&table).unwrap_err().to_string();
        assert!(err.contains("empty `from`"), "got: {err}");
    }

    #[test]
    fn dependency_rule_is_not_treated_as_host_section() {
        let table = root_table(
            r#"
project = "ratatoskr"

[[dependency_rule]]
from = "app"
forbid = "db"
"#,
        );
        let hosts = parse_hosts(&table).unwrap();
        assert!(hosts.is_empty());
    }

    #[test]
    fn parse_test_rejects_legacy_sweeps_section() {
        let table = root_table(
            r#"
project = "pbfhogg"

[test]

[test.sweeps.all]
features = ["a"]
"#,
        );
        let err = parse_test(&table).unwrap_err().to_string();
        assert!(err.contains("[test.sweeps]"), "got: {err}");
    }

    #[test]
    fn validate_check_against_test_catches_dangling_sweep_reference() {
        let check = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "tier1".into(),
            ProfileDef {
                description: None,
                certifies: None,
                skip_phases: None,
                isolation: None,
                lanes: None,
                extends: None,
                sweeps: Some(vec!["all".into(), "consumer".into()]),
                tests: None,
                only: None,
                skip: None,
                include_ignored: None,
                test_threads: None,
                env: None,
            },
        );
        let test = TestConfig {
            default_package: None,
            default_profile: None,
            gate_profile: None,
            debug: false,
            doctests: false,
            profiles,
        };
        let err = validate_check_against_test(&check, Some(&test), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("'consumer'"), "got: {err}");
    }

    #[test]
    fn validate_check_against_test_catches_typoed_default_profile() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test]
default_profile = "teir1"
[test.profiles.tier1]
sweeps = ["all"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("teir1"), "got: {err}");
    }

    #[test]
    fn validate_check_against_test_catches_dangling_extends() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.tier1]
extends = "nope"
sweeps = ["all"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("'nope'"), "got: {err}");
    }

    #[test]
    fn profile_filters_are_held_to_the_same_floor_as_entry_filters() {
        // The floor lives at load precisely so it reaches profiles the
        // coverage phase never enumerates - this one certifies nothing, so
        // the alive-check would never see it.
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.tier1]
sweeps = ["all"]
skip = [{ package = "core", pattern = "io" }]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("shorter than 4"), "got: {err}");
        assert!(err.contains("[test.profiles.tier1]"), "got: {err}");
    }

    #[test]
    fn gate_profile_must_name_existing_profile() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test]
gate_profile = "nope"
[test.profiles.tier1]
sweeps = ["all"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("gate_profile"), "got: {err}");
        assert!(err.contains("'nope'"), "got: {err}");
    }

    #[test]
    fn gate_profile_must_certify_complete() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test]
gate_profile = "edit"
[test.profiles.edit]
certifies = "partial"
sweeps = ["all"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("complete"), "got: {err}");
    }

    #[test]
    fn gate_profile_accepts_clean_complete_profile() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test]
doctests = true
gate_profile = "gate"
[test.profiles.gate]
certifies = "complete"
sweeps = ["all"]
include_ignored = true
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        validate_check_against_test(&check, test.as_ref(), &[]).unwrap();
    }

    #[test]
    fn skip_phases_requires_partial_claim() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.edit]
skip_phases = ["textlint"]
sweeps = ["all"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("skip_phases"), "got: {err}");
        assert!(err.contains("partial"), "got: {err}");
    }

    #[test]
    fn skip_phases_validates_phase_names() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.edit]
certifies = "partial"
skip_phases = ["clipy"]
sweeps = ["all"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("'clipy'"), "got: {err}");
        assert!(err.contains("not a skippable check phase"), "got: {err}");
    }

    #[test]
    fn partial_profile_with_skips_and_skip_phases_is_accepted() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.edit]
certifies = "partial"
skip_phases = ["script_check", "textlint"]
sweeps = ["all"]
skip = ["tier2::"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        validate_check_against_test(&check, test.as_ref(), &[]).unwrap();
    }

    #[test]
    fn complete_profile_allows_audited_narrowing() {
        // Feature 4 relaxed the interim rule: skip lists, sweep-level
        // filters, and include_ignored left unset are all legal at load
        // time now - the coverage phase audits them against [[quarantine]]
        // at run time.
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
skip = ["flaky::"]
[test]
doctests = true
[test.profiles.gate]
certifies = "complete"
sweeps = ["all"]
skip = ["tier2::"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        validate_check_against_test(&check, test.as_ref(), &[]).unwrap();
    }

    #[test]
    fn complete_profile_requires_doctests_or_quarantine() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.gate]
certifies = "complete"
sweeps = ["all"]
include_ignored = true
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("doctests"), "got: {err}");

        // A doctests quarantine entry with an issue justifies the same
        // config.
        let q = vec![QuarantineEntry {
            pattern: None,
            package: None,
            category: Some("doctests".into()),
            issue: "B42".into(),
            reason: "42/55 persistence doctests fail to compile".into(),
        }];
        validate_check_against_test(&check, test.as_ref(), &q).unwrap();
    }

    #[test]
    fn doctests_quarantine_goes_stale_when_doctests_run() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test]
doctests = true
[test.profiles.tier1]
sweeps = ["all"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let q = vec![QuarantineEntry {
            pattern: None,
            package: None,
            category: Some("doctests".into()),
            issue: "B42".into(),
            reason: "no longer true".into(),
        }];
        let err = validate_check_against_test(&check, test.as_ref(), &q)
            .unwrap_err()
            .to_string();
        assert!(err.contains("stale"), "got: {err}");
    }

    #[test]
    fn complete_profile_rejects_extends() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test]
doctests = true
[test.profiles.base]
sweeps = ["all"]
[test.profiles.gate]
certifies = "complete"
extends = "base"
include_ignored = true
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("extends"), "got: {err}");
    }

    #[test]
    fn lanes_profile_rejects_run_shaping_fields() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.tier1]
sweeps = ["all"]
[test.profiles.combo]
lanes = ["tier1"]
sweeps = ["all"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("combines `lanes`"), "got: {err}");
    }

    #[test]
    fn lanes_must_reference_existing_profiles() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.combo]
lanes = ["nope"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("lane 'nope'"), "got: {err}");
    }

    #[test]
    fn lane_may_not_declare_certifies() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.tier1]
certifies = "partial"
sweeps = ["all"]
[test.profiles.combo]
lanes = ["tier1"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("declares `certifies`"), "got: {err}");
    }

    #[test]
    fn lanes_do_not_nest() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.tier1]
sweeps = ["all"]
[test.profiles.inner]
lanes = ["tier1"]
[test.profiles.outer]
lanes = ["inner"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("do not nest"), "got: {err}");
    }

    #[test]
    fn complete_lanes_profile_validates_each_lane() {
        // The structural rule (`extends` under a complete claim) is still
        // enforced per lane, with the composing profile named in the error.
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test]
doctests = true
[test.profiles.base]
sweeps = ["all"]
[test.profiles.tier1]
extends = "base"
[test.profiles.gate]
certifies = "complete"
lanes = ["tier1"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("via lanes"), "got: {err}");
        assert!(err.contains("extends"), "got: {err}");
    }

    #[test]
    fn complete_lanes_profile_with_clean_lanes_passes() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test]
doctests = true
gate_profile = "gate"
[test.profiles.lane-par]
sweeps = ["all"]
include_ignored = true
test_threads = 0
[test.profiles.lane-ser]
sweeps = ["all"]
include_ignored = true
[test.profiles.gate]
certifies = "complete"
lanes = ["lane-par", "lane-ser"]
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        validate_check_against_test(&check, test.as_ref(), &[]).unwrap();
    }

    #[test]
    fn process_isolation_requires_serial_threads() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.serial]
sweeps = ["all"]
isolation = "process"
test_threads = 4
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        let err = validate_check_against_test(&check, test.as_ref(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("serial by construction"), "got: {err}");
    }

    #[test]
    fn process_isolation_accepts_serial_profile() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.serial]
sweeps = ["all"]
only = ["serial::"]
isolation = "process"
test_threads = 1
"#,
        );
        let check = parse_check(&table).unwrap();
        let test = parse_test(&table).unwrap();
        validate_check_against_test(&check, test.as_ref(), &[]).unwrap();
    }

    #[test]
    fn certifies_rejects_unknown_value_at_parse() {
        let table = root_table(
            r#"
project = "pbfhogg"
[[check]]
name = "all"
[test.profiles.gate]
certifies = "full"
sweeps = ["all"]
"#,
        );
        let err = parse_test(&table).unwrap_err().to_string();
        assert!(err.contains("full"), "got: {err}");
    }

    #[test]
    fn ratatoskr_harness_binary_defaults_to_package() {
        let h = HarnessConfig {
            package: "app".into(),
            binary: None,
            features: Vec::new(),
            debug: None,
        };
        assert_eq!(h.binary_name(), "app");
    }

    #[test]
    fn ratatoskr_harness_binary_override_wins() {
        let h = HarnessConfig {
            package: "app".into(),
            binary: Some("parent_death_helper".into()),
            features: Vec::new(),
            debug: None,
        };
        assert_eq!(h.binary_name(), "parent_death_helper");
    }

    #[test]
    fn ratatoskr_harness_rejects_legacy_sweep_field() {
        let raw = r#"
project = "ratatoskr"
[ratatoskr.harness]
sweep = "harness"
binary = "app"
"#;
        let root: toml::Value = toml::from_str(raw).unwrap();
        let table = root.as_table().unwrap();
        let err = parse_ratatoskr(table).unwrap_err().to_string();
        assert!(err.contains("sweep"), "got: {err}");
        assert!(err.contains("no longer supported"), "got: {err}");
    }

    #[test]
    fn ratatoskr_harness_parses_new_schema() {
        let raw = r#"
project = "ratatoskr"
[ratatoskr.harness]
package = "app"
debug = true
"#;
        let root: toml::Value = toml::from_str(raw).unwrap();
        let table = root.as_table().unwrap();
        let cfg = parse_ratatoskr(table).unwrap().unwrap();
        let h = cfg.harness.unwrap();
        assert_eq!(h.package, "app");
        assert_eq!(h.binary_name(), "app");
        assert!(h.features.is_empty());
        assert_eq!(h.debug, Some(true));
    }

    #[test]
    fn check_entry_cargo_feature_args_shapes() {
        // No flags → no args at all (use cargo defaults).
        let bare = CheckEntry {
            name: "bare".into(),
            features: Vec::new(),
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        };
        assert!(bare.cargo_feature_args().is_empty());

        // --features only.
        let feats = CheckEntry {
            name: "f".into(),
            features: vec!["a".into(), "b".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        };
        assert_eq!(feats.cargo_feature_args(), vec!["--features", "a,b"]);

        // --no-default-features only.
        let nd = CheckEntry {
            name: "nd".into(),
            features: Vec::new(),
            no_default_features: true,
            build_packages: Vec::new(),
            ..Default::default()
        };
        assert_eq!(nd.cargo_feature_args(), vec!["--no-default-features"]);

        // Both.
        let consumer = CheckEntry {
            name: "consumer".into(),
            features: vec!["commands".into()],
            no_default_features: true,
            build_packages: vec!["pbfhogg-cli".into()],
            ..Default::default()
        };
        assert_eq!(
            consumer.cargo_feature_args(),
            vec!["--no-default-features", "--features", "commands"]
        );
    }

    // -----------------------------------------------------------------------
    // [<host>.tilegen.<name>] ocean statements
    // -----------------------------------------------------------------------

    fn hosts_with_ocean(ocean: &[&str]) -> HashMap<String, HostConfig> {
        let mut tilegen = HashMap::new();
        tilegen.insert(
            "default".into(),
            TilegenConfig {
                ocean: ocean.iter().map(|s| (*s).to_owned()).collect(),
                ..Default::default()
            },
        );
        let mut hosts = HashMap::new();
        hosts.insert(
            "myhost".into(),
            HostConfig {
                data: None,
                scratch: None,
                output: None,
                target: None,
                port: None,
                drives: None,
                features: Vec::new(),
                datasets: HashMap::new(),
                tilegen,
                worktree_keep: None,
            },
        );
        hosts
    }

    const LOW: &str = "z0-z7:simplified.shp";
    const HIGH: &str = "z8-z14:full.shp";
    const ALL: &str = "z0-z14:full.shp";
    const ARTIFACT: &str = "ocean-tiles.pmtiles";

    #[test]
    fn ocean_accepts_the_two_legal_partitions() {
        assert!(validate_tilegen(&hosts_with_ocean(&[LOW, HIGH])).is_ok());
        assert!(validate_tilegen(&hosts_with_ocean(&[ALL])).is_ok());
        assert!(validate_tilegen(&hosts_with_ocean(&[LOW, HIGH, ARTIFACT])).is_ok());
        assert!(validate_tilegen(&hosts_with_ocean(&[ALL, ARTIFACT])).is_ok());
    }

    /// Omitting `ocean` entirely is the statement elivagar's removed
    /// `--no-ocean` used to make, so it must stay legal.
    #[test]
    fn ocean_absent_means_no_ocean() {
        assert!(validate_tilegen(&hosts_with_ocean(&[])).is_ok());
    }

    /// The z7/z8 split is the only one `ocean::selected_pass_grid` implements.
    /// A partial partition must be refused rather than quietly served at z7.
    #[test]
    fn ocean_rejects_a_partial_partition() {
        let err = validate_tilegen(&hosts_with_ocean(&[LOW])).unwrap_err();
        assert!(format!("{err}").contains("partition z0-z14 exactly"));

        let err = validate_tilegen(&hosts_with_ocean(&[HIGH])).unwrap_err();
        assert!(format!("{err}").contains("partition z0-z14 exactly"));
    }

    #[test]
    fn ocean_rejects_an_unimplemented_band() {
        let err = validate_tilegen(&hosts_with_ocean(&["z0-z5:simplified.shp"])).unwrap_err();
        assert!(format!("{err}").contains("z0-z5"));
    }

    #[test]
    fn ocean_rejects_overlapping_bands() {
        let err = validate_tilegen(&hosts_with_ocean(&[ALL, LOW, HIGH])).unwrap_err();
        assert!(format!("{err}").contains("partition z0-z14 exactly"));
    }

    #[test]
    fn ocean_rejects_a_band_named_twice() {
        let err = validate_tilegen(&hosts_with_ocean(&[LOW, "z0-z7:other.shp", HIGH])).unwrap_err();
        assert!(format!("{err}").contains("named twice"));
    }

    /// The artifact is a cache over the shapefiles, not a substitute: an
    /// extract computes its boundary band from them, and the artifact's key is
    /// validated by re-hashing them.
    #[test]
    fn ocean_rejects_a_lone_artifact() {
        let err = validate_tilegen(&hosts_with_ocean(&[ARTIFACT])).unwrap_err();
        assert!(format!("{err}").contains("cannot stand alone"));
    }

    #[test]
    fn ocean_rejects_two_artifacts() {
        let err =
            validate_tilegen(&hosts_with_ocean(&[LOW, HIGH, ARTIFACT, "other.pmtiles"])).unwrap_err();
        assert!(format!("{err}").contains("at most one"));
    }

    #[test]
    fn ocean_rejects_a_shapefile_without_a_band() {
        let err = validate_tilegen(&hosts_with_ocean(&["full.shp"])).unwrap_err();
        assert!(format!("{err}").contains("needs a zoom band prefix"));
    }

    #[test]
    fn ocean_spec_round_trips_its_band() {
        assert_eq!(OceanSpec::parse(LOW).unwrap().file(), "simplified.shp");
        assert_eq!(
            OceanSpec::parse(LOW).unwrap().render("/data/simplified.shp"),
            "z0-z7:/data/simplified.shp"
        );
        assert_eq!(
            OceanSpec::parse(ARTIFACT).unwrap().render("/data/ocean-tiles.pmtiles"),
            "/data/ocean-tiles.pmtiles"
        );
    }

    // -------------------------------------------------------------------
    // [dellingr.workloads.*] parsing
    // -------------------------------------------------------------------

    /// A `[dellingr]` block with the given workload body appended.
    fn dellingr_table(workload_body: &str) -> toml::map::Map<String, toml::Value> {
        root_table(&format!(
            "project = \"dellingr\"\n\n[dellingr]\nexample = \"hotpath\"\n\n\
             [dellingr.workloads.w]\n{workload_body}"
        ))
    }

    #[test]
    fn dellingr_accepts_a_workload_with_both_pins() {
        let cfg = parse_dellingr(&dellingr_table(
            "file = \"bench/w.lua\"\nxxh128 = \"00\"\n\
             hotpath_file = \"examples/w.lua\"\nhotpath_xxh128 = \"11\"\n",
        ))
        .unwrap()
        .unwrap();
        let w = &cfg.workloads["w"];
        assert_eq!(w.hotpath_file.as_deref(), Some(Path::new("examples/w.lua")));
        assert_eq!(w.hotpath_xxh128.as_deref(), Some("11"));
    }

    #[test]
    fn dellingr_accepts_a_workload_with_no_hotpath_pin() {
        let cfg = parse_dellingr(&dellingr_table(
            "file = \"bench/w.lua\"\nxxh128 = \"00\"\n",
        ))
        .unwrap()
        .unwrap();
        assert!(cfg.workloads["w"].hotpath_file.is_none());
    }

    /// The absolute-path guard exists so a workload cannot escape the project
    /// root; `hotpath_file` is resolved the same way `file` is, so it needs the
    /// same guard - `Path::join` would silently discard the root.
    #[test]
    fn dellingr_rejects_an_absolute_hotpath_file() {
        let err = parse_dellingr(&dellingr_table(
            "file = \"bench/w.lua\"\nxxh128 = \"00\"\n\
             hotpath_file = \"/elsewhere/w.lua\"\nhotpath_xxh128 = \"11\"\n",
        ))
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("hotpath_file"), "{msg}");
        assert!(msg.contains("absolute"), "{msg}");
    }

    #[test]
    fn dellingr_rejects_an_absolute_file() {
        let err = parse_dellingr(&dellingr_table(
            "file = \"/elsewhere/w.lua\"\nxxh128 = \"00\"\n",
        ))
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(".file"), "{msg}");
        assert!(msg.contains("absolute"), "{msg}");
    }

    /// An empty digest would otherwise surface as a hash mismatch at run time,
    /// which reads like drift rather than the config hole it is.
    #[test]
    fn dellingr_rejects_an_empty_digest_on_either_pin() {
        let err = parse_dellingr(&dellingr_table(
            "file = \"bench/w.lua\"\nxxh128 = \"  \"\n",
        ))
        .unwrap_err();
        assert!(format!("{err}").contains(".xxh128 is empty"), "{err}");

        let err = parse_dellingr(&dellingr_table(
            "file = \"bench/w.lua\"\nxxh128 = \"00\"\n\
             hotpath_file = \"examples/w.lua\"\nhotpath_xxh128 = \"\"\n",
        ))
        .unwrap_err();
        assert!(
            format!("{err}").contains(".hotpath_xxh128 is empty"),
            "{err}"
        );
    }

    /// Named at parse time, so a half-registered workload is reported even
    /// when nobody runs *that* workload today.
    #[test]
    fn dellingr_rejects_a_half_registered_hotpath_pair() {
        let err = parse_dellingr(&dellingr_table(
            "file = \"bench/w.lua\"\nxxh128 = \"00\"\n\
             hotpath_file = \"examples/w.lua\"\n",
        ))
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("hotpath_file without hotpath_xxh128"), "{msg}");

        let err = parse_dellingr(&dellingr_table(
            "file = \"bench/w.lua\"\nxxh128 = \"00\"\nhotpath_xxh128 = \"11\"\n",
        ))
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("hotpath_xxh128 without hotpath_file"), "{msg}");
    }

    // -----------------------------------------------------------------------
    // User-wide config layer
    // -----------------------------------------------------------------------

    fn user_path() -> PathBuf {
        PathBuf::from("/home/u/.config/brokkr/brokkr.toml")
    }

    const USER_RULE: &str = "[[textlint]]\n\
         name = \"no-shouting\"\n\
         pattern = \"!!\"\n\
         paths = [\"**/*.md\"]\n\
         message = \"one exclamation mark is enough\"\n";

    #[test]
    fn user_config_carries_textlint_and_script_check() {
        let cfg = parse_user(
            &format!(
                "{USER_RULE}\n\
                 [[script_check]]\n\
                 name = \"shellcheck\"\n\
                 command = \"true\"\n\
                 expect = \"ok\"\n"
            ),
            user_path(),
        )
        .unwrap();
        assert_eq!(cfg.textlint.len(), 1);
        assert_eq!(cfg.script_checks.len(), 1);
        assert_eq!(cfg.textlint[0].name, "no-shouting");
    }

    /// The whole point of the thin schema: a project-shaped key in a machine-wide
    /// file is a mistake, not a setting, and saying so beats silently ignoring it.
    #[test]
    fn user_config_rejects_project_shaped_keys() {
        for body in ["project = \"brokkr\"\n", "[[check]]\nname = \"x\"\n"] {
            let err = parse_user(body, user_path()).unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("not allowed in the user-wide config"), "{msg}");
            assert!(msg.contains("/home/u/.config/brokkr/brokkr.toml"), "{msg}");
        }
    }

    /// Errors name the file they came from - the user is not in this directory.
    #[test]
    fn user_config_errors_are_attributed_to_the_file() {
        let err = parse_user("[[textlint]]\nname = \"x\"\n", user_path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("/home/u/.config/brokkr/brokkr.toml: [[textlint]]"),
            "{msg}"
        );
    }

    /// Presets are resolved within the file that defines them: a user preset
    /// serves user rules, and is dead if it serves none.
    #[test]
    fn user_config_resolves_its_own_presets() {
        let cfg = parse_user(
            "[textlint_preset.md]\npaths = [\"**/*.md\"]\n\n\
             [[textlint]]\nname = \"r\"\npattern = \"!!\"\nmessage = \"m\"\n\
             preset = \"md\"\n",
            user_path(),
        )
        .unwrap();
        assert_eq!(cfg.textlint[0].paths, vec!["**/*.md".to_owned()]);

        let err = parse_user("[textlint_preset.md]\npaths = [\"a\"]\n", user_path()).unwrap_err();
        assert!(format!("{err}").contains("no `[[textlint]]` rule"), "{err}");
    }

    #[test]
    fn merging_puts_user_entries_first_and_lets_the_project_shadow_by_name() {
        let mut project = vec!["b".to_owned(), "shared".to_owned()];
        merge_named(
            &mut project,
            vec!["a".to_owned(), "shared".to_owned()],
            |s| s.as_str(),
        );
        assert_eq!(project, vec!["a", "b", "shared"]);
    }

    #[test]
    fn merging_nothing_leaves_the_project_untouched() {
        let mut project = vec!["b".to_owned()];
        merge_named(&mut project, Vec::new(), |s| s.as_str());
        assert_eq!(project, vec!["b"]);
    }

    /// An explicit empty override is the opt-out, not a path to a file named "".
    #[test]
    fn empty_env_override_disables_the_layer() {
        let old = std::env::var_os("BROKKR_USER_CONFIG");
        unsafe { std::env::set_var("BROKKR_USER_CONFIG", "") };
        let path = user_config_path();
        match old {
            Some(v) => unsafe { std::env::set_var("BROKKR_USER_CONFIG", v) },
            None => unsafe { std::env::remove_var("BROKKR_USER_CONFIG") },
        }
        assert!(path.is_none());
    }
}
