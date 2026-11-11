# Script benches in brokkr.toml

A script bench is a `[bench.NAME]` table holding the Lua that runs one
benchmark, with the build settings and params it runs with. The scripts run on
dellingr, a pure Rust Lua VM linked into brokkr; they are dellingr Lua, not Lua
5.4 (no `pcall`, long strings, bitwise operators or `goto`).

What brokkr does with them: it loads and validates every table below when it
reads the config, `brokkr check`'s `bench` phase compiles every bench's script
and every `bench_lib` module and resolves their global names without running
any of it, and runs the contract tests `bench_tests` holds against virtual
hosts, `brokkr bench NAME` runs one and stores the run
(`brokkr man bench`), and `brokkr results` reads the stored runs (`brokkr man
results`, Script benches).

Every refusal this page lists happens when the config loads, for every
command, so a mistake in a bench table is named before anything is built,
except the ones Binding params lists, which need the command line or the
resolved config and happen when `brokkr bench` binds the bench's params,
before anything is built. A broken *script* does not block other commands:
it fails `brokkr check`.

## Validating a script

Run `brokkr check`. Its `bench` phase reads every bench's script (the inline
string, or the `script_file` from disk), compiles it with dellingr and
resolves its global names, running none of it, and the config load before it
validates every bench table. One run names every problem in every script,
each with the bench, the file to open, and the line; an inline script's line
is the config file's (Inline line numbers).

What it catches:

- what the parser rejects: a syntax error, an unreadable or missing file;
- every global the script reads that nothing defines, such as a misspelt
  builtin (`binray_with`) or helper, named once at its first read, whether or
  not that code would ever run;
- a script that never assigns `bench` (a `local function bench` does not);
- a bench with a `validated = true` param whose script declares no
  `validate()` (Params).

A name is defined when the State a sample runs in already holds it (brokkr's
builtins, `json`, `record`, and dellingr's library) or when the script
assigns it anywhere, at the top level or inside a function. A script's
globals are closed: nothing else can supply one, so any other name is nil in
every run. Assigned anywhere is all it asks, so a helper read before the line
that sets it still passes, and fails when it runs.

A script that reads `_G` is the exception: holding the global table, it can
define any name through it (`_G[k] = v`, `rawset(_G, k, v)`), so its names
are not checked at all, and the phase warns which bench and line made it so.
The compile still is.

With `bench_scripts` set, the phase also refuses every `.lua` file under
that directory no bench registers (`bench_scripts`).

With `bench_lib` set and a bench declared or `bench_scripts` set (without
either the phase is inert, library and all), the phase also checks every module in it, once, each
finding naming the module's file: it compiles, it assigns no global (a
module exports through what it returns, so `local function` and `local M =
{}`), it reads no `_G`, and every other name it reads is a builtin or one of
dellingr's library globals, since a module shares the requiring script's
State but nothing in it is the module's to rely on. That one rule is what
keeps each file's check sound on its own: no module can define a name the
script then reads.

Not caught by the compile: a misspelt field (`json.encdoe`, `params.thread`),
a wrong `binary_with` option key, a wrong argument type, a nil value. Those
fail only when the code holding them runs. `brokkr bench --dry-run NAME
PARAMS` binds an invocation and runs its `validate()` (`brokkr man bench`,
Dry run), so the checks a script puts there are tried without a build. The
rest of a script's behaviour is what contract tests run: with `bench_tests`
set, the phase runs every test file under it, each driving the real script
against scripted fixtures (`brokkr man bench`, Contract tests), so the code
a test reaches runs on every check, and each failed case is a finding
naming its file.

