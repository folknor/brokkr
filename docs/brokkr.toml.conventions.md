# Convention gates

This page covers the brokkr.toml tables that configure `brokkr check`'s
source-convention phases, its rustdoc phase and its fixture-lint phase. The
convention phases are gremlins, header, textlint, script checks, manifest and
dependency rules. Each `##` section is one table, addressable by its name:
`brokkr man conventions script_check`. The rest of the schema is in
`brokkr man config`, and how each phase runs is in `brokkr man check`.

## gremlins

The `[gremlins]` section tunes the scan for banned Unicode (curly quotes,
dashes, invisible characters) and the `--fix-gremlins` rewrite. Omit the
section to scan every default file with the built-in banned set. This example
sets every key:

```toml
[gremlins]
disable = false                     # skip the phase entirely
exclude = ["docs/reference-manual", "vendor/upstream-docs"]
include = ["LICENSE", "**/*.txt"]   # scan beyond the built-in list
allow = ["U+2019"]                  # un-ban these codepoints
ban = ["U+2011"]                    # flag these codepoints
```

The keys:

- **`disable`** (default `false`): skip the whole gremlin phase, both the
  scan and `--fix-gremlins`. Use it to drive a foreign checkout whose Unicode
  you don't want brokkr to police or edit.
- **`exclude`**: directories that both the scan and `--fix-gremlins` skip.
  Use it for vendored material from an outside source that legitimately has
  typographic punctuation, BOMs and bidi marks. Entries are
  project-root-relative directories, matched by path prefix on the
  git-relative path: `docs/manual` covers `docs/manual/` and below, but not a
  sibling `docs/manual-extra`. brokkr rejects empty and absolute entries at
  parse time. `exclude` wins over `include` and over the built-in extension
  list.
- **`include`** (default empty): globs, in the syntax of the path selectors
  section, naming files to scan beyond the built-in extension list. That list
  is `.rs`, `.toml`, `.md`, `.markdown`, `.js`, `.sh`, `.py`, `.yml` and
  `.yaml`, case-insensitive. Without `include`, brokkr doesn't scan an
  extensionless file such as `LICENSE`, or a file with any other extension.
  brokkr rejects an invalid glob at load time.
- **`allow`**: codepoints to remove from the built-in banned set. The scan
  skips them and `--fix-gremlins` leaves them in place. For example, permit
  `U+2019` if a repo deliberately uses curly apostrophes.
- **`ban`**: codepoints to flag beyond the built-in set. This is
  **scan-only**: brokkr has no ASCII mapping for an arbitrary codepoint, so
  `--fix-gremlins` doesn't rewrite them, and you fix them by hand.

`--fix-gremlins` follows the scan, so it can rewrite an included file too.
Name paths deliberately, never test fixtures that have typographic Unicode on
purpose. An included file gets the same byte scan and fix as any other, with
no check that it is text:

- A binary passes or fails on its bytes, and invalid UTF-8 is reported byte
  by byte.
- A selected symlink is read and rewritten through to its target.
- A selected gitlink of an initialized submodule fails the phase as
  unreadable.

A broad glob such as `**` pulls in lockfiles and data, and the scan cost grows
with it.

An `allow` or `ban` entry is a `U+XXXX` codepoint string, with a
case-insensitive `U+` prefix and 1 to 6 hex digits. It can also be an
inclusive range, `U+AAAA..U+BBBB` or `U+AAAA..=U+BBBB`. A range bans a whole
block in one entry: `ban = ["U+0400..U+04FF"]` bans Cyrillic. brokkr rejects
a bad token, a reversed range, or a codepoint listed on both sides at parse
time. The `U+XXXX` form keeps `brokkr.toml` itself free of literal, possibly
invisible gremlin characters.

brokkr reads files as bytes. A scanned file that isn't valid UTF-8, such as a
Latin-1 `.md` whose raw 0xA0 is an NBSP, reports each invalid byte as
`byte 0xNN INVALID UTF-8` at its own line and column. `--fix-gremlins` leaves
such a file untouched, since rewriting it would mean guessing its encoding.

A file that can't be read at all fails the phase, naming the file. The same
holds for the header, textlint and manifest phases. brokkr passes over only a
path deleted from the working tree that `git ls-files` still lists.

## Path selectors

The header, textlint, manifest and gremlin `include` keys share one glob
syntax. Each glob is project-root-relative and matches against the check's
snapshot of tracked and untracked-not-ignored files. `**` crosses directory
separators; `*` stays within one path component.

A dead selector doesn't fail or warn in an ordinary check. That covers a glob
that selects nothing, and an exclusion that excuses everything its selector
picked. An exclusion may reserve a path that is absent today, and user-wide
rules name paths that most projects don't contain. `brokkr check --audit`
reports dead selectors for every key below and for `[gremlins].exclude`,
without running anything else (see `brokkr man check audit`). A
`[[textlint]]` rule can opt in to failing on its own dead scope with
`require_live_paths`.

brokkr judges liveness over the snapshot's file list, less the paths observed
absent from disk. git keeps listing a file that is deleted but not yet
staged, and no scanner reads it, so it keeps no selector live. A dangling
symlink is absent too, and a path whose presence can't be determined is an
error. Presence is one observation, not a lock on the tree, and it says
nothing about content.

The selectors and what each does:

| Selector | Effect |
| --- | --- |
| `[header].paths`, `[header].exempt` | Select files for the header check, then excuse matching selected files. |
| `[[textlint]].paths`, `[[textlint]].exclude` | Select files for each rule, then excuse matching files from that rule. |
| `[manifest].paths`, `[manifest].exclude` | Select manifests for all enabled checks (default `**/Cargo.toml`), then excuse matching manifests from all checks. |
| `[manifest].shape_exclude` | Skip structural and target-shape checks for matching included manifests; dependency-content checks still run. |
| `[gremlins].include` | Add matching files to the gremlin scan beyond the built-in extension list. A glob counts as matching even when the extension list already covers its files or `[gremlins].exclude` removes them. |

