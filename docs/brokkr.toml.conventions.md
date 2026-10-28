# brokkr.toml: convention gates

The brokkr.toml sections that configure `brokkr check`'s source-convention
phases (gremlins, header, textlint, script checks, manifest, dependency rules),
its rustdoc phase, and its fixture-lint phase. The rest of the schema is in `docs/brokkr.toml.md`
(`brokkr man config`); how each phase runs is in `brokkr man check`. Each `##`
heading is addressable: `brokkr man conventions script_check`.

## `[gremlins]` section

```toml
[gremlins]
disable = false                                    # skip the phase entirely
exclude = ["docs/reference-manual", "vendor/upstream-docs"]
include = ["LICENSE", "**/*.txt"]                  # scan these beyond the built-in list
allow = ["U+2019"]                                 # un-ban these codepoints
ban = ["U+2011"]                                   # flag these codepoints
```

- `disable` (default `false`) - skip the whole gremlin phase, both the scan
  and `--fix-gremlins`. The escape hatch for driving a foreign checkout whose
  Unicode you don't want brokkr to police or edit.
- `exclude` - directories the scanner skips (both the scan and
  `--fix-gremlins`) - for vendored material from an outside source that
  legitimately carries typographic punctuation, BOMs, and bidi marks. Entries
  are project-root-relative directories matched by path prefix on the
  git-relative path: `docs/manual` covers `docs/manual/` and below, but not a
  sibling `docs/manual-extra`. Empty/absolute entries are rejected at parse
  time. `exclude` wins over `include` and over the built-in extension list.
- `include` (default empty) - globs, in the shared selector syntax below,
  naming files to scan in addition to the built-in extension list
  (`.rs`/`.toml`/`.md`/`.markdown`/`.js`/`.sh`/`.py`/`.yml`/`.yaml`,
  case-insensitive). Without it an extensionless file such as `LICENSE`, or
  one with any other extension, is not scanned. `--fix-gremlins` follows the
  scan, so an included file can be rewritten too: name paths deliberately,
  never test fixtures that carry typographic Unicode on purpose. An included
  file gets the same byte scan and fix as any other, with no check that it
  is text: a binary passes or fails on its bytes (invalid UTF-8 is reported
  byte by byte), a selected symlink is read and rewritten through to its
  target, and a selected gitlink of an initialized submodule fails the phase
  as unreadable. A broad glob such as `**` drags in lockfiles and data, and
  the scan cost grows with it. An invalid glob is rejected at load time.
- `allow` - codepoints to remove from the built-in banned set. The scan skips
  them and `--fix-gremlins` leaves them in place, even though they are normally
  gremlins (e.g. permit `U+2019` if a repo deliberately uses curly
  apostrophes).
- `ban` - codepoints to flag beyond the built-in set. **Scan-only**: brokkr has
  no ASCII mapping for an arbitrary codepoint, so `--fix-gremlins` does not
  rewrite banned chars - the scan flags them and you fix them by hand.

`allow` and `ban` entries are `U+XXXX` codepoint strings (case-insensitive
`U+` prefix, 1-6 hex digits) or inclusive ranges `U+AAAA..U+BBBB` (both ends
included; `..=` also accepted). Ranges let you ban a whole block cheaply, e.g.
`ban = ["U+0400..U+04FF"]` for Cyrillic. A bad token, a reversed range, or a
codepoint listed on both sides is rejected at parse time. The `U+XXXX` form
keeps `brokkr.toml` itself free of literal, possibly-invisible gremlin
characters. Omit the section to scan everything with the built-in set (the
default).

Files are read as bytes. A scanned file that is not valid UTF-8 (a Latin-1
`.md` whose raw 0xA0 is an NBSP) reports each invalid byte as
`byte 0xNN INVALID UTF-8` at its own line and column; `--fix-gremlins` leaves
such a file untouched, since rewriting it would mean guessing its encoding. A
file that cannot be read at all fails the phase naming it - the same holds for
the header, textlint and manifest phases. Only a path deleted from the working
tree (still listed by `git ls-files`) is passed over.

## Shared convention path selectors

`[header]`, `[[textlint]]`, `[manifest]` and `[gremlins].include` use the same project-root-relative
glob syntax over the check's tracked and untracked-not-ignored file snapshot.
`**` crosses directory separators; `*` stays within one path component.

A glob that selects nothing, or an exclusion that excuses everything its
selector picked, is neither a failure nor a warning in an ordinary check: an
exclusion may reserve a path that is absent today, and user-wide rules often
name paths most projects do not contain. `brokkr check --audit` reports them,
for every selector below and `[gremlins].exclude`, without running anything
else (see `brokkr man check audit`).

| Selector | Effect |
| --- | --- |
| `[header].paths`, `[header].exempt` | Select files for the header check, then excuse matching selected files. |
| `[[textlint]].paths`, `[[textlint]].exclude` | Select files for each rule, then excuse matching files from that rule. |
| `[manifest].paths`, `[manifest].exclude` | Select manifests for all enabled checks (default `**/Cargo.toml`), then excuse matching manifests from all checks. |
| `[manifest].shape_exclude` | Skip structural and target-shape checks for matching included manifests; dependency-content checks still run. |
| `[gremlins].include` | Add matching files to the gremlin scan beyond the built-in extension list. A glob counts as matching even when the extension list already covers its files or `[gremlins].exclude` removes them. |

