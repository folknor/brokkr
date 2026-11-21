# Script benches in brokkr.toml

This page covers the config for script benches: the `[bench.NAME]` tables,
their params, the resolved view the params select from, `[bench_params]`,
`[data]`, and the `bench_lib`, `bench_scripts` and `bench_tests` keys.
Validating a script explains what `brokkr check` catches; the sections after
it are the per-key reference.

A script bench is a `[bench.NAME]` table holding the Lua that runs one
benchmark, with the build settings and params it runs with. The scripts run
on dellingr, a pure Rust Lua VM linked into brokkr. They are dellingr Lua,
not Lua 5.4: there is no `pcall`, no long strings, no bitwise operators and
no `goto`.

brokkr uses the tables in these places:

- brokkr loads and validates every table below when it reads the config.
- `brokkr check`'s `bench` phase compiles every bench's script and every
  `bench_lib` module and resolves their global names, without running any of
  it. The phase also runs the contract tests `bench_tests` holds against
  virtual hosts.
- `brokkr bench NAME` runs one bench and stores the run (`brokkr man bench`).
- `brokkr results` reads the stored runs (`brokkr man results`, Script
  benches).

Every refusal this page lists happens when the config loads, for every
command, so brokkr names a mistake in a bench table before anything is
built. The exceptions are the refusals Binding params lists: they need the
command line or the resolved config, so they happen when `brokkr bench`
binds the bench's params, still before anything is built. A broken script
doesn't block other commands: it fails `brokkr check`.

## Validating a script

To validate your scripts, run `brokkr check`. Its `bench` phase reads every
bench's script (the inline string, or the `script_file` from disk), compiles
it with dellingr and resolves its global names, running none of it. The
config load before the phase validates every bench table. One run names
every problem in every script, each with the bench, the file to open and the
line. An inline script's line is the config file's (Inline line numbers).

The phase catches:

- what the parser rejects: a syntax error, an unreadable or missing file
- every global the script reads that nothing defines, such as a misspelt
  builtin (`binray_with`) or helper, named once at its first read, whether
  or not that code would ever run
- a script that never assigns `bench` (a `local function bench` does not)
- a bench with a `validated = true` param whose script declares no
  `validate()` (Params)

A name is defined in two cases. Either the State a sample runs in already
holds it (brokkr's builtins, `json`, `record`, and dellingr's library), or
the script assigns it anywhere, at the top level or inside a function. A
script's globals are closed: nothing else can supply one, so any other name
is nil in every run. Assigned anywhere is all the check asks, so a helper
read before the line that sets it still passes, and fails when it runs.

A script that reads `_G` is the exception. Holding the global table, it can
define any name through it (`_G[k] = v`, `rawset(_G, k, v)`), so its names
aren't checked at all, and the phase warns which bench and line made it so.
The phase still compiles it.

With `bench_scripts` set, the phase also refuses:

- every bench whose script is inline or whose `script_file` lies outside
  that directory
- every `.lua` file under the directory no bench registers, apart from a
  module, a contract-test file or an exemption (`bench_scripts`)

These findings name the bench or the file, with no line: they're about where
a script is, not what it says.

With `bench_lib` set, the phase also checks every module in it, once, each
finding naming the module's file. This needs a bench declared or
`bench_scripts` set; without either the phase is inert, library and all. A
module must pass four checks:

- it compiles
- it assigns no global: a module exports through what it returns, so it uses
  `local function` and `local M = {}`
- it reads no `_G`
- every other name it reads is a builtin or one of dellingr's library
  globals

The last rule holds because a module shares the requiring script's State,
but nothing in that State is the module's to rely on. That one rule keeps
each file's check sound on its own: no module can define a name the script
then reads.

The compile doesn't catch a misspelt field (`json.encdoe`, `params.thread`),
a wrong `binary_with` option key, a wrong argument type or a nil value. Those
fail only when the code holding them runs. `brokkr bench --dry-run NAME
PARAMS` binds an invocation and runs its `validate()` (`brokkr man bench`,
Dry run), so the checks a script puts there run without a build.