`[gremlins].exclude` is a directory-prefix list, not a glob selector; the
gremlins section states its path rules. The audit judges its entries against
the built-in extension list plus `include`.

## header

The `[header]` section turns on the header phase, which checks that each
selected file contains the configured `pattern`. brokkr expands `{year}` in
the pattern to the current UTC year, and missing text or a stale year fails.
brokkr searches for the pattern as a byte substring anywhere in the file and
doesn't check its position. The section is absent by default.

This example requires a copyright line in every crate source file except
examples and one vendored tree:

```toml
[header]
paths = ["crates/**/*.rs"]
pattern = "Copyright (C) 2015-{year}"
exempt = ["**/examples/**", "**/core/rust/**"]
```

`paths` and `exempt` follow the path selectors section. The byte search also
works on files that aren't valid UTF-8, so brokkr judges them on their
content rather than skipping them. brokkr takes the current year from Unix
time, with no date-crate dependency.

## textlint

The `[[textlint]]` array declares rules that forbid a regex on a line, the
textlint phase. It is the generic engine behind grep-style convention hooks.
Each entry scans the files matching `paths`, and a line matching `pattern` is
a violation. The array is empty by default.

This example declares three rules of increasing scope control:

```toml
[[textlint]]
name = "no-todo-macro"
pattern = "todo!\\("
paths = ["crates/**/*.rs"]
message = "finish or file an issue instead of todo!()"

[[textlint]]
name = "anyhow-import-context-only"
pattern = '^\s*use anyhow::'
paths = ["crates/**/*.rs"]
exclude = ["**/*ANYHOW*", "**/anyhow_style_guide*"]  # docs that demo it
except = ['^\s*use anyhow::Context;\s*(//.*)?$']     # the allowed form
message = "only `use anyhow::Context;` is allowed; fully-qualify the rest"

[[textlint]]
name = "no-tokio-spawn-in-adapters"
pattern = 'tokio::spawn\('
paths = ["crates/adapters/**/*.rs"]
exclude = ["**/tests/**"]
skip_after = '^#\[cfg\(test\)\]'   # ignore everything after the test module
message = "adapters must use get_runtime().spawn()"
```

Every rule has four fields:

- **`name`**: the rule's label in violation output
- **`pattern`**: a linear-time `regex`; a match is a violation
- **`paths`**: the files the rule scans, per the path selectors section
- **`message`**: the text shown with each violation

The optional modifiers are bounded. `exclude` follows the path selectors
section; the rest are local to the rule:

- **`exclude`**: files excused after `paths`, such as docs that deliberately
  show the forbidden pattern, or `tests/` trees.
- **`allow_marker`**: a line containing this literal is skipped, such as an
  author's `// allow-...` comment. `allow_marker_above = N` also skips a
  line when the marker is on one of the N lines above it; 0 means the same
  line only. Use it for markers that a wrapped construct pushes off the
  offending line.
- **`only_if_file_matches`**: a regex; the rule fires only in files where
  some line matches it. This is a cheap stand-in for import awareness: flag a
  bare `Instant::now()` only where the file imports `Instant`. Set
  `only_if_file_matches_above = true` to require the precondition at or above
  each match, so a later line can't arm an earlier match.
- **`region`**: `code`, `string` or `comment`, for Rust files. It scopes
  where `pattern` may match: `code` never flags a pattern quoted in a comment
  or string, and `string` targets message text, such as a `", got"` phrasing
  rule. brokkr tokenizes with the rustc tokenizer as `ra-ap-rustc_lexer`
  publishes it, so current literal forms such as raw C strings split
  correctly. Only `pattern` is scoped; markers, `except` and reporting still
  use the physical line.
- **`join_wrapped_use`**: when `true`, brokkr matches `pattern` against whole
  `use ...;` statements. It first reconstructs a rustfmt-wrapped import onto
  one line, with comments stripped, so `use tracing::.*warn` catches a
  multi-line `use` block. brokkr reports the violation at the `use` line, and
  `allow_marker` matches on any physical line of the statement. Rust-only.
- **`except`**: regexes; a line matching any of them is exempt. Use it to
  allow one specific form of an otherwise forbidden pattern.
- **`in_toml_section`**: only consider lines while the last-seen `[section]`
  header equals this value.
- **`table_row_only`**: only consider markdown table rows.
- **`skip_after`**: a regex; once a line in a file matches it, every
  *following* line in that file is exempt, though brokkr still checks the
  matching line itself. Use `skip_after = '^#\[cfg\(test\)\]'` to stop a rule
  firing inside the test module. It is a textual cut; `test_code` is the
  structural one.
- **`test_code`**: `"include"` (the default), `"skip"` or `"only"`. It
  scopes `pattern` to production code or to test code, as the test_code
  section describes. It composes with `region` and can't be combined with
  `join_wrapped_use`. `skip_test = true` reads as `test_code = "skip"` and
  `skip_test = false` as `"include"`; one table may not set both keys.
- **`must_scan`**: exact project-relative paths this rule must actually scan.
  A listed path the rule didn't scan is a violation at `path:0`; the
  conditions follow this list.
- **`require_live_paths`**: when `true`, the rule's dead scope fails the
  textlint phase instead of being an audit finding only; the conditions
  follow this list. It is off by default. A preset may set it, and a rule may
  set it back to `false`.

`join_wrapped_use` *replaces* the per-physical-line pass for its rule. The
rule no longer scans ordinary lines, only reconstructed `use` statements, so
it is for import rules, not use-site rules. brokkr rejects a rule that sets it
alongside an `except` that exempts ordinary `use` lines at load time, since
that rule could never fire.

Each `must_scan` path must be a present, readable text file selected by
`paths` and not `exclude`, with at least one eligible line. An eligible line
meets every condition:

- It isn't cut off by `skip_after`.
- It satisfies `only_if_file_matches`, at or above the line when positional.
- Under `test_code` in a `.rs` file, it holds code in scope, not only a
  comment beside it. Any other file admits on non-blank content under
  `"skip"`, and never under `"only"`.