`[gremlins].exclude` is a directory-prefix list rather than a glob selector;
its path rules are described in the gremlins section above. Its entries are
judged against the built-in extension list plus `include`.

## `[header]` section

The header phase checks that each selected file contains the configured
`pattern`, with `{year}` expanded to the current UTC year. Missing text or a
stale year fails. The pattern is searched as a byte substring anywhere in the
file; its position is not checked. Absent by default.

```toml
[header]
paths = ["crates/**/*.rs"]
pattern = "Copyright (C) 2015-{year}"
exempt = ["**/examples/**", "**/core/rust/**"]
```

Path selection and the `--audit` report of unmatched globs follow the shared
convention rules above. The byte search also works on files that are not valid UTF-8, so they
are judged on their content rather than skipped. The current year comes from
Unix time through the shared `CivilDate` conversion (no date-crate dependency).
Ported from
nautilus_trader's `check_copyright_year` hook; see `src/header.rs`.

## `[[textlint]]` array

Declarative "forbid a regex on a line" rules (the textlint phase) - the generic
engine behind most grep-style convention hooks. Each entry scans files matching
`paths`; a line matching `pattern` is a violation. Empty by default.

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
exclude = ["**/*ANYHOW*", "**/anyhow_style_guide*"]  # skip the docs that demo it
except = ['^\s*use anyhow::Context;\s*(//.*)?$']     # the one allowed form
message = "only `use anyhow::Context;` is allowed; fully-qualify the rest"

[[textlint]]
name = "no-tokio-spawn-in-adapters"
pattern = 'tokio::spawn\('
paths = ["crates/adapters/**/*.rs"]
exclude = ["**/tests/**"]
skip_after = '^#\[cfg\(test\)\]'   # ignore everything after the test module
message = "adapters must use get_runtime().spawn()"
```

Fields: `name`, `pattern` (a linear-time `regex`; a match is a violation),
`paths`, `message`, plus optional bounded modifiers. `paths` and `exclude` use
the shared convention path-selector rules above; the remaining selectors and
predicates are rule-local:

- `exclude` - files matching are excused after `paths`, for docs that
  deliberately show the forbidden pattern or `tests/` trees.
- `allow_marker` (a line containing this literal, e.g. an author's
  `// allow-...` comment, is skipped). `allow_marker_above = N` widens it to
  also suppress when the marker is on one of the N lines above (0 = same line
  only; for markers a wrapped construct pushes off the offending line).
- `only_if_file_matches` (regex; the rule fires only in files where some line
  matches it) - a cheap import-awareness stand-in, e.g. flag bare
  `Instant::now()` only where the file imports `Instant`. Set
  `only_if_file_matches_above = true` to require that precondition at or above
  each match, so a later line cannot arm an earlier match.
- `region` (`code` / `string` / `comment`; Rust files, tokenized with the
  rustc tokenizer as `ra-ap-rustc_lexer` publishes it, so current literal forms
  such as raw C strings split correctly) scopes where `pattern` may match: `code` never flags a pattern
  quoted in a comment or string, `string` targets message text (a `", got"`
  phrasing rule). Only `pattern` is scoped; markers/`except`/reporting stay on
  the physical line.
- `join_wrapped_use = true` matches `pattern` against whole `use ...;`
  statements: a rustfmt-wrapped import is reconstructed onto one line (comments
  stripped) first, so `use tracing::.*warn` catches a multi-line `use` block.
  Reported at the `use` line; `allow_marker` matches on any physical line of
  the statement. Rust-only. This *replaces* the per-physical-line pass for the
  rule - it no longer scans ordinary lines, only reconstructed `use` statements,
  so it is for import rules, not use-site rules. Setting it on a rule whose
  `except` exempts ordinary `use` lines is a config error (the rule could never
  fire), rejected at load time.
- `except` (regexes; a line matching any is exempt) - the way to allow one
  specific form of an otherwise-forbidden pattern.
- `in_toml_section` (only consider lines while the last-seen `[section]` header
  equals this).
- `table_row_only` (only markdown table rows).
- `skip_after` (regex; once a line in a file matches it, every *following* line
  in that file is exempt - the matching line itself is still checked). For
  "don't fire inside the test module": `skip_after = '^#\[cfg\(test\)\]'`.
  A textual cut; `test_code` below is the structural answer.
