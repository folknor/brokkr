#!/usr/bin/env bash
# Smoke test for TIERED-CHECK step 3: certifies + skip_phases + --gate.
#
# Generates a throwaway crate under scratch/certifies-smoke and drives
# brokkr check through the partial/complete/gate paths, asserting on exit
# codes (the 0/10/1 contract, clap's 2 for flag conflicts). Run from the
# brokkr repo root after installing the binary under test:
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
EOF

cat > "$smoke/brokkr.toml" <<'EOF'
project = "brokkr"

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
[test.profiles.lane-par]
sweeps = ["default"]
test_threads = 0
skip = ["skipme"]

[test.profiles.lane-ser]
sweeps = ["default"]
isolation = "process"
skip = ["skipme"]

[test.profiles.gate]
certifies = "complete"
lanes = ["lane-par", "lane-ser"]

# Coverage failure modes, driven below: an unjustified skip (orphan) and
# a quarantine entry justifying nothing (stale).
[test.profiles.gate-orphan]
certifies = "complete"
sweeps = ["default"]
skip = ["adds", "skipme"]

[test.profiles.gate-stale]
certifies = "complete"
sweeps = ["default"]

[[quarantine]]
pattern = "skipme"
issue = "B1"
reason = "flaky teardown; tracked upstream"
EOF

cd "$smoke"
git init -q
git add -A

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

echo "=== bare check: partial default profile ==="
brokkr check --json
expect "bare check = partial" 10 $?

echo "=== --gate: complete profile ==="
brokkr check --gate --json
expect "--gate = complete" 0 $?

echo "=== --profile gate-orphan: unjustified skip fails coverage ==="
brokkr check --profile gate-orphan --json
expect "orphaned pair = exit 1" 1 $?

echo "=== --profile gate-stale: quarantine justifying nothing fails ==="
brokkr check --profile gate-stale --json
expect "stale quarantine = exit 1" 1 $?

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