`must_scan` guards against a rule whose scope has silently stopped covering
the files it was written for. It is a sentinel, not proof that every byte of
the file was covered.

Under `require_live_paths`, every `paths` glob must select a present file.
brokkr judges each glob alone, so a live sibling can't hide a dead one.
`exclude`, all its globs together, must leave at least one of the files
`paths` selects. Each failure is a violation of the rule, printed as
`[name] require_live_paths: ...` with no file or line. A dead `exclude` glob
stays legal, since an exclusion may reserve a path that is absent today, and
the audit keeps reporting it.

`require_live_paths` judges by path, like the audit. A rule whose selected
files are all binary, refused by `only_if_file_matches`, or emptied by
`test_code` still passes; `must_scan` is the guard for files that must
actually be scanned.

Textlint does no arbitrary multiline matching; `join_wrapped_use` is the one
exception, bounded to `use` statements.

brokkr decodes a matched file that isn't valid UTF-8 lossily and scans it.
ASCII, which patterns are written against, survives verbatim, and an invalid
byte becomes U+FFFD. The exception is `test_code`: a Rust file the classifier
reaches in production context must be valid UTF-8 to parse, so an invalid one
refuses the classification instead.

brokkr skips a *binary* file, one with a NUL in its first 8000 bytes, which
is git's heuristic. A broad `paths` glob legitimately sweeps in images and
archives, where a line pattern means nothing.

### test_code

The `test_code` key scopes a textlint rule to production code or to test
code in `.rs` files. This example declares one rule of each kind:

```toml
[[textlint]]
name = "no-private-epsilon"
pattern = '[0-9.][eE]-[0-9]'
paths = ["crates/strategy/src/**/*.rs"]
exclude = ["crates/strategy/src/tolerance.rs"]
region = "code"
test_code = "skip"
allow_marker = "// epsilon-ok:"
must_scan = ["crates/strategy/src/fill/sorting.rs"]
message = "name a tolerance-module constant instead of a private epsilon"

[[textlint]]
name = "test-data-paths-via-common"
pattern = '\bdata/'
paths = ["src/**/*.rs", "tests/**/*.rs"]
exclude = ["tests/common/**"]
region = "string"
test_code = "only"
message = "resolve datasets through tests/common, not a data/ literal"
```

`test_code = "skip"` blanks every byte that brokkr's test-code classifier
proves absent from every non-test build, before matching `pattern`.
`test_code = "only"` is its complement, for code that exists for tests. It
blanks every other byte, and also what is proven absent from test builds as
well, as described below. `allow_marker`, `except` and the context windows
still read the raw physical line.

The classifier needs a Cargo workspace. Its roots are every workspace target
from `cargo metadata --no-deps`, with integration-test targets as test roots
and every other kind as production roots. The classifier runs only when some
rule with a `test_code` scope selects at least one tracked `.rs` file. A
user-wide scoped rule costs nothing in a tree with no Rust to scan.

`"skip"` skips three kinds of code:

- Files reached only through test contexts: `#[cfg(test)] mod x;` and
  everything below it, `#[path]` targets of such declarations, and
  integration-test targets. Modules resolve with rustc's directory rules, and
  `#[path]` is followed plainly or under `cfg_attr`. A file isn't classified
  by its name: a production file called `tests.rs` stays covered.
- Items, statements, fields, arms, generic parameters and arguments whose
  `cfg` is false whenever `test` is false. The proof uses three-valued logic
  with every other cfg atom unknown, so `cfg(all(test, feature = "x"))` is
  skipped and `cfg(any(test, feature = "x"))` isn't.
- Free functions with a bare `#[test]` (single segment, no arguments), with
  everything they declare or include.

The bare `#[test]` rule applies only when no reachable source can shadow the
builtin attribute. A shadowing site is a `use` binding `test`, or a
`#[macro_use] extern crate` that doesn't exclude it, directly or under any
`cfg_attr`. Macro input counts, such as a `use ... test` or `as test` inside
a `cfg_if!`, read conservatively. With any shadowing site, every bare
`#[test]` is scanned as production, and a warning names the sites. A glob
import isn't a site: a glob-imported `test` colliding with the builtin is an
ambiguity error.

A file reached in several contexts keeps a byte skipped only when every
production occurrence skips it. A file nothing reaches, or one the tripwire
hands back, is scanned as production with only its own `cfg` masks applied.
Its bare `#[test]` functions stay scanned, since its crate root, where a
shadowing `#[macro_use]` could sit, is out of sight.

The classification is scoped to the **unexpanded source model**. Macro
expansion and generated source are outside it, for module edges and for
skipped spans alike:

- Macro input is opaque. A `#[cfg(test)]` inside a `macro_rules!` body or a
  macro call proves nothing, and that code is scanned. brokkr reads macro
  input only in the loud direction: for literal `include!`s, shadowing
  imports and the tripwire.
- Attribute macros may rewrite the cfg attributes inside their input, and
  derives may emit code no source scan sees.
- brokkr accepts `include!(concat!(env!("OUT_DIR"), ...))` as generated
  source without following it.

brokkr follows literal `include!`s, including ones inside macro input.

The classifier refuses rather than guess, naming each site, and the textlint
phase fails with that refusal. It refuses when a production context holds any
of these:

- any other computed `include!`
- a node `syn` keeps verbatim
- an inline module with a `cfg_attr` path
- a module with both `x.rs` and `x/mod.rs`
- a reachable file that doesn't parse

A refusal withholds every `test_code` rule from every `.rs` file, since none
of them can be judged without the classification. brokkr still scans every
other pair, so the phase reports the rest of its findings ahead of the
refusal: violations, `require_live_paths` dead scope, and `must_scan` paths
that are untracked or unselected. The same holds when the classification
can't run at all, such as `cargo metadata` failing: the phase fails with that
error, not a generic failure.

