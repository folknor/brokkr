# Consolidations

One decision answered in more than one place, from the design hunt. Each entry
is the question, every site that answers it, whether those sites already
disagree, and where the single owner should live. Siblings: `structural.md`
(STR), `bugs.md` (BUG), and the hygiene documents (`hygiene-*.md`), where design
findings about mechanisms those documents already held were added to the
existing entries instead. Entries record what hunters reported; nothing has
been verified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

## CON-023 - Host features are applied per build path

Reported by: config-cli.

Residue: there is still no single build-purpose resolver across run, install, check, clippy, test and bench.

## CON-033 - Residual second sources beside the typed lane and command identity

Reported by: the wave-6 and wave-7 reviewers, laterally.

Residue: the sidecar database still keeps its own command column and migration sequence.