Contract tests cover the rest of a script's behaviour. With `bench_tests`
set, the phase runs every test file under it, each driving the real script
against scripted fixtures (`brokkr man bench`, Contract tests). The code a
test reaches therefore runs on every check, and each failed case is a
finding naming its file.

The phase is inert in a config without benches, `bench_scripts` or
`bench_tests`. A partial profile skips it by naming `bench` in
`skip_phases`. It runs on a markdown-only tree too: a `script_file` beside an
included config file may live outside the checkout, where the tree's own
status can't vouch for it.

## bench

The `[bench.NAME]` table declares one bench. This example sets every build
key:

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

A bench gives exactly one of `script` and `script_file`. Both, neither, or a
blank one is a load error:

- **`script`**: the Lua, inline. `brokkr check` refuses one in a config that
  sets `bench_scripts`.
- **`script_file`**: a `.lua` file holding the Lua. brokkr resolves it
  against the directory of the config file that set `script_file`, the same
  base `include` paths use, and the real directory when that file is reached
  through a symlink. brokkr resolves the script's own directory on disk too,
  so `../benches/cat.lua` is shown and stored without its dot components. The
  file name stays as spelled, so a linked script is named as itself.

Under `include`, `script` and `script_file` are one setting: a layer giving
either replaces whichever a layer beneath it gave, so an including file can
swap an included inline script for a file. Every other bench field merges
key by key like any table, so a bench's script and its build settings may
come from different files.

The build keys:

- **`target`**: the bin target to build. Absent means `[bin] default`; blank
  is refused.
- **`profile`**: the cargo profile, as `[bin] profile` takes it (`dev`,
  `release`, or a custom profile). The names Cargo or brokkr own under the
  target directory are refused.
- **`features`**: cargo features to build with, after the host section's own
  `features`. A blank entry is refused.
- **`default_features`**: `false` builds without the package's default
  features. Absent is `true`.
- **`hotpath_features`**: the package's own features that instrument it for
  hotpath, added to the build under `brokkr bench --hotpath`. Absent or
  empty, the bench can't be profiled. Each is a feature name of the built
  package itself. brokkr refuses a dependency's feature (`dep/feature`), an
  optional dependency (`dep:name`), a blank entry and one also in
  `features`. A timing run whose resolved features include one fails its arm
  (`brokkr man bench`, The build).
- **`alloc_features`**: the same, for hotpath's allocation tracking. For a
  crate on hotpath this is typically the feature enabling
  `hotpath/hotpath-alloc`. brokkr adds them to the build under `brokkr bench
  --alloc` in place of `hotpath_features`. Absent or empty, the bench can't
  be alloc-profiled. The same rules apply, and a timing run that resolves one
  fails its arm too.
- **`rustflags`**: rustc flags added to the bench's build, described below.
- **`iterations`**: the default for `brokkr bench --iterations`, a positive
  integer up to 4294967295. Absent is 3. Use 1 when the child samples
  internally and records its own summary. The command line overrides it,
  including with a larger count. The key belongs to the bench, not to a
  `bench_params` set.
- **`uses`**: `[bench_params]` sets whose params the bench takes
  (bench_params).
- **`params`**: the bench's own params (Params).
- **`description`, `version`, `date`, `changelog`**: the devs' notes on the
  bench (Annotations).

The `rustflags` entries apply in order. Each entry is a token string, or `{
from_param = "NAME" }`, which splices in the array of strings at the key the
scalar `from` param NAME selected. An empty array adds nothing. brokkr
refuses at load:

- a blank token, or one holding a NUL or 0x1f (cargo's encoded-flags
  separator)
- a splice naming no param of the bench
- a splice naming a param without `from`, or a `many` one

When the bench binds, brokkr refuses a spliced param left unbound, or one
whose selected entry isn't an array of strings. Every arm is judged before
anything is built.

The flags join the project's own at cargo's live rustflags layer, never
displacing them. A bench that sets the key at all, even to `[]`, builds in a
target dir of its resolved recipe's own (`brokkr man bench`, The build).
Absent, the build is the plain one. A fixed list pins a build setting (a
code alignment, say); a splice lets a param fan out over flag sets. This
example fans one bench out over two layouts:

```toml
[data.layouts]
none = []
f6l64 = [
  "-Cllvm-args=-align-all-functions=6",
  "-Cllvm-args=-align-loops=64",
]

[bench.ab]
script_file = "bench/ab.lua"
params.layout = { from = "data.layouts", default = "none" }
rustflags = ["--cfg", "ab_ensemble", { from_param = "layout" }]
```

`brokkr bench ab --layout all` then runs one arm per layout, each its own
build.

Any other bench key is refused. A bench name is letters, digits, `_` and
`-`, and doesn't start with `-`, since it's the argument `brokkr bench`
takes. It names the bench, not the subcommand it drives: `[bench.cat]` is a
fine name for a bench of pbfhogg's `cat`.

## Annotations

Annotations are notes devs keep for each other about a bench, every one
optional. This example sets all four:

```toml
[bench.cat]
script_file = "benches/cat.lua"
description = "Full-file decode, single pass"
version = 3
date = 2026-11-08
changelog = [
  { version = 3, date = 2026-11-08, note = "Read via io_uring preflight" },
  { version = 2, date = 2026-09-14, note = "Drop the warmup iteration" },
]
```

brokkr reads nothing into them: no version is ordered, paired on or compared
with a changelog, and no date is checked against another. The keys:

- **`description`**: what the bench measures, a non-blank string.
- **`version`**: a positive integer, the bench's revision as its devs number
  it.
- **`date`**: when the current version was set, a TOML local date written
  bare (`2026-11-08`). A quoted date, a time or an offset is refused.
- **`changelog`**: an array of tables, in the order written. Each entry has
  a non-blank `note`, and optionally a `version` and a `date` taking the same
  values as above; any other key in an entry is refused. `changelog = []` is
  allowed and isn't the same as no changelog. Under `include`, the layer that
  sets `changelog` replaces it whole, unlike other arrays of tables, which
  join: two histories never interleave.

brokkr refuses a wrong type or a blank text when the config loads, like any
bench key. `brokkr bench` lists each bench's version and the first non-blank
line of its description, and `brokkr bench NAME --help` shows them all
(`brokkr man bench`). Every run stores them with the rest of the config, so
`brokkr results --compare` names a bench whose annotations differ between
the sides (`brokkr man results`). They're config like any other: a script
can read them through `config()`.

## Params

A param is an argument of one bench, given on the command line after the
bench's name as `--NAME VALUE`. brokkr's own options go before the name. You
declare a param as a table:

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

Every param declares its domain, the values it takes, in exactly one of
these ways, so no value reaches a script unchecked by oversight:

- **`from`**: a dot-separated path into the resolved config (The resolved
  view) whose keys are the param's valid values. A segment is a key, or
  `{NAME}`, which stands for the key another param of the same bench chose.
- **`choices`**: a list of the strings the param takes. brokkr checks every
  item of every value and a `default` alike when the bench binds, and the
  bench's `--help` lists them. Refused at load: an empty list, a blank or
  repeated choice, and a choice holding `,`, which the command line splits
  values on (unless the param sets `split = false`). For a `many` param a
  choice holding `..` is refused too, since binding reads it there as a
  range. Neither kind could ever be given.
- **`validated = true`**: any string, which the script's `validate()` checks
  (`brokkr man bench`, Validate). Use it for a domain the script owns, such
  as one row of a table the Lua holds, or a rule between params. A bench
  whose script declares no `validate()` is refused when it runs and by
  check's `bench` phase.