The classifier also runs a tripwire, as detection rather than proof. brokkr
hands a file the walk calls test-only back to production, with a warning,
when production macro input names it. A name is a `mod NAME` inside macro
tokens, or a path-shaped string literal whose components are a suffix of the
file's path. A string literal is path-shaped when it contains `/` or ends in
`.rs`.

`"only"` selects exactly what `"skip"` skips, less code that no test build
contains. A byte stays selected when some occurrence of its file doesn't
prove it absent from test builds. That proof evaluates `cfg` with `test` true
and every other atom unknown, and also reads the `cfg` a `cfg_attr`
introduces. So `#[cfg(not(test))]` inside a test module, `#[cfg(any())]`, and
a module declared at such a position are dropped. brokkr doesn't track
correlations between atoms: `#[cfg(feature = "x")] #[cfg(not(feature =
"x"))]` is impossible but stays selected.

The exclusive definition has these consequences:

- Shipping code is never test code, even when an integration test also
  includes it by `#[path]`. A rule meant for both uses the default
  `include`.
- `#[cfg(any(test, feature = "x"))]` helpers ship under a feature, so they
  are production and not selected.
- Test code the source model can't see, inside macro input or generated by a
  macro, isn't selected; nor is any file that isn't Rust.
- A file nothing reaches, or one the tripwire hands back, has only its own
  `#[cfg(test)]` spans selected.

When something could shadow the builtin `#[test]`, `"skip"` scans a bare
`#[test]` function as production, which errs toward scanning. For `"only"`
the same doubt would drop the function silently. So the classifier also
classifies as if every bare `#[test]` were the builtin. An `"only"` rule that
selects a file whose selection differs between the two readings refuses,
naming the files and the shadowing sites.

That refusal withholds only those (rule, file) pairs. The classification
stands for every other rule and file, which brokkr scans and reports ahead of
the refusal. brokkr doesn't refuse a file that an independent proof settles
either way, such as production code also reached through a bare test
function.

## textlint_preset

A `[textlint_preset.NAME]` table is a named bundle of textlint scope and
predicate fields that rules pull in with `preset`. Use it for a family of
rules that differ only in `pattern` and `message` but share a long `exclude`
list, the shape a ported hook suite converges on. The preset single-sources
the scope, so adding one exempt file is a one-line edit, not the same edit in
every rule of the family.

This example shares one scope between two rules, and the second rule adds an
exclusion of its own:

```toml
[textlint_preset.dst-scope]
paths = ["crates/*/src/**/*.rs"]
exclude = [
    "crates/adapters/**", "crates/backtest/**", "crates/pyo3/**",
    "**/tests/**", "**/*_test.rs",
]
region = "code"
skip_after = '^\s*#\[cfg\((all\()?test'
allow_marker = "dst-ok"

[[textlint]]
name = "dst-no-wallclock-qualified"
preset = "dst-scope"
pattern = 'std::time::Instant::now\(\)|chrono::Utc::now\(\)'
message = "route through a DST seam"

[[textlint]]
name = "dst-tcp-seam-qualified"
preset = "dst-scope"
pattern = 'tokio::net::TcpStream::connect\b'
exclude = ["crates/network/src/net.rs"]   # appended to the preset's list
message = "route through nautilus_network::net"
```

A preset may set any `[[textlint]]` field except `name`, `pattern` and
`message`, the three that identify one rule. An unknown or misspelled field
in a preset is a load-time error, even if no rule uses the preset. A preset
that **no rule references** is also a load-time error: brokkr rejects dead
config that would load clean, so define a preset only where a rule draws on
it.

brokkr merges a preset into a rule by these rules:

- **Nearest value wins.** brokkr keeps a field the rule sets itself, even one
  set back to its own default: `join_wrapped_use = false` beats a preset's
  `true`. The merge works on raw TOML before deserialization, so "set to
  false" and "absent" stay distinguishable.
- **`paths`, `exclude` and `except` concatenate**, preset entries first, so a
  rule *adds* to a shared list instead of replacing it. Narrow a preset's
  `paths` with the rule's own `exclude`.
- **A list of presets** (`preset = ["rust-src", "dst-markers"]`) applies left
  to right under the same rule. For a scalar, the rule's own value wins, then
  the first-listed preset, and so on. For a list, the entries concatenate in
  declaration order: `preset = ["a", "b"]` gives `a`'s entries, then `b`'s,
  then the rule's own. Lists and scalars agree that the earlier-listed source
  wins.

brokkr resolves presets entirely at parse time. The check phases see ordinary
fully resolved rules and have no notion of a preset.

## script_check

The `[[script_check]]` array runs commands and asserts their output, the
script-check phase. It is the escape hatch for gates brokkr's native phases
can't express, such as semantic analysers or external formatter conventions.
Each entry runs `command` through `sh -c` and passes only if the captured
output matches `expect`; on failure, brokkr shows the captured output.
Matching a sentinel, not the exit code, catches a check stubbed to `exit 0`.

brokkr keeps at most 1 MiB per output stream. If a stream the rule selects
exceeds that bound, or its pipe stays open past the drain grace, the check
fails, because the capture can't prove the full output matched.

This example sets every key, with each key's values and default in its
comment:

```toml
[[script_check]]
name    = "docs-conventions"
command = "bash .pre-commit-hooks/check_docs_conventions.sh"
expect  = "All documentation conventions are valid"
match   = "last-line"   # exact | last-line | contains
stream  = "stdout"      # stdout | stderr | both
stage   = "pre-clippy"  # pre-clippy | pre-test | post-test | background
diagnostics = "opaque"  # opaque | rustc
when    = "always"      # off | gate | always
note    = "notice:"     # line prefix echoed on a pass, default none
```

The keys:

- **`name`**: the label shown when the entry **fails**
  (`beta: stdout did not match "ok"`), or ahead of a passing entry's `note`
  lines.
- **`command`**: run as `sh -c "COMMAND"` with the code tree as the working
  directory, so pipes, redirects and environment expansion work.
