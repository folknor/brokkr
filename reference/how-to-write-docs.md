# How to write docs

House rules for prose in `docs/` and `reference/`. Read this before editing
either tree. The rules started from the Vercel writing guidelines
(vercel-labs/writing-guidelines, `command.md`), by way of piners' copy, but
every rule brokkr keeps is stated here, so this file stands alone: a rule not
written here does not apply. Where this file and AGENTS.md disagree, AGENTS.md
wins.

## Know which page you are writing

Each page does one job. Decide which before writing:

- **How-to**: the reader is mid-task. Terse, imperative, numbered steps.
- **Reference**: the reader looks one thing up. Neutral, exhaustive,
  quotable, organized by surface (key, command, flag).
- **Conceptual**: the reader wants the model. Explain it so they could teach
  it back; examples welcome.
- **Troubleshooting**: the reader hit a refusal. Name the cause, then the fix.

Title a page by the reader's question, not the engineer's name for the
feature, and give it a goal you could test ("configure", "explain",
"debug").

Most `docs/` pages mix a short conceptual opener with reference sections.
That's fine if the shape is visible: the user-facing explanation first, then
the per-key or per-command reference, then the refusal table.

## Open every page and section with a summary

Every page and section starts by saying what it covers:

- The page opens with one paragraph saying what it covers and which sections
  hold the reference part.
- Every `##` and `###` section opens with a sentence that says what the
  section is about. A section that starts with a list or a code block has no
  antecedent, and `brokkr man TOPIC SECTION` prints that section alone.
- A body paragraph whose first sentence could stand alone as a heading is a
  cold open; carry the prior subject forward instead.

## Voice

Write to the reader directly and say what happens:

- Address the reader as `you`, never "the user" or "one".
- Active voice. Test: append "by monkeys". If the sentence still parses, it's
  passive; name who does the thing (usually brokkr, cargo, or you).
- Imperative for steps: "Run `brokkr check`", not "You will need to run".
- Contractions are fine (`can't`, `doesn't`, `you'll`).
- Present tense, unless the behavior is genuinely future.
- Limit `we` to deliberate project actions ("we refuse this because..."),
  never as a stand-in for `you`.
- No rhetorical questions.
- No spec-sheet voice: "provides", "is configurable", "is explicitly
  labeled". Say what happens.
- No personified artifacts doing physical things. "The lock holds" becomes
  "the lock is held"; "the plan carries" becomes "the plan has".
- No metaphor verbs standing in for the literal step: `lands`, `hits`,
  `moves through`, `bites`, `carries`. Name the action.

## Sentences and paragraphs

Keep sentences short and paragraphs to one idea:

- Aim for sentences under 20 words. Long technical rules can run longer, but
  if you have to reread one, split it: name the subject, the action and the
  consequence.
