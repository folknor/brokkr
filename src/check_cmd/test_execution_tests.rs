#[cfg(test)]
mod serial_shim_tests {
    use super::serial_shim_fallback;

    #[test]
    fn forwarded_target_and_config_force_first_failure_mode() {
        let root = std::path::Path::new("/nonexistent");
        for arg in ["--target", "--target=other", "--config", "--config=target.x.runner=tool"] {
            let reason = serial_shim_fallback(root, &[arg.into()], &[], &Default::default())
                .expect("forwarded override must disable shim");
            assert!(reason.contains("forwarded"));
        }
    }

    #[test]
    fn explicit_target_env_forces_first_failure_mode() {
        let reason = serial_shim_fallback(
            std::path::Path::new("/nonexistent"),
            &[],
            &[("CARGO_BUILD_TARGET".into(), "other".into())],
            &Default::default(),
        )
        .expect("target env must disable shim");
        assert!(reason.contains("CARGO_BUILD_TARGET"));
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::useless_vec
    )]
    use super::*;

    #[test]
    fn decide_active_sweeps_legacy_default_when_nothing_configured() {
        let sweeps = decide_active_sweeps(&[], None, None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        // The legacy-fallback label tracks the cargo flag so callers
        // that need to recognize this branch don't have to compare
        // feature-arg vectors. Don't change without updating
        // `brokkr test` (it relied on this distinction pre-fix).
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[0].features.configured_key(), vec!["--all-features"]);
        assert!(sweeps[0].build_packages.is_empty());
        assert!(sweeps[0].filters.libtest_args().is_empty());
    }

    #[test]
    fn decide_active_sweeps_cli_features_create_ad_hoc() {
        // --features commands → single ad-hoc sweep, taking no `[[check]]`
        // entry. (It still inherits profile run shaping when a `[test]`
        // section exists - see the ad_hoc_inherits_* tests below.)
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["pbfhogg-cli".into()],
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            None,
            None,
            &["commands".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].features.configured_key(), vec!["--features", "commands"]);
        // The invocation named them: never projected onto a `-p` selection.
        assert!(sweeps[0].features.invocation_explicit);
        // No build_packages on ad-hoc - the user is spot-checking.
        assert!(sweeps[0].build_packages.is_empty());
    }

    #[test]
    fn decide_active_sweeps_no_default_features_alone_is_ad_hoc() {
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(&entries, None, None, &[], true).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].features.configured_key(), vec!["--no-default-features"]);
    }

    #[test]
    fn decide_active_sweeps_check_entries_no_profile() {
        let entries = vec![
            CheckEntry {
                name: "all".into(),
                features: vec!["a".into(), "b".into()],
                no_default_features: false,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
            CheckEntry {
                name: "consumer".into(),
                features: vec!["commands".into()],
                no_default_features: true,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
        ];
        let sweeps = decide_active_sweeps(&entries, None, None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].features.configured_key(), vec!["--features", "a,b"]);
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert!(sweeps[0].filters.libtest_args().is_empty());
        assert_eq!(sweeps[1].label, "consumer");
    }

    #[test]
    fn decide_active_sweeps_default_profile_when_no_explicit() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["tier2::"]
include_ignored = false
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["pbfhogg-cli".into()],
            ..Default::default()
        }];
        let sweeps =
            decide_active_sweeps(&entries, Some(&test_cfg), None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].filters.libtest_args(), vec!["--skip", "tier2::"]);
    }

    #[test]
    fn decide_active_sweeps_explicit_profile_overrides_default() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]

[profiles.full]
sweeps = ["all"]
include_ignored = true
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps =
            decide_active_sweeps(&entries, Some(&test_cfg), Some("full"), &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert!(sweeps[0].filters.libtest_args().contains(&"--include-ignored".into()));
    }

    /// The regression this whole split exists for: `check -p <pkg>
    /// --features x` used to issue a bare `cargo test` with no `--skip` at
    /// all, because the ad-hoc branch dropped the profile's run shaping
    /// along with its sweep selection. It then failed on tests the default
    /// profile had always excluded (nautilus: 8 `serial_tests::` + 2
    /// `logging::macros::`, deterministic), reporting a red that reads as a
    /// code failure.
    #[test]
    fn ad_hoc_inherits_default_profile_run_shaping() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["serial_tests::", "logging::macros::"]