- `test_code` (`"include"`, the default, `"skip"` or `"only"`) scopes
  `pattern` to production code or to test code (see
  [Test code](#test-code-test_code)). Composes with `region`; cannot combine
  with `join_wrapped_use`. The older `skip_test = true` still reads as
  `test_code = "skip"` and `skip_test = false` as `"include"`; one table may
  not set both keys.
- `must_scan` (exact project-relative paths) - files this rule must actually
  scan. Each must be a present, readable text file selected by `paths` and not
  `exclude`, with at least one eligible line: not cut off by `skip_after`, satisfying
  `only_if_file_matches` (at or above the line, when positional), and - under
  `test_code` in a `.rs` file - holding code in scope, not just a comment
  beside it (any other file admits on non-blank content under `"skip"`, and
  never under `"only"`).
  A listed path the rule did not scan is a violation at `path:0`. It is the
  guard against a rule whose scope has silently stopped covering the files it
  was written for: a sentinel, not proof that every byte of the file was
  covered.

No arbitrary multiline matching, except `join_wrapped_use` (bounded to `use`
statements). See `src/textlint.rs` and `src/lex.rs`.

A matched file that is not valid UTF-8 is decoded lossily and scanned (ASCII,
which patterns are written against, survives verbatim; an invalid byte becomes
U+FFFD). The exception is `test_code`: a Rust file the classifier reaches in
production context must be valid UTF-8 to parse, so an invalid one refuses
the classification instead. A *binary* file - a NUL in its first 8000 bytes,
git's heuristic - is skipped, because a broad `paths` glob legitimately sweeps
in images and archives where a line pattern means nothing.

### Test code (`test_code`)

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

In `.rs` files, `test_code = "skip"` blanks every byte the test-code
classifier (`src/test_code.rs`) proves absent from every non-test build
before `pattern` is matched. `test_code = "only"` is its complement for code
that exists for tests: it blanks every other byte, and also what is proven
absent from test builds as well (below). `allow_marker`, `except` and the
context windows still read the raw physical line. It needs a Cargo workspace:
the roots are every workspace target from `cargo metadata --no-deps`, with
integration-test targets as test roots and every other kind as production
roots. The classifier runs only when some rule with a `test_code` scope
selects at least one tracked `.rs` file, so a user-wide scoped rule costs
nothing in a tree it has no Rust to scan in.

What `"skip"` skips:

- files reached only through test contexts: `#[cfg(test)] mod x;` and
  everything below it, `#[path]` targets of such declarations, and
  integration-test targets. Modules resolve with rustc's directory rules, and
  `#[path]` is followed plainly or under `cfg_attr`. A file is not named by
  convention: a production file called `tests.rs` stays covered.
- items, statements, fields, arms, generic parameters and arguments whose
  `cfg` is false whenever `test` is false. The proof uses three-valued logic
  with every other cfg atom unknown, so `cfg(all(test, feature = "x"))` is
  skipped and `cfg(any(test, feature = "x"))` is not.
- free functions carrying a bare `#[test]` (single segment, no arguments),
  with everything they declare or include. This applies only when no
  reachable source has a `use` binding `test` or a `#[macro_use] extern
  crate` that does not exclude it, directly or under any `cfg_attr`. Macro
  input counts here (a `use ... test` or `as test` inside a `cfg_if!`, say),
  read conservatively. Any of these makes the builtin attribute shadowable,
  so every bare `#[test]` is then scanned as production, and a warning names
  the shadowing sites. (A glob import is not a site: a glob-imported `test`
  colliding with the builtin is an ambiguity error.)

A file reached in several contexts keeps a byte skipped only when every
production occurrence skips it. A file nothing reaches (or one the tripwire
below hands back) is scanned as production with only its own `cfg` masks
applied. Its bare `#[test]` functions stay scanned, since its crate root, where
a shadowing `#[macro_use]` could sit, is out of sight.

The claim is scoped to the **unexpanded source model**. Macro expansion and
generated source are outside it, for module edges and for skipped spans alike:

- Macro input is opaque. A `#[cfg(test)]` inside a `macro_rules!` body or a
  macro call proves nothing, and that code is scanned. Macro input is only
  read in the loud direction: for literal `include!`s, shadowing imports and
  the tripwire.
- Attribute macros may rewrite the cfg attributes inside their input, and
  derives may emit code no source scan sees.
- `include!(concat!(env!("OUT_DIR"), ...))` is accepted as generated source
  without being followed.

Literal `include!`s are followed, including ones inside macro input.

The classifier refuses the whole rule set, naming each site, rather than
guess. It refuses when a production context holds:

- any other computed `include!`;
- a node `syn` keeps verbatim;
- an inline module with a `cfg_attr` path;
- a module with both `x.rs` and `x/mod.rs`;
- a reachable file that does not parse.

As detection, not proof, it also runs a tripwire. A file the walk calls
test-only is handed back to production, with a warning, when production macro
input names it: `mod <name>` inside macro tokens, or a path-shaped string
literal (one containing `/` or ending in `.rs`) whose components are a suffix
of the file's path.

What `"only"` selects is exactly what `"skip"` skips, less code no test build
contains: a byte stays selected when some occurrence of its file does not
prove it absent from test builds. That proof evaluates `cfg` with `test` true
and every other atom unknown, and also reads the `cfg` a `cfg_attr`
introduces, so `#[cfg(not(test))]` inside a test module, `#[cfg(any())]`, and
a module declared at such a position are dropped. Correlations between atoms
are not tracked: `#[cfg(feature = "x")] #[cfg(not(feature = "x"))]` is
impossible but stays selected. Consequences of the exclusive definition:

- shipping code is never test code, even when an integration test also
  includes it by `#[path]`; a rule meant for both uses the default `include`.
- `#[cfg(any(test, feature = "x"))]` helpers ship under a feature, so they are
  production and not selected.
- test code the source model cannot see (inside macro input, generated by a
  macro) is not selected; nor is any file that is not Rust.
- a file nothing reaches, or one the tripwire hands back, has only its own
  `#[cfg(test)]` spans selected.

When something could shadow the builtin `#[test]`, a bare `#[test]` function
is scanned as production by `"skip"`, which errs toward scanning. For `"only"`
the same doubt would drop it silently, so the classifier also classifies as if
every bare `#[test]` were the builtin, and an `"only"` rule that selects a file
whose selection differs between the two readings refuses, naming the files and
the shadowing sites. A file an independent proof settles either way, such as
production code also reached through a bare test function, is not refused.

## `[textlint_preset]` blocks

`[textlint_preset.<name>]` - a named bundle of textlint scope/predicate fields that rules pull in with
`preset`. For a family of rules that differ only in `pattern` and `message` but
share a long `exclude` list - the shape a ported hook suite converges on - this
single-sources the scope, so adding one exempt file is a one-line edit rather
than the same edit repeated across every rule in the family.

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

A preset may set any `[[textlint]]` field except `name`, `pattern`, and
`message` - the three that identify one rule. An unknown or misspelled field in
a preset is a load-time error even if no rule uses the preset. A preset that
**no rule references** is also a load-time error - dead config that would load
clean is exactly what this parser rejects, so define a preset only where a rule
draws on it.

Merge rules:

- **Nearest value wins.** A field the rule sets itself is kept. This includes a
  field set back to its own default (`join_wrapped_use = false` beats a preset's
  `true`) - the merge happens on raw TOML before deserialization, so "set to
  false" and "absent" stay distinguishable.
- **`paths`, `exclude`, and `except` concatenate**, preset entries first, so a
  rule *adds* to a shared list instead of replacing it. Narrow a preset's
  `paths` with the rule's own `exclude`.
- `preset` also takes a list (`preset = ["rust-src", "dst-markers"]`), applied
  left to right under the same nearest-wins rule: for a **scalar** the rule's
  own value wins, then the first-listed preset, and so on. For a **list** the
  entries concatenate in that same declaration order - `preset = ["a", "b"]`
  gives `a`'s entries, then `b`'s, then the rule's own - so lists and scalars
  agree on "earlier-listed wins".

Presets are resolved entirely at parse time - the check phases see ordinary
fully-resolved rules and have no notion of a preset.

## `[[script_check]]` array

Run a command and assert its output (the script-check phase) - the escape hatch
for gates brokkr's native phases can't express (semantic analysers, external
formatter conventions). Each entry runs `command` via `sh -c` and passes iff the
captured output matches `expect`; on failure the captured output is shown.
Matching the sentinel (not the exit code) catches a check stubbed to `exit 0`.
At most 1 MiB per output stream is retained; if a stream selected by the rule
exceeds that bound or its pipe remains open past the drain grace, the check
fails because capture cannot prove that the full output matched.

```toml
[[script_check]]
name    = "docs-conventions"
command = "bash .pre-commit-hooks/check_docs_conventions.sh"
expect  = "All documentation conventions are valid"
match   = "last-line"   # exact | last-line | contains   (default: last-line)
stream  = "stdout"      # stdout | stderr | both          (default: stdout)
stage   = "pre-clippy"  # pre-clippy | pre-test | post-test | background (default: pre-clippy)
diagnostics = "opaque"  # opaque | rustc                  (default: opaque)
when    = "always"      # off | gate | always             (default: always)
```

- `name` - label shown when the entry **fails** (`beta: stdout did not match
  "ok"`). A passing stage reports once - `script-check (N checks)` on the
  conventions line for `pre-clippy`, its own `script-check: ok (N checks)`
  line for a later stage - rather than a line per entry: a passing gate's name carries
  nothing to act on, while the count still says which corpus passed. A stage
  with some failures prints `script-check: M of N ok` above the failure block.
- `command` - run as `sh -c "<command>"`, cwd = the code tree, so
  pipes/redirects/env expansion work.
- `expect` - the success sentinel. Keep it ASCII: a non-ASCII sentinel (e.g. a
  check mark) would trip the gremlin scan on `brokkr.toml` itself.
- `match` - `exact` (whole trimmed output equals `expect`), `last-line` (last
  non-empty line equals `expect`; the default), or `contains` (substring).
- `stream` - `stdout` (default), `stderr`, or `both`. `both` concatenates the
  separately captured streams as stdout, a newline, then stderr; it cannot
  preserve their original write order. `exact` compares that full combined
  text after trimming, so it can match a stderr-only sentinel or an expected
  value containing both streams. `last-line` uses the last non-empty line of
  that concatenation, which comes from stderr whenever stderr has a non-empty
  line. If the sentinel must follow actual print order, redirect both streams
  into one stream in the command (for example, `2>&1`) and select that stream.
- `stage` - where in the check pipeline the entry runs (below).
- `diagnostics` - the output shape a **failure** is rendered against (below).
- `when` - which runs the entry is part of (below).

The command's exit code is ignored - only the output match decides. See
`src/script_check.rs` and `docs/commands/check.md`.

### `when`

The same key and vocabulary as `[member_check]` and `[[fixture_lint]]`:

| `when` | Runs |
| --- | --- |
| `always` (default) | In every `brokkr check` that runs the `script_check` phase |
| `gate` | Only under a `certifies = "complete"` profile (`--gate`, or a complete `--profile` or `default_profile`) |
| `off` | Never in a plain check |

The defaults split on purpose. `[[script_check]]` has always run everywhere,
so an entry without `when` keeps doing so, and `[[lint_sweep]]`
(`brokkr man sweeps`) defaults to `always` with it; `[member_check]` and
`[[fixture_lint]]` are expensive by nature and start gate-only. `gate` is for
an expensive check such as Miri that belongs in the pre-landing run and not the
edit loop, without the `skip_phases` profile that would also drop the cheap
phases. It is orthogonal to `stage`, so a gate-only entry may sit at any stage;
`background` suits a slow one.

`brokkr check --script NAME` runs the named entry whatever its `when` - an
explicit request, and the way to try a gate-only check without running the
gate. `off` plus `--script` is the spelling for a check run only on demand.
A profile's `skip_phases = ["script_check"]` still drops every entry, whatever
its `when` (a complete profile cannot skip phases).

An entry left out by its `when` is not counted on the stage's
`script-check (N checks)` line. The run log names the ones left out, for
example `script-check: 1 gate-only check not run, no complete claim: miri`;
the console stays quiet, since that would be noise on every edit-loop run.

A `gate` entry the project's own config defines (its `brokkr.toml` or an
include) is a **load error** when no `[test.profiles.*]` declares `certifies =
"complete"`: it could never run in any check, and nothing would say so. Add a
complete profile, or set `when = "off"`. An include's entry is changed by
repeating the whole entry under the same name, since shadowing replaces the
entry rather than merging its keys. Entries from the user-wide config are
exempt: that file reaches every project, gate or not, and trees with no
`brokkr.toml`, and a project cannot edit it. Such an entry runs under a
complete claim, like a project's own, and is logged as not run on every other
run, including every run in a project with no complete profile.

### `diagnostics`

Declares what the command's output looks like, so a failure can be rendered
down to the part worth reading. It changes nothing about whether the entry
passes - that is always the `expect` match.

| `diagnostics` | Failure shows |
| --- | --- |
| `opaque` (default) | Both captured streams, with recognized credentials and the current home path redacted for display |
| `rustc` | Every `error:` / `error[CODE]:` block, then a line counting the warnings not shown |

Both views redact recognized credential fields, credential flags, URL user
information and credential query parameters, bearer/basic values, and the
current `HOME` path. Redaction affects display only; the check still matches
the original captured streams. Neither value caps by line or block count. The
only narrowing `rustc` performs
is by diagnostic level, which is a claim about what the reader needs; the
warning count says out loud that the omission was brokkr's choice, so a
`warning:`-shaped cause reads as withheld rather than absent.

Set `rustc` on a command that emits rustc-shaped diagnostics - a `cargo doc`,
`cargo build` or `cargo rustdoc` gate. The measured case: a
`cargo doc --no-deps --workspace` entry failing on 27 denied
`broken-intra-doc-links` errors, interleaved with several hundred
`private-intra-doc-links` warnings from every other workspace crate. A 1:20
signal-to-noise ratio, roughly 12 lines per diagnostic, with the signal
uniformly at `error` - unreadable in full, and a third of a screen once the
failing level is printed alone.

Only `error` and `warning` at **column zero** open a block; a column-zero
`note:` does not, so rustc's trailing notes stay attached to the diagnostic they
explain instead of outnumbering it. Indented source context quoting the word
cannot forge a block. An entry that declares `rustc` but fails with no `error`
block falls back to the `opaque` view - a gate can fail because its sentinel
never appeared at all, and that failure's evidence is the output.

Leave it off for a bespoke analyser: a tool free to print the word `error` in
prose is exactly what the default is for, which is why the shape is declared
rather than sniffed.

### `stage`

An entry runs once, at the point its `stage` names:

| `stage` | Runs | For |
| --- | --- | --- |
| `pre-clippy` (default) | With the other convention phases, before clippy builds anything | Cheap source-level gates - they fail before the run spends time compiling |
| `pre-test` | After clippy, before the test phase | Gates that want a clippy-clean tree but shouldn't wait on the suite |
| `post-test` | After the test phase and the coverage audit | Gates that need built binaries or test output |
| `background` | Started with the convention phases, concurrently with everything up to the prepare phase's builds; joined before any test binary is listed | Slow gates (one that drives its own cargo, say) that should not hold the pipeline up |

Entries at the same stage run in declaration order, and every entry at a stage
runs even when an earlier one failed, so a run surfaces all broken gates at
once.

`background` entries are the exception to ordering: each runs on its own, at the
same time as the others and as the convention phases, clippy, rustdoc and the
prepare phase's builds. They are joined - and their one `script-check: ok (N
background checks)` line or failure block printed - after those builds and
before anything is listed, so a background `cargo` never runs beside a listing
or a test (both run under wall-clock limits). A run with no listing joins them
before its verdict, and every run joins them before it ends. Each gets the
script_check ceiling measured from its own start. A failure fails the run and
names `script_check` as the failed phase even when a later phase also failed,
since the entry sits at the front of the pipeline.

The stage is opt-in because brokkr cannot tell from a command what it reads and
writes. A `background` command must write nothing the convention phases, clippy
or the build read - sources, generated inputs, the target dirs the run builds
into - and must not depend on anything they write. A script that runs `cargo`
should give it a target dir of its own.

`post-test` entries are **skipped entirely when the test phase failed**. The
test phase fails fast, so its later lanes never ran; a script-check has no
partial-run reading, unlike the coverage audit, which deliberately still runs
there. They are also skipped when the test phase **did not run** - a profile's
`skip_phases = ["test"]`, or the markdown-only shortcut - with a
`script-check: N post-test checks skipped` line saying so, since there is no
test phase for them to judge. Leaving `stage` off is exactly the old behaviour.

All stages share the one `script_check` phase name, so a profile's
`skip_phases = ["script_check"]` drops every stage, and a failure is reported
as `failed_phase: "script_check"` regardless of where it happened - the failing
entry is named in the output either way.

## `[manifest]` section

Native structural `Cargo.toml` conventions (the manifest phase), on the
model of discrete named toggles, not a rule DSL. Each check reads a
manifest with `toml_edit`, so it sees structure a value-only parse discards
(blank-line groups, key order). Inert unless a check is enabled; absent = the
phase is skipped.

```toml
[manifest]
paths = ["**/Cargo.toml"]   # default when omitted
exclude = ["fuzz/**"]
sort_dependencies = true    # keys sorted within each blank-line dependency group
```

- `paths` / `exclude` / `shape_exclude` - see the shared convention
  path-selector rules above. `shape_exclude` skips structural and target-shape
  checks while dependency-content checks still apply.
- `sort_dependencies` (default `false`) - dependency keys must be alphabetical
  within each blank-line-separated group of a `[dependencies]` /
  `[dev-dependencies]` / `[build-dependencies]` / `[workspace.dependencies]`
  table (including the Cargo underscore aliases and target-cfg variants).
  Only package, target, and workspace dependency tables are checked; similarly
  named tables under package metadata are ignored. A blank line resets the
  ordering, so intentionally grouped manifests pass.
- `section_order` (list; empty = off) - required relative order of top-level
  sections. Only sections both present and listed are constrained; a listed
  section appearing before an earlier-listed one is a violation.
- `crate_type_order` (list; empty = off) - required relative order of
  `[lib] crate-type` entries, e.g. `["rlib", "staticlib", "cdylib"]`.
- `package_field_order` (list; empty = off) - required relative order of
  `[package]` keys.
- `lints_workspace_required` (default `false`) - a crate with a `[lib]` or
  `[[bin]]` target must set `[lints] workspace = true`.
- `bin_doc_false` / `bin_test_false` (default `false`) - every `[[bin]]` must
  set `doc = false` / `test = false` (a missing or `true` flag is a violation).
- `example_doc_false` (default `false`) - every `[[example]]` must set
  `doc = false`.
- `cargo_machete_ignored_declared` (default `false`) - each
  `[package.metadata.cargo-machete] ignored` entry must name a declared
  dependency.
- `[[manifest.version_align]]` (repeatable) - `crates = [...]` whose version
  requirements must agree at `granularity` (`"minor"` default, or `"major"`).
  Absent crates are skipped, so a group only fires when 2+ are present. Reads
  both the bare-string and `{ version = "..." }` dep forms.

```toml
[[manifest.version_align]]
crates = ["arrow", "parquet"]
granularity = "minor"
```

- `[manifest.adapter_group]` - a comment-labelled group inside the workspace
  root's `[workspace.dependencies]` (found by `marker`, a substring of the
  header comment) whose members must not be depended on by the crates in
  `forbidden_in`. A workspace-level check: it reads the group from the root
  manifest and scans every member's package and target dependency tables.

```toml
[manifest.adapter_group]
marker = "Adapter dependencies"
forbidden_in = ["nautilus-core", "nautilus-model"]
```

The section/target-shape checks (`section_order`, `crate_type_order`,
`package_field_order`, `lints_workspace_required`, the bin/example flags) skip a
`cargo-fuzz = true` crate, matching the hook's standalone-fuzz-workspace
exemption. The dependency-content checks (`sort_dependencies`, cargo-machete,
`version_align`) apply to every manifest.

A matched manifest that is not valid UTF-8 or does not parse as TOML is one
`unparseable` violation for that file (not a skip, which passed every enabled
check on a file none of them read; not a phase error, which one broken fixture
would turn into a dead phase). Exempt a deliberately non-manifest file - a
`cargo-generate` template, a fixture - with `exclude`.

See `src/manifest.rs`.

## `[rustdoc]` section

Turns on `brokkr check`'s `rustdoc` phase: `cargo doc --no-deps
--message-format=json` per build shape, failing on any rustdoc diagnostic and
rendering it like clippy's. The table's presence is the opt-in; absent, the
phase is inert. On a nightly rustdoc the phase lints without rendering
(`--check`), detected automatically - see `brokkr man check rustdoc`.

```toml
[rustdoc]
document_private_items = true   # optional, default false
```

- `document_private_items` passes `--document-private-items`, so private items
  get pages and links to them resolve. It does **not** silence
  `rustdoc::private_intra_doc_links`: rustdoc still warns when a public item's
  docs link a private one, because that link breaks in a public-only build.
  Measured on a 14-crate workspace, the flag on: 120 of 158 diagnostics were
  that lint.

To accept private links, allow the lint in `Cargo.toml`
(`[lints.rustdoc] private_intra_doc_links = "allow"`, or the
`[workspace.lints.rustdoc]` form). That is where cargo hands rustdoc its lint
levels, and it costs nothing elsewhere. brokkr's `[lints] allow` also works - the
phase drops the lint by exact code - but the same list reaches the test phase's
rustflags, so adding a rustdoc lint there forces a full test rebuild.
`allow_exact` suppresses single sites. See the phase in
`brokkr man check rustdoc`.

## `[[fixture_lint]]` array

Proves clippy's `disallowed_methods`, `disallowed_macros` and
`disallowed_types` actually fire. Those lints fail open: a configured path
that no longer resolves (an item moved, a typo) seals nothing, and a nested
`clippy.toml` silently shadows the root one. The phase lints an in-tree
fixture crate that calls the sealed items, once per environment variant, and
holds the diagnostics to a manifest.

```toml
[[fixture_lint]]
name = "origin-seal"
path = "origin-seal"                  # the fixture crate, relative to the root
manifest = "origin-seal/expect.toml"
seed_lock = true
lint_args = ["-W", "clippy::disallowed_methods", "-W", "unfulfilled-lint-expectations"]
when = "gate"                         # "off" | "gate" (default) | "always"

