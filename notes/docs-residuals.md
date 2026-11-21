# Docs rewrite residuals

Open items left after the house-style rewrite of docs/ and reference/
(reference/how-to-write-docs.md). Remove an item once it is resolved.

## Structure

- docs/brokkr.toml.sweeps.md has six `##` sections, so bare
  `brokkr man sweeps` prints the whole page instead of listing sections.
  Sections still resolve by slug (`brokkr man sweeps check-target`).
  Promoting the heavy keys to `##` would list them.
- reference/architecture.md has long bench subsections that run several
  screens. The style guide has no rule for a page that size; splitting the
  script-bench area into its own reference file is a candidate.
- docs/commands/run.md's exit-status table covers check, test, clippy and
  the rustc guard. Those codes may belong in check.md.

## Tables

- Some table rows run past 80 columns (bench-store failure kinds, run.md,
  the refusal tables in hygiene and measure). Table cells can't be
  hard-wrapped; the guide could state that tables are exempt.

## Code

- The executable-artifact refusal tells a ratatoskr user to "name one with
  --bin", but the ratatoskr config key is `binary`. The message is shared
  with `brokkr run`, so the fix is a caller-supplied hint.
- The sticky stop flag that refuses further lock acquisitions after an
  accepted stop has no distinct message, so the hygiene refusal table has
  no row for it.
- src/ratatoskr/saehrimnir.rs spells the name with a non-ASCII character in
  its module doc. The gremlin scan doesn't flag it; the docs now spell it
  saehrimnir.

## Lints

- The docs-no-source-paths textlint rule lists brokkr's distinctive module
  names. A new top-level module isn't caught until it joins the list, and a
  bare internal type name is never caught.