The phase is inert in a config without benches, `bench_scripts` or
`bench_tests`, is skipped by name as `bench`
in a partial profile's `skip_phases`, and runs on a markdown-only tree too: a
`script_file` beside an included config file may live outside the checkout,
where the tree's own status cannot vouch for it.

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
params.jobs = { free = true, help = "worker threads" }
```

- `script` - the Lua, inline. Or:
- `script_file` - a `.lua` file holding it, resolved against the directory of
  the config file that set `script_file` (the same base `include` paths use,
  and the real directory when that file is reached through a symlink). The
  script's own directory is resolved on disk too, so
  `../benches/cat.lua` is shown and stored without its dot components; the
  file name stays as spelled, a linked script named as itself. A bench gives exactly one of the two; both, neither, or a blank one is a load
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
- `hotpath_features` - the package's own features that instrument it for
  hotpath, added to the build under `brokkr bench --hotpath`; absent or
  empty, the bench cannot be profiled. Each is a feature name of the built
  package itself: a dependency's (`dep/feature`), an optional dependency
  (`dep:name`), a blank entry and one also in `features` are refused. A
  timing run whose resolved features include one fails its arm (`brokkr man
  bench`, The build).
- `alloc_features` - the same, for hotpath's allocation tracking (for a
  crate on hotpath, typically the feature enabling `hotpath/hotpath-alloc`):
  added to the build under `brokkr bench --alloc` in place of
  `hotpath_features`; absent or empty, the bench cannot be alloc-profiled.
  Held to the same rules, and a timing run that resolves one fails its arm
  too.
- `uses` - `[bench_params]` sets whose params the bench takes (below).
- `params` - the bench's own params (below).
- `description`, `version`, `date`, `changelog` - the devs' notes on the
  bench (Annotations, below).

Any other key is refused. A bench name is letters, digits, `_` and `-`, and
does not start with `-`, since it is the argument `brokkr bench` takes. It
names the bench, not the subcommand it drives: `[bench.cat]` is a fine name
for a bench of pbfhogg's `cat`.

## Annotations

```toml
[bench.cat]
script_file = "benches/cat.lua"
description = "Full-file decode, single pass"
version = 3
date = 2026-11-08
changelog = [
  { version = 3, date = 2026-11-08, note = "Read through the io_uring preflight" },
  { version = 2, date = 2026-09-14, note = "Drop the warmup iteration" },
]
```

Notes devs keep for each other about a bench, every one optional. brokkr
reads nothing into them: no version is ordered, paired on or compared with a
changelog, and no date is checked against another.

- `description` - what the bench measures, a non-blank string.
- `version` - a positive integer, the bench's revision as its devs number it.
- `date` - when the current version was set, a TOML local date written bare
  (`2026-11-08`); a quoted date, a time or an offset is refused.
- `changelog` - an array of tables, in the order written, each with a
  non-blank `note` and optionally a `version` and a `date` taking the same
  values as above; any other key in an entry is refused. `changelog = []`
  is allowed and is not the same as no changelog. Under `include`, the layer
  that sets `changelog` replaces it whole, unlike other arrays of tables,
  which join: two histories never interleave.

A wrong type or a blank text is refused when the config loads, like any
bench key. `brokkr bench` lists each bench's version and the first
non-blank line of its description, and `brokkr bench NAME --help` shows them all (`brokkr man
bench`). Every run stores them with the rest of the config, so `brokkr
results --compare` names a bench whose annotations differ between the sides
(`brokkr man results`). They are config like any other: a script can read
them through `config()`.

## Params

A param is an argument of one bench, given on the command line after the
bench's name as `--NAME VALUE`; brokkr's own options go before the name. A
param is declared as a table:

```toml
params.dataset = { from = "datasets", default = "denmark" }
params.variant = { from = "datasets.{dataset}.pbf", default = "indexed" }
params.osc     = { from = "datasets.{dataset}.osc", many = true }
params.cache   = { choices = ["cold", "warm"], required = true }
params.row     = { validated = true, required = true }
params.label   = { free = true, help = "a tag stored with the run" }
params.jobs    = { integer = "u32", min = 1, default = "4" }
params.region  = { bbox = true }
```

Every param declares its **domain**, the values it takes, in exactly one
of these ways, so no value reaches a script unchecked by oversight:

- `from` - a dot-separated path into the resolved config (The resolved
  view, below) whose keys are the param's valid values. A segment is a key,
  or `{NAME}`, which stands for the key another param of the same bench
  chose.
- `choices` - a list of the strings it takes, checked when the bench
  binds, every item of every value and a `default` alike, and listed by the
  bench's `--help`. Refused at load: an empty list, a blank or repeated
  choice, a choice holding `,`, which the command line splits values on
  (unless the param sets `split = false`), and for a `many` param a choice holding `..`, which binding reads there
  as a range, so neither could ever be given.
- `validated = true` - any string, which the script's `validate()` checks
  (`brokkr man bench`, Validate): for a domain the script owns, such as
  one row of a table the Lua holds, or a rule between params. A bench whose
  script declares no `validate()` is refused when it runs and by check's
  `bench` phase.
- `free = true` - any string, deliberately unchecked.
- `integer = "WIDTH"` - a canonical decimal (digits only, no `+`, no
  leading zero, a `-` only on a nonzero value of a signed width) within
  `WIDTH`, one of `u8`, `u16`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`,
  and within the optional inclusive `min` and `max`, TOML integers inside
  the width (so a `u64` bound above 2^63 - 1 cannot be written; the width's
  own maximum needs none). A spelling Rust's parser would also take, such
  as `+5` or `007`, is refused: one value, one stored key.