[[fixture_lint.variant]]
name = "walk-up"                      # clippy must find the root clippy.toml itself
unset = ["CLIPPY_CONF_DIR"]

[[fixture_lint.variant]]
name = "explicit"
env = { CLIPPY_CONF_DIR = "{root}" }  # {root} is the project root
```

The manifest maps every fixture source file to what it must produce:

```toml
[files."src/submit_order_method.rs"]
expect = [{ lint = "clippy::disallowed_methods", path = "nautilus_trading::Strategy::submit_order" }]

[files."src/lib.rs"]
clean = true
```

**The fixture.** `path` and `manifest` must resolve inside the project tree
(after `..` and symlinks), and `name` is letters, digits, `-` and `_` (it
names the target dir). Its `Cargo.toml` is its own single-package workspace
(a `[package]` plus an empty `[workspace]` table). The lint selects its lib
target, or its only bin when it has no lib; any other shape is refused.

**The run, per variant.** `cargo clean -p <fixture>` first, then `cargo clippy
--no-deps --locked <target> --message-format=json -- <lint_args>`, both under
the variant's environment (`unset` removes a variable outright; an empty value
would not test absence) and the persistent target dir
`<target>/fixture-<name>`, whose size the ok line reports. The clean is
because cargo replays a fresh unit's cached diagnostics, so a variant whose
difference clippy did not fingerprint would silently report the previous
variant's output; the run must also produce the fixture target's artifact
with `fresh: false`. `--no-deps` limits clippy to the fixture: the product
crates it depends on are path dependencies and would otherwise be linted
under the variant's config too. No `[lints]` allows are injected - one could
suppress the very lint being proven.

**Warnings, not errors.** The run must compile: rustc emits no artifact after
an error, and the artifact is the proof the whole crate was analysed. So
expected diagnostics are warnings, `lint_args` passes `-W`, and `-D` / `-F`
there is a load error. Any error diagnostic fails the variant - including one
from an ambient `-D warnings` in a cargo config the fixture inherits.
Whether the lints are denied in the product is the product clippy phase's
question.

**The verdict, per file and variant.** Every expectation must appear in its
file: the diagnostic's lint code equal to `lint` (`-` and `_` unified) and
one backticked token of its message exactly equal to `path` - never a
substring, since `submit_order` is a prefix of `submit_order_list`. Every
fixture warning must match an expectation of the file its primary span is in;
a `clean` file must produce nothing, and a warning whose span is in no listed
file fails as unattributed. A manifest where every file is clean is refused.
Reading the path out of the message is a toolchain-dependent adapter: a
rewording surfaces as a visible failure, never a loose match.

**The inventory.** The `.rs` files under the fixture dir (except its own
`target/`) must equal the manifest's files, and every listed file must be
reached from the target root through unconditional `mod` declarations: a
`cfg` or `cfg_attr` on any route (a `mod` item, an inline module, a file's
inner attributes) is refused, a `mod` inside a function or macro is not
followed, `include!` is never followed, and `#[path]` is followed only at file
scope. So a `clean` file means "compiled, and produced nothing" - except for
item-level `cfg` inside a reached file, which the walk does not judge: keep
sealed calls and `#[expect]` attributes out of conditional items.