test_threads = 0
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["ignored".into()],
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            Some(&test_cfg),
            None,
            &["defi".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps.len(), 1);
        // Sweep selection IS overridden: the ad-hoc feature set, and no
        // `[[check]]` entry (hence no build_packages).
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].features.configured_key(), vec!["--features", "defi"]);
        assert!(sweeps[0].build_packages.is_empty());
        // Run shaping is NOT: the profile's filters and thread policy ride along.
        assert_eq!(
            sweeps[0].filters.libtest_args(),
            vec!["--skip", "serial_tests::", "--skip", "logging::macros::"]
        );
        assert_eq!(
            sweeps[0].lane_exec,
            crate::profile::LaneExec::Serial { threads: crate::profile::Threads::CargoDefault }
        );
    }

    /// The quiet half of the same defect: entry-level `env` is a
    /// build-affecting invariant (the `HIGH_PRECISION` class), and an ad-hoc
    /// run dropping it went green having compiled a different width than the
    /// gate. `brokkr clippy`'s ad-hoc path already unioned it; `check` now
    /// takes the same union from the same function, so the two commands can't
    /// resolve different widths from one config.
    #[test]
    fn ad_hoc_inherits_unioned_check_entry_env() {
        let entries = vec![
            CheckEntry {
                name: "default".into(),
                env: [("HIGH_PRECISION".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            CheckEntry {
                name: "cli".into(),
                env: [("HIGH_PRECISION".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        ];
        let sweeps =
            decide_active_sweeps(&entries, None, None, &["defi".to_owned()], false).unwrap();
        assert_eq!(
            sweeps[0].env.get("HIGH_PRECISION").map(String::as_str),
            Some("1")
        );
    }

    /// Disagreement is a hard error naming the key, never a coin flip - the
    /// property that makes the union safe to take implicitly.
    #[test]
    fn ad_hoc_env_conflict_across_entries_errors() {
        let entries = vec![
            CheckEntry {
                name: "high".into(),
                env: [("HIGH_PRECISION".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            CheckEntry {
                name: "std".into(),
                env: [("HIGH_PRECISION".to_owned(), "0".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        ];
        let err = decide_active_sweeps(&entries, None, None, &["defi".to_owned()], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("HIGH_PRECISION"), "got: {err}");
        // The remedy must name flags `check` actually has - not clippy's
        // `--env` / `--sweep`.
        assert!(err.contains("run a profile"), "got: {err}");
    }

    /// Entry env overlays profile env on a key collision, matching
    /// `build_resolved_sweep`'s precedence for non-ad-hoc sweeps.
    #[test]
    fn ad_hoc_entry_env_overlays_profile_env() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
env = { HIGH_PRECISION = "0", ONLY_PROFILE = "p" }
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            env: [("HIGH_PRECISION".to_owned(), "1".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            Some(&test_cfg),
            None,
            &["defi".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(
            sweeps[0].env.get("HIGH_PRECISION").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            sweeps[0].env.get("ONLY_PROFILE").map(String::as_str),
            Some("p")
        );
    }

    /// `test_exclude_packages` stays out of the union: a selection
    /// workaround, not an invariant, so inheriting it would narrow what a
    /// scoped run tests while the rest of the run claims to be wider.
    #[test]
    fn ad_hoc_does_not_inherit_test_exclude_packages() {
        let entries = vec![CheckEntry {
            name: "default".into(),
            test_exclude_packages: vec!["nautilus-pyo3".into()],
            ..Default::default()
        }];
        let sweeps =
            decide_active_sweeps(&entries, None, None, &["defi".to_owned()], false).unwrap();
        assert!(sweeps[0].test_exclude_packages.is_empty());
    }

    /// A `lanes` profile shapes nothing of its own, so an ad-hoc run under
    /// one inherits no filters rather than silently borrowing one lane's.
    #[test]
    fn ad_hoc_under_lanes_profile_inherits_nothing() {
        let toml_text = r#"
default_profile = "gate"

[profiles.gate]
lanes = ["tier1"]

[profiles.tier1]
sweeps = ["all"]
skip = ["serial_tests::"]
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            Some(&test_cfg),
            None,
            &["defi".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps.len(), 1);
        assert!(sweeps[0].filters.libtest_args().is_empty());
    }

    /// `--profile` still selects which shaping an ad-hoc run inherits.
    #[test]
    fn ad_hoc_honours_explicit_profile_for_shaping() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["tier1_only::"]

[profiles.edit]
sweeps = ["all"]
skip = ["edit_only::"]
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            Some(&test_cfg),
            Some("edit"),
            &["defi".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps[0].filters.libtest_args(), vec!["--skip", "edit_only::"]);
    }

    /// No `[test]` section at all: nothing to inherit, and the ad-hoc sweep
    /// stays exactly as it was.
    #[test]
    fn ad_hoc_without_test_config_is_unshaped() {
        let sweeps =
            decide_active_sweeps(&[], None, None, &["defi".to_owned()], false).unwrap();
        assert!(sweeps[0].filters.libtest_args().is_empty());
        assert_eq!(sweeps[0].lane_exec, crate::profile::LaneExec::default());
    }

    #[test]
    fn decide_active_sweeps_profile_without_test_section_errors() {
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let err = decide_active_sweeps(&entries, None, Some("tier1"), &[], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--profile tier1"), "got: {err}");
    }

    #[test]
    fn sweep_tag_formats() {
        assert_eq!(sweep_tag(&[], 1), None);
        assert_eq!(sweep_tag(&["consumer".into()], 2), Some("[consumer]".into()));
        // Hit in both of two active sweeps -> `[both]` is honest.
        assert_eq!(
            sweep_tag(&["all-features".into(), "consumer".into()], 2),
            Some("[both]".into())
        );
    }

    #[test]
    fn sweep_tag_avoids_both_when_more_than_two_sweeps_active() {
        // B5: `[both]` with three active sweeps would hide which two
        // actually triggered the hit. Fall through to the explicit
        // joined form so the reader sees the real pair.
        assert_eq!(
            sweep_tag(&["a".into(), "b".into()], 3),
            Some("[a+b]".into())
        );
    }

    fn run(label: &str, selected: Option<&[&str]>) -> SweepResult {
        SweepResult {
            label: label.into(),
            shape: String::new(),
            command: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            success: true,
            selected: selected.map(|s| s.iter().map(|p| (*p).to_owned()).collect()),
            diagnostics: Vec::new(),
            suppressed_manifest_blocks: Vec::new(),
            listed_manifest_errors: Vec::new(),
        }
    }

    /// `r` with its manifest lints attributed, as the clippy phase does.
    fn attributed(mut r: SweepResult, info: &crate::build::ProjectInfo) -> SweepResult {
        let owners = crate::diagnostic::OwnerIndex::from_metadata_at(
            &info.workspace_root,
            &info.workspace_root,
            &info.workspace_members,
            &info.member_manifests,
        );
        r.suppressed_manifest_blocks =
            cargo_json::manifest_lints_by_block(&rustc_text::rustc_blocks(&r.stderr))
                .into_iter()
                .filter_map(|(block, diagnostic)| diagnostic.is_warning().then_some(block))
                .collect();
        for (block, diagnostic) in attribute_manifest_lints(&r, info, &owners) {
            if diagnostic.is_error() {
                r.listed_manifest_errors.push((block, diagnostic.clone()));
            }
            r.diagnostics.push(diagnostic);
        }
        r
    }

    /// A clippy run that failed in a build script, as cargo reports it: stdout
    /// is JSON records only, stderr carries a manifest lint, its tally, a
    /// build-script error, cargo's status line and a build script's own
    /// warnings. The report lists the manifest lint as a diagnostic, keeps the
    /// error block minus the build script's directives, drops every JSON
    /// record, and counts only the warnings it really withheld.
    #[test]
    fn failed_build_script_run_reports_manifest_lints_and_the_error() {
        let info = two_member_info();
        let mut r = run("default", None);
        r.success = false;
        r.stdout = [
            r#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#regex@1.13.1","filenames":["a"]}"#,
            r#"{"reason":"build-script-executed","package_id":"registry+https://github.com/rust-lang/crates.io-index#libm@0.2.15"}"#,
            r#"{"reason":"build-finished","success":false}"#,
        ]
        .join("\n");
        r.stderr = "\
warning: unused dependency `anyhow`
  --> core/Cargo.toml:25:1
   |
25 | anyhow = { workspace = true }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: `cargo::unused_dependencies` is set to `warn` by default
help: consider removing the dependency on `anyhow`
warning: `core` (manifest) generated 1 warning
   Compiling fontconfig-sys v6.0.1
error: failed to run custom build command for `fontconfig-sys v6.0.1`

Caused by:
  process didn't exit successfully: `build_script_build` (exit status: 101)
  --- stdout
  cargo:rerun-if-env-changed=PKG_CONFIG
  cargo:rerun-if-env-changed=PKG_CONFIG_PATH
  cargo:warning=probing fontconfig

  --- stderr
  The system library `fontconfig` required by crate `fontconfig-sys` was not found.
warning: build failed, waiting for other jobs to finish...
warning: desktop: Exported component 'A' doesn't inherit Window
warning: desktop: Exported component 'B' doesn't inherit Window
"
        .to_owned();
        let r = attributed(r, &info);

        let root = crate::test_scratch::scratch("check-output", "failed_build_script_run");
        let out = format_clippy_multi("cargo clippy", &[r], Some(&info), &root, 1, &|_| true);

        assert!(out.starts_with("cargo clippy: 1 error\n"), "{out}");
        assert!(
            out.contains("error[cargo::unused_dependencies] core/Cargo.toml:25:1 unused dependency `anyhow`"),
            "{out}"
        );
        assert!(out.contains("error: failed to run custom build command"), "{out}");
        assert!(out.contains("`fontconfig` required by crate"), "{out}");
        assert!(!out.contains("\"reason\""), "{out}");
        // The listed lint is not repeated from the stream: its excerpt's
        // column-zero line number (`25 | ...`) is part of the diagnostic, not
        // foreign text that would force the block to print.
        assert!(!out.contains("consider removing"), "{out}");
        // A build script's directives go; its messages stay.
        assert!(!out.contains("rerun-if-env-changed"), "{out}");
        assert!(out.contains("cargo:warning=probing fontconfig"), "{out}");
        assert!(out.contains("(2 build-script directives not shown)"), "{out}");
        // Withheld: the two build-script warnings. Not the listed manifest
        // lint, not the tally, not cargo's `build failed` status.
        assert!(out.contains("2 warnings not shown"), "{out}");
        assert!(!out.contains("Exported component"), "{out}");
    }

    /// Under `--keep-going` one run can carry a rustc error the list shows and
    /// a build-script panic it cannot: the panic prints too, and the listed
    /// error is not repeated. A run whose only stderr error is cargo's
    /// `could not compile` summary prints nothing beyond the list.
    #[test]
    fn a_build_script_panic_beside_a_listed_error_is_shown() {
        let error = r#"{"reason":"compiler-message","package_id":"id-core","message":{"level":"error","code":{"code":"E0425"},"message":"cannot find value `x`","spans":[{"file_name":"core/src/a.rs","line_start":1,"column_start":1,"line_end":1,"column_end":2,"is_primary":true}],"children":[],"rendered":"RENDERED-E0425"}}"#;
        let root = crate::test_scratch::scratch("check-output", "a_build_script_panic_beside");

        let mut r = run("default", None);
        r.success = false;
        r.stdout = error.to_owned();
        r.diagnostics = cargo_json::parse_cargo_diagnostics(&r.stdout);
        r.stderr = "\
error: failed to run custom build command for `x-sys v1.0.0`

Caused by:
  boom
error: could not compile `core` (lib) due to 1 previous error
"
        .to_owned();
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, 1, &|_| true);
        assert!(out.contains("error[E0425] core/src/a.rs:1:1 cannot find value `x`"), "{out}");
        assert!(out.contains("failed to run custom build command for `x-sys"), "{out}");
        assert!(!out.contains("RENDERED-E0425"), "{out}");

        let mut r = run("default", None);
        r.success = false;
        r.stdout = error.to_owned();
        r.diagnostics = cargo_json::parse_cargo_diagnostics(&r.stdout);
        r.stderr = "error: could not compile `core` (lib) due to 1 previous error\n".to_owned();
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, 1, &|_| true);
        assert!(!out.contains("could not compile"), "{out}");
    }

    #[test]
    fn cargo_status_lines_are_not_warnings() {
        for status in [
            "warning: `a` (lib) generated 3 warnings",
            "warning: `a` (lib) generated 1 warning",
            "warning: `a` (lib) generated 3 warnings (1 duplicate)",
            "warning: `a` (lib test) generated 2 warnings (run `cargo clippy --fix --lib -p a --tests` to apply 2 suggestions)",
            "warning: `a` (manifest) generated 2 warnings",
            "warning: build failed, waiting for other jobs to finish...",
        ] {
            assert!(is_cargo_status(status), "{status}");
        }
        for warning in [
            "warning: unused dependency `a`",
            "warning: desktop: the build (x) generated output",
        ] {
            assert!(!is_cargo_status(warning), "{warning}");
        }

        fn is_cargo_status(line: &str) -> bool {
            crate::rustc_text::classify_line(line) == crate::rustc_text::BlockKind::CargoStatus
        }
    }

    /// An error an allow filtered from the list is still why cargo failed: the
    /// report renders its typed fields rather than cargo's rendered duplicate.
    #[test]
    fn a_filtered_error_still_explains_the_failure() {
        let mut r = run("default", None);
        r.success = false;
        r.stdout = [
            r#"{"reason":"compiler-message","package_id":"p","message":{"level":"error","code":{"code":"E0425"},"message":"cannot find value `x`","spans":[],"children":[],"rendered":"error[E0425]: cannot find value `x` in this scope\n --> src/a.rs:1:1\n"}}"#,
            r#"{"reason":"compiler-artifact","package_id":"q","filenames":["a"]}"#,
        ]
        .join("\n");
        r.diagnostics = cargo_json::parse_cargo_diagnostics(&r.stdout);
        let root = crate::test_scratch::scratch("check-output", "a_filtered_error");
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, 1, &|_| false);
        assert!(out.contains("error[E0425] cannot find value `x`"), "{out}");
        assert!(!out.contains("compiler-artifact"), "{out}");
    }

    /// Text a warning header swallowed only because no header followed it is
    /// printed, not withheld with the warning.
    #[test]
    fn a_warning_block_with_foreign_text_is_printed() {
        let mut r = run("default", None);
        r.success = false;
        r.stderr = "\
warning: relaying tool output
TOOL CRASHED: out of disk
error: could not compile `a`
warning: plain one-liner
"
        .to_owned();
        let root = crate::test_scratch::scratch("check-output", "a_warning_block_with_foreign_text");
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, 1, &|_| true);
        assert!(out.contains("TOOL CRASHED: out of disk"), "{out}");
        assert!(out.contains("error: could not compile"), "{out}");
        assert!(!out.contains("plain one-liner"), "{out}");
        assert!(out.contains("1 warning not shown"), "{out}");
    }

    /// A denied cargo lint an allow filtered is cargo's reason for failing: its
    /// error block prints. And a tally that swallowed foreign text prints
    /// whole, like any other warning block.
    #[test]
    fn a_filtered_manifest_error_and_a_swallowing_tally_both_print() {
        let mut r = run("default", None);
        r.success = false;
        r.stderr = "\
error: unused dependency `anyhow`
  --> a/Cargo.toml:25:1
   = note: `cargo::unused_dependencies` is set to `deny` in `[lints]`
warning: `a` (manifest) generated 1 warning
TOOL CRASHED: out of disk
warning: `b` (manifest) generated 1 warning
"
        .to_owned();
        let r = attributed(r, &two_member_info());
        let root = crate::test_scratch::scratch("check-output", "a_filtered_manifest_error");
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, 1, &|_| false);
        assert!(out.contains("error: unused dependency `anyhow`"), "{out}");
        assert!(out.contains("TOOL CRASHED: out of disk"), "{out}");
        // The self-contained tally is dropped, and never counted.
        assert!(!out.contains("`b` (manifest)"), "{out}");
        assert!(!out.contains("not shown"), "{out}");
    }

    /// A manifest lint is attributed by the `Cargo.toml` it names, resolved
    /// against the workspace root: a member's follows the run's selection,
    /// the root manifest's is every run's, and any other manifest - outside
    /// the tree or inside it but not a member - is a dependency's, whose
    /// warning does not fail the gate.
    #[test]
    fn manifest_lints_are_attributed_by_manifest() {
        let info = two_member_info();
        let lint = |file: &str| {
            format!("warning: unused dependency `x`\n  --> {file}:1:1\n   = note: `cargo::unused_dependencies` is set to `warn` by default\n")
        };
        let stderr = ["core/Cargo.toml", "./daemon/../core/Cargo.toml", "Cargo.toml", "../sibling/Cargo.toml", "vendor/x/Cargo.toml"]
            .map(lint)
            .concat();
        let owners = |selected: Option<&[&str]>| -> Vec<(String, crate::diagnostic::Owner)> {
            let mut r = run("r", selected);
            r.stderr.clone_from(&stderr);
            let owner_index = crate::diagnostic::OwnerIndex::from_metadata_at(
                &info.workspace_root,
                &info.workspace_root,
                &info.workspace_members,
                &info.member_manifests,
            );
            attribute_manifest_lints(&r, &info, &owner_index)
                .into_iter()
                .map(|(_, e)| {
                    (
                        e.site
                            .as_ref()
                            .map_or_else(String::new, |site| site.file.display().to_string()),
                        e.owner,
                    )
                })
                .collect()
        };
        assert_eq!(
            owners(None),
            [
                ("core/Cargo.toml".to_owned(), crate::diagnostic::Owner::Member("id-core".into())),
                ("./daemon/../core/Cargo.toml".to_owned(), crate::diagnostic::Owner::Member("id-core".into())),
                ("Cargo.toml".to_owned(), crate::diagnostic::Owner::WorkspaceRoot),
                (
                    "../sibling/Cargo.toml".to_owned(),
                    crate::diagnostic::Owner::NonMember(crate::diagnostic::NonMemberOwner::Path(
                        "/sibling/Cargo.toml".into(),
                    )),
                ),
                (
                    "vendor/x/Cargo.toml".to_owned(),
                    crate::diagnostic::Owner::NonMember(crate::diagnostic::NonMemberOwner::Path(
                        "/ws/vendor/x/Cargo.toml".into(),
                    )),
                ),
            ]
        );
        // `-p daemon`: core's manifest is not this run's; the root's still is.
        let narrowed: Vec<String> = owners(Some(&["daemon"])).into_iter().map(|(f, _)| f).collect();
        assert_eq!(narrowed, ["Cargo.toml", "../sibling/Cargo.toml", "vendor/x/Cargo.toml"]);

        let dependency = |file: &str| {
            let r = attributed(
                SweepResult {
                    stderr: lint(file),
                    ..run("r", None)
                },
                &info,
            );
            r.diagnostics[0].is_dependency_warning()
        };
        assert!(!dependency("core/Cargo.toml"));
        assert!(!dependency("Cargo.toml"));
        assert!(dependency("../sibling/Cargo.toml"));
        assert!(dependency("vendor/x/Cargo.toml"));
        let external = attributed(
            SweepResult {
                stderr: lint("../sibling/Cargo.toml"),
                ..run("r", None)
            },
            &info,
        );
        assert!(external.diagnostics[0].package_id.is_none());
    }

    /// Two members, both default: `core` and `daemon`.
    fn two_member_info() -> crate::build::ProjectInfo {
        crate::build::ProjectInfo {
            target_dir: std::path::PathBuf::from("target"),
            bare_selection_is_whole_workspace: true,
            workspace_members: [
                ("id-core".to_owned(), "core".to_owned()),
                ("id-daemon".to_owned(), "daemon".to_owned()),
            ]
            .into(),
            default_members: ["id-core".to_owned(), "id-daemon".to_owned()].into(),
            workspace_root: std::path::PathBuf::from("/ws"),
            member_manifests: [
                (std::path::PathBuf::from("/ws/core/Cargo.toml"), "id-core".to_owned()),
                (std::path::PathBuf::from("/ws/daemon/Cargo.toml"), "id-daemon".to_owned()),
            ]
            .into(),
        }
    }

    /// Shape of the reporting run: a workspace sweep plus a sweep of one
    /// package. A daemon diagnostic both report is the code's, not a shape's.
    #[test]
    fn no_tag_when_every_covering_sweep_reported_it() {
        let info = two_member_info();
        let results = [run("default", None), run("daemon-shape", Some(&["daemon"]))];
        let both = ["default".to_owned(), "daemon-shape".to_owned()];
        assert_eq!(
            coverage_tag(&both, Some("id-daemon"), &results, Some(&info), 2),
            None
        );
        // A core diagnostic from the workspace sweep alone: the daemon sweep
        // never selected core, so nothing covering it stayed silent.
        let default_only = ["default".to_owned()];
        assert_eq!(
            coverage_tag(&default_only, Some("id-core"), &results, Some(&info), 2),
            None
        );
    }

    /// The case the tag exists for: a shape-specific diagnostic, here one the
    /// package's own sweep found and the workspace sweep did not.
    #[test]
    fn tag_when_a_covering_sweep_did_not_report_it() {
        let info = two_member_info();
        let results = [run("default", None), run("daemon-shape", Some(&["daemon"]))];
        let shape_only = ["daemon-shape".to_owned()];
        assert_eq!(
            coverage_tag(&shape_only, Some("id-daemon"), &results, Some(&info), 2),
            Some("[daemon-shape]".into())
        );
    }

    /// Without a package id, every run counts as covering.
    #[test]
    fn unknown_package_falls_back_to_every_run() {
        let info = two_member_info();
        let results = [run("default", None), run("daemon-shape", Some(&["daemon"]))];
        let default_only = ["default".to_owned()];
        assert_eq!(
            coverage_tag(&default_only, None, &results, Some(&info), 2),
            Some("[default]".into())
        );
    }

    #[test]
    fn merge_clippy_dedups_and_combines_sweeps() {
        let stderr_a = "\
warning: x [unused_variables]
 --> src/foo.rs:1:1
  |
warning: y [needless_pass_by_value]
 --> src/bar.rs:2:1
  |
";
        let stderr_b = "\
warning: x [unused_variables]
 --> src/foo.rs:1:1
  |
warning: z [too_many_lines]
 --> src/baz.rs:3:1
  |
";
        let parses = vec![
            ("all-features".to_owned(), cargo_filter::parse_clippy(stderr_a)),
            ("consumer".to_owned(), cargo_filter::parse_clippy(stderr_b)),
        ];
        let merged = merge_clippy(&parses);
        // 3 unique diagnostics: foo (both), bar (a), baz (b).
        assert_eq!(merged.len(), 3);
        let foo = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("foo.rs"))
            .unwrap();
        assert_eq!(foo.sweeps, vec!["all-features", "consumer"]);
        let bar = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("bar.rs"))
            .unwrap();
        assert_eq!(bar.sweeps, vec!["all-features"]);
        let baz = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("baz.rs"))
            .unwrap();
        assert_eq!(baz.sweeps, vec!["consumer"]);
    }

    fn json_compiler_message(
        level: &str,
        code: Option<&str>,
        message: &str,
        file: &str,
        line: u64,
        col: u64,
    ) -> String {
        let code_field = match code {
            Some(c) => format!(r#""code":{{"code":"{c}"}},"#),
            None => "\"code\":null,".to_string(),
        };
        format!(
            r#"{{"reason":"compiler-message","message":{{{code_field}"level":"{level}","message":"{message}","spans":[{{"file_name":"{file}","line_start":{line},"column_start":{col},"line_end":{line},"column_end":{col},"is_primary":true}}],"children":[],"rendered":"rendered"}}}}"#
        )
    }

    #[test]
    fn json_to_clippy_uses_code_for_every_occurrence() {
        // Regression: in cargo's pretty-printed text, only the first
        // occurrence of each lint per crate carries a `= note: #[warn(rule)]`
        // line, so the old text scraper left subsequent warnings as bare
        // `warning`. With JSON ingestion every diagnostic carries
        // `message.code.code`, so they all keep the rule in the header.
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::collapsible_if"),
            "this `if` statement can be collapsed",
            "src/compose.rs",
            219,
            9,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "warning",
            Some("clippy::collapsible_if"),
            "this `if` statement can be collapsed",
            "src/compose.rs",
            228,
            9,
        ));

        let parsed = parse_clippy_from_json(&input, false, &[]);
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.diagnostics.len(), 2);
        for d in &parsed.diagnostics {
            assert!(d
                .format_as(crate::rustc_text::Level::Error)
                .starts_with("error[clippy::collapsible_if] src/compose.rs:"));
        }
    }

    #[test]
    fn json_to_clippy_uses_primary_label_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/foo.rs","line_start":20,"column_start":5,"line_end":20,"column_end":10,"is_primary":true,"label":"expected `i32`, found `&str`"}],"children":[],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false, &[]);
        assert_eq!(parsed.diagnostics.len(), 1);
        let d = &parsed.diagnostics[0];
        assert_eq!(
            d.format_as(crate::rustc_text::Level::Error)
                .split(' ')
                .next(),
            Some("error[E0308]")
        );
        assert_eq!(
            d.format_as(crate::rustc_text::Level::Error),
            "error[E0308] src/foo.rs:20:5 mismatched types - expected `i32`, found `&str`"
        );
    }

    #[test]
    fn json_to_clippy_falls_back_to_child_note_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/lib.rs","line_start":42,"column_start":12,"line_end":42,"column_end":15,"is_primary":true,"label":"arguments to this function are incorrect"}],"children":[{"level":"note","message":"expected reference `&Vec<u8>`\n   found reference `&Vec<i32>`","spans":[]}],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false, &[]);
        assert_eq!(parsed.diagnostics.len(), 1);
        let d = &parsed.diagnostics[0];
        assert!(
            d.format_one()
                .contains("- expected reference `&Vec<u8>`, found reference `&Vec<i32>`"),
            "got: {}",
            d.format_one()
        );
    }

    #[test]
    fn json_to_clippy_no_code_falls_back_to_bare_level() {
        // Some diagnostics lack a code (e.g. cargo-emitted notes). The
        // header degrades gracefully to a bare `error`.
        let input = json_compiler_message(
            "warning",
            None,
            "something happened",
            "src/foo.rs",
            10,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false, &[]);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0]
                .format_as(crate::rustc_text::Level::Error)
                .split(' ')
                .next(),
            Some("error")
        );
    }

    /// Only a warning from outside the workspace is exempt. The same warning
    /// from a member, one with no package id, and a dependency's error all
    /// count - and with no member list nothing is a dependency.
    #[test]
    fn only_a_dependency_warning_is_not_an_error() {
        let event = |level: &str, package: Option<&str>| {
            let mut json = json_compiler_message(level, Some("unused_imports"), "unused", "src/a.rs", 1, 1);
            if let Some(id) = package {
                json = json.replacen("{\"reason\"", &format!("{{\"package_id\":\"{id}\",\"reason\""), 1);
            }
            cargo_json::parse_cargo_diagnostics(&json).remove(0)
        };
        let members: HashMap<String, String> = [("member#a@1.0.0".to_owned(), "a".to_owned())].into();
        let dep = "path+file:///elsewhere#b@1.0.0";

        // The warning with no package id sits at `src/a.rs`, which only the
        // member's manifest directory makes the workspace's own.
        let manifests: HashMap<std::path::PathBuf, String> =
            [(std::path::PathBuf::from("/ws/Cargo.toml"), "member#a@1.0.0".to_owned())].into();
        let owners = crate::diagnostic::OwnerIndex::from_metadata_at(
            Path::new("/ws"),
            Path::new("/ws"),
            &members,
            &manifests,
        );
        let is_dependency_warning = |mut diagnostic: crate::diagnostic::Diagnostic| {
            diagnostic.classify_owner(&owners);
            diagnostic.is_dependency_warning()
        };
        assert!(is_dependency_warning(event("warning", Some(dep))));
        assert!(!is_dependency_warning(event("warning", Some("member#a@1.0.0"))));
        assert!(!is_dependency_warning(event("warning", None)));
        assert!(!is_dependency_warning(event("error", Some(dep))));
    }

    #[test]
    fn a_capped_lint_is_reported_as_an_error() {
        // Under `--cap-lints=warn` a deny lint arrives at `warning` level; it is
        // an error all the same, in the flag and the header.
        let input = json_compiler_message(
            "warning",
            Some("clippy::manual_string_new"),
            "empty String is being created manually",
            "src/a.rs",
            3,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false, &[]);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].level, crate::rustc_text::Level::Warning);
        assert_eq!(
            parsed.diagnostics[0].format_as(crate::rustc_text::Level::Error).split(' ').next(),
            Some("error[clippy::manual_string_new]")
        );
    }

    #[test]
    fn allow_exact_drops_matching_diagnostic_at_ingestion() {
        // The sited allow acts after clippy has spoken: even an error-level
        // diagnostic (the `#[expect]`-sibling shape that defeats `-A`) is
        // filtered when lint and file both match. A bare lint name in config
        // matches the `clippy::`-qualified code cargo emits.
        let input = json_compiler_message(
            "error",
            Some("clippy::unused_async_trait_impl"),
            "unused `async` for async trait impl function",
            "crates/system/src/kernel.rs",
            747,
            5,
        );
        let sited =
            [SitedAllow::parse("unused_async_trait_impl@crates/system/src/kernel.rs").unwrap()];
        let parsed = parse_clippy_from_json(&input, false, &sited);
        assert!(parsed.diagnostics.is_empty());
        assert!(!parsed.parse_failed);
    }

    #[test]
    fn allow_exact_is_file_scoped_not_workspace_wide() {
        // Same lint in a different file must survive - the whole point of the
        // sited form over `allow`.
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::unused_async"),
            "unused `async`",
            "src/a.rs",
            1,
            1,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "warning",
            Some("clippy::unused_async"),
            "unused `async`",
            "src/b.rs",
            2,
            2,
        ));
        let sited = [SitedAllow::parse("clippy::unused_async@src/a.rs").unwrap()];
        let parsed = parse_clippy_from_json(&input, false, &sited);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0].location().as_deref(),
            Some("src/b.rs:2:2")
        );
    }

    #[test]
    fn allow_exact_does_not_touch_other_lints_in_the_file() {
        let input = json_compiler_message(
            "warning",
            Some("clippy::needless_return"),
            "unneeded return statement",
            "src/a.rs",
            3,
            5,
        );
        let sited = [SitedAllow::parse("clippy::unused_async@src/a.rs").unwrap()];
        let parsed = parse_clippy_from_json(&input, false, &sited);
        assert_eq!(parsed.diagnostics.len(), 1);
    }

    #[test]
    fn json_to_clippy_sets_parse_failed_when_sweep_failed_with_no_events() {
        // cargo crashed before producing any compiler-message events.
        let parsed = parse_clippy_from_json("", true, &[]);
        assert!(parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    #[test]
    fn json_to_clippy_no_parse_failed_when_sweep_succeeded() {
        // Empty stdout but successful exit (clean compile). Not a parse
        // failure - just nothing to report.
        let parsed = parse_clippy_from_json("", false, &[]);
        assert!(!parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    fn diag(header: &str, location: &str) -> crate::diagnostic::Diagnostic {
        let level = if header.starts_with("error") {
            crate::rustc_text::Level::Error
        } else {
            crate::rustc_text::Level::Warning
        };
        let code = header.find('[').and_then(|start| {
            header[start + 1..]
                .find(']')
                .map(|end| header[start + 1..start + 1 + end].to_owned())
        });
        let mut diagnostic = crate::diagnostic::Diagnostic::new(level, code, "msg".into());
        let mut parts = location.rsplitn(3, ':');
        let column = parts.next().and_then(|part| part.parse().ok());
        let line = parts.next().and_then(|part| part.parse().ok());
        let file = parts.next().unwrap_or(location);
        diagnostic.site = Some(crate::diagnostic::Site {
            file: file.into(),
            line,
            column,
        });
        diagnostic
    }

    #[test]
    fn clippy_sort_key_orders_errors_before_warnings() {
        let warn = diag("warning[clippy::aaaa]", "src/a.rs:1:1");
        let err = diag("error[E0308]", "src/z.rs:99:99");
        assert!(clippy_sort_key(&err) < clippy_sort_key(&warn));
    }

    #[test]
    fn clippy_sort_key_groups_same_lint_together() {
        // Three warnings - two with the same lint code on different files,
        // one with a different code in between alphabetically. After sort,
        // the same-lint pair should be adjacent.
        let mut diags = vec![
            diag("warning[clippy::collapsible_if]", "src/b.rs:1:1"),
            diag("warning[clippy::needless_return]", "src/a.rs:1:1"),
            diag("warning[clippy::collapsible_if]", "src/a.rs:1:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].code.as_deref(), Some("clippy::collapsible_if"));
        assert_eq!(diags[1].code.as_deref(), Some("clippy::collapsible_if"));
        assert_eq!(diags[2].code.as_deref(), Some("clippy::needless_return"));
        // Within the same lint, file order kicks in: a.rs before b.rs.
        assert_eq!(diags[0].location().as_deref(), Some("src/a.rs:1:1"));
        assert_eq!(diags[1].location().as_deref(), Some("src/b.rs:1:1"));
    }

    #[test]
    fn clippy_sort_key_orders_lines_numerically() {
        // Same lint, same file: line 9 before line 100 (lexical sort
        // would put 100 first - check we're parsing the integer).
        let mut diags = vec![
            diag("warning[clippy::xxx]", "src/a.rs:100:1"),
            diag("warning[clippy::xxx]", "src/a.rs:9:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].location().as_deref(), Some("src/a.rs:9:1"));
        assert_eq!(diags[1].location().as_deref(), Some("src/a.rs:100:1"));
    }

    #[test]
    fn clippy_sort_key_pushes_bare_level_to_end() {
        // A bare `warning` (no code) should sort after every coded
        // warning, since there's no useful key to group it with.
        let mut diags = vec![
            diag("warning", "src/a.rs:1:1"),
            diag("warning[clippy::zzz]", "src/b.rs:1:1"),
            diag("warning[clippy::aaa]", "src/c.rs:1:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].code.as_deref(), Some("clippy::aaa"));
        assert_eq!(diags[1].code.as_deref(), Some("clippy::zzz"));
        assert_eq!(diags[2].code, None);
    }

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn format_override_is_refused_on_the_final_argv() {
        let own = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>();
        // brokkr's own injected pair, alone: fine.
        assert!(
            reject_format_override(&own(&["-Z", "unstable-options", "--format", "json"])).is_ok()
        );
        // A forwarded override after it wins in libtest, so it must be refused -
        // this is the case that silently degraded the per-test cap to the idle
        // ceiling.
        assert!(
            reject_format_override(&own(&["--format", "json", "--format", "pretty"])).is_err()
        );
        // The `=` spelling is one argv entry and was missed by an equality check.
        assert!(reject_format_override(&own(&["--format", "json", "--format=pretty"])).is_err());
        assert!(reject_format_override(&own(&["--format=pretty"])).is_err());
        // Unrelated args are untouched.
        assert!(reject_format_override(&own(&["--nocapture", "--test-threads=1"])).is_ok());
    }

    #[test]
    fn forwarded_args_split_no_separator_all_cargo_level() {
        // `brokkr check -- --test read_paths` (clap consumed the leading
        // `--`, leaving us with the two tokens). Cargo gets `--test
        // read_paths`; nothing crosses to libtest.
        let extra = s(&["--test", "read_paths"]);
        let forwarded = ForwardedArgs::parse(&extra);
        let (cargo, libtest) = (forwarded.cargo(), forwarded.libtest());
        assert_eq!(cargo, &["--test", "read_paths"]);
        assert!(libtest.is_empty());
    }

    #[test]
    fn forwarded_args_split_double_dash_routes_to_libtest() {
        // `brokkr check -- -- --ignored`: clap consumed the first `--`,
        // we see `["--", "--ignored"]`. The literal `--` we observe is
        // the *cargo/libtest* boundary - everything after it is libtest.
        let extra = s(&["--", "--ignored"]);
        let forwarded = ForwardedArgs::parse(&extra);
        let (cargo, libtest) = (forwarded.cargo(), forwarded.libtest());
        assert!(cargo.is_empty());
        assert_eq!(libtest, &["--ignored"]);
    }

    #[test]
    fn forwarded_args_split_mixed_form_routes_each_side() {
        // `brokkr check -- --test cli -- --ignored --nocapture`: cargo
        // gets the test filter, libtest gets the runtime flags.
        let extra = s(&["--test", "cli", "--", "--ignored", "--nocapture"]);
        let forwarded = ForwardedArgs::parse(&extra);
        let (cargo, libtest) = (forwarded.cargo(), forwarded.libtest());
        assert_eq!(cargo, &["--test", "cli"]);
        assert_eq!(libtest, &["--ignored", "--nocapture"]);
    }

    #[test]
    fn forwarded_args_split_empty_input() {
        let extra: Vec<String> = Vec::new();
        let forwarded = ForwardedArgs::parse(&extra);
        let (cargo, libtest) = (forwarded.cargo(), forwarded.libtest());
        assert!(cargo.is_empty());
        assert!(libtest.is_empty());
    }

    #[test]
    fn forwarded_target_selector_detects_explicit_targets() {
        // A profile's `--test <name>` scope, or any user-supplied target
        // flag, means doctests are already off and `--tests` must not be
        // appended on top.
        assert!(ForwardedArgs::parse(&s(&["test", "--test", "cli_sort"])).has_target_selector());
        assert!(ForwardedArgs::parse(&s(&["test", "--tests"])).has_target_selector());
        assert!(ForwardedArgs::parse(&s(&["test", "--lib"])).has_target_selector());
        assert!(ForwardedArgs::parse(&s(&["test", "--bins"])).has_target_selector());
        assert!(ForwardedArgs::parse(&s(&["test", "--bin=foo"])).has_target_selector());
        assert!(ForwardedArgs::parse(&s(&["test", "--doc"])).has_target_selector());
        assert!(ForwardedArgs::parse(&s(&["test", "--all-targets"])).has_target_selector());
        assert!(ForwardedArgs::parse(&s(&["test", "--example", "e"])).has_target_selector());
        assert!(ForwardedArgs::parse(&s(&["test", "--bench", "b"])).has_target_selector());
    }

    #[test]
    fn forwarded_target_selector_ignores_non_target_flags() {
        // The default sweep shape: feature scoping + package, no target
        // selector - so `--tests` is what suppresses doctests here.
        assert!(!ForwardedArgs::parse(&s(&[
            "test",
            "-p",
            "pkg",
            "--features",
            "a,b",
            "--message-format=json",
        ])).has_target_selector());
        assert!(!ForwardedArgs::parse(&s(&[
            "test",
            "--workspace",
            "--exclude",
            "slow",
        ])).has_target_selector());
    }

    #[test]
    fn build_test_env_emits_absolute_paths() {
        // B8: CARGO_TARGET_TMPDIR used to be the literal string
        // "target/tmp", which only resolves correctly when the cargo
        // subprocess inherits cwd=project_root. Make sure the helper
        // emits an absolute path joined onto the cargo-resolved
        // target_dir.
        let target = Path::new("/home/u/proj/target");
        let env = build_test_env(Some(Project::Nidhogg), target, "debug");
        let tmp = env
            .iter()
            .find(|(k, _)| k == "CARGO_TARGET_TMPDIR")
            .map(|(_, v)| v.as_str())
            .expect("CARGO_TARGET_TMPDIR set for nidhogg");
        assert_eq!(tmp, "/home/u/proj/target/tmp");
        let bin = env
            .iter()
            .find(|(k, _)| k == "BROKKR_TEST_BIN_DIR")
            .map(|(_, v)| v.as_str())
            .expect("BROKKR_TEST_BIN_DIR set");
        assert_eq!(bin, "/home/u/proj/target/debug");
    }

    #[test]
    fn build_test_env_no_tmpdir_for_non_nidhogg() {
        let env = build_test_env(Some(Project::Pbfhogg), Path::new("/x/target"), "release");
        assert!(env.iter().all(|(k, _)| k != "CARGO_TARGET_TMPDIR"));
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "BROKKR_TEST_BIN_DIR")
                .map(|(_, v)| v.as_str()),
            Some("/x/target/release"),
        );
    }

    fn rustflags_sweep() -> PlannedSweep {
        crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            rustflags: vec!["--cfg".into(), "madsim".into()],
            ..crate::profile::SweepDecl::default()
        })
    }

    fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn rustflags_value(env: &[(String, String)]) -> Option<&str> {
        get(env, "RUSTFLAGS").or_else(|| get(env, "CARGO_ENCODED_RUSTFLAGS"))
    }

    #[test]
    fn sweep_runtime_env_isolates_target_dir_for_rustflags() {
        let sweep = rustflags_sweep();
        let key = crate::config::check::rustflags_target_key(&sweep.rustflags).unwrap();
        let env = sweep_runtime_env(
            &sweep,
            Some(Project::Pbfhogg),
            Path::new("/meta/target"),
            "debug",
            &[],
        );
        // S3-20: the isolated dir sits beside cargo's resolved target dir, not
        // under an assumed `<project_root>/target`.
        let dir = format!("/meta/target/rustflags-{key}");
        assert_eq!(get(&env, "CARGO_TARGET_DIR"), Some(dir.as_str()));
        // BROKKR_TEST_BIN_DIR tracks the isolated dir, not the plain one.
        assert_eq!(get(&env, "BROKKR_TEST_BIN_DIR"), Some(format!("{dir}/debug").as_str()));
        // RUSTFLAGS carries the sweep's flags (composed with any inherited).
        let rf = rustflags_value(&env).expect("rustflags env set");
        assert!(rf.contains("--cfg") && rf.contains("madsim"), "got: {rf}");
    }

    #[test]
    fn sweep_runtime_env_isolated_dir_follows_offroot_target() {
        // S3-20 regression: a workspace `.cargo/config.toml` can put the target
        // dir on another drive. The isolated dir must land there, not on the
        // project root's drive (where `brokkr clean` never reclaims it).
        let sweep = rustflags_sweep();
        let key = crate::config::check::rustflags_target_key(&sweep.rustflags).unwrap();
        let env = sweep_runtime_env(
            &sweep,
            Some(Project::Pbfhogg),
            Path::new("/media/folk/Banan/cargo"),
            "debug",
            &[],
        );
        assert_eq!(
            get(&env, "CARGO_TARGET_DIR"),
            Some(format!("/media/folk/Banan/cargo/rustflags-{key}").as_str())
        );
    }

    #[test]
    fn sweep_runtime_env_plain_sweep_uses_metadata_target() {
        let env = sweep_runtime_env(
            &crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default()),
            Some(Project::Pbfhogg),
            Path::new("/meta/target"),
            "debug",
            &[],
        );
        assert_eq!(get(&env, "BROKKR_TEST_BIN_DIR"), Some("/meta/target/debug"));
        assert!(get(&env, "CARGO_TARGET_DIR").is_none());
        assert!(rustflags_value(&env).is_none());
    }

    #[test]
    fn sweep_cargo_env_omits_bin_dir() {
        let key = crate::config::check::rustflags_target_key(&rustflags_sweep().rustflags).unwrap();
        let env = sweep_cargo_env(&rustflags_sweep(), Path::new("/meta/target"), &[]);
        // Clippy shares the same isolated dir the test phase builds into.
        assert_eq!(
            get(&env, "CARGO_TARGET_DIR"),
            Some(format!("/meta/target/rustflags-{key}").as_str())
        );
        assert!(rustflags_value(&env).is_some());
        // Clippy has no test binary to spawn.
        assert!(get(&env, "BROKKR_TEST_BIN_DIR").is_none());
        // A plain sweep contributes nothing.
        assert!(
            sweep_cargo_env(&crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default()), Path::new("/meta/target"), &[]).is_empty()
        );
        // A plain sweep with env-sink lint allows carries them and nothing else.
        let allows = vec!["-Aclippy::assert_is_empty".to_owned()];
        let env = sweep_cargo_env(&crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default()), Path::new("/meta/target"), &allows);
        assert!(get(&env, "CARGO_TARGET_DIR").is_none());
        assert!(rustflags_value(&env)
            .expect("allow flags reach the env")
            .contains("-Aclippy::assert_is_empty"));
    }

    /// The one request a sweep's own selection runs as in `phase`.
    fn only_run(sweep: &PlannedSweep, phase: SelectionPhase) -> ResolutionRun {
        let sel = PhaseSelection::for_check(phase, std::slice::from_ref(sweep), &[], &FeatureOracle::unavailable())
            .unwrap();
        sel.attempt(LaneIdx::new(0)).unwrap().runs()[0].clone()
    }

    /// The support request for `package`, built as the selection builds it.
    fn support_req(sweep: &PlannedSweep, package: &str) -> SupportRequest {
        let with = crate::profile::PlannedSweep::from(crate::profile::SweepDecl { build_packages: vec![package.to_owned()], ..sweep.declaration().clone() });
        let sel = PhaseSelection::for_check(
            SelectionPhase::Test,
            std::slice::from_ref(&with),
            &[],
            &FeatureOracle::unavailable(),
        )
        .unwrap();
        sel.attempt(LaneIdx::new(0)).unwrap().support()[0].clone()
    }

    fn release_sweep() -> PlannedSweep {
        crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            label: "timing".into(),
            profile: Some(crate::config::SweepProfile::Release),
            ..crate::profile::SweepDecl::default()
        })
    }

    #[test]
    fn a_pinned_profile_reaches_every_compiling_path() {
        // The invariant the key stands on: clippy, the test invocation, the
        // process-isolated selection and the coverage enumeration all compile
        // the same sweep, and a profile that reached only some of them would
        // mean linting one build, running another, and auditing a third.
        let sweep = release_sweep();
        assert_eq!(sweep_profile_args(&sweep), ["--release"]);

        let run = only_run(&sweep, SelectionPhase::Test);
        let test_args = sweep_selection_args(&sweep, &run);
        assert_eq!(test_args.first().map(String::as_str), Some("--release"));

        let clippy = clippy_args(&sweep, &only_run(&sweep, SelectionPhase::Clippy), &crate::config::LintsConfig::default());
        assert!(clippy.iter().any(|a| a == "--release"), "got: {clippy:?}");

        let enumeration = shape_selection_args(&sweep, &run);
        assert_eq!(enumeration.first().map(String::as_str), Some("--release"));

        // `--release` must not read as a target selector - that would suppress
        // the `--tests` scoping and let doctests back into the sweep.
        assert!(!ForwardedArgs::parse(&test_args).has_target_selector());
    }

    #[test]
    fn a_pinned_unification_reaches_every_compiling_path() {
        // The twin of the profile invariant above, and the one that shipped
        // broken: the ordinary test path never emitted the pin, so a lane was
        // labelled `unification package`, isolated into its own target dir,
        // and then compiled under workspace resolution. It ran 839 green tests
        // against a defect it was created to catch. Assert the flags on every
        // compiling path, not the label.
        let pkg = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            packages: vec!["daemon".to_owned()],
            feature_unification: crate::config::FeatureUnification::Package,
            ..crate::profile::SweepDecl::default()
        });
        let pin = "resolver.feature-unification=\"package\"";

        let daemon = only_run(&pkg, SelectionPhase::Test);
        assert_eq!(daemon.selection, Selection::Explicit(vec!["daemon".to_owned()]));
        let test_args = sweep_selection_args(&pkg, &daemon);
        assert!(test_args.iter().any(|a| a == "-Zfeature-unification"), "test: {test_args:?}");
        assert!(test_args.iter().any(|a| a == pin), "test: {test_args:?}");

        let clippy = clippy_args(&pkg, &daemon, &crate::config::LintsConfig::default());
        assert!(clippy.iter().any(|a| a == pin), "clippy: {clippy:?}");

        let enumeration = shape_selection_args(&pkg, &daemon);
        assert!(enumeration.iter().any(|a| a == pin), "audit: {enumeration:?}");

        // The `build_packages` pre-build and the rustdoc phase compile the
        // sweep too; both once went out without the pin.
        let pre_build = pre_build_args(&pkg, &support_req(&pkg, "daemon"), &[]);
        assert!(pre_build.iter().any(|a| a == pin), "pre-build: {pre_build:?}");
        // And it reports what it built, so a complete plan can hash it.
        assert!(pre_build.iter().any(|a| a == "--message-format=json"), "{pre_build:?}");

        let doc = doc_args(&pkg, &daemon, &crate::config::RustdocConfig::default());
        assert!(doc.iter().any(|a| a == pin), "rustdoc: {doc:?}");

        // The pin is a cargo option, so it must land before the `--` split or
        // libtest receives it.
        if let Some(split) = test_args.iter().position(|a| a == "--") {
            let z = test_args.iter().position(|a| a == "-Zfeature-unification");
            assert!(z.is_some_and(|i| i < split), "pin after `--`: {test_args:?}");
        }
    }

    /// A pre-build's stream yields its executables - a dependency's lib and
    /// build script are not support executables - and their fingerprint
    /// follows content.
    #[test]
    fn a_pre_build_reports_its_executables() {
        let dir = crate::test_scratch::scratch("output", "support_artifacts");
        let exe = dir.join("server");
        std::fs::write(&exe, b"v1").unwrap();
        let stdout = format!(
            "{}\n{}\n{}\n{}\n",
            r#"{"reason":"compiler-artifact","target":{"name":"dep","kind":["lib"]},"executable":null}"#,
            r#"{"reason":"compiler-artifact","target":{"name":"build-script-build","kind":["custom-build"]},"executable":"/t/build/x/build-script-build"}"#,
            format_args!(
                r#"{{"reason":"compiler-artifact","target":{{"name":"server","kind":["bin"]}},"executable":"{}"}}"#,
                exe.display()
            ),
            r#"{"reason":"build-finished","success":true}"#,
        );
        let arts = support_artifacts(&stdout, "server-pkg");
        assert_eq!(arts.len(), 1, "{arts:?}");
        assert_eq!(arts[0].target, "server");
        let before = support_fingerprint(&arts).unwrap();
        std::fs::write(&exe, b"v2").unwrap();
        assert_ne!(before, support_fingerprint(&arts).unwrap());
    }

    #[test]
    fn an_unpinned_sweep_is_untouched() {
        // Every repo that never sets the key must produce byte-identical argv
        // to what it produced before the key existed.
        let plain = crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default());
        let run = only_run(&plain, SelectionPhase::Test);
        assert!(sweep_profile_args(&plain).is_empty());
        assert!(!sweep_selection_args(&plain, &run).iter().any(|a| a == "--release"));
        assert!(!clippy_args(&plain, &run, &crate::config::LintsConfig::default()).iter().any(|a| a == "--release"));
        assert!(!shape_selection_args(&plain, &run).iter().any(|a| a == "--release"));
        // Same for unification: `auto` un-promoted pins nothing anywhere.
        for args in [
            sweep_selection_args(&plain, &run),
            clippy_args(&plain, &run, &crate::config::LintsConfig::default()),
            shape_selection_args(&plain, &run),
            pre_build_args(&plain, &support_req(&plain, "bin"), &[]),
        ] {
            assert!(!args.iter().any(|a| a == "-Zfeature-unification"), "got: {args:?}");
        }
    }

    /// rustc knows no `cargo` lint tool: a `cargo::` allow is applied at
    /// ingestion, never passed to clippy as `-A`.
    #[test]
    fn clippy_args_leave_cargo_lints_out_of_the_allow_flags() {
        let allow = crate::config::LintsConfig {
            allow: ["clippy::unused_async", "cargo::unused_dependencies"]
                .map(crate::config::check::LintName::parse)
                .to_vec(),
            allow_exact: Vec::new(),
        };
        let plain = crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default());
        let args = clippy_args(&plain, &only_run(&plain, SelectionPhase::Clippy), &allow);
        assert!(args.iter().any(|a| a == "clippy::unused_async"), "{args:?}");
        assert!(!args.iter().any(|a| a.starts_with("cargo::")), "{args:?}");
    }

    /// A `cargo::` `allow_exact` entry never reaches a build, so the verdict
    /// line does not claim it widens there - not even beside entries that do.
    #[test]
    fn the_verdict_line_calls_cargo_allow_exact_entries_sited() {
        let sited = |s: &str| SitedAllow::parse(s).unwrap();
        let lints = |exact: Vec<SitedAllow>| crate::config::LintsConfig { allow: Vec::new(), allow_exact: exact };
        let cargo_only =
            verdict_context(&None, 1, &lints(vec![sited("cargo::unused_dependencies@a/Cargo.toml")]));
        assert!(cargo_only.contains("allow_exact: cargo::unused_dependencies (sited)"), "{cargo_only}");
        let mixed = verdict_context(
            &None,
            1,
            &lints(vec![sited("cargo::unused_dependencies@a/Cargo.toml"), sited("dead_code@src/a.rs")]),
        );
        assert!(
            mixed.contains("allow_exact: dead_code, cargo::unused_dependencies (sited)"),
            "{mixed}"
        );
    }

    #[test]
    fn the_shape_line_names_a_pinned_profile() {
        // Same reason `rustflags` is surfaced: it redirects the sweep to
        // another target subdirectory and buys a full recompile there, and an
        // unexplained rebuild is what the collapsed log must not hide.
        let line = describe(&release_sweep(), true, &Selection::Bare);
        assert!(line.contains("profile release"), "got: {line}");
        // An unpinned sweep says nothing - it is the command's default.
        assert!(!describe(&crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default()), true, &Selection::Bare).contains("profile"));
    }

    fn parsed(passed: usize, failed: usize, ignored: usize, filtered_out: usize, suites: usize) -> crate::test_runner::ParsedTestResults {
        crate::test_runner::ParsedTestResults {
            failures: Vec::new(),
            passed,
            failed,
            ignored,
            filtered_out,
            suites,
            duration: None,
            completeness: crate::test_runner::Completeness::Complete,
        }
    }

    #[test]
    fn zero_test_run_flags_zero_suites() {
        // Cargo exited 0 but the parser never saw a `test result:` line.
        // Either parse failure or `--test cli_x` matched no test crate.
        assert!(zero_test_run(&parsed(0, 0, 0, 0, 0)));
    }

    #[test]
    fn zero_test_run_flags_all_filtered_out() {
        // The classic silent-wrong-run shape: `--skip` or the positional
        // name filter excluded every test in the matched suite(s).
        assert!(zero_test_run(&parsed(0, 0, 0, 5, 1)));
    }

    #[test]
    fn zero_test_run_does_not_flag_empty_suite() {
        // A package that legitimately defines no tests reports
        // `0 passed; 0 filtered out` over one suite. Not a wrong run.
        assert!(!zero_test_run(&parsed(0, 0, 0, 0, 1)));
    }

    #[test]
    fn zero_test_run_does_not_flag_normal_pass() {
        assert!(!zero_test_run(&parsed(12, 0, 0, 3, 1)));
    }

    #[test]
    fn zero_test_run_does_not_flag_only_ignored() {
        // `#[ignore]` tests still count - the user opted in.
        assert!(!zero_test_run(&parsed(0, 0, 4, 0, 1)));
    }

    #[test]
    fn forwarded_args_split_only_dashes_separator() {
        // Just `-- --` after the brokkr `--`. First `--` is consumed by
        // clap, the second is our split point: both sides empty.
        let extra = s(&["--"]);
        let forwarded = ForwardedArgs::parse(&extra);
        let (cargo, libtest) = (forwarded.cargo(), forwarded.libtest());
        assert!(cargo.is_empty());
        assert!(libtest.is_empty());
    }

    fn sweep(label: &str) -> PlannedSweep {
        crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            label: label.into(),
            ..crate::profile::SweepDecl::default()
        })
    }

    /// [`describe_sweep`] with the sweep's configured features - what a run
    /// over the sweep's own selection carries.
    fn describe(sweep: &PlannedSweep, for_test: bool, selection: &Selection) -> String {
        describe_sweep(sweep, for_test, selection, &sweep.features.configured_key())
    }

    #[test]
    fn describe_sweep_reports_package_scope() {
        // No package flags: cargo's default selection, never called the
        // workspace.
        assert_eq!(describe(&sweep("default"), false, &Selection::Bare), "default selection");

        // The sweep's own `packages` list; one package is named.
        let scoped = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            packages: s(&["nautilus-core", "nautilus-model"]),
            ..sweep("ffi").declaration().clone()
        });
        let own = Selection::configured(&scoped, SelectionPhase::Clippy);
        assert_eq!(describe(&scoped, false, &own), "2 pkgs");
        assert_eq!(describe(&scoped, false, &Selection::Explicit(s(&["nautilus-core"]))), "-p nautilus-core");

        // `--workspace --exclude` is a test-phase-only shape; the clippy line
        // describes the clippy selection, matching what actually runs.
        let excluded = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            test_exclude_packages: s(&["nautilus-pyo3", "nautilus-cli"]),
            ..sweep("default").declaration().clone()
        });
        let clippy = Selection::configured(&excluded, SelectionPhase::Clippy);
        assert_eq!(describe(&excluded, false, &clippy), "default selection");
        let test = Selection::configured(&excluded, SelectionPhase::Test);
        assert_eq!(
            describe(&excluded, true, &test),
            "workspace -2 pkgs, serial"
        );
    }

    #[test]
    fn describe_sweep_reads_features_back_out_of_argv() {
        let all = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            features: crate::profile::FeatureConfig { all_features: true, ..Default::default() },
            ..sweep("all").declaration().clone()
        });
        assert_eq!(describe(&all, false, &Selection::Bare), "default selection, all-features");

        let consumer = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            features: crate::profile::FeatureConfig {
                tokens: s(&["commands"]),
                no_default_features: true,
                ..Default::default()
            },
            ..sweep("consumer").declaration().clone()
        });
        assert_eq!(
            describe(&consumer, false, &Selection::Bare),
            "default selection, no-default +commands"
        );

        // The `--features=x,y` spelling is equivalent.
        assert_eq!(describe_features(&s(&["--features=ffi,live"])).as_deref(), Some("+ffi,live"));
    }

    /// The line renders the features a run carries, not the configured list:
    /// a `-p` run that dropped a token does not claim to have built it.
    #[test]
    fn describe_sweep_renders_the_projected_features() {
        let s0 = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            features: crate::profile::FeatureConfig { tokens: s(&["a/x", "b/y"]), ..Default::default() },
            ..sweep("two").declaration().clone()
        });
        assert_eq!(
            describe_sweep(&s0, false, &override_of(&["a"]), &s(&["--features", "a/x"])),
            "-p a, +a/x"
        );
    }

    #[test]
    fn describe_sweep_does_not_restate_the_label() {
        // The legacy no-`[[check]]` path names its synthesized sweep after the
        // feature shape, which would otherwise print twice on one line.
        let legacy = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            features: crate::profile::FeatureConfig { all_features: true, ..Default::default() },
            ..sweep("all-features").declaration().clone()
        });
        assert_eq!(describe(&legacy, false, &Selection::Bare), "default selection");
    }

    #[test]
    fn describe_sweep_surfaces_rustflags_and_isolation() {
        // rustflags silently redirect the sweep to its own target dir; the
        // collapsed form must not hide the cause of a full recompile.
        let sim = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            rustflags: s(&["--cfg", "madsim"]),
            ..sweep("sim").declaration().clone()
        });
        assert!(describe(&sim, false, &Selection::Bare).contains("rustflags --cfg madsim"));
        assert!(describe(&sim, false, &Selection::Bare).contains("isolated target"));
    }

    #[test]
    fn describe_sweep_summarises_libtest_filters_by_count() {
        // The 14-skip list is the bulk of nautilus's command line and is
        // identical across its three sweeps - a count is the whole signal.
        let skips = ["a", "b", "c"]
            .into_iter()
            .map(|name| crate::profile::Filter {
                kind: crate::profile::FilterKind::Skip,
                pattern: name.to_owned(),
                package: None,
                origin: crate::profile::FilterOrigin::Profile("tier1".into()),
            })
            .collect();
        let tier = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            filters: crate::profile::LaneFilters {
                filters: skips,
                targets: Vec::new(),
                include_ignored: true,
            },
            lane_exec: crate::profile::LaneExec::Serial {
                threads: crate::profile::Threads::CargoDefault,
            },
            ..sweep("tier1").declaration().clone()
        });
        assert_eq!(
            describe(&tier, true, &Selection::Bare),
            "default selection, 3 skips, include-ignored, parallel"
        );
        // Clippy never takes libtest filters, so its line omits them.
        assert_eq!(describe(&tier, false, &Selection::Bare), "default selection");
    }

    #[test]
    fn describe_sweep_joins_test_filter_pairs() {
        // Cargo renders a target selector as the pair `--test cli_sort`; the
        // shape must show each target as one item, not `--test` and the bare
        // name as two comma-separated fragments.
        let targets = |names: &[&str]| crate::profile::LaneFilters {
            targets: names.iter().map(|n| crate::profile::TargetName::new(*n)).collect(),
            ..Default::default()
        };
        let one = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            filters: targets(&["cli_sort"]),
            ..sweep("sort").declaration().clone()
        });
        assert_eq!(
            describe(&one, true, &Selection::Bare),
            "default selection, --test cli_sort, serial"
        );

        // Two filters stay two distinct items, each self-contained.
        let two = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            filters: targets(&["cli_sort", "cli_env"]),
            ..sweep("sort").declaration().clone()
        });
        assert_eq!(
            describe(&two, true, &Selection::Bare),
            "default selection, --test cli_sort, --test cli_env, serial"
        );

        // Clippy never takes cargo test filters, so its line omits them.
        assert_eq!(describe(&one, false, &Selection::Bare), "default selection");
    }

    #[test]
    fn describe_sweep_thread_policy_tracks_watchdog_lane() {
        // The watchdogged serial lane (`test_threads` unset or 1) says serial;
        // libtest-mediated threads (`test_threads` 0 or above 1) say parallel.
        let serial = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            lane_exec: crate::profile::LaneExec::Serial {
                threads: crate::profile::Threads::Watchdogged,
            },
            ..sweep("serial").declaration().clone()
        });
        assert_eq!(describe(&serial, true, &Selection::Bare), "default selection, serial");
        let four = std::num::NonZeroU32::new(4).unwrap();
        for threads in [crate::profile::Threads::CargoDefault, crate::profile::Threads::Cargo(four)] {
            let parallel = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
                lane_exec: crate::profile::LaneExec::Serial { threads },
                ..sweep("par").declaration().clone()
            });
            assert_eq!(describe(&parallel, true, &Selection::Bare), "default selection, parallel");
        }
    }

    /// One sweep's entry in one `check` phase under a CLI `-p` set.
    fn entry(sweep: &PlannedSweep, phase: SelectionPhase, cli: &[&str]) -> LaneEntry {
        let sel =
            PhaseSelection::for_check(phase, std::slice::from_ref(sweep), &s(cli), &FeatureOracle::unavailable())
                .unwrap();
        sel.entry(LaneIdx::new(0)).unwrap().clone()
    }

    fn override_of(packages: &[&str]) -> Selection {
        Selection::Override { packages: s(packages), provenance: crate::check_cmd::selection::Provenance::Cli }
    }

    #[test]
    fn cli_package_replaces_sweep_selection_or_skips() {
        // Default-selection sweep: `-p` applies; without `-p` the sweep stands.
        let plain = sweep("default");
        assert_eq!(entry(&plain, SelectionPhase::Test, &["x"]).attempt().unwrap().selection(), &override_of(&["x"]));
        assert_eq!(entry(&plain, SelectionPhase::Test, &[]).attempt().unwrap().selection(), &Selection::Bare);

        // The exclusion list rules the package out - but only for the test
        // phase; clippy ignores test_exclude_packages by design.
        let excluded = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            test_exclude_packages: s(&["x"]),
            ..sweep("default").declaration().clone()
        });
        assert!(matches!(entry(&excluded, SelectionPhase::Test, &["x"]), LaneEntry::Excluded(_)));
        assert_eq!(
            entry(&excluded, SelectionPhase::Clippy, &["x"]).attempt().unwrap().selection(),
            &override_of(&["x"])
        );

        // A `packages` list admits members and skips everything else.
        let scoped = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            packages: s(&["a", "b"]),
            ..sweep("ffi").declaration().clone()
        });
        assert_eq!(entry(&scoped, SelectionPhase::Test, &["a"]).attempt().unwrap().selection(), &override_of(&["a"]));
        assert!(matches!(entry(&scoped, SelectionPhase::Clippy, &["x"]), LaneEntry::Excluded(_)));
    }

    #[test]
    fn cli_package_set_intersects_per_sweep() {
        // A multi `-p` set runs the in-scope subset with a note per dropped
        // package; the sweep is excluded only when nothing survives.
        let scoped = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            packages: s(&["a", "b"]),
            ..sweep("ffi").declaration().clone()
        });
        let mixed = entry(&scoped, SelectionPhase::Test, &["a", "x"]);
        assert_eq!(mixed.attempt().unwrap().selection(), &override_of(&["a"]));
        assert_eq!(mixed.notes().len(), 1);
        assert!(mixed.notes()[0].to_string().contains("-p x"));
        assert!(matches!(entry(&scoped, SelectionPhase::Test, &["x", "y"]), LaneEntry::Excluded(_)));

        // Test-phase exclusion drops per package too.
        let excluded = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            test_exclude_packages: s(&["x"]),
            ..sweep("default").declaration().clone()
        });
        let mixed = entry(&excluded, SelectionPhase::Test, &["a", "x"]);
        assert_eq!(mixed.attempt().unwrap().selection(), &override_of(&["a"]));
        assert_eq!(mixed.notes().len(), 1);
    }

    #[test]
    fn cli_package_excluded_from_doc_only_sweep_skips_the_lane() {
        // A documented contract, not an accident: downstream configs close a
        // bin-only crate's doctest failure with `test_exclude_packages` on a
        // `doc_only` sweep, and `check -p <that crate>` must then skip the
        // doctest lane rather than run `cargo test --doc -p <crate>`.
        let doctests = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            lane_exec: crate::profile::LaneExec::doc_only(),
            test_exclude_packages: s(&["bin-only"]),
            ..sweep("doctests").declaration().clone()
        });
        assert!(matches!(entry(&doctests, SelectionPhase::Test, &["bin-only"]), LaneEntry::Excluded(_)));
        let mixed = entry(&doctests, SelectionPhase::Test, &["bin-only", "lib"]);
        assert_eq!(mixed.attempt().unwrap().selection(), &override_of(&["lib"]));
        assert_eq!(mixed.notes().len(), 1);
    }

    #[test]
    fn describe_sweep_reports_cli_package_scope() {
        // The nautilus bug shape: an exclude-carrying sweep under CLI `-p`
        // must say `-p x`, not `workspace -2 pkgs` - the shape describes
        // what runs, and the CLI scope replaced the sweep's selection.
        let excluded = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            test_exclude_packages: s(&["a", "b"]),
            ..sweep("default").declaration().clone()
        });
        assert_eq!(describe(&excluded, true, &override_of(&["x"])), "-p x, serial");
        assert_eq!(describe(&excluded, false, &override_of(&["x"])), "-p x");
        assert_eq!(describe(&excluded, false, &override_of(&["x", "y"])), "-p x -p y");
    }

}