- `number = true` - text Rust parses as a finite f64 (exponents and a
  leading `+` included, `inf` and `NaN` not), within the optional inclusive
  `min` and `max`, a finite float or an integer within 2^53. It is judged on
  its parsed f64, so a value a hair past a bound can round onto it, and
  `1`, `1.0` and `1e0` are three keys.
- `bbox = true` - `lon_min,lat_min,lon_max,lat_max` under the dataset bbox
  rules (`brokkr man datasets`): four finite coordinates, longitude within
  -180..180, latitude within -90..90, each minimum not above its maximum.
  Its own commas separate the coordinates, so a bbox param is never split:
  each occurrence is one value, and `split = true` beside it is refused.

`integer`, `number` and `bbox` are checked when the bench binds, every item
of every value and a `default` alike (a default at load, as a `choices` one
is), and the value still reaches the script as the string given, like any
plain param's: they decide what is refused, not what a script receives.
Lua compares such strings as strings (`"10" < "2"`), so a script doing
arithmetic on one converts it with `tonumber`.

A param with none of them, or with two, is refused at load. The rest:

- `help` - the text the bench's `--help` shows.
- `default` - the value when the command line gives none, written as the
  command line would write it, so a list, a range or `all` too (Binding
  params, below).
- `default_only` - `true`, on a `from` param that is not `many`: the
  command line giving none, the param binds the one key its table holds, in
  each arm under that arm's parents, recorded as any bound key. A table
  holding no key or several is refused, naming them, so a second key added
  later never silently changes what a run binds. Beside `default` or
  `required` it is refused at load.
- `many` - `true` makes the param a collection, binding a list of values to
  one sample; any other param given several values fans out, one run per
  value.
- `split` - `false` takes each value whole instead of splitting it on `,`
  (Binding params, below), so a value such as `w/highway=primary,secondary`
  reaches the script as one string. Absent or `true`, values split.
- `required` - `true` makes the command line give the param: its absence
  is refused when the bench binds (Binding params, below), before anything
  is built and without storing a run, where a script's own refusal would
  come only after the build, as a failed run. Absent or `false`, a param
  the command line does not give is unbound or takes its `default`.

Refused at load: an unknown key; no domain or two; a `choices` list as
above, and a `default` holding a value not among its choices or one twice,
so a default that loads binds, and the same for an `integer`, `number` or
`bbox` default's items; `min` or `max` on a param that is neither
`integer` nor `number`, outside the width, not a number or above the other;
an `integer` width not listed above; a blank
`default`; `required = true`
beside a `default`, which would never be used; `default_only` on a param
without `from`, on a `many` one, or beside `default` or `required`; a `from` path with an
empty segment, or with `{` or `}` anywhere but around a whole segment; a
`{NAME}` that is not a param of the bench; params whose `from` paths reference
each other in a cycle; and a `{NAME}` naming a `many` param, since a path takes
one key. The references are judged on the bench's effective params, after
`uses` (below), so a shared set may reference a param the bench supplies.
Whether a key exists under a `from` path is judged when the bench runs
(Binding params, below).

A param name is also its `--flag`: lowercase, starting with a letter, then
letters, digits, `_` or `-`. `help` is brokkr's (every bench takes `--help`)
and is the only name a param cannot take: brokkr's own options precede the
bench's name, so none of them can collide with a param.

## Binding params

`brokkr bench NAME --dataset denmark --variant=raw`: each param is
`--NAME VALUE` or `--NAME=VALUE` after the bench's name; the token after
`--NAME` is its value whatever it looks like, so `--text --help` gives
`--text` the value `--help`. `--help` or `-h` where a param's name could
stand describes the bench instead of running it (`brokkr man bench`). A
param the command line does not give takes its `default`; one with neither
is unbound: the script sees nil for it and the run stores nothing for it,
unless it is `required`, which is refused. A required param counts as given
by any value on the command line: a blank one meets the blank refusal, and
a collection's `all` over an empty table binds an empty collection.

