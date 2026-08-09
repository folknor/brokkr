# brokkr mogwai

Benchmark a mogwai surface. Two kinds of surface, one row shape, no layers.

```
brokkr mogwai                                  # list both kinds
brokkr mogwai -- gen --type summary --seed 1   # CLI surface
brokkr mogwai screen_projection --hotpath      # harness surface
```

## The two surfaces

**Argv-shaped**, through the shipped bin: `gen` and its `--type` variants,
`tick-composition`, `preflight`, `measure`, `fit`, `cache`, `synth`,
`arrival-screen`. A process, an argv, a wall. Benching these through
`target/release/mogwai` measures what ships, startup and argument parsing
included, which is the honest end-to-end number.

**Harness-shaped**, through a cargo example: the engine's matching loop and
divergence seam, the `TickSource` implementations, the arrival draw, the
screen's projection, and eventually the serving path and the adapter. These have
no command line, so the harness is the addressable thing.

The second kind is the majority of the eventual measurable surface. A registry
that could only hold argv is what forced everything else into a second "layer".

## What the registry holds

Targets and their feature shapes, plus out-of-git inputs. Nothing else.

```toml
[mogwai]
package = "mogwai-cli"     # cargo -p for the CLI
bin     = "mogwai"         # optional, defaults to the package name

[mogwai.targets.screen_projection]
package  = "mogwai-lab"    # optional, defaults to [mogwai] package
example  = "screen_projection_bench"
features = ["hotpath"]

[bygg.datasets.mnq-tbbo-july]
path   = "research/market-data/databento/mnqv/2026-07.full.tbbo"
xxh128 = "280ade40376bd49f50c579bb127f3fbd"
```

- **CLI surfaces need no registration.** The bin is registered once; the argv is
  composed at the call site.
- **Harness surfaces resolve by name** against `[mogwai.targets.*]`. Adding one
  to the measurable set is registering a target - work you were going to do the
  moment you wanted to optimize it.
- **Harnesses take an argv**, like the bin. Every surface here is config-shaped
  (preset, window, seed, cell), so an argument-free harness would need a new
  entry per shape: the enumeration trap at one remove.
- **`--bench`, `--hotpath` and `--alloc` apply uniformly to both kinds.** One row
  shape, one query surface.

`features` is the field that earns its place. `--hotpath` and `--alloc` are
inert without the feature that compiles the instrumentation in, so a target that
has to be *remembered* to be built with `--features hotpath` is a target that
records profile-less rows. A call-site `-F` appends to the registered list
rather than replacing it, so an extra feature adds an arm instead of silently
dropping the shape that makes the mode work at all.

## Why there is no workload registry

The predecessor registered a name per hand-written argv, with `args`, `runs`,
`expect_seconds`, `timing`, `timing_reason`, `identity_counters`, `corpus` and
`successor` per entry. It is gone.

The surface it needed to cover is the whole product of commands, presets,
windows, seeds, cells and flag axes, plus every library-level loop with no
command at all. That product cannot be enumerated, and a registry that must be
edited before a new question can be asked will not survive a decade of asking
them.

pbfhogg is the precedent: roughly 25 commands across several flag axes and ten
datasets, and its `brokkr.toml` registers **zero** workloads. It registers
inputs. Invocations are composed at the call site and captured verbatim, and
pairing rows is a query rather than a name lookup.

What went with it, and why:

| Removed | Why |
|---|---|
| `timing` / `timing_reason` | Redefined what the `elapsed` column MEANS per entry, so one row's elapsed was an external wall and another's was internal phases with setup excluded, and nothing in the row said which. Markers plus `brokkr sidecar --durations` produce the same narrowing with the excluded setup still visible as its own phase, instead of deleted from the record. |
| `expect_seconds` | An estimate the first stored row supersedes. The history is the expectation. |
| `runs` | Already a call-site flag everywhere else. How much time to spend is a decision made on the day. |
| `identity_counters` | Only controlled fatality - undeclared counters were captured and diffed anyway. See below. |
| `{corpus}` and its token | Datasets, under a different name and without the query surface. |
| `successor`, name-as-promise | The DB pairs on the captured argv, which is stronger than a name because it cannot lie. |
| `args` | The invocation belongs at the call site, captured verbatim. |