- **`expect`**: the success sentinel. Keep it ASCII: a non-ASCII sentinel,
  such as a check mark, would trip the gremlin scan on `brokkr.toml` itself.
- **`match`**: `exact` (the whole trimmed output equals `expect`),
  `last-line` (the last non-empty line equals `expect`; the default), or
  `contains` (substring).
- **`stream`**: `stdout` (the default), `stderr` or `both`.
- **`stage`**: where in the check pipeline the entry runs; see the stage
  section.
- **`diagnostics`**: the output shape a **failure** is rendered against; see
  the diagnostics section.
- **`when`**: which runs the entry is part of; see the when section.
- **`note`**: a line prefix for the script's pass-time disclosures; see the
  note section.

`stream = "both"` concatenates the separately captured streams: stdout, a
newline, then stderr. It can't preserve their original write order. `exact`
compares the full combined text after trimming, so it can match a stderr-only
sentinel or an expected value containing both streams. `last-line` uses the
last non-empty line of the concatenation, which comes from stderr whenever
stderr has a non-empty line. If the sentinel must follow actual print order,
redirect both streams into one in the command, for example with `2>&1`, and
select that stream.

brokkr ignores the command's exit code; only the output match decides. See
also `brokkr man check`.

A passing stage reports once, not one line per entry. For `pre-clippy` that
is `script-check (N checks)` on the conventions line; a later stage prints
its own `script-check: ok (N checks)` line. A passing gate's name gives you
nothing to act on, while the count still says which corpus passed. A stage
with some failures prints `script-check: M of N ok` above the failure block.

### when

The `when` key decides which runs a script check is part of. It uses the
same vocabulary as `[member_check]` and `[[fixture_lint]]`:

| `when` | Runs |
| --- | --- |
| `always` (default) | In every `brokkr check` that runs the `script_check` phase |
| `gate` | Only under a `certifies = "complete"` profile (`--gate`, or a complete `--profile` or `default_profile`) |
| `off` | Never in a plain check |

The defaults differ on purpose. A `[[script_check]]` entry without `when`
runs everywhere, and `[[lint_sweep]]` (`brokkr man sweeps`) also defaults to
`always`. `[member_check]` and `[[fixture_lint]]` are expensive by nature and
default to gate-only.

Use `gate` for an expensive check, such as Miri, that belongs in the
pre-landing run and not the edit loop. It avoids a `skip_phases` profile,
which would also drop the cheap phases. `when` is orthogonal to `stage`, so a
gate-only entry may sit at any stage, and `background` suits a slow one.

`brokkr check --script NAME` runs the named entry whatever its `when`. It is
an explicit request, and the way to try a gate-only check without running the
gate. Set `off` and use `--script` for a check you run only on demand. A
profile's `skip_phases = ["script_check"]` still drops every entry whatever
its `when`, and a complete profile can't skip phases.

brokkr doesn't count an entry its `when` leaves out on the stage's
`script-check (N checks)` line. The run log names the entries left out, for
example `script-check: 1 gate-only check not run, no complete claim: miri`.
The console stays quiet, since that line would be noise on every edit-loop
run.

A `gate` entry defined in the project's own config, its `brokkr.toml` or an
include, is a **load error** when no `[test.profiles.*]` declares `certifies =
"complete"`. That entry could never run in any check, and nothing would say
so. Add a complete profile, or set `when = "off"`. To change an include's
entry, repeat the whole entry under the same name: shadowing replaces the
entry, it doesn't merge keys.

Entries from the user-wide config are exempt from that load error. That file
reaches every project, gate or not, and trees with no `brokkr.toml`, and a
project can't edit it. Such an entry runs under a complete claim, like a
project's own. On every other run it is logged as not run, including every
run in a project with no complete profile.

### note

The `note` key lets a passing script check report what it skipped. A passing
entry prints nothing of its own, so a script that passes while skipping an
optional part, such as a cross-check whose tool isn't installed, looks the
same as one that ran it. When the entry **passes**, brokkr prints each
captured line that starts with the prefix, after leading whitespace, as
`script-check: NAME: LINE`. stdout's lines come before stderr's, whatever
`stream` says.

A note never changes the verdict, and the stage's count line is unchanged. A
failing entry prints its whole output anyway.

brokkr shows at most five note lines per entry, then
`and N more note line(s)`. For each shown line, brokkr does the following:

- strips terminal escapes and control characters
- redacts recognized credential forms and the current home path
- clips it to 240 characters

When a stream overran the 1 MiB capture limit, a line says brokkr didn't scan
the output past it.

Notes print as each entry finishes, so under `pre-clippy` they come before
the conventions line. A `background` stage prints its notes when it is
joined, and not at all if the run was interrupted. A blank or multi-line
prefix is a load error. The absence of a note is the script's silence, not
proof the optional part ran.

### diagnostics

The `diagnostics` key declares what a script check's output looks like, so
brokkr can render a failure down to the part worth reading. It doesn't change
whether the entry passes: the `expect` match always decides.

| `diagnostics` | Failure shows |
| --- | --- |
| `opaque` (default) | Both captured streams, with recognized credentials and the current home path redacted for display |
| `rustc` | Every `error:` / `error[CODE]:` block, then a line counting the warnings not shown |

Both views redact the following:

- recognized credential fields and credential flags
- URL user information and credential query parameters
- bearer and basic values
- the current `HOME` path

Redaction affects display only; the check still matches the original
captured streams. Neither value caps by line or block count.

The only narrowing `rustc` performs is by diagnostic level, which is a claim
about what you need. The warning count says that brokkr chose the omission,
so a `warning:`-shaped cause reads as withheld rather than absent.

Set `rustc` on a command that emits rustc-shaped diagnostics, such as a
`cargo doc`, `cargo build` or `cargo rustdoc` gate. In the measured case, a
`cargo doc --no-deps --workspace` entry failed on 27 denied
`broken-intra-doc-links` errors. They were interleaved with several hundred
`private-intra-doc-links` warnings from every other workspace crate. That is
a 1:20 signal-to-noise ratio at roughly 12 lines per diagnostic, with the
signal uniformly at `error`. The full output was unreadable; the failing
level alone took a third of a screen.