- No pronouns reaching back several sentences. Repeat the noun.
- No stop-start fragments splitting one idea ("It refuses. This is
  deliberate. It prevents X." is one sentence).
- No recap transitions ("With this in place...", "Now that we've seen...").
  Pivot straight to the next point.
- No template framing ("The question most teams face is whether..."): the
  angle comes from this page.
- Paragraphs of two to four sentences, one idea each. A paragraph past five
  sentences is either two paragraphs or a list.
- Three or more parallel items in a sentence become a list.
- Don't say the same thing twice ("never bites, so it never refuses").

## Words

Pick words that carry a claim:

- Banned as descriptions of reader effort: `easy`, `simple`, `quick`,
  `simply`.
- Filler to cut: `very`, `just`, `really`.
- Weasel words need a figure or a narrower claim: `many`, `often`,
  `typically`, `generally`, `significantly`.
- Vague quantifiers (`near-zero`, `sub-second`, `most runs`) need the
  figure and its source.
- Spell out an acronym on first use, and define a term the first time it
  appears, linking to the section that explains it.
- Sizes and durations take a space and an uppercase unit (`64 KB`,
  `200 ms`); seconds stay bare (`30s`).
- No all-caps emphasis. Bold is for a list item's term or a critical fact,
  never for tone; reaching for bold means the sentence is weak.
- Inline code for paths, keys, commands, flags, identifiers and short
  snippets: anything that would look wrong outside a monospace font.

## Say each fact once

A fact stated in two sections drifts: one copy gets the new case and the
other doesn't. State it once, in the section that owns it, and link there
from everywhere else.

Two places can still both mention a rule when one is the reader's entry
point (a refusal table row, a command's flag list) and the other the full
statement; the entry point names it and links, it doesn't restate the
conditions.

## Earn every detail

Every sentence must change what the reader understands or does:

- A document states the current contract. Dated history ("this changed",
  "used to", "since issue 212") and deferred work belong in git history and
  `notes/`, not in `docs/` or `reference/`.
- Cut a number, name or implementation detail if a more general phrasing
  wouldn't change what the reader understands or does. A count that drifts
  ("four lanes", "six projects") becomes a phrase that doesn't ("every
  lane").
- Internal type and function names and source paths don't belong in
  `docs/`: the reader can't act on them. They belong in `reference/` or a
  code comment. `reference/` may name them, but not source line numbers.
  The `docs-no-source-paths` script check catches brokkr's module paths,
  `crate::`, `DevError::` and a backticked compound type name brokkr
  defines (`MechanicalDb`) in `docs/`, reading the lists from `src/`. It
  doesn't catch a single-word type name or an unquoted one.
- `docs/` doesn't send readers to AGENTS.md, CLAUDE.md or `notes/`. Write
  the context in.
- Design rationale ("we weighed X and passed over it") belongs in
  `reference/` when it constrains future changes, and in the commit message
  otherwise. `docs/` states what happens.

## Headings

Headings are titles, slugs and an interface at once:

- Sentence case: "Running one lane", not "Running One Lane".
- Descriptive, so a reader can guess the content: never `Overview`, `Notes`,
  `Caveats` alone.
- A section about one config key or command is headed by its bare name in
  plain text: "## worktree_keep", "## script_check". Put the table path and
  shape in the opening sentence ("The `[[script_check]]` array declares...").
- A heading quoted elsewhere is an interface. Code, error messages and other
  docs cite sections by heading text or slug (`brokkr man conventions
  script_check`). Before renaming one, search the tree for the old text and
  slug and update every citation in the same commit, or keep the text.

## Headings as man section slugs

Every `docs/` file is a `brokkr man` topic, and a topic with eight or more
`##` sections is section-addressed: `brokkr man config` lists one slug per
`##` and `###`, and you type a slug to read that section. brokkr derives the
slug from the heading text, so the heading is the command-line name:

- Alphanumerics and `_` survive, lowercased; every other run of characters
  becomes one `-`. "`strip_rustflags`: removing flags from the tree's cargo
  config" becomes `strip_rustflags-removing-flags-from-the-tree-s-cargo-config`,
  which nobody types. Write "## strip_rustflags" and move the gloss to the
  opening sentence.
- A parenthesized tail and one trailing shape word (`section`, `array`,
  `entries`, `entry`, `blocks`, `block`, `phase`) are dropped from the slug.
  Don't lean on either: a heading that needs a parenthesis to make sense is
  two ideas.
- A `###` slug is prefixed with its parent `##` slug unless it already
  starts with it, so `### stage` under `## script_check` is
  `script_check-stage`. Don't repeat the parent's name in the child heading.
- Aim for a slug of one to three words. The listing is the topic's table of
  contents, so every slug must read as a subject on its own.
- Don't give two sections in one topic the same slug: brokkr reports the
  name as ambiguous instead of picking one.

## Headings in the terminal

`brokkr man` renders markdown for a terminal, and the renderer decides what a
heading can safely contain:

- **One H1 per file, and it is the page title.** H1 renders as the bare text
  on an inverse background, with no `#` marker. Keep it short (two to four
  words) and plain.
- **H2 and H3 only below that.** H2 and H3 render with their literal `##`
  and `###` markers, so the marker count is the only depth cue. H4 and
  deeper aren't addressable sections and are hard to tell apart; turn a
  fourth level into a bolded list lead.
- **Plain text only.** Inline code in a heading renders with visible
  backticks in the code colour and breaks the heading style; `**` and `*`
  render as literal markers; a link's text takes the link colour.
- **One terminal line.** The renderer doesn't wrap headings. Stay under
  about 60 characters.
- **No trailing punctuation**, and no colon-led subtitle doing a second
  heading's job. Put the detail in the opening sentence.
- **Link text must stand alone.** Links render as text only, so a reader in
  a terminal can't follow `[Grammar](#grammar)`. Name the target section by
  its heading text, or as `brokkr man TOPIC SECTION`.
- **`NO_COLOR` and pipes strip all styling.** The structure must still read
  from the markers and the blank lines alone.

## How brokkr man reads the tree

The bundled topics come from a fixed list, not a directory scan:

- Every `docs/` file is compiled into the binary, and each topic is listed
  in `TOPICS` and in AGENTS.md's detailed docs list. Adding, renaming or
  removing a page updates both.
- Topics are filtered by project, so a page about one project's command is
  shown only in that project. Don't write a shared page that assumes one
  project's config.
- `reference/` files are not man topics; the terminal rules still apply to
  their headings, since they are read in the same editors and pagers.

## Lists

List items follow one shape per list:

- Introduce a list with a colon.
- Bulleted for unordered, numbered for sequences.
- `- **Term**: description` for definition-style lists.
- No trailing period on fragment items; full-sentence items take periods.
  Don't mix the two in one list.

## Code blocks and examples

Every block is introduced and kept small:

- Every block has a language tag (`toml`, `text`, `sh`).
- Introduce every block in prose: what it declares and why it's there.
- Omit defaults, keep comments inside a block minimal, and inline the code
  rather than sending the reader to a full example file.
- Keep blocks at or under 80 columns where the content allows, and under 25
  lines; split longer ones with prose.
- Placeholders are descriptive, not angle-bracketed: `NAME`, `bench = "NAME"`.
  Never `<TOKEN>` or `xxx`; a fake number counts up (`1234567890`).

## Links and source layout

Links and spacing follow these rules:

- Link text names the destination; never a bare URL, `here` or `link`.
- One blank line before a heading and around a code block; no `---` rules
  between sections and no extra blank lines.

## Refusal tables

A page whose command can refuse ends with a table of refusals:

- Columns: the refusal text as you see it, the cause, the fix.
- The fix is an action the reader can take, imperative.
- A row whose full conditions live elsewhere links there instead of
  restating them.

## Typography and wrapping

Files stay plain ASCII and hard-wrapped:

- No curly quotes, no ellipsis character, no non-breaking spaces, no em or
  en dashes. Those are gremlins, and `brokkr check` refuses them.
- Don't use a spaced hyphen (" - ") as punctuation either. `brokkr check`
  doesn't catch it, but it's the same dash in disguise: use a colon, comma
  or period.
- Hard-wrap prose at about 78 columns.
- Tables are exempt from the wrap: a Markdown row can't be broken across
  lines. Keep cells short where nothing forces the length; a cell that quotes
  a refusal message verbatim stays whole, since you search for it by its text.
- A table whose cells need full sentences or more than one idea becomes a
  list of `- **Term**: description` items instead.

## Before you commit a rewrite

Check a rewrite against the old text before committing:

1. Every technical claim survived: reword, don't re-derive. A claim that
   changes is a factual correction or a retirement, not a rewrite: verify it
   against the code, and say in the commit message what changed and why.
2. Every heading or slug cited elsewhere still exists, or every citation was
   updated.
3. Every internal anchor link resolves (GitHub slugs: lowercase, spaces to
   hyphens, punctuation dropped).
4. `brokkr check` passes.
