#!/usr/bin/env bash
# Smoke test: certifies + skip_phases + --gate.
#
# Generates a throwaway crate under scratch/certifies-smoke and drives
# brokkr check through the partial/complete/gate paths. The verdict
# scenarios assert on exit codes (the 0/10/1 contract, clap's 2 for flag
# conflicts). The coverage-failure scenarios cannot stop there: exit 1 is
# brokkr's universal failure code (config, gremlin, clippy, build, test all
# produce it), so they additionally parse the `--json` summary (schema 2) and
# assert on `failed_phase`, `policy_coverage.*` and `execution_accounting.*` -
# proving each fails IN the phase it should, for the right reason, not for
# some unrelated reason that never ran the audit. Requires `jq`.
#
# The binary under test is `$BROKKR_BIN` (default: `brokkr` on PATH), so a
# fresh build can be smoke-tested without installing it. Run from the brokkr
# repo root:
#
#   bash scripts/smoke-certifies.sh
#   BROKKR_BIN=/path/to/brokkr bash scripts/smoke-certifies.sh
#
# The generated directory is left behind for inspection; it is disposable
# and regenerated on every run.
set -u

root="$(cd "$(dirname "$0")/.."; pwd)"
brokkr_bin="${BROKKR_BIN:-brokkr}"
smoke="$root/scratch/certifies-smoke"
rm -rf "$smoke"
mkdir -p "$smoke/src"

# Every member a default member, so the bare selection IS the workspace: a
# complete profile under `doctests = true` needs a workspace-shaped doctest
# carrier (`packages` empty and the bare selection the whole workspace).
cat > "$smoke/Cargo.toml" <<'EOF'
[package]
name = "certifies-smoke"
version = "0.1.0"
edition = "2021"

[workspace]
members = ["member", "pm"]
default-members = [".", "member", "pm"]
resolver = "2"
EOF

# Proc-macro test binaries link libstd dynamically (rustc dlopens
# proc-macro crates), so enumerating one is the regression test for the
# loader-path fix: direct-exec --list must supply the toolchain libdir.
mkdir -p "$smoke/pm/src"
cat > "$smoke/pm/Cargo.toml" <<'EOF'
[package]
name = "pm"
version = "0.1.0"
edition = "2021"

[lib]
proc-macro = true
EOF

cat > "$smoke/pm/src/lib.rs" <<'EOF'
use proc_macro::TokenStream;

#[proc_macro]
pub fn noop(input: TokenStream) -> TokenStream {
    input
}

#[cfg(test)]
mod tests {
    #[test]
    fn pm_unit_test_runs() {
        assert_eq!(2 + 2, 4);
    }
}
EOF

mkdir -p "$smoke/member/src"
cat > "$smoke/member/Cargo.toml" <<'EOF'
[package]
name = "member"
version = "0.1.0"
edition = "2021"
EOF

# The same `shared::` module path as the root package - textually
# indistinguishable by name-based skips, the feature-11 case.
cat > "$smoke/member/src/lib.rs" <<'EOF'
pub fn double(x: u64) -> u64 {
    x * 2
}

#[cfg(test)]
mod shared {
    #[test]
    fn member_only() {
        assert_eq!(super::double(2), 4);
    }
}

// Outside `shared::` on purpose: the package-scoped sweep needs a test the
// gate-scoped profile's `shared::` skip does not reach, or that sweep would
// evaluate nothing and the run-time guard would fire instead.
#[cfg(test)]
mod own {
    #[test]
    fn member_scoped_probe() {
        assert_eq!(super::double(3), 6);
    }
}
EOF

cat > "$smoke/src/lib.rs" <<'EOF'
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add(2, 2), 4);
    }

    // Skipped by the gate lanes; justified by the [[quarantine]] entry.
    #[test]
    fn skipme_flaky() {
        assert_eq!(super::add(1, 1), 2);
    }

    // Source-level suppression: counted as ignored by coverage, not orphaned.
    #[test]
    #[ignore]
    fn ignored_manual() {
        assert_eq!(super::add(3, 3), 6);
    }
}