**`seed_lock`.** Copies the main workspace's `Cargo.lock` into the fixture,
lets cargo add the fixture's own entries once (an unlocked `cargo metadata`),
then checks **external identity containment**: every registry or git package
in the fixture's lock must be in the main lock with the same name, version and
source (git rev included) - a package the fixture alone pulls in fails rather
than resolving freely. It is containment, not graph equivalence: where the
main lock holds two versions of a crate, a fixture edge may land on either.
Every lint run is then `--locked`, and the fixture lock must not change. The
fixture's `Cargo.lock` must be gitignored, or the phase refuses (it is
rewritten every run).

**Path identity**, always: every path package in the fixture's resolve other
than its root must be a path package of the main resolve (same name, version,
canonical manifest), and every direct dependency of the fixture root whose
resolved package name is a main workspace member must resolve to that member -
a registry or git twin, renamed or not, fails. `[patch]` and `[replace]` are
not read: cargo applies only the root's overrides, and their effect is in the
resolved packages that are compared.

When it runs: per entry, `when` against the claim like `[member_check]`;
independent of `-p` (a fixture is not a workspace package). A partial profile
may name `fixture_lint` in `skip_phases`. What stays in the project: checking
that the manifest's paths equal the paths configured in `clippy.toml`, and
scanning for nested `clippy.toml` files - both only read files, so they are
script checks.

