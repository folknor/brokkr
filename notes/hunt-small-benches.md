I found about 45 findings in scope. Several are real bugs, not only hygiene, and the first two change results today:

- **`thead` is excluded from element scoring.** `is_head_path` tests `path.contains("head[")`, and every `thead[N]` path contains that string, so all table headers drop out of the score.
- **Sluggrs `approve` loosens its own ratchet.** It copies `output.png` over `approved.png` but records the pixel diff against the *old* baseline. Later runs are compared to the new image but judged against the old number, so a real regression that size passes.

This was a read-only pass. Nothing was built or run, and every finding comes from reading the code and docs. Findings are grouped by your eight questions, not ranked. "Enforce:" says how the fixed version could be held mechanically.

## 1. One value, one owner

- **`results.db`'s `PRAGMA user_version` has three writers.** `ResultsDb` (`src/db/schema.rs`, sets 18), `MechanicalDb::migrate` (`src/litehtml/db.rs`, reads and sets 1) and `SnapshotDb` (no versioning at all) all open the same file via `resolve::results_db_path`.
  - Litehtml's `version < 1` migration is dead on any file `ResultsDb` has touched, and any future litehtml `version < 2` step will never run.
  - In the other direction, a legacy `runs` table at version 0 opened by litehtml first gets stamped 1, so `migrate_uuid` is skipped.
  - The comment in `src/cli/visibility.rs` says the two share "the file and nothing else", which is false. It also says sluggrs uses `MechanicalDb`; sluggrs uses `SnapshotDb`.
  - Enforce: give each subsystem its own DB file, or have one migration owner that knows every table. A test can open the file in either order and check both schemas.
- **The UUIDv4 generator exists four times:** `litehtml/mod.rs::generate_run_id`, `sluggrs/mod.rs::generate_run_id`, `db/types.rs::generate_uuid`, and inline in `harness_mod/types_run.rs`. Likewise `run_id[..8.min(..)]` is spelled three times beside `db::types::short_uuid`.
  - Enforce: a `[[textlint]]` banning `"/dev/urandom"` outside `db/types.rs` and `hold.rs`.
- **The litehtml and sluggrs `cmd.rs` files are near-clones:** `open_db`, `format_pct`, `print_table_header`, prefix-matching `resolve_fixture`/`resolve_snapshot`, `print_run_summary`, `format_status_columns`, and the clean-tree check in approve. The two `db.rs` files differ only in column names.
  - They have already diverged. Sluggrs fixed the `FAIL_THRESHOLD (FAIL_THRESHOLD)` double status (its comment describes that bug); litehtml `format_status_columns` still has `s => format!(" ({s})")`.
  - The right shape is one `visual/` module that is generic over the subject (fixture or snapshot). Enforce with a structural rule: no `crate::litehtml` imports from `sluggrs`.
- **Three inline-tag lists, and they disagree.** `compare.rs::is_inline_tag`, `prepare.js` `INLINE_ELEMENTS`, and litehtml-rs `is_inline_tag` (cross-repo, "keep in sync", unenforced).
  - The JS list lacks `font`, `big`, `del`, `ins` and `output`. `prettyPrint` therefore treats `<font>` (very common in email HTML) as a block and re-indents around it, which is the whitespace invention its own comment says was fixed.
  - Enforce: generate the JS list from one source, or add a Rust test that parses `prepare.js` and compares the sets. The cross-repo copy can only be kept in step by vendoring the list or adding a script check.
- **The fallback aspect ratio of 2.0** appears in `cmd.rs` (`unwrap_or(2.0)`) and in `prepare.js` (`|| 2.0`). The JS side also turns a configured 0 or a NaN into 2.0 without saying so.
- **The outline depth default of 4** appears three times: the clap `default_value`, the doc comment "(default: 4)", and `prepare.js` `opts.depth`. The JS copy can never be reached.
- **The viewport is split.** The default 800 is in `CAPTURE_JS` and also comes from config; height 600 and the 30 s timeout are only in the embedded JS.
- **The regression tolerance 0.5** is `REGRESSION_TOLERANCE` in `compare.rs`, but it is re-typed as `- 0.5` for "(improved)" in both `format_status_columns`. The delta-display cutoff 0.05 is also duplicated.
- **Scratch paths:**
  - `.brokkr/dellingr` is spelled in `dellingr/cmd.rs::SCRATCH_REL` and again in `main_parts/commands.rs::clean_artefact_trees`.
  - `.brokkr/mogwai` (`mogwai/cmd.rs`) copied the pattern, but clean was never taught about it, so these have already diverged and mogwai scratch is never cleaned.
  - Sluggrs hotpath uses `ctx.paths.scratch_dir` (`data/scratch`), which is exactly what dellingr's comment says to avoid.
  - `.brokkr/capture.js` and `.brokkr/prepare-cache` are not cleaned either.
  - Enforce: one `Project::scratch_rel()` read by both the writers and `clean`.
