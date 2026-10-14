# Bugs

Defects turned up by the design hunt, plus any that the hygiene documents had
already described inside a broader entry; those were moved here whole. Siblings:
`consolidations.md` (CON), `structural.md` (STR), and the hygiene documents
(`hygiene-*.md`). Entries record what hunters reported; nothing has been
verified. Entries marked *suspected* are ones the hunter flagged without
demonstrating them.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

## BUG-003 - The stray reaper kills brokkr's own parent cargo; the orphan reaper exempts ancestors

Reported by: process-control.

`stray.rs`'s test `descendant_expansion_never_crosses_into_brokkrs_own_subtree` asserts that in `cargo(10) -> brokkr(20, us) -> cargo(30)` the outer cargo is a stray **and is killed**. Expansion stops at brokkr, but the root itself is killed. So `cargo run --bin brokkr -- check`, or any brokkr started under a cargo (a script check run by an outer brokkr's cargo, an xtask), SIGKILLs its own parent cargo at the first fresh hold. `test_orphans::select` makes the opposite choice on purpose ("Exempt: this process, its ancestors and its descendants"). Neither reaper mentions the other, and a test on each side asserts its own answer. Even if killing the parent is intended (it does hold the target-dir lock), the decision should be stated once, in one exemptions type that both reapers use.

## BUG-015 - A `--stop` run records the configured marker, not whether it fired

Reported by: measurement.

The row's `stop_marker` stores the configured spelling, not the marker that fired, and not whether any marker fired at all. A `--stop X` run whose marker never appeared records `stop_marker = X` beside a full-length wall, which is indistinguishable from a run that stopped. `stop_marker: Option<&str>` is also re-interpreted on every FIFO drain by `stop_match`, which accepts three spellings. The hunter proposes parsing it once into `StopSpec { Exact | SpanEnd | Lenient }` and recording the resolved marker as part of how the child ended (filed in `structural.md`).

## BUG-031 - ratatoskr fixture names can escape the fixtures directory

Reported by: a wave-1 fixer, laterally.

`ratatoskr/saehrimnir.rs::resolve_fixture` joins the frontmatter or config fixture name straight onto `fixtures_dir`. An absolute name, or one containing `..`, resolves outside the configured fixture tree. Refuse names that are not a single normal path component (or canonicalise and check containment).

## BUG-032 - *Suspected:* the guard's idle admission drops its compile lease before rustc runs

Reported by: a wave-1 fixer, laterally.

In `src/bin/rustc_guard.rs`, the guard takes a shared compile lease, but the "no hold exists" decision releases it before `exec`. A hold acquired between the probe and the exec drains no lease and can overlap that rustc. `docs/commands/check.md` already calls the exclusion guarantee conditional; whether this window is accepted or should keep the lease through `exec` is the open question.

## BUG-033 - `deps` focus `tildify` mangles paths when `$HOME` is empty

Reported by: a wave-1 fixer, laterally.

`deps/focus.rs::tildify` reads `HOME` directly; with an empty value every absolute path matches the empty prefix and renders as `~...`. It should use `user_dirs::home()`, which treats empty as unset.

## BUG-034 - `merge-changes` still requires a PBF it never reads

Reported by: a wave-1 fixer, laterally.

`pbfhogg/dispatch.rs::build_pbfhogg_context` resolves a PBF unconditionally, including for `InputKind::OscOnly`, and the resolver checks the file exists. So `merge-changes` fails when the dataset's PBF is absent. The fix needs `CommandContext.pbf_path` to become optional (or the resolved-inputs enum proposed in `structural.md`).

## BUG-035 - piners comparison treats an unparsable or legacy selector as an empty selection

Reported by: a wave-1 fixer, laterally.

`piners/corpus_db/compare.rs::selected_ids()` resolves a selector it cannot parse, or one from before the `ids` field, to an empty list, so every emitted probe in such a run is marked report-only. For legacy rows the original selection may be unrecoverable, but the result should read as "selection unknown", not "nothing selected".

## BUG-037 - Run mode's dirty-tree gate is lifted per command, not by the mode

Reported by: the wave-1 reviewer, laterally.

The harness now stores nothing in Run mode, so a dirty tree is harmless there. nidhogg `api`/`tiles` and elivagar `planetiler`/`tilemaker` lift the dirty gate for run mode, but `mogwai`, `dellingr` and the elivagar `cmd.rs` handlers still pass `req.force` through, so a dirty tree refuses a run that stores nothing. Decide it once (in `HarnessContext::with_request` or the dirty check itself) from the mode.

## BUG-038 - Wave-1 behaviour changes that went further than their defect

Reported by: the wave-1 reviewer.

Each is defensible but unasked for; confirm or narrow:
- `check_cmd/direct_runtime.rs::refuse_configured_runner` now treats an empty `CARGO_BUILD_TARGET` or `CARGO_TARGET_<host>_RUNNER` as configured and refuses direct execution. Cargo may treat empty as unset, in which case this over-refuses.
- `guard --remove`/`--install` now refuse a `brokkr-rustc-guard` wrapper from a different install path. If that install no longer exists, the user must hand-edit `~/.cargo/config.toml`, and the message points at "the install that owns it".
- A single non-UTF-8 tracked filename now fails gremlins, header, textlint and `brokkr wc`, not only gremlins.
- The `deps` NDJSON `git_dependency` event changed shape (`requested_ref {kind,value}` plus `resolved_rev`) with no schema bump.
- `.brokkr/parallel-timings.toml` keys moved from `<package>/<target>` to `<package>/<kind>:<target>`; old entries are orphaned and never pruned.

## BUG-036 - `brokkr results --limit` silently does not apply to uuid and compare views

Reported by: a wave-1 fixer, laterally.

The flag is described as a maximum row count, but the uuid lookup and `--compare` ignore it. Skipping it for compare may be deliberate (truncation would break pairing), but the help text does not say so; either scope the flag in clap or document it.