- **`free = true`**: any string, deliberately unchecked.
- **`integer = "WIDTH"`**: a canonical decimal within `WIDTH`, one of `u8`,
  `u16`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`. Canonical means digits
  only, no `+`, no leading zero, and a `-` only on a nonzero value of a
  signed width. The value must also lie within the optional inclusive `min`
  and `max`, TOML integers inside the width, so a `u64` bound above 2^63 - 1
  can't be written; the width's own maximum needs none. A spelling Rust's
  parser would also take, such as `+5` or `007`, is refused: one value, one
  stored key.
- **`number = true`**: text Rust parses as a finite f64, exponents and a
  leading `+` included, `inf` and `NaN` not. The value must lie within the
  optional inclusive `min` and `max`, each a finite float or an integer
  within 2^53. brokkr judges the parsed f64, so a value a hair past a bound
  can round onto it, and `1`, `1.0` and `1e0` are three keys.
- **`bbox = true`**: `lon_min,lat_min,lon_max,lat_max` under the dataset
  bbox rules (`brokkr man datasets`): four finite coordinates, longitude
  within -180..180, latitude within -90..90, each minimum not above its
  maximum. Its own commas separate the coordinates, so brokkr never splits a
  bbox param: each occurrence is one value, and `split = true` beside it is
  refused.

brokkr checks `integer`, `number` and `bbox` values when the bench binds,
every item of every value and a `default` alike; a default is checked at
load, as a `choices` one is. The value still reaches the script as the
string given, like any plain param's: these domains decide what is refused,
not what a script receives. Lua compares such strings as strings (`"10" <
"2"`), so a script doing arithmetic on one converts it with `tonumber`.

A param with no domain, or with two, is refused at load. The other keys:

- **`help`**: the text the bench's `--help` shows.
- **`default`**: the value when the command line gives none, written as the
  command line would write it, so a list, a range or `all` too (Binding
  params).
- **`default_only`**: `true`, on a `from` param that isn't `many`. When the
  command line gives none, the param binds the one key its table holds, in
  each arm under that arm's parents, recorded as any bound key. A table
  holding no key or several is refused, naming them, so a second key added
  later never silently changes what a run binds. Beside `default` or
  `required` it's refused at load.
- **`many`**: `true` makes the param a collection, binding a list of values
  to one sample. Any other param given several values fans out, one run per
  value.
- **`split`**: `false` takes each value whole instead of splitting it on `,`
  (Binding params), so a value such as `w/highway=primary,secondary` reaches
  the script as one string. Absent or `true`, values split.
- **`required`**: `true` makes the command line give the param. brokkr
  refuses its absence when the bench binds (Binding params), before anything
  is built and without storing a run; a script's own refusal would come only
  after the build, as a failed run. Absent or `false`, a param the command
  line doesn't give is unbound or takes its `default`.

brokkr refuses at load:

- an unknown key, or no domain or two
- a `choices` list as above
- a `default` holding a value not among its choices, or one value twice, so
  a default that loads binds; the same goes for an `integer`, `number` or
  `bbox` default's items
- `min` or `max` on a param that is neither `integer` nor `number`, or one
  outside the width, not a number, or above the other
- an `integer` width not listed above
- a blank `default`
- `required = true` beside a `default`, which would never be used
- `default_only` on a param without `from`, on a `many` one, or beside
  `default` or `required`
- a `from` path with an empty segment, or with `{` or `}` anywhere but
  around a whole segment
- a `{NAME}` that isn't a param of the bench
- params whose `from` paths reference each other in a cycle
- a `{NAME}` naming a `many` param, since a path takes one key

brokkr judges the references on the bench's effective params, after `uses`
(bench_params), so a shared set may reference a param the bench supplies.
Whether a key exists under a `from` path is judged when the bench runs
(Binding params).

A param name is also its `--flag`: lowercase, starting with a letter, then
letters, digits, `_` or `-`. `help` is brokkr's (every bench takes `--help`)
and is the only name a param can't take. brokkr's own options precede the
bench's name, so none of them can collide with a param.

## Binding params

Binding turns the command line after a bench's name into values, as in
`brokkr bench NAME --dataset denmark --variant=raw`. Each param is `--NAME
VALUE` or `--NAME=VALUE`. The token after `--NAME` is its value whatever it
looks like, so `--text --help` gives `--text` the value `--help`. `--help`
or `-h` where a param's name could stand describes the bench instead of
running it (`brokkr man bench`).

A param the command line doesn't give takes its `default`. One with neither
is unbound: the script sees nil for it and the run stores nothing for it,
unless it's `required`, which is refused. Any value on the command line
counts as giving a required param: a blank one meets the blank refusal, and
a collection's `all` over an empty table binds an empty collection.

A param may be given more than once, and a value is a list. brokkr splits it
on every `,`, and the param's values are every item of every occurrence, in
command-line order (`--osc 4705,4706 --osc 4710`). brokkr reads a `default`
the same way. A value of a param that splits can't hold a `,`; a table key
holding one is reachable there only through `all`.

brokkr doesn't split a param with `split = false`: each occurrence, and the
`default`, is exactly one item, commas included. Several values then come
only from repeating the flag (`--expr a,b --expr c` is two values), and a
`default` is one. Only the comma splitting changes: `all` and `LO..HI` still
mean what they mean below, an item at a time, and a blank or repeated item
is still refused. The param's `--help` line says it takes one value per
flag.

Two more spellings have a meaning for some params:

- **`all`**, for a `from` param, is every key of its table, in key order.
  The order is numeric when every key is a canonical decimal (digits, no
  sign, no leading zero, `0` itself one), byte order otherwise. `all` stands
  alone: beside another value it's refused. A plain param's `all` is the
  string `all`.
- **`LO..HI`**, for a `many` param with a `from` path, is a range. Both ends
  are canonical decimals (at most 2^64 - 1), `LO` not above `HI`, expanded
  in place ascending, and every key in it must exist under the table. A
  plain collection's range is refused, since it has no keys to range over,
  and any other param's `..` is part of its key.

What the values do depends on the param:

- **A `many` param** binds them all, as one list, to one sample: a
  collection. One value is a collection of one, and `all` over an empty
  table an empty collection.
- **Any other param** given several values fans out: the bench runs once per
  value, each run its own arm, stored as its own run. Several fanned params
  give every combination, parents outer, each param's values in their order.
  A dependent resolves under each of its parents' values: `--dataset all
  --variant all` runs every variant of every dataset, each dataset's own.

Params bind in dependency order, so a `{NAME}` in a `from` path stands for a
key already chosen. Each kind of param accepts a value as follows:

- a `from` param's value must be a key of the table its path names in the
  resolved view
- a `choices` param's value must be one of its choices
- an `integer`, `number` or `bbox` param's value must be inside its domain
  (Params)
- a `validated` or `free` param's value is taken as it is, and the script's
  `validate()` judges a `validated` one next

brokkr binds every arm before anything runs. A refusal in any arm refuses
the whole invocation, before anything is built and without storing a run.
brokkr names every refusal, each with what it found, and doesn't judge a
refused param's dependents under it. The refusals:

- an unknown param, a missing or blank value, a blank item between commas
  (of a param that splits), or a token that isn't `--NAME`
- a `required` param the command line doesn't give, refused once however
  many arms there are, even in an arm whose parent was refused, its
  dependents not judged. For a `from` param the refusal lists the keys under
  each distinct path the arms' bound parents lead to, where that path is a
  table. Where a path under `datasets` or `host` leads nowhere, it names the
  key this host's view lacks.
- a `from` path that doesn't lead to a table (a missing key on the way, or a
  value that isn't a table), and a bound param whose path needs a param that
  has no value
- a `choices` value not among its choices, with them listed; for an absent
  `required` one, its choices are listed
- an `integer`, `number` or `bbox` item outside its domain, naming the item
  and why (the width, the bounds, or the bbox rule it breaks)
- a value that isn't a key under its path, with the valid keys listed; for a
  `default`, the refusal says so. A default is judged only when it's used,
  so a dependent default (`indexed`) may be missing under some parents as
  long as the command line then gives the param. It's refused when it's used
  under any parent that lacks it.
- a `default_only` param the command line doesn't give whose table, in some
  arm, holds no key or several, with the keys it holds listed
- a key given twice, after lists, repeats, ranges and `all` are expanded
- a range end that isn't a canonical decimal, a range running backwards, a
  range with keys missing (the first few named, and how many more), and
  `all` beside other values
- a fanned param whose values select no key under some parent (`all` over an
  empty table), since nothing would run there
- a key holding a `.` where it's substituted into another param's path,
  since the stored path is its segments joined by dots; a key selected at
  the end of a path may hold one

`datasets` and `host` are the two tables that differ from host to host (The
resolved view). Where a refusal's cause lies in one of them, the refusal
names the host. The causes are a missing key, a missing table on the way, an
empty table under a fan-out, or an empty `default_only` table. For example:

```text
--dataset norway selects "norway", but host bygg has no such key under
datasets (keys: denmark, sweden)
```

For a host with no section, the refusal says that instead of listing keys.
It states only what this host's view of the config holds, never why a key
is absent, since a misspelling is absent too. A default's refusal there has
no advice to fix the default, since the default may be right and the host
the one lacking it. Anywhere else the wording names no host.

A selected entry that is a file entry (The resolved view) is the run's
input. Before any build, brokkr reserves every arm's bound inputs for
reading, and refuses when another reservation writes one. The inputs stay
reserved until the last arm ends. brokkr then hashes each (XXH128, through
the hash cache), hashing a file several arms share once.

brokkr checks each input against its pin:

- A pinned entry must match its pin, or brokkr refuses the invocation,
  naming the file, both digests and where the pin was set.
- An unpinned entry is hashed and its digest recorded.
- Two entries naming one file are each judged against their own pin.

A `file` must be a regular file. A plain dataset's `path` may be a
directory, digested as a tree. Neither may be a FIFO, device or socket.
brokkr refuses a file that changes while it's hashed (its stamp moves, or
its path comes to name another file) and doesn't cache its digest.
`--dry-run` checks every input's shape and hashes only the pinned ones,
against their pins, since it records nothing; it reserves none.

Each run stores every bound param with its `from` path, `{NAME}` references
substituted, and its keys in bound order, with a file's digest beside each
(`reference/bench-store.md`, The run document). A file pairs on its key and
digest, any other param on its key alone, and a collection on its keys and
digests in order, so `--osc 4706,4705` and `--osc 4705,4706` don't pair.

## The resolved view

The resolved view is the table `from` paths walk and `config()` returns. It
starts from the composed brokkr.toml, includes folded and the user-wide
layer beneath, and changes it in four ways:

- brokkr drops every hostname section and selects the one for this host
  (`[HOSTNAME]` or `[host.HOSTNAME]`). Its datasets become top-level
  `datasets`, and the rest becomes `host`, with `name` the hostname and
  `data`, `scratch` and `output` absolute (defaults `data`, `data/scratch`,
  `data/tilegen`). A host without a section gets empty `datasets` and the
  defaults.
- every dataset file entry's `file` is made absolute against the host's data
  directory, and a plain dataset's `path` against the project root.
- each dataset's primary also appears as `snapshot.base`: its `pbf` and
  `osc` tables (empty when it has none) and its `download_date`. So `from =
  "datasets.{dataset}.snapshot.{snapshot}.pbf"` with `snapshot` defaulting
  to `base` covers the primary and every snapshot.
- every `[data]` file entry's `file` is made absolute against the project
  root.

A file entry is a table holding a string `file` under `datasets` or `data`,
pinned by `xxhash`, or a dataset holding a string `path`, pinned by
`xxh128`. brokkr refuses a pin that isn't a 32-digit XXH128 string when its
entry is bound. A file entry's subtables may be file entries too, and one
inside an array has its `file` made absolute, though no `from` path can
select it. Every other table is left as composed.

The raw TOML doesn't change. brokkr derives the view per run from what the
run stores (its config text, project root and hostname), and doesn't store
the view itself.

## bench_params

The `[bench_params.NAME]` table declares params several benches share, once.
This example declares a `dataset` set and a bench that uses it:

```toml
[bench_params.dataset]
dataset = { from = "datasets", default = "denmark" }
variant = { from = "datasets.{dataset}.pbf", default = "indexed" }