- **`snapshots/*/approved.png`** is built in `sluggrs/cmd.rs` and excluded by a literal pathspec in `git.rs::check_clean`. If sluggrs moves the directory, the exclusion stops matching without any warning.
- **The "NOTE: alloc profiling" banner** exists 8 times across the repo and has diverged (`--` vs `-` in nidhogg/elivagar).
- **The mode-to-features rule** (`uses_hotpath` plus the `if uses_hotpath { hotpath_features } else { features }` block) is copied into sluggrs, dellingr and mogwai. The dellingr doc says restating the feature names "would only create a way for them to disagree", but the selection logic itself is restated. Only mogwai dedups (`hotpath,hotpath`), and feature order in the recorded `cargo_features` differs between mogwai and the others.
  - Enforce: move it onto `MeasureRequest::build_features(registered)`.

## 2. Values nobody can find, change, or trust

- **`LitehtmlConfig.mode` and `LitehtmlFixture.expected` are free `String`s,** compared at use (`mode == "ahem"`, `expected == "fail"`). A typo like `"Ahem"` or `"failure"` silently turns into the other behaviour. Enforce with serde enums; they would be refused at load time.
- **No config-time validation** of duplicate fixture/snapshot ids (`fixture_by_id` takes the first), or of ids containing `/` or `..`. The id is joined straight into `fixtures/<id>`.
- **The comparison tuning has no injection point:** `FUZZ_THRESHOLD`, `POS_TOLERANCE`, `SIZE_TOLERANCE`, `ZERO_HEIGHT`, `REGRESSION_TOLERANCE` and `OFFENDER_PRINT_LIMIT` are consts scattered through `compare.rs` and `cmd.rs`, and nothing lists them.
- **`[litehtml]` and `[sluggrs]` are documented nowhere.** `brokkr.toml.md` points `[litehtml]` to `litehtml.md`, which says "See `docs/brokkr.toml.md` for full schema": a loop. `waive_element_threshold`, per-fixture `mode`/`viewport_width`/thresholds, `notes`, and the whole `[sluggrs]` schema have no doc.
- **`scripts_dir()` uses `env!("CARGO_MANIFEST_DIR")`,** so the installed binary depends on the brokkr source checkout still existing where it was built. `pnpm install` (not `--frozen-lockfile`) writes into brokkr's own tree from any litehtml project, and deps are only installed when `node_modules` is missing, so a `package.json` bump never re-installs.

## 3. One channel, one implementation

- **Dry-run output is invisible by default** for dellingr, mogwai and sluggrs hotpath. `[dry-run]` lines go through `bench_msg`, which is quiet-gated, and `run_measured` sets quiet unless `--verbose`. pbfhogg and elivagar use `run_msg`, which always prints. `hotpath.md` says `--dry-run` "prints them".
- **The mogwai bare index** sends its header through the gated `bench_msg` but the body through a raw `print!`, so without `-v` you get the list with no header.
- **Hint strings name commands that don't exist:**
  - "run `brokkr litehtml test` first" (litehtml `approve`) and "run `brokkr sluggrs test` first" (sluggrs `approve`). The real command is `brokkr visual`.
  - All the `project::require` labels ("litehtml test", "litehtml extract", "litehtml outline", "sluggrs status", ...) render as `'brokkr litehtml extract' is only available...`.
  - Enforce: derive the label from the clap subcommand name, or add a test that every `require` label is a key in `TABLE`.
- **`sluggrs/cmd.rs::sluggrs_msg`** is a wrapper that adds nothing.
- **Failures don't say why.** Sluggrs render failure prints only the first stderr line. Pixel-compare errors print nothing (see section 4).

## 4. Errors