## `[[dependency_rule]]` array

Optional. Each entry judges the direct Cargo dependencies of one or more
workspace packages, either against a `forbid` list (listed names are
violations) or an `allow` list (unlisted names are violations). `brokkr check` enforces these
rules before clippy/tests by reading `cargo metadata --no-deps`. With no
entries, the phase is skipped silently.

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

- `name` (optional) - label surfaced in violation output.
- `from` (required) - workspace package name, or an array of names, whose
  direct dependency list is checked. The wildcard `"*"` means every workspace
  package - use it to ban an external crate across the whole workspace.
- `forbid` - package name, or array of package names, that may not
  appear in those direct dependencies. This can name workspace crates or
  external crates.
- `allow` - the inverse polarity: package name(s) that are the *only*
  permitted direct dependencies; any other in-scope edge is a violation.
  Exactly one of `forbid` / `allow` must be set. `allow = []` means "no
  in-scope dependencies at all"; an entry nothing depends on is not an error
  (the list states what is permitted, not what is required).
- `except` (optional) - workspace packages to drop from the `from` set. Pairs
  with `from = "*"` to express "no crate may depend on X, except these". Every
  entry must match a package selected by `from`; unmatched entries are refused.
  The phase refuses on the first bad `from` or `except` name; `brokkr check
  --audit` lists every one across all rules (see `brokkr man check audit`).