Only `error` and `warning` at **column zero** open a block. A column-zero
`note:` doesn't, so rustc's trailing notes stay attached to the diagnostic
they explain instead of outnumbering it. Indented source context quoting the
word can't forge a block.

An entry that declares `rustc` but fails with no `error` block falls back to
the `opaque` view. A gate can fail because its sentinel never appeared at
all, and that failure's evidence is the output.

Leave `diagnostics` off for a bespoke analyser. The default exists for a tool
free to print the word `error` in prose, which is why you declare the shape
rather than brokkr sniffing it.

### stage

The `stage` key names the point in the check pipeline where a script check
runs, once:

| `stage` | Runs | For |
| --- | --- | --- |
| `pre-clippy` (default) | With the other convention phases, before clippy builds anything | Cheap source-level gates, which fail before the run spends time compiling |
| `pre-test` | After clippy, before the test phase | Gates that want a clippy-clean tree but shouldn't wait on the suite |
| `post-test` | After the test phase and the coverage audit | Gates that need built binaries or test output |
| `background` | Started with the convention phases, concurrently with everything up to the prepare phase's builds; joined before any test binary is listed | Slow gates, such as one that drives its own cargo, that should not hold the pipeline up |

Entries at the same stage run in declaration order. Every entry at a stage
runs even when an earlier one failed, so a run surfaces all broken gates at
once.

`background` entries are the exception to ordering. Each runs on its own,
concurrently with the other background entries, the convention phases,
clippy, rustdoc and the prepare phase's builds. brokkr joins them after those
builds and before anything is listed, and prints their one
`script-check: ok (N background checks)` line or failure block then. So a
background `cargo` never runs beside a listing or a test, both of which run
under wall-clock limits. A run with no listing joins them before its verdict,
and every run joins them before it ends.

Each background entry gets the script_check ceiling, measured from its own
start. A failure fails the run and names `script_check` as the failed phase,
even when a later phase also failed, since the entry sits at the front of the
pipeline.

The `background` stage is opt-in because brokkr can't tell from a command
what it reads and writes. A `background` command must write nothing that the
convention phases, clippy or the build read: sources, generated inputs, and
the target dirs the run builds into. It must not depend on anything they
write either. A script that runs `cargo` should give it a target dir of its
own.

brokkr skips `post-test` entries **entirely when the test phase failed**. The
test phase fails fast, so its later lanes never ran. A script check has no
partial-run reading, unlike the coverage audit, which deliberately still runs
there.

brokkr also skips `post-test` entries when the test phase **didn't run**,
because of a profile's `skip_phases = ["test"]` or the markdown-only
shortcut. A `script-check: N post-test checks skipped` line says so, since
there is no test phase for them to judge.

All stages share the one `script_check` phase name. A profile's
`skip_phases = ["script_check"]` drops every stage, and brokkr reports a
failure as `failed_phase: "script_check"` wherever it happened. The output
names the failing entry either way.

## manifest

The `[manifest]` section configures the manifest phase: native structural
`Cargo.toml` conventions, as discrete named toggles rather than a rule
language. Each check reads a manifest with `toml_edit`, so it sees structure
that a value-only parse discards, such as blank-line groups and key order.
The section is inert unless a check is enabled, and without it the phase is
skipped.

This example sorts dependencies in every manifest outside `fuzz/`:

```toml
[manifest]
paths = ["**/Cargo.toml"]   # default when omitted
exclude = ["fuzz/**"]
sort_dependencies = true
```

The keys:

- **`paths`, `exclude`, `shape_exclude`**: file selection, per the path
  selectors section. `shape_exclude` skips structural and target-shape checks
  while dependency-content checks still apply.
- **`sort_dependencies`** (default `false`): dependency keys must be
  alphabetical within each blank-line-separated group of a dependency table.
  The tables are `[dependencies]`, `[dev-dependencies]`,
  `[build-dependencies]` and `[workspace.dependencies]`, including the Cargo
  underscore aliases and target-cfg variants. Only package, target and
  workspace dependency tables are checked; brokkr ignores similarly named
  tables under package metadata. A blank line resets the ordering, so
  intentionally grouped manifests pass.
- **`section_order`** (a list; empty means off): the required relative order
  of top-level sections. Only sections both present and listed are
  constrained; a listed section before an earlier-listed one is a violation.
- **`crate_type_order`** (a list; empty means off): the required relative
  order of `[lib] crate-type` entries, such as
  `["rlib", "staticlib", "cdylib"]`.
- **`package_field_order`** (a list; empty means off): the required relative
  order of `[package]` keys.
- **`lints_workspace_required`** (default `false`): a crate with a `[lib]` or
  `[[bin]]` target must set `[lints] workspace = true`.
- **`bin_doc_false`, `bin_test_false`** (default `false`): every `[[bin]]`
  must set `doc = false` or `test = false`; a missing or `true` flag is a
  violation.
- **`example_doc_false`** (default `false`): every `[[example]]` must set
  `doc = false`.
- **`cargo_machete_ignored_declared`** (default `false`): each
  `[package.metadata.cargo-machete] ignored` entry must name a declared
  dependency.
- **`[[manifest.version_align]]`**: repeatable groups, shown below.
- **`[manifest.adapter_group]`**: a workspace-level boundary, shown below.

Each `[[manifest.version_align]]` entry lists `crates` whose version
requirements must agree at `granularity`, `"minor"` (the default) or
`"major"`. brokkr skips absent crates, so a group fires only when two or more
are present. It reads both the bare-string and `{ version = "..." }`
dependency forms. This example keeps `arrow` and `parquet` on one minor
version:

```toml
[[manifest.version_align]]
crates = ["arrow", "parquet"]
granularity = "minor"
```