**The exposure this accepts:** a harness with an argv can be invoked in a shape
nobody meant, and no registry entry prevents it. pbfhogg carries the same
exposure across every command it has. What answers it is not a config constraint
but the captured argv - an invocation that is not comparable is *visible* in the
row rather than prevented, and reading rules turn that into a verdict.

## Rows and pairing

Rows file under the target name, or the bin name for a CLI invocation. The
invocation is captured verbatim in `cli_args` and `brokkr_args`, and
`brokkr_args` stays in the pairing key - two different invocations must not
average into one row.

Selecting an arm is therefore a query. `brokkr results --grep` and `--grep-v`
run over the whole invocation, which is the only way to select an arm defined by
an *absent* flag. See `brokkr man results`.

`input_file` stays empty: an input a surface reads is named in its argv, which
the row already carries, and duplicating it into that column would key pairing
on the same fact twice.

## Counters

Every external run scrapes the winning run's stderr for `key=value` counters -
free, because stderr is captured either way. `--compare` reports the ones that
moved:

```
    counters: cells_evaluated 5000 -> 4000, prints 1240 -> 1180
```

A wall on its own cannot distinguish "the code got faster" from "the code did
less". This is what turns "12% faster" into "12% faster on 8% fewer cells".

It is reported, never fatal. A predecessor let an entry declare which counters
were identity-bearing, making a move in one of them an error rather than a
reading - but the declaration only controlled fatality, and a gate that fires on
the first legitimate win (doing less work is what most optimization past the
free-lane stage actually is) earns a bypass flag and then gets passed out of
habit. Where a moved count really does invalidate a series - a seeded tape whose
draw moved - that owes a `TAPE_PROTOCOL_VERSION` bump, which is unconditional
and cannot be waived per comparison.

Counters from the winning run only: averaging across iterations would hide an
iteration that differed rather than surface it. `meta.`, `env.` and `prev.`
pairs are excluded as provenance.

## Datasets

`[<host>.datasets.<name>]` records an out-of-git input: which delivery, and
whether the file under that path drifted since a row was recorded against it.
Per host, because deliveries live wherever the machine holding them put them.

This is **not** a substitute for the run's own content verification, which asks a
different question - whether the data is what the ledger says. This asks whether
the bytes moved under a recorded row.

> [!IMPORTANT]
> The digest is **XXH128**, brokkr's standard file hash - not the SHA-256 the
> delivery manifests carry. There is nothing to transcribe. Register the path,
> run `brokkr env`, and the entry lists with the digest computed from disk:
>
> ```
> datasets:    mnq-tbbo-july <tick> (no hash configured, actual: 280ade40...)
> ```
>
> Paste it back. A digest that cannot be computed reports why rather than a bare
> `error`.

`path` may name a **directory**. Deliveries frequently are one - a Databento
delivery is two `.csv.zst` archives beside `manifest.json`, `metadata.json` and
`condition.json`, and `measure --corpus` takes the directory. A directory
digests as the fold, sorted by path relative to the root, of
`<relative path>\0<file digest>` over every file beneath it:

- Sorting makes it reproducible; readdir order is a filesystem detail that
  differs between two copies of identical data.
- The relative path is inside the fold, so a rename, or two files swapping
  contents, changes the digest. A delivery is its layout as well as its bytes.
- Per-file digests reuse the mtime cache, so an unchanged multi-gigabyte
  delivery is a stat per file, not a re-read.
- Symlinks are recorded by target text and never followed.
- An empty directory is refused - a wrong path far more often than an input.

Pinning one file *inside* a delivery is not a substitute: it reads as verified
while covering a 4 KB descriptor and leaving the actual input unpinned.

## Deferred

**The serving path** is designed for, not measured yet. It is harness-shaped and
fits the scheme when it arrives. Excluding it on principle, as the retired
registry did, would have written off the class the entire end state lives in.

**Mogwai's reading rules** - what makes a delta real - live in mogwai's
`reference/performance.md`, not here. pbfhogg's are load-bearing and almost
entirely inapplicable: it is I/O bound, so its error model is drive state, trim
debt and page cache, while the screen and the walk are CPU and RNG bound.
