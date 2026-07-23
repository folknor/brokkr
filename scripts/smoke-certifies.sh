#!/usr/bin/env bash
# Smoke test for TIERED-CHECK step 3: certifies + skip_phases + --gate.
#
# Generates a throwaway crate under scratch/certifies-smoke and drives
# brokkr check through the partial/complete/gate paths. The three verdict
# scenarios assert on exit codes (the 0/10/1 contract, clap's 2 for flag
# conflicts). The two coverage-failure scenarios can't: exit 1 is brokkr's
# universal failure code (config, gremlin, clippy, build, test all produce
# it), so they additionally parse the `--json` summary and assert on
# `failed_phase`/`coverage.*` - proving each fails IN the coverage phase for
# the right reason, not for some unrelated reason that never ran the audit.
# Requires `jq`. Run from the brokkr repo root after installing the binary
# under test:
#
#   bash scripts/smoke-certifies.sh
#
# The generated directory is left behind for inspection; it is disposable
# and regenerated on every run.
set -u

root="$(cd "$(dirname "$0")/.."; pwd)"
smoke="$root/scratch/certifies-smoke"
rm -rf "$smoke"
mkdir -p "$smoke/src"

cat > "$smoke/Cargo.toml" <<'EOF'
[package]
name = "certifies-smoke"
version = "0.1.0"
edition = "2021"

[workspace]
members = ["member", "pm"]
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

cat > "$smoke/brokkr.toml" <<'EOF'
project = "brokkr"

[[check]]
name = "default"
packages = ["certifies-smoke", "member", "pm"]

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
[test.profiles.lane-par]
sweeps = ["default"]
test_threads = 0
skip = ["skipme", "shared::"]

# The isolated lane runs root's shared:: but package-skips member's -
# a name-based skip cannot make that distinction (feature 11).
[test.profiles.lane-ser]
sweeps = ["default"]
isolation = "process"
skip = ["skipme", { package = "member", pattern = "shared::" }]

[test.profiles.gate]
certifies = "complete"
lanes = ["lane-par", "lane-ser"]

# Coverage failure modes, driven below: an unjustified skip (orphan) and
# a quarantine entry justifying nothing (stale).
[test.profiles.gate-orphan]
certifies = "complete"
sweeps = ["default"]
skip = ["adds", "skipme", "shared::"]

[test.profiles.gate-stale]
certifies = "complete"
sweeps = ["default"]

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

# Runs `brokkr "$@"` capturing stdout - whose LAST line, under --json, is the
# machine-readable summary - to a file so a scenario can assert on the
# summary's discriminating fields. stderr streams live; stdout is echoed
# afterwards so the run is still visible. Sets `rc` (exit code) and `summary`
# (the JSON trailer). A resolve-time error emits no summary, so `summary` is
# then a human line and jq fails to parse it - which correctly fails the
# assertion rather than passing vacuously.
summary=""
rc=0
check_json() {
  local out="$smoke/check.stdout"
  brokkr "$@" >"$out"
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

echo "=== bare check: partial default profile ==="
brokkr check --json
expect "bare check = partial" 10 $?

echo "=== --gate: complete profile ==="
check_json check --gate --json
expect "--gate = complete" 0 $rc
# The point of a complete gate is that the audit RAN: a green exit with a null
# coverage object would be a pass that certified nothing.
expect_json "--gate ran the coverage phase" \
  '.failed_phase == null and .coverage != null and .coverage.orphaned == 0'

echo "=== --profile gate-orphan: unjustified skip fails coverage ==="
check_json check --profile gate-orphan --json
expect "orphaned pair = exit 1" 1 $rc
# Must fail IN the coverage phase with orphaned pairs - not at load, not in
# build/test. `adds` and root's `shared::` are skipped but unquarantined.
expect_json "gate-orphan failed on coverage with orphans" \
  '.failed_phase == "coverage" and .coverage.orphaned > 0'

echo "=== --profile gate-stale: quarantine justifying nothing fails ==="
check_json check --profile gate-stale --json
expect "stale quarantine = exit 1" 1 $rc
# The stale signature: coverage failed with zero orphans (every test ran, so
# the two [[quarantine]] entries justify nothing). Distinguishes stale from
# orphan, and both from any non-coverage failure.
expect_json "gate-stale failed on coverage with no orphans" \
  '.failed_phase == "coverage" and .coverage != null and .coverage.orphaned == 0'

echo "=== --gate -p: rejected by clap ==="
brokkr check --gate -p certifies-smoke
expect "--gate -p = usage error" 2 $?

echo "=== --profile gate -p: rejected at resolve time ==="
brokkr check --profile gate -p certifies-smoke
expect "complete + -p = config error" 1 $?

echo "=== --profile edit -p: scoped partial ==="
brokkr check --profile edit -p certifies-smoke --json
expect "partial + -p = exit 10" 10 $?

if [ "$fail" -eq 0 ]; then
  echo "smoke: all scenarios passed"
fi
exit $fail