- `kinds` (optional) - dependency kinds the rule applies to: `normal`, `dev`,
  `build` (string or array). Empty = every kind (the default; never flips to
  silently ignore a kind you used to catch). `kinds = ["normal"]` means a
  `[dev-dependencies]` entry is allowed - the self-documenting "dev-deps OK".
- `optional` (optional) - when set, only match deps whose `optional` flag
  equals it. `optional = false` matches only non-optional deps, i.e. "if this
  crate is present it must be `optional = true`".

`kinds` and `optional` both scope the *same* present-dep match (absence is
never a violation), so manifest conventions fall straight out of the forbid
mechanism:

```toml
[[dependency_rule]]
name = "openssl-only-in-tls-adapter"
from = "*"
forbid = "openssl"
except = ["tls-adapter"]

# Sync-core crates must not have tokio as a regular dependency (dev-deps OK).
[[dependency_rule]]
name = "sync-core-no-tokio"
from = ["nautilus-core", "nautilus-model", "nautilus-data"]
forbid = "tokio"
kinds = ["normal"]

# The common crate's tokio must be optional (a non-optional tokio is flagged).
[[dependency_rule]]
name = "common-tokio-optional"
from = "nautilus-common"
forbid = "tokio"
kinds = ["normal"]
optional = false
```

A leaf boundary is an allow-list, not a forbid-list: a forbid-list fails open,
since a crate added to the workspace or lockfile later is permitted until
someone extends every list.

```toml
# Everything except these two is a violation as a normal dependency;
# dev-deps stay unconstrained.
[[dependency_rule]]
name = "test-scratch-is-a-leaf"
from = "broadarrow-test-scratch"
allow = ["broadarrow-file-lock", "libc"]
kinds = ["normal"]
```

Both polarities match by real package name, never the rename key, so
`foo = { package = "bar" }` is judged as `bar` and a rename can't slip past
either list. `kinds`, `optional`, `from = "*"` and `except` scope an `allow`
rule exactly as they scope a `forbid` rule.

Rules are intentionally direct-edge checks: `app -> db` is rejected when `db`
appears in `app`'s manifest dependencies. Transitive architectural constraints
should be encoded by adding rules for the intermediate crates too.