**Several values.** A param may be given more than once, and a value is a
list: it is split on every `,`, and the param's values are every item of
every occurrence, in command-line order (`--osc 4705,4706 --osc 4710`). A
`default` is read the same way. A value of a param that splits cannot hold
a `,`; a table key holding one is reached there only through `all`.

A param with `split = false` is not split: each occurrence, and the
`default`, is exactly one item, commas included, so several values come
only from repeating the flag (`--expr a,b --expr c` is two values), and a
`default` is one. Only the comma splitting changes: `all` and `LO..HI`
below still mean what they mean, an item at a time, and a blank or repeated
item is refused as before. Its `--help` line says it takes one value per
flag.

Two more spellings have a meaning for some params:

- `all`, for a `from` param, is every key of its table, in key order:
  numeric when every key is a canonical decimal (digits, no sign, no
  leading zero, `0` itself one), byte order otherwise. It stands alone: `all`
  beside another value is refused. A plain param's `all` is the string
  `all`.
- `LO..HI`, for a `many` param with a `from` path, is a range: both ends
  canonical decimals (at most 2^64 - 1), `LO` not above `HI`, expanded in
  place ascending, every key in it required under the table. A plain
  collection's range is refused, since it has no keys to range over, and
  any other param's `..` is part of its key.

What the values do depends on the param:

- a `many` param binds them all, as one list, to one sample (a collection;
  one value is a collection of one, and `all` over an empty table an empty
  collection);
- any other param given several **fans out**: the bench runs once per value,
  each run its own arm, stored as its own run. Several fanned params give
  every combination, parents outer, each param's values in their order. A
  dependent resolves under each of its parents' values: `--dataset all --variant all`
  runs every variant of every dataset, each dataset's own.

Params bind in dependency order, so a `{NAME}` in a `from` path stands for a
key already chosen. A `from` param's value must be a key of the table its
path names in the resolved view, a `choices` param's one of its
choices, and an `integer`, `number` or `bbox` param's inside its domain
(Params, above); a `validated` or `free` param's value is taken as it is, the
script's `validate()` judging a `validated` one next.
Every arm is bound before anything runs, and a refusal in any arm refuses
the whole invocation, before anything is built and without storing a run;
every refusal is named, each with what it found (a refused param's
dependents are not judged under it):

- an unknown param, a missing or blank value, a blank item between commas
  (of a param that splits),
  or a token that is not `--NAME`;
- a `required` param the command line does not give, refused once however
  many arms there are, even in an arm whose parent was refused, and its
  dependents not judged; for a `from` param the refusal lists the keys
  under each distinct path the arms' bound parents lead to, where that path
  is a table;
- a `from` path that does not lead to a table (a missing key on the way, or
  a value that is not a table), and a bound param whose path needs a param
  that has no value;
- a `choices` value not among its choices, with them listed, and for a
  `required` one absent, its choices listed;
- an `integer`, `number` or `bbox` item outside its domain, naming the
  item and why (the width, the bounds, or the bbox rule it breaks);
- a value that is not a key under its path, with the valid keys listed; for
  a `default`, the refusal says so. A default is judged only when it is
  used, so a dependent default (`indexed`) may be missing under some
  parents as long as the command line then gives the param, and is refused
  when it is used under any parent that lacks it;
- a `default_only` param the command line does not give whose table, in
  some arm, holds no key or several, with the keys it holds listed;
- a key given twice, after lists, repeats, ranges and `all` are expanded;
  a range end that is not a canonical decimal, a range running backwards, a
  range with keys missing (the first few named, and how many more), and
  `all` beside other values;
- a fanned param whose values select no key under some parent (`all` over
  an empty table), since nothing would run there;
- a key holding a `.` where it is substituted into another param's path,
  since the stored path is its segments joined by dots; a key selected at
  the end of a path may hold one.

