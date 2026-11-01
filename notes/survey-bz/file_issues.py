"""File the 2026-11-01 contract survey's remaining bugs with bz.

One-off; each body is passed on stdin so no shell sees it.
"""

import subprocess
import sys

SURVEY = "Found by the 2026-11-01 contract survey (brokkr notes/project-contracts.md)."

ISSUES = [
    (
        "folknor/brokkr",
        "elivagar: dead and removed contract bits still sent (ELIVAGAR_NODE_STATS, --seam-reconcile-layers)",
        [],
        f"""ELIVAGAR_NODE_STATS (checked in code): src/elivagar/plan.rs still sets ELIVAGAR_NODE_STATS=1 on every elivagar plan. elivagar deleted its reader at ab935ff (2026-07-15).

--seam-reconcile-layers (not reproduced): brokkr still emits it when configured, and docs/brokkr.toml.elivagar.md shows it as an example. elivagar removed it at 099611b (2026-08-07). No host config sets it today, so latent.

{SURVEY}""",
    ),
    (
        "folknor/brokkr",
        "pbfhogg: flags offered on commands that refuse them with exit 64",
        [],
        f"""Not reproduced; read from brokkr's command layer against pbfhogg's CLI.

- --direct-io and --compression are on every measured pbfhogg command (PbfArgs) and build_args appends them unconditionally. pbfhogg refuses both on merge-changes, --compression on check, inspect, digest and diff, and both in OSC-mode tags-filter.
- --io-uring: commands.rs marks diff-osc supports_io_uring = true, but pbfhogg's diff has no io-uring arg at HEAD or at 0.5.0. pbfhogg.md's I/O list (diff-osc) is wrong too.
- tags-filter: pbfhogg refuses -j with -R and in OSC mode, and -R with --remove-tags. brokkr allows all three, and its help says -t is a "No-op under -R".
- --compression help says zlib:N (N=1-9). pbfhogg accepts zlib:0-9 and zstd:-7..22.

Fix: declare per command which axis flags it accepts and refuse the rest before the build, as io-uring already does.

{SURVEY}""",
    ),
    (
        "folknor/brokkr",
        "merge-changes writes scratch but brokkr declares none for it",
        [],
        f"""Not reproduced.

Since pbfhogg c97b9985 (2026-10-30), merge-changes writes scratch when it has several inputs and no --simplify, and takes --tmp-dir. pbfhogg refuses --tmp-dir on a path that writes none. brokkr's merge-changes arm declares no scratch, so its scratch lands wherever pbfhogg defaults it, outside the per-launch private directory. pbfhogg.md's list of scratch-writing commands is stale.

The declaration is conditional on the input count and --simplify, so a plain writes_scratch = true is wrong for the single-input and --simplify cases.

{SURVEY}""",
    ),
    (
        "folknor/brokkr",
        "results --compare pairs rows that measured different things: digest across 16154b0d, tilegen across config blocks",
        ["bench"],
        f"""Not reproduced.

- digest: pbfhogg 16154b0d (2026-10-29) added digest --form and changed the default from v1 content to v2 full. A defaulted brokkr digest row from before and one from after pair, but measured different forms. pbfhogg.md covers a baseline that predates --form, not this.
- elivagar tilegen: pairing never reads cli_args and elivagar sets no arm_qualifier, so rows built under different [<host>.tilegen.*] blocks (ocean set, budgets, thread counts) pair. The archive store key (dataset, variant, commit) has the same gap: two runs at one commit with different blocks overwrite each other.

{SURVEY}""",
    ),
    (
        "folknor/brokkr",
        "arm_key does not stop at --, so child argv tokens like -v or --bench N are stripped from the pairing key",
        [],
        f"""Not reproduced; read from src/run_invocation.rs.

The pairing key drops brokkr provenance flags (-v, --verbose, --commit X, --bench N) wherever they appear, including after --, where they belong to the child. A child flag with one of those spellings would be stripped or altered, so two different child invocations could pair. Latent: mogwai's CLI has none of those flags today. It is likely the real collision behind mogwai's note that fill_walk_bench would collide with the mode flag.

{SURVEY}""",
    ),
    (
        "folknor/brokkr",
        "Rows from --commit runs omit what else shaped the run: sibling path deps, pinned script content, external tool versions",
        [],
        f"""Not reproduced; read from code.

- Sibling path dependencies: src/worktree.rs links a path dependency outside the repo to the live sibling checkout. A baseline of nidhogg (../pbfhogg, ../elivagar), ratatoskr (../bifrost) or broadarrow (../mogwai, ../piners) is the old commit built against today's siblings, possibly dirty, and the row records one commit. A gate bisection cannot walk bifrost changes, and old nidhogg commits mostly will not build.
- ratatoskr sync: the gate checks the baseline row's script path, fixture, profile and features, never content. Under --commit, git state comes from the clean worktree, so an uncommitted edit to the live script or fixture is stored as a clean row and judged against an old baseline. The saehrimnir binary's identity is recorded nowhere either. dellingr's xxh128 pin is the model.
- External tools: planetiler, tilemaker and osmium versions are absent from rows, so a bump silently changes what baseline rows measured.

{SURVEY}""",
    ),
    (
        "folknor/piners",
        "corpus binary installs no hotpath guard, so brokkr corpus --hotpath likely cannot read a report",
        [],
        f"""Not reproduced; read from code.

brokkr corpus --hotpath builds piners-harness with the hotpath feature and expects a JSON report at HOTPATH_OUTPUT_PATH. Only src/bin/bench.rs installs a HotpathGuard; the corpus binary does not, and it forks a process per probe, so a monitor-level guard would profile nothing. The likely result is brokkr's "cannot read hotpath report".

Also: brokkr --alloc asks for a hotpath-alloc feature piners-harness no longer defines (removed 73a4b7204, 2026-06-03); tracked on brokkr's side as brokkr#82. And piners never reads BROKKR_HARNESS_ARTEFACT_DIR or BROKKR_TEST_BIN_DIR, which brokkr sets.

Worth deciding whether the corpus binary is meant to be profiled at all, and if so where the guard belongs (per probe child).

{SURVEY}""",
    ),
    (
        "folknor/sluggrs",
        "gpu-bench example cannot be reached by brokkr hotpath --target gpu",
        [],
        f"""Checked in code.

brokkr hotpath --target NAME builds the NAME_bench example. Cargo.toml names this one gpu-bench (path examples/gpu_bench.rs), so --target gpu builds gpu_bench, which does not exist. The example nonetheless carries the mandatory elapsed_ms logic for brokkr. email_bench was renamed for the same convention (de59498).

Fix: name the example gpu_bench.

{SURVEY}""",
    ),
    (
        "folknor/sluggrs",
        "Example comments name brokkr commands that do not exist",
        [],
        f"""Not reproduced.

- examples/hotpath.rs and examples/email2_bench.rs say brokkr sluggrs hotpath. There is no sluggrs namespace; the command is brokkr hotpath [--target NAME].
- email2_bench.rs says --example email2-bench; the target is email2_bench.

{SURVEY}""",
    ),
    (
        "folknor/mogwai",
        "Self-reported elapsed_ms is discarded by brokkr, and comments still describe it as used",
        [],
        f"""Not reproduced; read from code.

crates/mogwai-cli/src/measure.rs (report_cost) and crates/mogwai-data/examples/arrival_walk_bench.rs emit elapsed_ms as "the one self-reporting workload". reference/performance.md says self-reporting was withdrawn on 2026-08-10. brokkr's external path drops the reserved elapsed_ms and total_ms names without storing them (not even as a counter), so the value is lost and the comments are stale.

Also: brokkr.toml's comment lists --dataset as a pairing query, but mogwai rows appear to carry no dataset (input_file is empty, and brokkr mogwai has no dataset flag). Not traced end to end.

And reference/performance.md says back-fill works to any commit whose CLI still parses the invocation, but every brokkr mogwai --commit before 3dc09b9 (2026-10-16) currently fails to build because [cargo_env] runs scripts/source_identity.py in the old tree (brokkr#79).

{SURVEY}""",
    ),
    (
        "folknor/ratatoskr",
        "Marker timestamps are UNIX-epoch microseconds, brokkr documents microseconds since process start",
        [],
        f"""Not reproduced; read from code.

ratatoskr writes marker timestamps with sidecar_timestamp_us as UNIX-epoch microseconds. brokkr's measure doc says markers carry microseconds since process start, and dellingr follows that. Span lengths are unaffected, since they are differences, but anything lining markers up against sidecar sample time would misplace them. One side should change; brokkr's doc is the shared contract.

{SURVEY}""",
    ),
    (
        "folknor/elivagar",
        "node_index.rs comment says brokkr discards stderr under --bench, which is no longer true",
        [],
        f"""Not reproduced.

The comment at node_index.rs (from ab935ff) justifies a choice by brokkr discarding stderr under --bench. brokkr now parses tilegen stderr: the LocationsOnWays substring becomes meta.locations_on_ways_detected, and every key=value line is stored. The rationale is stale; the decision it supports may need another look.

{SURVEY}""",
    ),
    (
        "folknor/litehtml-rs",
        "Pipeline prints timing on stderr for brokkr, but brokkr's visual path never reads it",
        [],
        f"""Not reproduced.

Since 9d5eab4 the pipeline prints elapsed_ms=, parse_ms= and other key=value lines on stderr "for brokkr". brokkr visual never reads them; nothing stores them. Either drop the output or ask brokkr to record it.

{SURVEY}""",
    ),
]


def main() -> int:
    failed = 0
    only = sys.argv[1] if len(sys.argv) > 1 else None
    for project, title, tags, body in ISSUES:
        if only is not None and only not in title:
            continue
        cmd = ["bz", "-p", project, "new", title, "-b", "-", "-t", "contracts"]
        for tag in tags:
            cmd += ["-t", tag]
        if project != "folknor/brokkr":
            cmd.append("--no-commit")
        result = subprocess.run(cmd, input=body, text=True, capture_output=True)
        if result.returncode == 0:
            print(f"{result.stdout.strip()}  {title}")
        else:
            failed += 1
            print(f"FAILED {project}: {title}: {result.stderr.strip()}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