- **Pixel-compare errors are dropped:** `Err(_) => (None, Status::Error)` in litehtml `score_fixture` and sluggrs `run_snapshot`. An unsupported 16-bit PNG shows up as a bare `ERROR`. Sluggrs `approve` goes further and maps a compare error to `0.0`.
- **Element-compare errors fail open.** `_ => (None, Vec::new())` turns an unreadable `pipeline.json` into "no element score", and then `determine_status` enforces neither the element threshold nor the ratchet. Approving that result stores `element_match_pct = NULL`, which switches the element ratchet off for good.
- **Litehtml `approve` records `pixel_diff_pct.unwrap_or(0.0)`,** so an `ERROR` row becomes an approved 0.0% baseline. `approve --all` does this to every errored fixture.
- **Litehtml and sluggrs handle a failing item differently.**
  - In litehtml, a capture or pipeline failure propagates with `?` and aborts the whole run. That leaves a `mechanical_runs` row with partial results, no summary, and possibly a stale `.brokkr/capture.js`. `report` then shows the run as if it were complete.
  - Sluggrs turns the same class of failure into one `ERROR` row and carries on.
- **Sluggrs `approve` is not atomic.** It does `fs::copy` to `approved.png` before `set_approval`; if the DB write fails, the image has changed but the record has not.

## 5. Tests that prove nothing or depend on the environment

- **Nothing tests the litehtml or sluggrs `cmd` and `db` code:** approve, status columns, prefix resolution, the shared-file schema interaction. The divergence in section 1 is exactly what such a test would catch.
- **`smoke.js` runs nowhere.** It needs node plus an installed `node_modules`, no `[[script_check]]` or test runs it, and it cites `HARNESS-IMPROVEMENTS.md`, which is not in this repo. It writes `smoke-tmp/` into the source tree, which is not gitignored. `visual.md` presents it as the assertion of the fidelity rules.
- **Capture depends on the host:** a globally installed puppeteer found through `npm root -g` (`NODE_PATH`), spawned once per captured fixture. The puppeteer version is not pinned, and `chrome.meta.json` exists only to explain drift afterwards. Adding puppeteer to `package.json` and the lockfile would make capture reproducible.
- **The `FUZZ_THRESHOLD` const-asserts only pin the literal 13;** they don't compute "5% of 255".
- **The `br_detected_by_tag_or_path` comment is stale.** It talks about a "`br[` prefix probe", but `is_br` compares tags for equality.
- **`head_paths_filtered` never tests `thead`,** which is why the bug at the top survived.

## 6. Guards and claims that have stopped holding

- **`is_head_path`'s `contains("head[")`** also matches `thead[N]`, so whole `thead` subtrees leave element scoring. This is a bug today.
- **`visual --suite` and `--recapture` are ignored silently on sluggrs.** Help says "(litehtml only)", but the dispatcher just drops them. Passing `--all` or `--suite` together with an ID also drops the ID without saying so.
- **Stale outputs pass the existence checks.** `pipeline.png`, `pipeline.json` and `chrome.json` are checked with `exists()`, not freshness, so a pipeline that stops writing one is compared against the previous run's file. A missing `chrome.png` is recaptured inside the test run, so a deleted reference quietly becomes a fresh one.
- **`visual.md`'s approve claim is not enforced.** It says the clean-tree check exists "so that the commit an approval is pinned to actually describes what rendered the image". But `approve` pins HEAD to the *latest result/`output.png`*, which may come from a dirty run or an older commit. `mechanical_runs.dirty/commit` is never consulted.
- **The sluggrs approve ratchet** described at the top: the new baseline image is the output, while the recorded pixel diff is against the old image.
- **Visual commands ignore the one-level-up layout.** `visual`, `approve` and `git::collect` all use `project_root` for the build and for git. The dispatcher has `build_root` in hand and does not pass it.
- **Stale or false doc claims:**
  - `dellingr.md` says the pair key is `(command, mode, input_file)`. It actually has five parts (`db/format/compare.rs::pair_key`), and `hotpath.md` states it correctly.
  - `hotpath.md` says "Rows carry `n/a` for dataset/variant". Those values never reach the DB; the dispatch comment for dellingr has the same wrong premise.
  - `dellingr.md` says the marker FIFO lives in `.brokkr/dellingr/`. For `--bench`, `run_external_inner` creates it in `db_dir`.
  - `mogwai.md` "Datasets" says an entry records "whether the bytes moved under a recorded row". The mogwai bench path never reads datasets and rows carry no digest, so the question cannot be answered.
  - `visual.md` describes an `expected_fail` flag; the field is `expected = "fail"`. Its intro says both projects compare against Chrome; sluggrs compares against `approved.png`.