A selected entry that is a file entry (The resolved view) is the run's
input. Before any build, brokkr reserves every arm's bound inputs for
reading, refused when another reservation writes one, and keeps them
reserved until the last arm ends; then hashes each (XXH128, through the hash
cache), a file several arms share once. A pinned entry must match its pin,
or the invocation is refused naming the file, both digests and where the pin
was set; an unpinned one is hashed and its digest recorded. Two entries
naming one file are each judged against their own pin. A `file` must be a
regular file; a plain dataset's `path` may be a directory, digested as a
tree, and neither may be a FIFO, device or socket. A file that changes
while it is hashed (its stamp moves, or its path comes to name another
file) is refused and its digest not cached. `--dry-run` checks every
input's shape and hashes only the pinned ones, against their pins, since it
records nothing; it reserves none.

What each run stores (`reference/bench-store.md`, The run document): each
bound param with its `from` path, `{NAME}` references substituted, and its
keys in bound order; a file's digest beside each. A file pairs on its key
and digest, any other param on its key alone, and a collection on its keys
and digests in order, so `--osc 4706,4705` and `--osc 4705,4706` do not
pair.

## The resolved view

The table `from` paths walk and `config()` returns: the composed brokkr.toml
(includes folded, the user-wide layer beneath), with

- every hostname section dropped, and the one for this host (`[<hostname>]`
  or `[host.<hostname>]`) selected: its datasets as top-level `datasets`, the
  rest as `host`, with `name` the hostname and `data`, `scratch` and
  `output` absolute (defaults `data`, `data/scratch`, `data/tilegen`). A
  host without a section gets empty `datasets` and the defaults;
- every dataset file entry's `file` absolute against the host's data
  directory, and a plain dataset's `path` against the project root;
- each dataset's primary also as `snapshot.base`: its `pbf` and `osc`
  tables (empty when it has none) and its `download_date`, so `from =
  "datasets.{dataset}.snapshot.{snapshot}.pbf"` with `snapshot` defaulting
  to `base` covers the primary and every snapshot;
- every `[data]` file entry's `file` absolute against the project root.