#[cfg(test)]
mod shared {
    #[test]
    fn runs_in_root() {
        assert_eq!(super::add(2, 3), 5);
    }
}
EOF

# The diagnostic-continuation fixture: one integration binary whose first test
# (libtest runs in name order) hangs only when SMOKE_HANG is set, followed by
# two ordinary tests that, under --test-threads=1, are still queued when the
# per-test cap kills the hang. Without the variable every test passes
# instantly, so every other scenario is unaffected. Names avoid the substrings
# the profiles' skip and only filters use (skipme, shared::, tests::adds).
mkdir -p "$smoke/tests"
cat > "$smoke/tests/hang.rs" <<'EOF'
#[test]
fn hang_probe() {
    if std::env::var_os("SMOKE_HANG").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[test]
fn zz_queued_one() {
    assert_eq!(2 + 2, 4);
}

#[test]
fn zz_queued_two() {
    assert_eq!(3 + 3, 6);
}
EOF

cat > "$smoke/brokkr.toml" <<'EOF'
project = "brokkr"

# Workspace-shaped (no `packages`): the doctest carrier a complete profile
# under `doctests = true` requires.
[[check]]
name = "default"

[test]
doctests = true
default_profile = "edit"
gate_profile = "gate"

# The loop answer: bare `brokkr check`. Partial, skips clippy, exits 10.
[test.profiles.edit]
certifies = "partial"
skip_phases = ["clippy"]
sweeps = ["default"]

# The gate: `brokkr check --gate`. Complete via two lanes sharing the
# default sweep - clippy dedupes on build shape, the test phase runs both,
# and the coverage phase audits the skipme skip against [[quarantine]].
# The serial lane runs every harness through the strict shim (attributable,
# so a complete profile accepts it - cargo-mediated `test_threads = 0` is
# refused, see gate-threaded) and is the doctest carrier.
[test.profiles.lane-serial]
sweeps = ["default"]
skip = ["skipme", "shared::"]

# The isolated lane runs root's shared:: but package-skips member's -
# a name-based skip cannot make that distinction (feature 11).
[test.profiles.lane-iso]
sweeps = ["default"]
isolation = "process"
skip = ["skipme", { package = "member", pattern = "shared::" }]

# The diagnostic-continuation case: a PARTIAL profile whose serial lane sets
# SMOKE_HANG, so tests/hang.rs's first test outlives the per-test cap. Serial
# (test_threads = 1) so the later tests of that binary are still queued, and
# the other binaries still ahead of the kill, when it fires.
[test.profiles.hang-partial]
certifies = "partial"
skip_phases = ["clippy"]
sweeps = ["default"]
test_threads = 1
env = { SMOKE_HANG = "1" }

[test.profiles.gate]
certifies = "complete"
lanes =["lane-serial", "lane-iso"]

# The same two lanes lifting `#[ignore]`. Preparation lists each binary's
# ignored subset with `--ignored`, and libtest refuses that beside
# `--include-ignored` - so an include-ignored lane once could not be
# prepared at all, serial or isolated. Spelled out rather than `extends`:
# a complete claim refuses a lane that inherits its filters.
[test.profiles.lane-serial-ig]
sweeps = ["default"]
skip = ["skipme", "shared::"]
include_ignored = true

[test.profiles.lane-iso-ig]
sweeps = ["default"]
isolation = "process"
skip = ["skipme", { package = "member", pattern = "shared::" }]
include_ignored = true

[test.profiles.gate-ignored]
certifies = "complete"
lanes = ["lane-serial-ig", "lane-iso-ig"]

# Cargo-mediated parallelism shares one stream across tests and harnesses,
# so no execution on it can be attributed: a complete profile refuses it in
# `prepare`, pointing at `parallel = { budget = N }`.
[test.profiles.gate-threaded]
certifies = "complete"
sweeps = ["default"]
test_threads = 0
skip = ["skipme", "shared::"]

# Coverage failure modes, driven below: an unjustified skip (orphan) and
# a quarantine entry justifying nothing (stale).
[test.profiles.gate-orphan]
certifies = "complete"
sweeps = ["default"]
skip = ["adds", "skipme", "shared::"]

[test.profiles.gate-stale]
certifies = "complete"
sweeps = ["default"]

# The false-death regression: one profile-level skip list across an
# unscoped sweep and a package-scoped one. `skipme` and root's `shared::`
# name tests that exist only in certifies-smoke, so both are necessarily
# dead in the member-scoped sweep while doing their job in the unscoped
# one. Judged per sweep this reported two dead filters and there was no
# way to silence it but to stop scoping sweeps or stop skipping tests.
# `curated` because a complete profile's universe is every [[check]] entry:
# an entry the other gate profiles do not reference has to declare itself
# outside the universe. Its own `only` is entry-scoped, so it is judged
# against this entry's sweeps alone - and it must survive the profile's
# `shared::` skip, which is why `own::` exists in member.
[[check]]
name = "scoped"
packages = ["member"]
curated = true
only = ["member_scoped_probe"]

[test.profiles.gate-scoped]
certifies = "complete"
sweeps = ["default", "scoped"]
skip = ["skipme", "shared::"]

# A skip naming a test that no longer exists, and an `only` whose every
# match the skip above removes. Neither subtracts anything from the lane's
# claim, so the orphan audit cannot see either - the alive-check must.
[test.profiles.gate-dead-skip]
certifies = "complete"
sweeps = ["default"]
skip = ["renamed_away_long_ago", "skipme", "shared::"]

# The live sibling is the point: `tests::adds` selects real work, so the
# lane runs tests and looks healthy, while `skipme` - matched by the only,
# removed by the skip - selects none. A lane where NO filter matches is
# caught earlier and elsewhere, by the test phase's `zero tests ran`
# refusal; this is the case only the alive-check sees.
[test.profiles.gate-dead-only]
certifies = "complete"
sweeps = ["default"]
skip = ["skipme"]
only = ["skipme", "tests::adds"]

[[quarantine]]
pattern = "skipme"
issue = "B1"
reason = "flaky teardown; tracked upstream"

# Package-scoped: justifies member's shared:: pairs only; root's
# same-named pairs stay auditable.
[[quarantine]]
package = "member"
pattern = "shared::"
issue = "B2"
reason = "member's shared suite needs a service the gate host lacks"
EOF

cd "$smoke"
git init -q
git add -A

if ! command -v jq >/dev/null 2>&1; then
  echo "smoke: jq is required (the coverage scenarios assert on the --json summary)" >&2
  exit 2
fi

fail=0
expect() {
  desc="$1"
  want="$2"
  got="$3"
  if [ "$got" -eq "$want" ]; then
    echo "ok   $desc (exit $got)"
  else
    echo "FAIL $desc: want exit $want, got $got"
    fail=1
  fi
}

# Runs the brokkr under test with "$@", capturing stdout - whose LAST line,
# under --json, is the machine-readable summary - to a file so a scenario can
# assert on the summary's discriminating fields. stderr streams live; stdout
# is echoed afterwards so the run is still visible. Sets `rc` (exit code) and
# `summary` (the JSON trailer). A resolve-time error emits no summary, so
# `summary` is then a human line and jq fails to parse it - which correctly
# fails the assertion rather than passing vacuously.
summary=""
rc=0
check_json() {
  local out="$smoke/check.stdout"
  "$brokkr_bin" "$@" >"$out"
  rc=$?
  cat "$out"
  summary="$(tail -n 1 "$out")"
}

# Asserts a jq boolean filter holds against the captured `summary`. Keeps the
# summary in the failure line so a broken audit is debuggable.
expect_json() {
  desc="$1"
  filter="$2"
  local got
  got="$(printf '%s' "$summary" | jq -r "$filter" 2>/dev/null)"
  if [ "$got" = "true" ]; then
    echo "ok   $desc"
  else
    echo "FAIL $desc: jq '$filter' => '$got' (summary: $summary)"
    fail=1
  fi
}

# A green complete run: schema 2, both claims passed, every expected
# execution passed with nothing anomalous, and every planned doctest carrier
# known to have completed (each presented at least one rustdoc stream).
green='.schema == 2 and .failed_phase == null and .termination == null
  and .policy_coverage.status == "passed" and .policy_coverage.plan_complete
  and .policy_coverage.orphaned == 0 and .policy_coverage.dead_filters == 0
  and .execution_accounting.scope == "binary_tests"
  and .execution_accounting.status == "complete"
  and .execution_accounting.expected_executions > 0
  and .execution_accounting.passed == .execution_accounting.expected_executions
  and .execution_accounting.anomalies == 0
  and .doctests.inventory == "unavailable" and .doctests.accounting == "unknown"
  and (.doctests.observed.carriers | length) >= 1
  and (.doctests.observed.carriers | all(.completed and .streams >= 1))'

echo "=== bare check: partial default profile ==="
"$brokkr_bin" check --json
expect "bare check = partial" 10 $?

echo "=== --gate: complete profile ==="
check_json check --gate --json
expect "--gate = complete" 0 $rc
# The point of a complete gate is that the audit RAN: a green exit with null
# accounting blocks would be a pass that certified nothing. Two lanes over
# one sweep, the quarantine, the package-qualified skip and the proc-macro
# member are all inside this one green.
expect_json "--gate certified policy and execution" "$green"
# The quarantine and the source-level #[ignore] are policy facts, never
# execution outcomes: the lanes do not lift #[ignore], so nothing expected is
# ignored. Two lanes over one sweep select shared pairs twice, so there are
# more expected executions than selected pairs.
expect_json "--gate keeps policy and execution apart" \
  '.policy_coverage.quarantined > 0 and .policy_coverage.ignored > 0
   and .execution_accounting.ignored == 0 and .execution_accounting.unobserved == 0
   and .execution_accounting.expected_executions > .policy_coverage.selected'

echo "=== --profile gate-ignored: include_ignored lanes under a complete claim ==="
check_json check --profile gate-ignored --json
expect "include_ignored complete = exit 0" 0 $rc
# Preparation lists the ignored subset with `--ignored` minus the lane's
# `--include-ignored`; with both, libtest refused the listing and the plan
# could not be prepared (failed_phase "prepare"). `ignored_manual` is now an
# expected execution on both lanes, and it passed.
expect_json "gate-ignored prepared, ran and certified" "$green"

echo "=== --profile gate-threaded: cargo-mediated parallelism is refused ==="
check_json check --profile gate-threaded --json
expect "test_threads = 0 under complete = exit 1" 1 $rc
# Refused in `prepare`, before any test runs: the plan is incomplete, and the
# accounting blocks still report it rather than going null.
expect_json "gate-threaded refused in prepare with an incomplete plan" \
  '.failed_phase == "prepare" and .policy_coverage.plan_complete == false
   and .policy_coverage.status == "incomplete"
   and .execution_accounting.status == "incomplete"'

echo "=== --profile gate-orphan: unjustified skip fails coverage ==="
check_json check --profile gate-orphan --json
expect "orphaned pair = exit 1" 1 $rc
# Must fail IN the coverage phase with orphaned pairs - not at load, not in
# build/test. `adds` and root's `shared::` are skipped but unquarantined.
expect_json "gate-orphan failed on coverage with orphans" \
  '.failed_phase == "coverage" and .policy_coverage.status == "failed"
   and .policy_coverage.orphaned > 0'

echo "=== --profile gate-stale: quarantine justifying nothing fails ==="
check_json check --profile gate-stale --json
expect "stale quarantine = exit 1" 1 $rc
# The stale signature: policy failed with zero orphans (every test ran, so
# the two [[quarantine]] entries justify nothing) while every execution
# passed. Distinguishes stale from orphan, and both from any non-coverage
# failure.
expect_json "gate-stale failed on coverage with no orphans" \
  '.failed_phase == "coverage" and .policy_coverage.status == "failed"
   and .policy_coverage.orphaned == 0 and .execution_accounting.status == "complete"'

echo "=== --profile gate-scoped: a profile filter spans its sweeps ==="
check_json check --profile gate-scoped --json
# Orphans are beside the point here (the scoped sweep audits member's own
# pairs); what must hold is that NO filter is reported dead. Both skips are
# live in the unscoped sweep and dead in the scoped one, which is the shape
# that per-sweep judging turned into a false gate failure.
expect_json "gate-scoped reports no dead filters" \
  '.policy_coverage != null and .policy_coverage.dead_filters == 0'

echo "=== --profile gate-dead-skip: a skip matching nothing fails ==="
check_json check --profile gate-dead-skip --json
expect "dead skip = exit 1" 1 $rc
# Exactly one filter is dead - the other two match. A dead filter moves no
# pair between buckets, which is exactly why the orphan audit cannot see it
# and this count has to exist.
expect_json "gate-dead-skip failed on coverage with one dead filter" \
  '.failed_phase == "coverage" and .policy_coverage.dead_filters == 1'

echo "=== --profile gate-dead-only: an only selecting nothing fails ==="
check_json check --profile gate-dead-only --json
expect "dead only = exit 1" 1 $rc
# `skipme` exists and is matched by the `only`, but the lane's `skip` removes
# it - so "matched something" holds while the filter selects no work. The
# live sibling keeps the lane running tests and the test phase green, which
# is the whole reason a folded assertion would miss this: the failure has to
# land in `coverage`, with exactly one of the two filters named.
expect_json "gate-dead-only failed on coverage with one dead filter" \
  '.failed_phase == "coverage" and .policy_coverage.dead_filters == 1'

echo "=== --profile hang-partial: a watchdog kill names what it left, and the replay works ==="
# Proves the downstream's real case end to end: a hang on a serial lane under
# a partial profile (no policy universe, nothing certified) must still name
# every test the kill left without a verdict, print a command that reaches
# them, and that command must run exactly them. Takes about the per-test cap
# (20s) because the hang is real.
check_json check --profile hang-partial --json
if [ "$rc" -ne 0 ]; then
  echo "ok   hang-partial fails (exit $rc)"
else
  echo "FAIL hang-partial fails: want nonzero exit, got 0"
  fail=1
fi
expect_json "hang-partial carries a continuation, certifying nothing" \
  '.diagnostic_continuation != null and .diagnostic_continuation.certifies == false
   and .termination != null and .policy_coverage == null'
# The queued tests never started, so they are unobserved. The test that timed
# out stands as its own result and is NOT a candidate; nothing may be both.
expect_json "queued tests are unobserved candidates" \
  '.diagnostic_continuation.candidates as $c
   | ["zz_queued_one", "zz_queued_two"]
   | all(. as $n | $c | any(.test == $n and .outcome == "unobserved"))'
expect_json "the timed-out test is not a candidate; every candidate is unresolved" \
  '.diagnostic_continuation.candidates
   | all(.test != "hang_probe" and (.outcome == "unobserved" or .outcome == "interrupted"))'
expect_json "the replay command is test --from-run with the run id" \
  '.diagnostic_continuation as $d
   | ($d.source_run_id | test("^[0-9]+-[0-9]+$"))
     and $d.replay.available == true
     and $d.replay.command.argv == ["brokkr", "test", "--from-run", $d.source_run_id]
     and $d.replay.command.display == "brokkr test --from-run \($d.source_run_id)"'

run_id="$(printf '%s' "$summary" | jq -r '.diagnostic_continuation.source_run_id')"
want_names="$(printf '%s' "$summary" | jq -r '.diagnostic_continuation.candidates[].test' | sort)"
want_count="$(printf '%s' "$summary" | jq -r '.diagnostic_continuation.candidates | length')"
# The recovery path: the original run's captured output is gone (here
# deleted; in life a hard exit that never printed it), and only the persisted
# record remains.
rm -f "$smoke/check.stdout"

# Candidate lines of the text report: after the optional output prefix, two
# spaces, the outcome word, then the test name. The closing notes also say
# "unobserved" but never start that way.
candidate_re='^(\[[a-z]+\] +)?  (unobserved|interrupted) +([^ ]+)'
list_names() {
  local line
  while IFS= read -r line; do
    if [[ "$line" =~ $candidate_re ]]; then
      printf '%s\n' "${BASH_REMATCH[3]}"
    fi
  done <"$1" | sort
}

echo "--- replay --list: executes nothing, same candidates, from the record alone ---"
cp -r ".brokkr/accounting/$run_id" "$smoke/source-record"
list1="$smoke/replay-list1.out"
"$brokkr_bin" test --from-run "$run_id" --list >"$list1" 2>&1
expect "--from-run --list = exit 0" 0 $?
cat "$list1"
got_names="$(list_names "$list1")"
if [ -n "$want_names" ] && [ "$got_names" = "$want_names" ]; then
  echo "ok   --list prints the summary's candidates ($want_count)"
else
  echo "FAIL --list prints the summary's candidates: want [$want_names], got [$got_names]"
  fail=1
fi
# --list writes no record of its own: the run count is the baseline the
# replay's single new record is measured against below.
runs=(.brokkr/accounting/*)
runs_before_replay="${#runs[@]}"

echo "--- replay: runs exactly the unresolved tests, diagnostic_completed ---"
replay_out="$smoke/replay.out"
"$brokkr_bin" test --from-run "$run_id" >"$replay_out" 2>&1
expect "replay = exit 0" 0 $?
cat "$replay_out"
if [[ "$(<"$replay_out")" =~ diagnostic_completed\ -\ ([0-9]+)\ execution ]] \
   && [ "${BASH_REMATCH[1]}" -eq "$want_count" ]; then
  echo "ok   replay reported diagnostic_completed over exactly $want_count executions"
else
  echo "FAIL replay reported diagnostic_completed over $want_count executions"
  fail=1
fi
# SMOKE_HANG rides the recorded environment, so re-running the timed-out test
# would have hung again: the zero exit above already says it was not
# selected. This is the same fact read off the output.
if grep -q hang_probe "$replay_out"; then
  echo "FAIL replay did not re-run the timed-out test: hang_probe appears in its output"
  fail=1
else
  echo "ok   replay did not re-run the timed-out test"
fi
# The continuation wrote its own record, and the source's is byte-identical
# (plan and journal) and still reports the same candidates.
runs=(.brokkr/accounting/*)
runs_after_replay="${#runs[@]}"
if [ "$runs_after_replay" -eq $((runs_before_replay + 1)) ]; then
  echo "ok   the continuation wrote its own record"
else
  echo "FAIL the continuation wrote its own record: $runs_before_replay runs before, $runs_after_replay after"
  fail=1
fi
if cmp -s "$smoke/source-record/plan.json" ".brokkr/accounting/$run_id/plan.json" \
   && cmp -s "$smoke/source-record/journal.jsonl" ".brokkr/accounting/$run_id/journal.jsonl"; then
  echo "ok   the source run's plan and journal are unchanged"
else
  echo "FAIL the source run's plan or journal changed under the replay"
  fail=1
fi
list2="$smoke/replay-list2.out"
"$brokkr_bin" test --from-run "$run_id" --list >"$list2" 2>&1
if cmp -s "$list1" "$list2"; then
  echo "ok   the source run still reports the same continuation"
else
  echo "FAIL the source run's --list report changed after the replay"
  fail=1
fi

echo "=== a filter under the length floor is a load-time error ==="
cp brokkr.toml brokkr.toml.bak
printf '\n[test.profiles.degenerate]\nsweeps = ["default"]\nskip = ["ser"]\n' >> brokkr.toml
"$brokkr_bin" check --profile edit
expect "three-character filter = config error" 1 $?
mv brokkr.toml.bak brokkr.toml

echo "=== --gate -p: rejected by clap ==="
"$brokkr_bin" check --gate -p certifies-smoke
expect "--gate -p = usage error" 2 $?

echo "=== --profile gate -p: rejected at resolve time ==="
"$brokkr_bin" check --profile gate -p certifies-smoke
expect "complete + -p = config error" 1 $?

echo "=== --profile edit -p: scoped partial ==="
"$brokkr_bin" check --profile edit -p certifies-smoke --json
expect "partial + -p = exit 10" 10 $?

if [ "$fail" -eq 0 ]; then
  echo "smoke: all scenarios passed"
fi
exit $fail
