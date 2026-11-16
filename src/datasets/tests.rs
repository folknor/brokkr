//! Dataset resolution regression tests.

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

    use crate::config::{Dataset, PbfEntry, ResolvedPaths};
    use crate::elivagar::archive_store::{ArchiveKey, ArchiveStore};

    use crate::datasets::resolve::*;

    fn unique_test_dir(name: &str) -> PathBuf {
        crate::test_scratch::scratch("resolve-parts", name)
    }

    fn mk_paths(data_dir: &Path, datasets: HashMap<String, Dataset>) -> ResolvedPaths {
        ResolvedPaths {
            hostname: String::from("test-host"),
            data_dir: data_dir.to_path_buf(),
            scratch_dir: data_dir.join("scratch"),
            output_dir: data_dir.join("tilegen"),
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
            getid_ids: None,
            getparents_ids: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
            polygon: HashMap::new(),
            snapshot: HashMap::new(),
            expect: crate::config::DatasetExpect::default(),
            path: None,
            xxh128: None,
        }
    }

    #[test]
    fn snapshot_ref_parses_base_sentinel() {
        assert!(matches!(
            SnapshotRef::parse("base").unwrap(),
            SnapshotRef::Base
        ));
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
    fn snapshot_ref_cli_parser_rejects_invalid_key() {
        assert!("bad key".parse::<SnapshotRef>().is_err());
    }

    #[test]
    fn resolve_snapshot_pbf_path_base_uses_legacy_table() {
        let dir = unique_test_dir("snap-base");
        std::fs::write(dir.join("planet-base.osm.pbf"), "x").expect("write");

        let mut ds = empty_dataset();
        ds.pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "planet-base.osm.pbf".into(),
                ..Default::default()
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_snapshot_pbf_entry(
            "planet",
            &SnapshotRef::Base,
            "indexed",
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(resolved.path.ends_with("planet-base.osm.pbf"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_snapshot_pbf_path_named_uses_snapshot_table() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-named");
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
                ..Default::default()
            },
        );
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_snapshot_pbf_entry(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            "raw",
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(resolved.path.ends_with("planet-20260411.osm.pbf"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_snapshot_pbf_path_missing_variant_emits_friendly_error() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-missing-variant");
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
                ..Default::default()
            },
        );
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let err = resolve_snapshot_pbf_entry(
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
            err.contains("pbfhogg-datasets -- download planet --as-snapshot 20260411"),
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
                ..Default::default()
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let err = resolve_snapshot_pbf_entry(
            "planet",
            &SnapshotRef::Named("missing-snap".into()),
            "indexed",
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no snapshot 'missing-snap'"), "got: {err}");
        assert!(
            err.contains("base"),
            "available list should mention base: {err}"
        );
    }

    #[test]
    fn archive_matcher_accepts_only_constructed_names() {
        let store = ArchiveStore::new(Path::new("/out"));
        let key = ArchiveKey::new("north-america", "raw", "abc1234").unwrap();
        let name = store.path_for(&key);
        let file = name.file_name().unwrap().to_str().unwrap();
        assert!(ArchiveStore::matches_name(file, "north-america", "raw"));
        // A hyphenated dataset name is never parsed back, only constructed, so
        // a split that constructs the same name is the same group by
        // definition - and prunes it identically. Not a bleed.
        assert!(ArchiveStore::matches_name(file, "north", "america-raw"));
        // Not an archive at all, and the pre-rename `<dataset>-<commit>` form.
        assert!(!ArchiveStore::matches_name(
            "ocean-tiles.pmtiles",
            "ocean",
            "raw"
        ));
        assert!(!ArchiveStore::matches_name(
            "denmark-abc1234.pmtiles",
            "denmark",
            "raw"
        ));
    }

    #[test]
    fn archive_matcher_does_not_let_a_variant_claim_its_dash_extension() {
        // The bug this closes: `denmark-raw-` prefixes every `denmark-raw-fast-*`
        // file, so `raw`'s keep-N window would evict `raw-fast`'s archives.
        assert!(ArchiveStore::matches_name(
            "denmark-raw-fast-abc1234.pmtiles",
            "denmark",
            "raw-fast"
        ));
        assert!(!ArchiveStore::matches_name(
            "denmark-raw-fast-abc1234.pmtiles",
            "denmark",
            "raw"
        ));
    }
}
