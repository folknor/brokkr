# Docs rewrite residuals

Open items left after the house-style rewrite of docs/ and reference/
(reference/how-to-write-docs.md). Remove an item once it is resolved.

## Structure

- reference/architecture.md has long bench subsections that run several
  screens. The style guide has no rule for a page that size; splitting the
  script-bench area into its own reference file is a candidate.
- The style guide wraps prose at about 78 columns but code blocks at 80.
  Possibly intended; reads like drift.

## Code

- After a committed stop, `current_hold()` returns None, so the bench
  runner's `set_child_pid` calls (preflight and host) skip recording a child
  spawned during the unwind, and `kill --hard` may miss it. Those sites want
  a lookup without the admission check, like `current_capability()`.
- The doc comment on `DevError::Interrupted` is bench-specific, but the
  variant is the general interrupt for every command.

## Lints

- The docs-never-cite-notes textlint rule doesn't lint AGENTS.md, though
  AGENTS.md states the rule.
- The docs source-path check misses single-word internal type names and
  unquoted mentions in prose; both collide with English and consumer names.
