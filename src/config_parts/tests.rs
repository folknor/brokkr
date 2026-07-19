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
            dependency_rules: Vec::new(),
            check: Vec::new(),
            test: None,
            capture_env: Vec::new(),
            gremlins: None,
            style: None,
            disable_toolchain: false,
        }
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
        assert!(cfg.allow.contains(&'\u{2019}'));
        // Case-insensitive `u+` prefix accepted.
        assert!(cfg.ban.contains(&'\u{2011}'));
    }

    #[test]
    fn parse_style_enabled() {
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[style]\nrust_blank_line_above_control_flow = true\n").unwrap();
        let cfg = parse_style(&table).unwrap().unwrap();
        assert!(cfg.rust_blank_line_above_control_flow);
    }

    #[test]
    fn parse_style_absent_or_empty_is_none() {
        // No section at all.
        let table: toml::map::Map<String, toml::Value> = toml::from_str("project = \"x\"\n").unwrap();
        assert!(parse_style(&table).unwrap().is_none());
        // Present but nothing enabled collapses to None so the phase stays inert.
        let empty: toml::map::Map<String, toml::Value> = toml::from_str("[style]\n").unwrap();
        assert!(parse_style(&empty).unwrap().is_none());
    }

    #[test]
    fn parse_style_rejects_unknown_key() {
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[style]\nbogus = true\n").unwrap();
        assert!(parse_style(&table).is_err());
    }

    #[test]
    fn style_section_is_not_treated_as_host() {
        // `[style]` must be reserved, not parsed as a hostname section.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[style]\nrust_blank_line_above_control_flow = true\n").unwrap();
        let hosts = parse_hosts(&table).unwrap();
        assert!(hosts.is_empty());
    }

    #[test]
    fn parse_gremlins_rejects_bad_codepoint_and_overlap() {
        // Not U+XXXX form.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nban = [\"2011\"]\n").unwrap();
        assert!(parse_gremlins(&table).is_err());

        // Non-hex.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nallow = [\"U+ZZZZ\"]\n").unwrap();
        assert!(parse_gremlins(&table).is_err());

        // A codepoint in both lists.
        let table: toml::map::Map<String, toml::Value> =
            toml::from_str("[gremlins]\nallow = [\"U+2011\"]\nban = [\"U+2011\"]\n").unwrap();
        let err = parse_gremlins(&table).unwrap_err().to_string();
        assert!(err.contains("both"), "got: {err}");
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
            blessed: None,
            snapshot: HashMap::new(),
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
        };
        let mut ds = Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            blessed: None,
            snapshot: HashMap::new(),
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
        };
        let mut ds = Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            blessed: None,
            snapshot: HashMap::new(),
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
            debug: false,
            profiles,
        };
        let err = validate_check_against_test(&check, Some(&test))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'consumer'"), "got: {err}");
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
}