`[manifest.adapter_group]` names a comment-labelled group inside the
workspace root's `[workspace.dependencies]`, found by `marker`, a substring
of the group's header comment. The crates in `forbidden_in` must not depend on
the group's members. brokkr reads the group from the root manifest and scans
every member's package and target dependency tables. This example keeps
adapter dependencies out of two core crates:

```toml
[manifest.adapter_group]
marker = "Adapter dependencies"
forbidden_in = ["nautilus-core", "nautilus-model"]
```

The section and target-shape checks skip a `cargo-fuzz = true` crate, which
matches a standalone fuzz workspace. Those checks are `section_order`,
`crate_type_order`, `package_field_order`, `lints_workspace_required` and the
bin and example flags. The dependency-content checks, `sort_dependencies`,
`cargo_machete_ignored_declared` and `version_align`, apply to every
manifest.

A matched manifest that isn't valid UTF-8 or doesn't parse as TOML is one
`unparseable` violation for that file. It isn't a skip, which would pass
every enabled check on a file none of them read. It isn't a phase error
either, which would let one broken fixture kill the phase. Exempt a
deliberately non-manifest file, such as a `cargo-generate` template or a
fixture, with `exclude`.

## rustdoc

The `[rustdoc]` section turns on `brokkr check`'s rustdoc phase. The phase
runs `cargo doc --no-deps --message-format=json` per build shape, fails on any
rustdoc diagnostic and renders it like clippy's. The table's presence is the
opt-in; without it, the phase is inert. On a nightly rustdoc, brokkr detects
the toolchain and lints without rendering (`--check`); see
`brokkr man check rustdoc`.

This example documents private items:

```toml
[rustdoc]
document_private_items = true   # default false
```

`document_private_items` passes `--document-private-items`, so private items
get pages and links to them resolve. It does **not** silence
`rustdoc::private_intra_doc_links`. rustdoc still warns when a public item's
docs link a private one, because that link breaks in a public-only build. On
a measured 14-crate workspace with the flag on, 120 of 158 diagnostics were
that lint.

To accept private links, allow the lint in `Cargo.toml`, with
`[lints.rustdoc] private_intra_doc_links = "allow"` or the
`[workspace.lints.rustdoc]` form. That is where cargo hands rustdoc its lint
levels, and it costs nothing elsewhere. brokkr's `[lints] allow` also works,
since the phase drops the lint by exact code. But the same list reaches the
test phase's rustflags, so adding a rustdoc lint there forces a full test
rebuild. `allow_exact` suppresses single sites.

## fixture_lint

The `[[fixture_lint]]` array proves that clippy's `disallowed_methods`,
`disallowed_macros` and `disallowed_types` actually fire. Those lints fail
open: a configured path that no longer resolves, after an item moved or a
typo, seals nothing, and a nested `clippy.toml` silently shadows the root
one. The fixture-lint phase lints an in-tree fixture crate that calls the
sealed items, once per environment variant, and holds the diagnostics to a
manifest.

This example declares one fixture, linted twice: once with clippy finding the
config itself, and once with the config directory set explicitly:

```toml
[[fixture_lint]]
name = "origin-seal"
path = "origin-seal"                  # fixture crate, relative to the root
manifest = "origin-seal/expect.toml"
seed_lock = true
lint_args = ["-W", "clippy::disallowed_methods",
             "-W", "unfulfilled-lint-expectations"]
when = "gate"                         # "off" | "gate" (default) | "always"

[[fixture_lint.variant]]
name = "walk-up"                      # clippy finds the root clippy.toml
unset = ["CLIPPY_CONF_DIR"]

[[fixture_lint.variant]]
name = "explicit"
env = { CLIPPY_CONF_DIR = "{root}" }  # {root} is the project root
```

The manifest maps every fixture source file to what it must produce:

```toml
[files."src/submit_order_method.rs"]
expect = [{ lint = "clippy::disallowed_methods",
            path = "nautilus_trading::Strategy::submit_order" }]

[files."src/lib.rs"]
clean = true
```

**The fixture.** `path` and `manifest` must resolve inside the project tree,
after `..` and symlinks. `name` is letters, digits, `-` and `_`, since it
names the target dir. The fixture's `Cargo.toml` is its own single-package
workspace: a `[package]` plus an empty `[workspace]` table. The lint selects
its lib target, or its only bin when it has no lib, and brokkr refuses any
other shape.

**The run, per variant.** brokkr first runs `cargo clean -p FIXTURE`, then
`cargo clippy --no-deps --locked TARGET --message-format=json -- LINT_ARGS`.
Both run under the variant's environment and the persistent target dir
`TARGET_DIR/fixture-NAME`, whose size the ok line reports. `unset` removes a
variable outright, since an empty value wouldn't test absence.

The clean is there because cargo replays a fresh unit's cached diagnostics.
Without it, a variant whose difference clippy didn't fingerprint would
silently report the previous variant's output. The run must also produce the
fixture target's artifact with `fresh: false`. `--no-deps` limits clippy to
the fixture: the product crates it depends on are path dependencies and would
otherwise be linted under the variant's config too. brokkr injects no
`[lints]` allows, since one could suppress the very lint being proven.

**Warnings, not errors.** The run must compile: rustc emits no artifact after
an error, and the artifact is the proof the whole crate was analysed. So
expected diagnostics are warnings, `lint_args` passes `-W`, and `-D` or `-F`
there is a load error. Any error diagnostic fails the variant, including one
from an ambient `-D warnings` in a cargo config the fixture inherits. Whether
the lints are denied in the product is the product clippy phase's question.

**The verdict, per file and variant.** Every expectation must appear in its
file. The diagnostic's lint code must equal `lint`, with `-` and `_` unified.
One backticked token of its message must exactly equal `path`, never a
substring, since `submit_order` is a prefix of `submit_order_list`.

Every fixture warning must match an expectation of the file its primary span
is in. A `clean` file must produce nothing, and a warning whose span is in no
listed file fails as unattributed. brokkr refuses a manifest where every file
is clean. Reading the path out of the message is a toolchain-dependent
adapter: a rewording shows up as a visible failure, never a loose match.

