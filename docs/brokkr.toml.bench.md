# Script benches in brokkr.toml

A script bench is a `[bench.NAME]` table holding the Lua that runs one
benchmark, with the build settings and params it runs with. The scripts run on
dellingr, a pure Rust Lua VM linked into brokkr; they are dellingr Lua, not Lua
5.4 (no `pcall`, long strings, bitwise operators or `goto`).

What brokkr does with them: it loads and validates every table below when it
reads the config, `brokkr check`'s `bench` phase compiles every bench's script
without running any of it, `brokkr bench NAME` runs one and stores the run
(`brokkr man bench`), and `brokkr results` reads the stored runs (`brokkr man
results`, Script benches). Binding params on the command line is not there
yet: `brokkr bench` refuses a bench that declares any.

Every refusal this page lists happens when the config loads, for every
command, so a mistake in a bench table is named before anything is built. A
broken *script* does not block other commands: it fails `brokkr check`.

## `[bench.NAME]`

```toml
[bench.cat]
script = """
function bench()
    -- the Lua that runs the bench
end"""
target = "pbfhogg"
profile = "release"
features = ["zlib-ng"]
default_features = true
uses = ["dataset"]
params.jobs = { help = "worker threads" }
```

- `script` - the Lua, inline. Or:
- `script_file` - a `.lua` file holding it, resolved against the directory of
  the config file that set `script_file` (the same base `include` paths use,
  and the real directory when that file is reached through a symlink). A
  bench gives exactly one of the two; both, neither, or a blank one is a load
  error. Under `include`, the two are one setting: a layer giving either
  replaces whichever a layer beneath it gave, so an including file can swap
  an included inline script for a file. Every other bench field merges key by
  key like any table, so a bench's script and its build settings may come
  from different files.
- `target` - the bin target to build; absent means `[bin] default`. Blank is
  refused.
- `profile` - the cargo profile, as `[bin] profile` takes it (`dev`,
  `release`, or a custom profile; the names Cargo or brokkr own under the
  target directory are refused).
- `features` - cargo features to build with, after the host section's own
  `features`; a blank entry is refused.
- `default_features` - `false` builds without the package's default
  features; absent is `true`.
- `uses` - `[bench_params]` sets whose params the bench takes (below).
- `params` - the bench's own params (below).

Any other key is refused. A bench name is letters, digits, `_` and `-`, and
does not start with `-`, since it is the argument `brokkr bench` takes. It
names the bench, not the subcommand it drives: `[bench.cat]` is a fine name
for a bench of pbfhogg's `cat`.

## Params

A param is an argument of one bench, given on the command line after the
bench's name as `--NAME VALUE`; brokkr's own options go before the name. A
param is declared as a table:

```toml
params.dataset = { from = "datasets", default = "denmark" }
params.variant = { from = "datasets.{dataset}.pbf", default = "indexed" }
params.osc     = { from = "datasets.{dataset}.osc", many = true }
params.jobs    = { help = "worker threads" }
```

- `help` - the text the bench's `--help` shows.
- `from` - a dot-separated path into the resolved config whose keys are the
  param's valid values. A segment is a key, or `{NAME}`, which stands for the
  key another param of the same bench chose. A param without `from` takes any
  string.
- `default` - the value when the command line gives none, written as the
  command line would write it.
- `many` - `true` makes the param a collection, binding a list of values to
  one sample.

Refused at load: an unknown key; a blank `default`; a `from` path with an
empty segment, or with `{` or `}` anywhere but around a whole segment; a
`{NAME}` that is not a param of the bench; params whose `from` paths reference
each other in a cycle; and a `{NAME}` naming a `many` param, since a path takes
one key. The references are judged on the bench's effective params, after
`uses` (below), so a shared set may reference a param the bench supplies.
Whether a key exists under a `from` path is judged when the bench runs.

A param name is also its `--flag`: lowercase, starting with a letter, then
letters, digits, `_` or `-`. `help` is brokkr's (every bench takes `--help`)
and is the only name a param cannot take: brokkr's own options precede the
bench's name, so none of them can collide with a param.

## `[bench_params.NAME]`

Params several benches share, declared once:

```toml
[bench_params.dataset]
dataset = { from = "datasets", default = "denmark" }
variant = { from = "datasets.{dataset}.pbf", default = "indexed" }

[bench.cat]
script_file = "benches/cat.lua"
uses = ["dataset"]
```

A bench takes every param of every set it `uses`, then its own `params`, each
of which replaces a set's param of the same name whole (not key by key). A
`uses` entry naming no set, a set listed twice, and two used sets declaring a
param the bench does not declare itself are refused. A set no bench uses is
not an error: a shared include may carry sets for several projects. Set names
follow bench names' rules; each declaration follows the param rules above.

## `[data]`

```toml
[data.workloads.same_obj_read.bench]
file = "examples/fields/same_obj_read.lua"
xxhash = "..."
```

Project data that bench params select from with `from` paths. brokkr reserves
the name and does not interpret what is inside it, beyond requiring a table: a
project's own tables need a home, since any other unknown top-level table is a
hostname section.

## Reserved names and the user-wide layer

`bench`, `bench_params` and `data` are reserved top-level names. A host
literally named one of them is written `[host.bench]`, `[host.bench_params]`
or `[host.data]`. The user-wide `brokkr.toml` cannot hold any of the three:
benches belong to one project.

## The `bench` check phase

`brokkr check` reads every bench's script (the inline string, or the
`script_file` from disk) and compiles it with dellingr, running none of it.
That catches what the parser rejects - a syntax error, an unreadable or
missing file - and nothing else: a call to a misspelt function compiles.
Every bench is checked, so one run names every broken script, each with the
bench, the file to open, and the line; an inline script's line counts from the
first line of the `script` string. Running benches on a fixture is a
project's option, as a `[[script_check]]`.

The phase is inert in a config without benches, is skipped by name as `bench`
in a partial profile's `skip_phases`, and runs on a markdown-only tree too: a
`script_file` beside an included config file may live outside the checkout,
where the tree's own status cannot vouch for it.
