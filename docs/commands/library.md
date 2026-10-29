# `brokkr library` and the library harness protocol

pbfhogg used as a library, not as its CLI. The harness is a bin of a pbfhogg
workspace member that links the library at the commit under test and runs
scripted sequences of library calls in one process; pbfhogg writes it to the
protocol below, and a pbfhogg commit without that member has nothing for this
command to build. `brokkr library` builds the harness per commit (so
`--commit` works) and, today, lists the scenarios it declares.

pbfhogg owns what a call does: the scenarios, the library calls they make
and the assertions about them that only the process itself can judge. brokkr
owns the build, the process and the checks a process boundary can make. It
knows no pbfhogg API semantics, only the identifiers the harness declares
and the shapes below.

This document is the protocol, binding on both sides. It describes protocol
version 1 as this brokkr implements it, and grows as brokkr does more with
it; a harness output it does not allow is refused, never guessed at. Version
1 is not yet frozen: a document carrying a field this brokkr does not
implement is refused as an unknown key, and compatibility between brokkr
releases is not promised until it is.

## Usage

```
brokkr library [--commit REF] [--debug | --release | --profile NAME]
```

Builds the harness and lists the scenarios it declares: per scenario its id,
call count, deadline and description, then one line per call, role and
assertion. The listing is data on stdout; build narration goes where
`brokkr man output-channels` says.

- `--commit REF` builds the harness from a git worktree at REF. Config,
  including `[pbfhogg.library]`, still comes from the current tree. A commit
  that predates the harness member fails its build.
- `--debug`, `--release`, `--profile NAME` choose the cargo profile,
  overriding the config's `debug`/`profile`. The default is release: the
  library is measured in release, and one build serves every use.

## `[pbfhogg.library]`

```toml
[pbfhogg.library.harness]
package = "pbfhogg-harness"   # the workspace member
binary = "pbfhogg-harness"    # its bin, when it is not named like the package
features = []                 # cargo features for the build
profile = "release"           # or debug = true
```

`[pbfhogg.library.harness]` is required inside `[pbfhogg.library]` and has
the shape of `[piners.harness]` (`brokkr man piners-config`). Nothing else is
accepted in either table.

`pbfhogg` is not a reserved top-level name: a `[pbfhogg]` table has always
been read as a hostname section, and still is. brokkr reads it as the pbfhogg
project section only when it holds `library`, which no host table could (the
host schema refuses unknown fields), so no config that loaded before loads
differently. The decision is made once, on the composed config. A claimed
`[pbfhogg]` holds nothing but `library`: host settings for a machine named
pbfhogg go in `[host.pbfhogg]`, which is always a host. The section belongs
to `project = "pbfhogg"`; in any other project it is a load error.

`brokkr install` does not install the harness: pbfhogg's `[bin] install`
names the CLI package only (`brokkr man run`).

## Running the harness

The harness is invoked with one mode flag:

- `HARNESS --describe-harness` prints the describe document.

### Describe mode

brokkr runs it in the project root with `BROKKR_MARKER_FIFO` and
`BROKKR_HARNESS_ARTEFACT_DIR` removed from its environment, and kills it at
60 seconds. It must exit 0, write nothing to stderr, and print exactly one describe
document of at most 1 MiB on stdout. A longer output, any stderr, another
exit, a signal or the deadline fails the command, naming which. In describe
mode the harness writes nothing anywhere; brokkr does not check the file
system for that.

## Documents

Every document either side writes is one JSON object in UTF-8 without a
byte order mark, and nothing follows it but whitespace. Its schema is
closed:

- an unknown key, a duplicate key, a missing required key, a key the
  object's shape forbids, and a value of the wrong type are each refused;
- integers are JSON integers, without fraction or exponent (and never
  `-0`), in the range a field states;
- an object is never accepted in array form.

Each refusal names the path into the document, such as
`describe.scenarios[0].roles[2].owner`. The exceptions are text that is not
JSON at all and a number beyond the range of a double (`1e400`), which the
JSON parser itself refuses before any path exists; those name a line and
column instead.

Every document the harness writes carries `protocol`, an integer. This
brokkr implements 1 and refuses any other value, naming both.

### Identifiers

Scenario, call, role and assertion ids match `[a-z0-9][a-z0-9_.-]{0,63}`:
each is usable as a FIFO call id (`brokkr man measure`, Marker and counter
rules), a file name and a command-line word without escaping. `setup` is
never a call id: it names the harness's own preparation. Assertion ids
starting `brokkr.` are reserved for results brokkr adds itself.

One-line text (a description or a label) is 1 to 200 bytes with no control
character.

## The describe document

```json
{
  "protocol": 1,
  "scenarios": [
    {
      "id": "repeat-cat",
      "description": "cat the input twice in a row",
      "deadline_s": 600,
      "roles": [
        {"id": "input", "kind": "pbf", "source": "dataset", "variant": "indexed"},
        {"id": "out-a", "kind": "pbf", "source": "output", "owner": "a"},
        {"id": "out-b", "kind": "pbf", "source": "output", "owner": "b"}
      ],
      "calls": [
        {"id": "a", "label": "cat input -> out-a"},
        {"id": "b", "label": "cat input -> out-b"}
      ],
      "assertions": [
        {"id": "a.outcome", "call": "a"},
        {"id": "b.outcome", "call": "b"},
        {"id": "outputs-equal", "call": null}
      ]
    }
  ]
}
```

- `scenarios`: at least one, ids unique.
- `description`: one line.
- `deadline_s`: 1 to 86400, the wall one process of the scenario gets.
- `calls`: at least one, ids unique within the scenario, none `setup`. Each
  has a one-line `label`.
- `roles`: every path the scenario uses, ids unique within the scenario;
  possibly none.
- `assertions`: the scenario's assertion inventory, ids unique within the
  scenario; possibly none. `call` is required: a call id of this scenario,
  or `null` for an assertion about the scenario as a whole.

### Roles

| `source` | `kind` | `owner` | `variant` |
|---|---|---|---|
| `dataset` | `pbf` | forbidden | required |
| `generated` | `pbf`, `file`, `directory` | forbidden | forbidden |
| `output` | `pbf`, `file`, `directory` | required | forbidden |
| `scratch` | `directory` | required | forbidden |

- A `dataset` role is a dataset file of the run in the named variant: read
  only, and shareable between calls. `variant` is a variant name
  (`[A-Za-z0-9_.-]`, 1 to 64 bytes). Dataset roles are PBFs only.
- A `generated` role is written by the harness's setup before any call and
  read by calls; setup owns it.
- An `output` role is written by its owner; a `scratch` role is its owner's
  run scratch. `owner` is a call id of this scenario, exactly one, so no two
  calls write one path.
- `kind: pbf` declares a PBF file.