**The inventory.** The `.rs` files under the fixture dir, except its own
`target/`, must equal the manifest's files. Every listed file must be reached
from the target root through unconditional `mod` declarations:

- A `cfg` or `cfg_attr` on any route is refused: a `mod` item, an inline
  module, or a file's inner attributes.
- A `mod` inside a function or macro isn't followed.
- `include!` is never followed.
- `#[path]` is followed only at file scope.

So a `clean` file means "compiled, and produced nothing". The exception is
item-level `cfg` inside a reached file, which the walk doesn't judge: keep
sealed calls and `#[expect]` attributes out of conditional items.

**`seed_lock`.** brokkr copies the main workspace's `Cargo.lock` into the
fixture and lets cargo add the fixture's own entries once, with an unlocked
`cargo metadata`. It then checks **external identity containment**: every
registry or git package in the fixture's lock must be in the main lock with
the same name, version and source, git rev included. A package that only the
fixture pulls in fails, rather than resolving freely.

That check is containment, not graph equivalence: where the main lock holds
two versions of a crate, a fixture edge may resolve to either. Every lint run
is then `--locked`, and the fixture lock must not change. The fixture's
`Cargo.lock` must be gitignored, since brokkr rewrites it every run, or the
phase refuses.

**Path identity** is always checked. Every path package in the fixture's
resolve other than its root must be a path package of the main resolve, with
the same name, version and canonical manifest. Every direct dependency of the
fixture root whose resolved package name is a main workspace member must
resolve to that member. A registry or git twin fails, renamed or not. brokkr
doesn't read `[patch]` and `[replace]`: cargo applies only the root's
overrides, and their effect shows in the resolved packages that are compared.

**When it runs.** Each entry's `when` is judged against the claim, as for
`[member_check]`, independent of `-p`, since a fixture isn't a workspace
package. A partial profile may name `fixture_lint` in `skip_phases`.

Two checks stay in the project as script checks, since both only read files:

- that the manifest's paths equal the paths configured in `clippy.toml`
- that no nested `clippy.toml` files exist

## dependency_rule

The optional `[[dependency_rule]]` array judges the direct Cargo dependencies
of one or more workspace packages. A rule has either a `forbid` list, where
listed names are violations, or an `allow` list, where unlisted names are
violations. `brokkr check` enforces these rules before clippy and tests, by
reading `cargo metadata --no-deps`. With no entries, the phase is skipped
silently.

This example forbids two crates in `app`, and `rusqlite` in two packages:

```toml
[[dependency_rule]]
name = "app-db-boundary"
from = "app"
forbid = ["db", "service-state"]

[[dependency_rule]]
name = "core-no-sqlite"
from = ["rtsk", "app"]
forbid = "rusqlite"
```

The keys:

- **`name`** (optional): the label shown in violation output.
- **`from`** (required): a workspace package name, or an array of names,
  whose direct dependency list is checked. The wildcard `"*"` means every
  workspace package; use it to ban an external crate across the whole
  workspace.
- **`forbid`**: a package name, or an array of names, that may not appear in
  those direct dependencies. It can name workspace or external crates.
- **`allow`**: the inverse polarity, the package names that are the *only*
  permitted direct dependencies; any other in-scope edge is a violation. Set
  exactly one of `forbid` and `allow`. `allow = []` means no in-scope
  dependencies at all. An `allow` entry nothing depends on isn't an error,
  since the list states what is permitted, not what is required.
- **`except`** (optional): workspace packages to drop from the `from` set.
  With `from = "*"` it says "no crate may depend on X, except these". Every
  entry must match a package `from` selects; brokkr refuses unmatched
  entries.
- **`kinds`** (optional): the dependency kinds the rule applies to,
  `normal`, `dev` or `build`, as a string or array. Empty, the default, means
  every kind, so the rule never silently stops catching a kind.
  `kinds = ["normal"]` allows a `[dev-dependencies]` entry, a
  self-documenting "dev-deps OK".
- **`optional`** (optional): when set, the rule matches only dependencies
  whose `optional` flag equals it. `optional = false` matches only
  non-optional dependencies, so it says "if this crate is present it must be
  `optional = true`".

The phase refuses on the first bad `from` or `except` name. `brokkr check
--audit` lists every bad name across all rules (see
`brokkr man check audit`).

`kinds` and `optional` both scope the *same* present-dependency match, and
absence is never a violation. So manifest conventions follow directly from
the forbid mechanism, as this example shows:

```toml
[[dependency_rule]]
name = "openssl-only-in-tls-adapter"
from = "*"
forbid = "openssl"
except = ["tls-adapter"]

# Sync-core crates: no tokio as a regular dependency, dev-deps OK.
[[dependency_rule]]
name = "sync-core-no-tokio"
from = ["nautilus-core", "nautilus-model", "nautilus-data"]
forbid = "tokio"
kinds = ["normal"]

# The common crate's tokio must be optional.
[[dependency_rule]]
name = "common-tokio-optional"
from = "nautilus-common"
forbid = "tokio"
kinds = ["normal"]
optional = false
```

Write a leaf boundary as an allow-list, not a forbid-list. A forbid-list
fails open: a crate added to the workspace or lockfile later is permitted
until someone extends every list. This example makes a test crate a leaf for
normal dependencies, leaving dev-dependencies unconstrained:

```toml
[[dependency_rule]]
name = "test-scratch-is-a-leaf"
from = "broadarrow-test-scratch"
allow = ["broadarrow-file-lock", "libc"]
kinds = ["normal"]
```

Both polarities match by real package name, never the rename key. So
`foo = { package = "bar" }` is judged as `bar`, and a rename can't slip past
either list. `kinds`, `optional`, `from = "*"` and `except` scope an `allow`
rule exactly as they scope a `forbid` rule.

The rules check direct edges only: brokkr rejects `app -> db` when `db`
appears in `app`'s manifest dependencies. Encode a transitive architectural
constraint by adding rules for the intermediate crates too.