#[cfg(test)]
mod serial_observe_tests {
    use super::*;
    use crate::test_runner::{ObsEvent, Observation, StreamSource};

    fn plan(strict: bool) -> (SerialPlan, BinaryUnit) {
        let b = test_binary_for_tests("core", "test", "suite");
        let unit = BinaryUnit::of(&b);
        let plan = SerialPlan {
            resolution: None,
            units: HashMap::from([(b.executable.clone(), unit.clone())]),
            hashes: HashMap::new(),
            expected: Vec::new(),
            rustdoc: false,
            strict,
        };
        (plan, unit)
    }

    fn hear(observe: &SerialObserve<'_>, executable: &str) {
        (observe.sink())(Observation {
            stream: 1,
            source: StreamSource::Harness { executable: executable.into() },
            event: ObsEvent::Started { name: "t".into() },
        });
    }

    /// A lane with an inventory attributes its harness streams to the planned
    /// binaries whether or not the run is certifying: the plan attributes, and
    /// only a strict plan also refuses. A harness the plan does not hold is an
    /// unattributed stream, not an attribution to the wrong binary.
    #[test]
    fn a_plan_attributes_harness_streams_without_being_strict() {
        let tap = LaneTap::new(41_001);
        let (p, unit) = plan(false);
        let observe = SerialObserve { tap: &tap, plan: Some(p) };
        hear(&observe, "/t/debug/deps/suite-1");
        assert!(tap.seen_units().contains(&unit), "the planned harness is attributed to its binary");
        let other = LaneTap::new(41_002);
        let (p, _) = plan(false);
        let observe = SerialObserve { tap: &other, plan: Some(p) };
        hear(&observe, "/t/debug/deps/unknown-9");
        assert!(other.seen_units().is_empty(), "an unplanned harness is not attributed");
    }
}

