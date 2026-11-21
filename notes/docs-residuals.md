# Docs rewrite residuals

Open items left after the house-style rewrite of docs/ and reference/
(reference/how-to-write-docs.md). Remove an item once it is resolved.

## Code

- After a committed stop, `current_hold()` returns None, so the bench
  runner's `set_child_pid` calls (preflight and host) skip recording a child
  spawned during the unwind, and `kill --hard` may miss it. Those sites want
  a lookup without the admission check, like `current_capability()`.