A file entry is a table holding a string `file` under `datasets` or `data`,
pinned by `xxhash`, or a dataset holding a string `path`, pinned by
`xxh128`; a pin that is not a 32-digit XXH128 string is refused when its
entry is bound. A file entry's subtables may be file entries too, and one
inside an array has its `file` made absolute, though no `from` path can
select it. Every other table is left as composed. The raw TOML does not
change: the view is derived per run from what the run stores (its config
text, project root and hostname), and is not stored itself.

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
[data.workloads.hotpath.same_obj_read]
file = "examples/fields/same_obj_read.lua"
xxhash = "..."
```

Project data that bench params select from with `from` paths. brokkr reserves
the name and does not interpret what is inside it, beyond requiring a table: a
project's own tables need a home, since any other unknown top-level table is a
hostname section.

The table's shape is how a project limits what a param can select. With the
scale outer and the workload inner, as above, a bench that must never run a
seconds-scale file (one built with a profiler that backlogs on it) binds
`workload = { from = "data.workloads.hotpath" }`, so that file is not a value
it can take, while another bench binds `scale = { from = "data.workloads" }`
and `workload = { from = "data.workloads.{scale}" }` over both. A refusal
`bench()` makes instead comes only once the build has run, and is stored as a
failed run; one a script's preflight makes (`brokkr man bench`, Preflight)
comes before any build and stores nothing, but is still the script's code,
not the shape of what it can be given.

## `bench_lib`

```toml
bench_lib = "bench/lib"
```

The directory a bench script's `require(name)` loads modules from (`brokkr
man bench`, Modules): `bench/lib/preflight.lua` is `require("preflight")`,
`bench/lib/io/uring.lua` is `require("io.uring")`. Resolved against the
directory of the config file that set it, as `script_file` is, so a shared
include can carry its library beside it; every bench of the project shares
it. A blank value or one that is not a string is refused; the directory
itself is read when `brokkr bench` starts and when `brokkr check` runs in a
project that declares a bench or sets `bench_scripts`, and one that cannot
be read refuses there.
Every name in it is one file: no directory or stem with a dot is a module,
and links are never followed, even one swapped in while the directory is
read. An entry whose name could not be a module (a `README`) is never
opened, so it cannot fail the read. Keep the bench scripts themselves out
of it: every `.lua` file there is a module, and a script assigns `bench`,
which `brokkr check` refuses in a module. A `[bench_lib]` table is not this key
but a hostname section, as it always was.

## `bench_scripts`

```toml
bench_scripts = "../benches"
bench_scripts_exempt = ["../benches/scratch", "../benches/old/probe.lua"]
```

The directory every bench script lives under, so that `brokkr check`'s
`bench` phase can refuse one no bench registers: each `.lua` file below it
must be the `script_file` of some `[bench.NAME]`, a module `bench_lib`
loads, a contract-test file `bench_tests` holds (exactly the files the
tests run, found by the same walk), or covered by `bench_scripts_exempt`.
Each finding names the file.
With it set the phase runs even with no bench declared, and then checks
`bench_lib` too. Unset, nothing is discovered.

Both keys resolve against the directory of the config file that set them,
as `script_file` does; the exemptions resolve beside the file that set the
list, which replaces whole across layers, so an inherited list keeps naming
paths beside its own file even where a later layer moves `bench_scripts`.
An exemption is a file, or a directory covering everything below it; one
that names nothing is not an error. `bench_scripts_exempt` without
`bench_scripts` is refused, as is a blank path or a value of another shape.

The directory is read from disk when the check runs, untracked files
included, since a new script is exactly what it is for. A file is its
location, the real directory it sits in plus its name, never what it
links to: a link `alias.lua` to a registered script needs a bench of its
own, and a link sitting outside an exempt directory is not exempt for
pointing into it. Only a regular file whose path makes a module name is a
`bench_lib` module here, the files `require` can load; anything else in
the library ending `.lua` needs a bench or an exemption. A linked directory
is not descended, so a script reached only through one is not discovered;
nothing found is opened. A directory that cannot be resolved, listed or
inspected is a finding rather than skipped.

A `[bench_scripts]` or `[bench_scripts_exempt]` table is not this key but a
hostname section, as it always was.

## `bench_tests`

```toml
bench_tests = "../benches/tests"
```

The directory of the project's contract tests (`brokkr man bench`,
Contract tests): every regular `.lua` file below it is a test file, which
`brokkr check`'s `bench` phase runs (with it set the phase runs even with no
bench declared) and `brokkr bench --test` runs alone. Resolved against the
directory of the config file that set it, as `bench_lib` is. A blank value or
one that is not a string is refused. The directory is read when the tests
run, the walk `bench_scripts` uses: untracked files included, a linked
directory not descended, and a `.lua` entry that is not a regular file (a
link included) or that cannot be inspected is a finding rather than a test.
A test file that is also a bench's `script_file` or a `bench_lib` module is
a finding: each runs in a different Lua state by different rules. Unset,
nothing is tested.

A `[bench_tests]` table is not this key but a hostname section, as it
always was.

## Reserved names and the user-wide layer

`bench`, `bench_params` and `data` are reserved top-level names, and
`bench_lib`, `bench_scripts`, `bench_scripts_exempt` and `bench_tests` are
reserved as values. A host literally named one of the first three is written
`[host.bench]`, `[host.bench_params]` or `[host.data]`. The user-wide
`brokkr.toml` cannot hold any of them: benches belong to one project.

## Inline line numbers

An inline `script` is a TOML string, so its Lua lines are not the file's:
the string starts partway down the file, and escapes, a multi-line string's
trimmed first newline and a line-ending backslash move lines about. brokkr
reads the file that set `script` again (per value, so under `include` the
layer that gave the script, wherever its build settings came from), finds
`bench.NAME.script` in any TOML spelling (a table, dotted keys, an inline
table, quoted keys), and decodes the string itself, keeping each byte's file
line. The map is used only when that decoding, the TOML parser's and the
script brokkr loaded are the same bytes; otherwise the lines count from the
string's first, the chunk is named `[bench.NAME] script`, and brokkr says
why (the file was edited since it was read, say): in the check finding
itself, and as a warning when `brokkr bench` reads the script. A Lua line is named by the file line its first byte came from,
so a line joined by a line-ending backslash is named by its first file line,
and several Lua lines written with `\n` escapes on one file line all name it.

With a map, everything that names a line names the file's: the `bench` check
phase, a compile failure when `brokkr bench` reads the script, and every
failure of an iteration, a Lua error's traceback included, under the config
file's path as the chunk's name (`/p/brokkr.toml:14: attempt to call a nil
value`). A `script_file` is its own file, so its lines need no map.
