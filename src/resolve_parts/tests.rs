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
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::{Dataset, OscEntry, PbfEntry, PmtilesEntry, ResolvedPaths};

    use super::*;

    fn unique_test_dir(name: &str) -> PathBuf {
        let cwd = std::env::current_dir().expect("cwd");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        cwd.join(".brokkr")
            .join("test-artifacts")
            .join(format!("resolve-{name}-{}-{stamp}", std::process::id()))
    }

    fn mk_paths(data_dir: &Path, datasets: HashMap<String, Dataset>) -> ResolvedPaths {
        ResolvedPaths {
            hostname: String::from("test-host"),
            data_dir: data_dir.to_path_buf(),
            scratch_dir: data_dir.join("scratch"),
            output_dir: data_dir.join("tilegen"),
            target_dir: data_dir.join("target"),
            drives: None,
            features: Vec::new(),
            datasets,
        }
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

    #[test]
    fn resolve_default_osc_path_errors_when_multiple_variants_exist() {
        let mut ds = empty_dataset();
        ds.osc.insert(
            String::from("4706"),
            OscEntry {
                file: String::from("b.osc.gz"),
                xxhash: None,
            },
        );
        ds.osc.insert(
            String::from("4705"),
            OscEntry {
                file: String::from("a.osc.gz"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);

        let paths = mk_paths(Path::new("/irrelevant"), datasets);
        let err = resolve_default_osc_path("denmark", &paths, Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple osc entries"));
        assert!(err.contains("4705, 4706"));
    }

    #[test]
    fn resolve_default_osc_path_uses_single_entry() {
        let dir = unique_test_dir("single-osc");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let osc = dir.join("one.osc.gz");
        std::fs::write(&osc, "x").expect("write");

        let mut ds = empty_dataset();
        ds.osc.insert(
            String::from("4705"),
            OscEntry {
                file: String::from("one.osc.gz"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved =
            resolve_default_osc_path("denmark", &paths, Path::new(".")).expect("resolve");
        assert_eq!(resolved, osc);

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_default_pmtiles_path_errors_when_multiple_variants_exist() {
        let mut ds = empty_dataset();
        ds.pmtiles.insert(
            String::from("z"),
            PmtilesEntry {
                file: String::from("z.pmtiles"),
                xxhash: None,
            },
        );
        ds.pmtiles.insert(
            String::from("a"),
            PmtilesEntry {
                file: String::from("a.pmtiles"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);

        let paths = mk_paths(Path::new("/irrelevant"), datasets);
        let err = resolve_default_pmtiles_path("denmark", &paths, Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple pmtiles entries"));
        assert!(err.contains("a, z"));
    }

    #[test]
    fn resolve_bbox_prefers_arg_then_dataset() {
        let mut ds = empty_dataset();
        ds.bbox = Some(String::from("1,2,3,4"));
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let explicit = resolve_bbox(Some("9,9,9,9"), "denmark", &paths).expect("bbox");
        assert_eq!(explicit, "9,9,9,9");

        let from_dataset = resolve_bbox(None, "denmark", &paths).expect("bbox");
        assert_eq!(from_dataset, "1,2,3,4");
    }

    #[test]
    fn snapshot_ref_parses_base_sentinel() {
        assert!(matches!(SnapshotRef::parse("base").unwrap(), SnapshotRef::Base));
    }

    #[test]
    fn snapshot_ref_parses_named_keys() {
        let parsed = SnapshotRef::parse("20260411").unwrap();
        assert!(matches!(parsed, SnapshotRef::Named(ref s) if s == "20260411"));
    }

    #[test]
    fn snapshot_ref_rejects_invalid_chars() {
        let err = SnapshotRef::parse("not a key").unwrap_err().to_string();
        assert!(err.contains("[a-zA-Z0-9_-]+"), "got: {err}");
    }

    #[test]
    fn snapshot_ref_rejects_empty() {
        let err = SnapshotRef::parse("").unwrap_err().to_string();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn snapshot_ref_from_opt_none_is_base() {
        assert!(matches!(SnapshotRef::from_opt(None).unwrap(), SnapshotRef::Base));
    }

    #[test]
    fn snapshot_ref_from_opt_some_base_is_base() {
        assert!(matches!(
            SnapshotRef::from_opt(Some("base")).unwrap(),
            SnapshotRef::Base
        ));
    }

    #[test]
    fn snapshot_ref_from_opt_some_named() {
        let parsed = SnapshotRef::from_opt(Some("unsorted")).unwrap();
        assert!(matches!(parsed, SnapshotRef::Named(ref s) if s == "unsorted"));
    }

    #[test]
    fn snapshot_ref_from_opt_propagates_validation() {
        assert!(SnapshotRef::from_opt(Some("bad key")).is_err());
    }

    #[test]
    fn resolve_snapshot_pbf_path_base_uses_legacy_table() {
        let dir = unique_test_dir("snap-base");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-base.osm.pbf"), "x").expect("write");

        let mut ds = empty_dataset();
        ds.pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "planet-base.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_snapshot_pbf_path(
            "planet",
            &SnapshotRef::Base,
            "indexed",
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(resolved.ends_with("planet-base.osm.pbf"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_snapshot_pbf_path_named_uses_snapshot_table() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-named");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-20260411.osm.pbf"), "x").expect("write");

        let mut snap = Snapshot {
            download_date: Some("2026-04-11".into()),
            seq: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
        };
        snap.pbf.insert(
            "raw".into(),
            PbfEntry {
                file: "planet-20260411.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_snapshot_pbf_path(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            "raw",
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(resolved.ends_with("planet-20260411.osm.pbf"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_snapshot_pbf_path_missing_variant_emits_friendly_error() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-missing-variant");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-20260411.osm.pbf"), "x").expect("write");

        // Snapshot has only `raw` - user asks for `indexed`. The error should
        // name `raw` as available and suggest both --variant raw and the
        // re-download path.
        let mut snap = Snapshot {
            download_date: Some("2026-04-11".into()),
            seq: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
        };
        snap.pbf.insert(
            "raw".into(),
            PbfEntry {
                file: "planet-20260411.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let err = resolve_snapshot_pbf_path(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            "indexed",
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();

        // Names the missing variant and the available one.
        assert!(
            err.contains("snapshot.20260411.pbf variant 'indexed'"),
            "got: {err}"
        );
        assert!(
            err.contains("available variants on this snapshot: raw"),
            "got: {err}"
        );
        // Names the workaround flag with the actual available variant.
        assert!(err.contains("--variant raw"), "got: {err}");
        // Names the re-download path as the proper fix.
        assert!(
            err.contains("brokkr download planet --as-snapshot 20260411"),
            "got: {err}"
        );

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_snapshot_pbf_path_errors_on_unknown_named_key() {
        let mut ds = empty_dataset();
        ds.pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "planet-base.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let err = resolve_snapshot_pbf_path(
            "planet",
            &SnapshotRef::Named("missing-snap".into()),
            "indexed",
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no snapshot 'missing-snap'"), "got: {err}");
        assert!(err.contains("base"), "available list should mention base: {err}");
    }

    #[test]
    fn resolve_single_osc_returns_explicit_seq() {
        let dir = unique_test_dir("single-osc-explicit");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-4914.osc.gz"), "x").expect("write");

        let mut ds = empty_dataset();
        ds.osc.insert(
            String::from("4914"),
            OscEntry {
                file: String::from("planet-4914.osc.gz"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("planet"), ds);
        let paths = mk_paths(&dir, datasets);

        let (path, seq) = resolve_single_osc(
            "planet",
            &SnapshotRef::Base,
            Some("4914"),
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(path.ends_with("planet-4914.osc.gz"));
        assert_eq!(seq, "4914");

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_single_osc_auto_selects_when_one_configured() {
        let dir = unique_test_dir("single-osc-auto");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("denmark-4705.osc.gz"), "x").expect("write");

        let mut ds = empty_dataset();
        ds.osc.insert(
            String::from("4705"),
            OscEntry {
                file: String::from("denmark-4705.osc.gz"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(&dir, datasets);

        let (path, seq) = resolve_single_osc(
            "denmark",
            &SnapshotRef::Base,
            None,
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(path.ends_with("denmark-4705.osc.gz"));
        assert_eq!(seq, "4705");

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_single_osc_errors_when_multiple_and_no_seq() {
        let mut ds = empty_dataset();
        for n in [4913u64, 4914, 4915] {
            ds.osc.insert(
                n.to_string(),
                OscEntry {
                    file: format!("planet-{n}.osc.gz"),
                    xxhash: None,
                },
            );
        }
        let mut datasets = HashMap::new();
        datasets.insert(String::from("planet"), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let err = resolve_single_osc(
            "planet",
            &SnapshotRef::Base,
            None,
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("multiple osc entries"), "got: {err}");
        assert!(err.contains("4913, 4914, 4915"), "got: {err}");
    }

    #[test]
    fn resolve_single_osc_named_snapshot_uses_snapshot_table() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-osc-named");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-20260411-seq4969.osc.gz"), "x").expect("write");

        let mut snap = Snapshot {
            download_date: Some("2026-04-11".into()),
            seq: Some(4969),
            pbf: HashMap::new(),
            osc: HashMap::new(),
        };
        snap.osc.insert(
            "4969".into(),
            OscEntry {
                file: "planet-20260411-seq4969.osc.gz".into(),
                xxhash: None,
            },
        );
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let (path, seq) = resolve_single_osc(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            Some("4969"),
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(path.ends_with("planet-20260411-seq4969.osc.gz"));
        assert_eq!(seq, "4969");

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_osc_range_named_snapshot_uses_snapshot_table() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-osc-range");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for n in 4969..=4971 {
            std::fs::write(dir.join(format!("planet-20260411-seq{n}.osc.gz")), "x")
                .expect("write");
        }

        let mut snap = Snapshot {
            download_date: Some("2026-04-11".into()),
            seq: Some(4969),
            pbf: HashMap::new(),
            osc: HashMap::new(),
        };
        for n in 4969..=4971u64 {
            snap.osc.insert(
                n.to_string(),
                OscEntry {
                    file: format!("planet-20260411-seq{n}.osc.gz"),
                    xxhash: None,
                },
            );
        }
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_osc_range(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            "4969..4971",
            &paths,
            Path::new("."),
        )
        .expect("range");
        assert_eq!(resolved.len(), 3);
        assert!(resolved[0].ends_with("planet-20260411-seq4969.osc.gz"));
        assert!(resolved[2].ends_with("planet-20260411-seq4971.osc.gz"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_single_osc_named_snapshot_unknown_key_errors() {
        let mut ds = empty_dataset();
        ds.osc.insert(
            "4913".into(),
            OscEntry {
                file: "planet-4913.osc.gz".into(),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let err = resolve_single_osc(
            "planet",
            &SnapshotRef::Named("missing".into()),
            None,
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no snapshot 'missing'"), "got: {err}");
    }

    #[test]
    fn resolve_osc_range_returns_paths_in_seq_order() {
        let dir = unique_test_dir("osc-range-ok");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for n in 4914..=4916 {
            std::fs::write(dir.join(format!("planet-{n}.osc.gz")), "x").expect("write");
        }

        let mut ds = empty_dataset();
        for n in 4914..=4916u64 {
            ds.osc.insert(
                n.to_string(),
                OscEntry {
                    file: format!("planet-{n}.osc.gz"),
                    xxhash: None,
                },
            );
        }
        let mut datasets = HashMap::new();
        datasets.insert(String::from("planet"), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_osc_range(
            "planet",
            &SnapshotRef::Base,
            "4914..4916",
            &paths,
            Path::new("."),
        )
        .expect("range");
        assert_eq!(resolved.len(), 3);
        assert!(resolved[0].ends_with("planet-4914.osc.gz"));
        assert!(resolved[1].ends_with("planet-4915.osc.gz"));
        assert!(resolved[2].ends_with("planet-4916.osc.gz"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_osc_range_fails_fast_on_missing_seq() {
        let dir = unique_test_dir("osc-range-missing");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-4914.osc.gz"), "x").expect("write");
        std::fs::write(dir.join("planet-4916.osc.gz"), "x").expect("write");

        let mut ds = empty_dataset();
        for n in [4914u64, 4916] {
            ds.osc.insert(
                n.to_string(),
                OscEntry {
                    file: format!("planet-{n}.osc.gz"),
                    xxhash: None,
                },
            );
        }
        let mut datasets = HashMap::new();
        datasets.insert(String::from("planet"), ds);
        let paths = mk_paths(&dir, datasets);

        let err = resolve_osc_range(
            "planet",
            &SnapshotRef::Base,
            "4914..4916",
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("missing osc.4915"), "got: {err}");

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_nidhogg_data_dir_requires_configured_data_dir() {
        let mut ds = empty_dataset();
        ds.pbf.insert(
            String::from("raw"),
            PbfEntry {
                file: String::from("raw.osm.pbf"),
                xxhash: None,
                seq: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(Path::new("/data-root"), datasets);

        let err = resolve_nidhogg_data_dir("denmark", &paths)
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no data_dir configured"));
    }
}