- **`visibility.rs` says each handler's `project::require` is "the authoritative refusal".** For `visual`, `list`, `report` and `visual-status` the dispatcher's `match project` is the gate, so the inner `require` calls can never fire. `hotpath` is required twice (dispatcher and `sluggrs::hotpath::cmd`).
- **Litehtml `expected = "fail"` fixtures that start passing** report plain `PASS` with no "unexpected pass" signal.

## 7. Policy invented per call site

- **Exit policy lives in positional `counts: [u32; 4]` arrays with different index meanings** in each project. `NoBaseline` counts as a failure in litehtml (the `_` arm) and as a non-failure in sluggrs. Enforce with a `Status::is_failure()` used by both.
- **Status goes to the DB as TEXT and is matched back as string literals** (`"PASS"`, `"NO_BASELINE"`); there is no `FromStr`, and an unknown value falls through silently.
- **`determine_status` takes 7 positional arguments,** including two adjacent `Option<f64>` approvals that can be swapped without a compile error. A struct would make that unrepresentable.
- **The working directory for the child differs by command:** sluggrs hotpath runs it in `build_root`, dellingr and mogwai in `project_root`.
- **`latest_result_for_*` orders by `datetime('now')`,** which has one-second resolution, so two runs in the same second are ambiguous.
- **`prepare.js` cache growth is unbounded:** the image cache and permanent `.miss` negative entries (a transient failure is cached forever), and `fetchUrl` has no response size cap. Its option parser ignores unknown flags.

## 8. Code that is no longer load-bearing

- **`latest_run` and `RunSummary`** in litehtml are `#[allow(dead_code)]` with no callers; `latest_run` in sluggrs is likewise uncalled.
- **Dead fields hidden behind blanket allows:**
  - `PixelDiffResult.total_pixels`/`diff_pixels` are unused.
  - `ElementMatchResult.total_elements`/`passing_elements` are read only by tests.
  - `SnapshotMeta.backend` and `LitehtmlFixture.notes` are unused.
- **Stale allows:** the `#[allow(dead_code)]` on sluggrs `map_run_summary` (it is used), and on the `SluggrsSnapshot` struct (every field is read).
- **Litehtml's `artifact_dir` migration** is a compatibility path, and it is the only reason litehtml writes `user_version` at all.
- **The `(Some, None) | (None, Some)` arm in `dellingr::workload::resolve`** is unreachable (the parser rejects that case) and carries a second wording of the parser's error.
- **`MeasureRequest.dataset`/`variant`** are passed as `""` or `"n/a"` for dellingr, mogwai and sluggrs, and none of them read either value.
- **The `print!` in `outline`** is fine as data output; I'm noting it only because it is the one raw stdout write in `litehtml/cmd.rs`.

## Enforcement already available

The repo's `brokkr.toml` defines only 3 `[[textlint]]` rules, and none touch this scope. The engines already in brokkr could hold most of the fixes above:
- **`[[textlint]]`:** ban `/dev/urandom` and `"NOTE: alloc profiling"` outside their owner files.
- **`[[dependency_rule]]`:** keep `sluggrs` from importing `crate::litehtml`, once the shared `visual/` module exists.
- **Serde enums and `deny_unknown_fields`:** `mode` and `expected`.
- **Unit tests:** `require` labels against `TABLE`, the inline-tag list parity, and opening `results.db` in both orders.

Cross-repo copies (litehtml-rs `is_inline_tag`) and the host Node/puppeteer dependency can't be enforced from inside this build short of vendoring them.

Key files: `/home/folk/Programs/brokkr/src/litehtml/{cmd,compare,db,mod}.rs`, `/home/folk/Programs/brokkr/src/sluggrs/{cmd,db,hotpath,mod}.rs`, `/home/folk/Programs/brokkr/src/dellingr/{cmd,workload}.rs`, `/home/folk/Programs/brokkr/src/mogwai/cmd.rs`, `/home/folk/Programs/brokkr/scripts/litehtml-prepare/{prepare,smoke}.js`, `/home/folk/Programs/brokkr/src/main_parts/{bootstrap,commands}.rs`, `/home/folk/Programs/brokkr/src/git.rs`, `/home/folk/Programs/brokkr/src/cli/visibility.rs`, `/home/folk/Programs/brokkr/docs/{commands/visual.md,commands/hotpath.md,projects/litehtml.md,projects/dellingr.md,projects/mogwai.md}`.