[bench.cat]
script_file = "benches/cat.lua"
uses = ["dataset"]
```

A bench takes every param of every set it `uses`, then its own `params`.
Each of its own replaces a set's param of the same name whole, not key by
key. brokkr refuses:

- a `uses` entry naming no set
- a set listed twice
- two used sets declaring a param the bench doesn't declare itself

A set no bench uses isn't an error: a shared include may hold sets for
several projects. Set names follow bench names' rules, and each declaration
follows the param rules (Params).

## data

The `[data]` table holds project data that bench params select from with
`from` paths. This example declares one file entry:

```toml
[data.workloads.hotpath.same_obj_read]
file = "examples/fields/same_obj_read.lua"
xxhash = "1234567890abcdef1234567890abcdef"
```

brokkr reserves the name and doesn't interpret what's inside it, beyond
requiring a table. A project's own tables need a home, since any other
unknown top-level table is a hostname section.

The table's shape is how a project limits what a param can select. Put the
scale outer and the workload inner, as above. A bench that must never run a
seconds-scale file (one built with a profiler that backlogs on it) binds
`workload = { from = "data.workloads.hotpath" }`, so that file isn't a value
it can take. Another bench binds `scale = { from = "data.workloads" }` and
`workload = { from = "data.workloads.{scale}" }` over both.

A refusal `bench()` makes instead comes only once the build has run, and is
stored as a failed run. A refusal a script's preflight makes (`brokkr man
bench`, Preflight) comes before any build and stores nothing, but it's still
the script's code, not the shape of what the script can be given.

## bench_lib

The top-level `bench_lib` key names the directory a bench script's
`require(name)` loads modules from (`brokkr man bench`, Modules):

```toml
bench_lib = "bench/lib"
```

With that setting, `bench/lib/preflight.lua` is `require("preflight")` and
`bench/lib/io/uring.lua` is `require("io.uring")`. brokkr resolves the key
against the directory of the config file that set it, as `script_file` is,
so a shared include can keep its library beside it. Every bench of the
project shares it.

brokkr refuses a blank value or one that isn't a string. It reads the
directory itself when `brokkr bench` starts, and when `brokkr check` runs in
a project that declares a bench or sets `bench_scripts`. A directory that
can't be read refuses there.

Every name in the directory is one file: no directory or stem with a dot is
a module. brokkr never follows links, even one swapped in while it reads the
directory. It never opens an entry whose name couldn't be a module (a
`README`), so such an entry can't fail the read.

Keep the bench scripts themselves out of the directory. Every `.lua` file
there is a module, and a script assigns `bench`, which `brokkr check`
refuses in a module. A `[bench_lib]` table isn't this key but a hostname
section.

## bench_scripts

The top-level `bench_scripts` key names the directory every bench script
lives under, and `bench_scripts_exempt` lists paths under it that need no
bench:

```toml
bench_scripts = "../benches"
bench_scripts_exempt = ["../benches/scratch", "../benches/old/probe.lua"]
```

`brokkr check`'s `bench` phase holds the directory both ways:

- Every bench of the composed config, whichever included file declared it,
  names a `script_file` located under the directory. An inline `script`, or
  a `script_file` located outside, is a finding naming the bench and the
  config file or the script.
- Each `.lua` file below the directory must be the `script_file` of some
  `[bench.NAME]`, a module `bench_lib` loads, a contract-test file
  `bench_tests` holds, or covered by `bench_scripts_exempt`. Each finding
  names the file. The contract-test files are exactly the files the tests
  run, found by the same walk.

Those last three excuse a file from needing a bench, never a bench from
living under the directory. With `bench_scripts` set the phase runs even
with no bench declared, and then checks `bench_lib` too. Unset, brokkr
discovers nothing, and an inline script or a `script_file` anywhere is fine.
Neither refusal is a load error: a misplaced script fails the check and
blocks no other command.

brokkr judges a script by its location when the check runs: its directory
resolved on disk, its name as spelled. So a link named `fam/x.lua` under the
directory is under it wherever it points, and a script reached through a
linked directory leading out is outside. A `script_file` whose directory
can't be resolved isn't judged for place, since reading it fails the phase
already. Inline scripts are refused even when the directory itself can't be
resolved.

Both keys resolve against the directory of the config file that set them,
as `script_file` does. The exemptions resolve beside the file that set the
list, and the list replaces whole across layers. An inherited list therefore
keeps naming paths beside its own file, even where a later layer moves
`bench_scripts`. An exemption is a file, or a directory covering everything
below it; one that names nothing isn't an error. brokkr refuses
`bench_scripts_exempt` without `bench_scripts`, a blank path, and a value of
another shape.

brokkr reads the directory from disk when the check runs, untracked files
included, since a new script is exactly what the key is for. A file is its
location, the real directory it sits in plus its name, never what it links
to. A link `alias.lua` to a registered script needs a bench of its own, and
a link sitting outside an exempt directory isn't exempt for pointing into
it.

Only a regular file whose path makes a module name counts as a `bench_lib`
module here: the files `require` can load. Anything else in the library
ending `.lua` needs a bench or an exemption. brokkr doesn't descend a linked
directory, so it doesn't discover a script reached only through one, and it
opens nothing it finds. A directory that can't be resolved, listed or
inspected is a finding rather than skipped.

A `[bench_scripts]` or `[bench_scripts_exempt]` table isn't this key but a
hostname section.

## bench_tests

The top-level `bench_tests` key names the directory of the project's
contract tests (`brokkr man bench`, Contract tests):

```toml
bench_tests = "../benches/tests"
```

Every regular `.lua` file below the directory is a test file. `brokkr
check`'s `bench` phase runs them, and with the key set the phase runs even
with no bench declared. `brokkr bench --test` runs them alone. brokkr
resolves the key against the directory of the config file that set it, as
`bench_lib` is, and refuses a blank value or one that isn't a string. Unset,
nothing is tested.

brokkr reads the directory when the tests run, with the walk `bench_scripts`
uses:

- untracked files are included
- a linked directory isn't descended
- a `.lua` entry that isn't a regular file (a link included), or that can't
  be inspected, is a finding rather than a test

A test file that is also a bench's `script_file` or a `bench_lib` module is
a finding: each runs in a different Lua state by different rules. A
`[bench_tests]` table isn't this key but a hostname section.

## Reserved names

Several top-level names belong to script benches, and the user-wide layer
can't hold any of them. `bench`, `bench_params` and `data` are reserved
top-level names, and `bench_lib`, `bench_scripts`, `bench_scripts_exempt`
and `bench_tests` are reserved as values. You write a host literally named
one of the first three as `[host.bench]`, `[host.bench_params]` or
`[host.data]`. The user-wide `brokkr.toml` can't hold any of these names:
benches belong to one project.

## Inline line numbers

An inline `script` is a TOML string, so its Lua lines aren't the file's. The
string starts partway down the file, and escapes, a multi-line string's
trimmed first newline and a line-ending backslash move lines about.

brokkr maps them back in these steps:

1. It reads the file that set `script` again, per value, so under `include`
   it reads the layer that gave the script, wherever its build settings came
   from.
2. It finds `bench.NAME.script` in any TOML spelling: a table, dotted keys,
   an inline table, quoted keys.
3. It decodes the string itself, keeping each byte's file line.

brokkr uses the map only when that decoding, the TOML parser's and the
script brokkr loaded are the same bytes. Otherwise the lines count from the
string's first, the chunk is named `[bench.NAME] script`, and brokkr says
why (the file was edited since it was read, say). It says so in the check
finding itself, and as a warning when `brokkr bench` reads the script.

A Lua line is named by the file line its first byte came from. A line
joined by a line-ending backslash is named by its first file line, and
several Lua lines written with `\n` escapes on one file line all name it.

With a map, everything that names a line names the file's line, under the
config file's path as the chunk's name:

- the `bench` check phase
- a compile failure when `brokkr bench` reads the script
- every failure of an iteration, a Lua error's traceback included

A failure then reads like this:

```text
/p/brokkr.toml:14: attempt to call a nil value
```

A `script_file` is its own file, so its lines need no map.
