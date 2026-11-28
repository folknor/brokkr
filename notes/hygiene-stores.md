# Hygiene: bench, results server, stores, sidecar, visual projects and datasets

Findings from the hygiene hunt: values with more than one owner, and assumptions about the host. Sibling files: `hygiene-host.md`, `hygiene-cargo.md`, `hygiene-process.md`. Defects are in `bugs.md`. Nothing here has been verified.

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

## Bench

## STORE-001 - Bench CLI intent has several owners

Sites: `src/cli/schema.rs`, `src/cli/visibility.rs`, `src/bench/runner/mod.rs`, `src/bench/runner/rounds/mod.rs`.
Clap shapes options, visibility reparses bench names and params to decide locking and logging, and the runner checks intent again. `--test --iterations N` ignores `N`. A bare invocation with `--iterations N` or a single `--commit REF` lists benches and ignores the option. Some invalid rounds invocations take the lock because `runs()` recognises executable-looking names before the runner checks the axes.
Enforcement: one parsed command intent driving dispatch, locking, logging and validation, with a CLI case table.
Reported by: bench.

## STORE-002 - Bench preparation is reconstructed per mode

Sites: `src/bench/runner/mod.rs`, `src/bench/runner/rounds/mod.rs`, `src/bench/runner/host.rs`, `src/bench/runner/contract/mod.rs`, `src/bench/params.rs`, `src/bench/store/document.rs`.
Arms and rounds each arrange destination opening, input holds, serve refusal, preflight, digesting and the live-HEAD check. Arms drops the preflight `Since` cache and starts another, while rounds passes it on, and `since()` caches raw revision spellings, so a moving ref gives mode-dependent answers. The contract driver sequences the steps again. A `rustflags` splice is checked in arm validation, resolved again for planning, and again for the document, where `.ok().flatten()` turns an error into absent flags. The flag grammar also lives in store validation, which decodes Cargo's flag names separately. Free and `from` defaults are only partly checked at load, and a malformed pin survives until bound, then hashes the file before parsing the pin. The many-choices bind defect is in `bugs.md`.
Enforcement: one prepared invocation with real and virtual effects, parsed pins and resolved flags kept on the arm, revision identity resolved where the invocation freezes it, and one flags codec.
Reported by: bench.

## STORE-003 - Predictable bench failures surface late

Sites: `docs/commands/bench.md`, `src/bench/runner/mod.rs`, `src/bench/runner/host.rs`.
Dry run can print `ok` with an invalid container placement or an unopenable destination (deferred by design). The real command defers scratch creation and admission until after the build, and destination probing only proves reachability and protocol. A static `script_file` type mismatch isn't refused before `fs::read`.
Enforcement: host and destination checks in the prepared invocation wherever the prerequisite is knowable early.
Reported by: bench.

## STORE-004 - Measured children have two environment models

Sites: `src/bench/runner/host.rs`, `src/bench/lua.rs`, `src/serve.rs`, `src/launch/probed.rs`.
The built snapshot uses Cargo's frozen environment, while an external child inherits the live one. `call_environment()` applies hotpath additions and user overrides in opposite orders for the two cases, so call overrides win for FIFO destinations and built-snapshot hotpath settings, while generated settings win for external programs. Lua masks this for explicit `HOTPATH_*` on marked calls, but inherited or frozen values still affect unmarked calls. Evidence records the explicit additions, not the effective environment. The hotpath names also cross into the instrumented program. See also `hygiene-process.md` (child-environment keys).
Enforcement: one child-environment builder that owns precedence and a redacted provenance projection, with the two bases as explicit inputs.
Reported by: bench, results-server-and-stores.

## STORE-005 - Real and virtual hosts diverge on completion and capture

Sites: `src/bench/runner/preflight.rs`, `src/bench/runner/host.rs`, `src/bench/runner/contract/host.rs`, `src/bench/store/document.rs`, `src/bench/json.rs`, `src/bench/view.rs`, `src/bench/lua.rs`.
Each constructs results, classifies exec errors, names the program and disposes of leases separately. Contract responses model exits, signals and marker stops but can't drive deadline, borrow-release, invalid-report or over-budget paths. The contract host duplicates the one-component name rule, preview and tail slicing, spool names and hotpath status strings, and the capture vocabulary is spelled across the real host, Lua, store, SQL decoder and fixture host. A report is decoded once for Lua and again for storage. The store checks only that a capture is an object. The `2^53` boundary is spelled in TOML projection, both JSON conversions, Lua argv and `filesize`, config bounds and fixture decoding, and depth 64 is in both Lua and the store. The preflight and virtual hosts repeat refusals the Lua phase gate makes unreachable, and the measured supervisor gets a preview cap that binds nothing. Capture-status defects are in `bugs.md`.
Enforcement: a shared completion policy and capture status type, a fake supervisor and lease, named depth and byte policies with asserted relationships, and phase-specific host interfaces.
Reported by: bench.

## STORE-006 - The child deadline leaves the operation open-ended

Sites: `src/bench/lua.rs`, `src/bench/runner/host.rs`.
It's optional and starts at launch, after resolution, digesting, admission and cache prep. It can't bound a pure Lua loop, a succession of children, a blocked syscall or publication. The Lua state installs an interrupt watcher and leaves dellingr's cost budget unset. `watched()` uses a scoped spawn that panics on thread-creation failure, and if the guarded work panics the flag is never set and the scope joins a watcher that waits for a stop.
Enforcement: run and stage budgets, VM fuel, an unwind-safe watcher guard, and an injected scheduler.
Reported by: bench.

## STORE-007 - Script loading and registry read different evidence

Sites: `src/bench/library.rs`, `src/bench/registry.rs`, `src/bench/runner/contract/mod.rs`, `src/bench/script_lines.rs`.
The registry reports entry errors and checks no-follow types. The library drops failed entries, skips ones that vanish on open, and later calls a demanded module missing, and it reads and compiles the whole tree. Contract discovery checks regular files through descriptors, then the runner reopens pathnames with `fs::read` (a FIFO blocks). Static checking and running each build inline source maps by rereading the declaring TOML.
Enforcement: a shared bounded source loader with identity, bytes, provenance and a line map.
Reported by: bench.

## STORE-008 - Pairing keys and document validity have several decoders

Sites: `src/bench/store/document.rs`, `src/bench/store/marks.rs`, `src/bench/results/compare.rs`, `src/bench/results/rounds.rs`, `src/bench/store/read.rs`, `src/bench/store/write.rs`, `src/bench/runner/stored_config.rs`, `src/bench/results/config_diff.rs`, `src/bench/results/annotations.rs`.
The writer builds four tagged JSON array layouts. Marks knows the name's position in each and rewrites reset wrappers recursively, comparison decodes the class separately, and listing extracts the tag. Unknown keys can stay opaque, render `(unknown)`, or miss a rename. Outcomes, endings, captures, marks and classes are spelled across Serde, SQL constraints, insert mappings, decoders, filters and renderers. Publication rejects deadline nanoseconds of a billion or more while the SQL decoder checks only `u32`. Cache preparation accepts any procedure string while reset validation refuses unknown ones. A half-present resolved-build column pair disappears on read. Config snapshots are written as sorted TOML, falling back to JSON and then an `unserializable` object, and comparison and annotations each decode the format, with annotations knowing the TOML library's private datetime key.
Enforcement: one versioned key codec with an explicit opaque historical case, one typed read and write codec, and a versioned snapshot type. Migration fingerprints already protect historical SQL.
Reported by: bench.

## STORE-009 - Bench host state assumes the lock and one clock

Sites: `src/bench/host_trace.rs`, `src/bench/runner/host.rs`, `src/process/identity.rs`, `src/host/cpu.rs`, `src/bench/cache.rs`.
`write_cursor` and `clear_cursor` assume the caller holds the lock, write a fixed `.json.tmp`, and the reader maps missing, permission and corrupt to `None`. A failed start write tries to remove its predecessor and doesn't report it if that also fails. Bench rereads the boot id and a subset of meminfo. It records CPU 0's governor as the host's. Device attribution needs `st_dev`, `/sys/dev/block` and optional `diskseq`, so overlay and tmpfs lose it. The fstrim timer reading is a fixed systemd stamp path. Sampler spawn and join failures become empty readings. Page-cache operations need `faccessat2` with descriptor flags and working `mmap`/`mincore`, and require write permission even where ownership would let the kernel answer truthfully. Cold prerequisites reject tmpfs while preparation reaches its own judgment.
Enforcement: put the cursor API on the lock guard with typed outcomes, an injected host-observation provider with reasoned availability, and one page-cache adapter shared by preflight and execution.
Reported by: bench, results-server-and-stores.

## STORE-010 - Bench resource consumers are uncapped

Sites: `reference/bench-resources.md`, `src/reservations/iteration.rs`, `src/bench/params.rs`, `src/bench/runner/rounds/mod.rs`, `src/bench/library.rs`, `src/bench/lua.rs`, `src/sidecar.rs`.
Each child writes full stdout and stderr spools and can write a full report before its size is checked. Call count, scratch bytes, handles and sample duration have no aggregate cap. Binding builds the Cartesian product before rounds enforces its slot bound, and plain runs have no arm bound. The contract driver captures every print. Sidecar evidence is bounded per child but not across children and iterations. Rounds clones the whole plan into each run.
Enforcement: iteration-level quotas, bounds checked before expansion and reads, and the plan as an invocation-owned row referenced by cells.
Reported by: bench.

## STORE-011 - Bench diagnostics lose context or disagree on level

Sites: `src/bench/evidence.rs`, `src/bench/host_trace.rs`, `src/bench/runner/host.rs`, `src/bench/runner/mod.rs`, `src/bench/runner/rounds/mod.rs`, `src/bench/lua.rs`, `src/bench/results/single.rs`.
Destination open can flatten a SQLite error without the path. Cursor write errors become bare strings. A failed iteration is a warning, then the arm reports the same reason as an error. The rounds retrieval hint is quiet-suppressible while run receipts are forced. The per-iteration wall line doesn't say it's block time when the result uses child time. Local publication errors use `Path::display` while success uses `output::path`. `r:ok()` quotes up to 2,000 raw stderr characters plus an ellipsis, while the single-result view counts escaped characters with the ellipsis inside its cap. Script `print` drops source and line metadata.
Enforcement: typed errors and events rendered once with project, run, iteration and child.
Reported by: bench.

## STORE-012 - Bench code that is dead or kept for history

Sites: `src/bench/store/wire.rs`, `src/bench/remote.rs`, `src/bench/wire_tests.rs`, `src/bench/mod.rs`, `src/bench/store/schedule.rs`, `reference/bench-store.md`.
`Failure::classify` reconstructs a typed `Classification.store` error that only tests read, while the live caller uses `retry` and `unapplied`. The module-wide dead-code expectation on the shared store is too broad to find individual unused items. The flat schedule decoder, legacy measurement columns, old pairing tags and old borrow naming all have live inputs.
Enforcement: split shared APIs by consumer so ordinary dead-code checks apply, and keep each historical adapter at its storage boundary with a fixture.
Reported by: bench.

## Results server and wire

## STORE-013 - Endpoint and identity have several definitions

Sites: `src/bench/store/wire.rs`, `src/config_parts/user.rs`, `src/bin/results_server/identity.rs`, `src/bin/results_server/deploy.rs`, `src/bench/remote.rs`, `src/version_cmd.rs`, `src/bin/results_server/http.rs`, `docs/results-server.md`, `reference/bench-store.md`.
`wire::PATH` owns `/bench`, but `ResultsServer::parse` appends a literal `/bench`. Deploy takes the full route and appends a literal `?probe=`, also spelled in the server's parser. The client refuses credentials and bad explicit ports. Deploy separately checks scheme, host, query and fragment, without credential or port checks or a check that the URL names the identity route, and finds a wrong route only after running the restart command. The client accepts explicit port `0` and arbitrary prefixes. Bench startup accepts JSON with the expected `protocol` alone. The version banner accepts any JSON, can print it as an unstamped server, and drops the local protocol line on a stamp read error. Deploy requires `server`, protocol, nonce and a 32-character hex build. The version request duplicates the client's headers, status handling and JSON parsing with its own timeouts. The deployment example repeats backend and public ports across `ExecStart`, the proxy and the configs. The listen address is a required CLI value with no default. The identity defect is in `bugs.md`.
Enforcement: a parsed URL type with base and identity-endpoint constructors, one identity codec with purpose-specific policies, and tests run against the real handler.
Reported by: launch-and-install, bench, results-server-and-stores, config-cli-startup, host-fs-output, worktree-clean-git-deps.

## STORE-014 - Proxy policy is ambient

Sites: `src/bench/remote.rs`, `src/bin/results_server/deploy.rs`.
The ureq agent takes the first usable `ALL_PROXY`, `HTTPS_PROXY` or `HTTP_PROXY` (either case), and `NO_PROXY` wins over `no_proxy` even when empty. Deploy disables proxying, so a shell can redirect publication while deploy verifies the direct endpoint. The write-certainty argument assumes a particular transport.
Enforcement: resolve proxy policy from the startup environment, report the effective route, and test direct, proxied and bypassed requests.
Reported by: bench, results-server-and-stores.

## STORE-015 - Publication certainty rests on transport assumptions

Sites: `src/bench/publication.rs`, `src/bench/remote.rs`, `src/bench/store/wire.rs`, `docs/commands/bench.md`, `reference/bench-store.md`.
The 20 s read and 120 s publish windows decide whether a retry starts, but one exchange can take 330 s, and an uncertain publication can start one more exchange after the window, all with the global lock held. A non-200 whose body fails to read becomes an empty envelope, losing the cause. A bare 4xx counts as proved-unapplied. A 200 settles a write even with an unreadable body or the wrong variant, and the response protocol header isn't checked. There's no total encoded-document budget, so config, source, records, samples and schedule can exceed 256 MiB, which is discovered after measurement, and nothing is persisted for replay if the command then exits.
Enforcement: one typed transport-proof and retry state machine, a versioned acknowledgement tied to the UUID, a total document budget enforced during collection, and a brokkr-owned per-user outbox.
Reported by: bench, results-server-and-stores.

## STORE-016 - Query and response sizes are unbounded

Sites: `src/bench/remote.rs`, `src/bench/store/list.rs`, `src/bench/results/mod.rs`, `src/bin/results_server/http.rs`, `src/bench/store/marks.rs`, `src/db/query.rs`, `src/db/compare.rs`, `src/db/migrate.rs`, `src/history.rs`.
Client reads set the body limit to `u64::MAX`, error replies included. The server materialises and serialises entire replies. `--limit` applies after all UUIDs are selected. Comparison fetches every matching run with children before pairing, one round trip per run. Marks are replayed for selection and append. A worker keeps running a job after the caller's wait expires. Typed UUID lookup chunks by 250 with an unchecked number of extra binds and truncates after merging. Historical migrations collect whole tables. `history --all` builds all output in memory. `--param` flattens keys with commas so different collections can match one string. CLI prefix validation only rejects empty, while wire requests can omit filters.
Enforcement: row, byte and execution budgets on server operations, revision-pinned pagination, typed identifiers and prefixes in both backends, and a structured param filter.
Reported by: bench, results-server-and-stores, host-fs-output.

## STORE-017 - Server admission, jobs and shutdown

Sites: `src/bin/results_server/main.rs`, `src/bin/results_server/http.rs`, `src/bin/results_server/worker.rs`, `src/bench/store/wire.rs`, `src/bin/results_server/tests.rs`.
`Limits` holds 256 connections, 64 in-flight, 64 queued, 1 GiB admitted and 256 MiB per request plus phase bounds, while the header buffer, reservation unit, accept retry, busy wait and worker pool live elsewhere. The accept loop retries every error forever and logs each one. The phase assertion checks whole seconds on the compiled default only and leaves egress out. Failure kinds have names and policy on the wire enum, a manual inverse list in the decoder, and a separate HTTP status map, so a new kind can become an unknown envelope. HTTP builds the `AtomicU8`, reply channel and permits independently. The worker compares a `std::time::Instant` to a Tokio deadline, claim checks time before the CAS, and a withdrawn job keeps its body, slot and permits until the sole worker reaches it. The file lock lives in the server task while the connection is in a detached worker, so shutdown can release ownership first. Rejected connections, header or connection timeouts and abandoned replies log nothing, failure lines have status and kind only, and there's no request id. A worker panic becomes a generic `internal`. The response-builder defect is in `bugs.md`.
Enforcement: one validated serve policy with an effective view, an exhaustive failure descriptor, a queue-owned job type with one clock, worker-owned lock and connection, and opaque request ids with overload counters.
Reported by: results-server-and-stores, host-fs-output, bench.

## STORE-018 - Store file ownership is cooperative

Sites: `src/bin/results_server/owner.rs`, `src/bench/store/schema.rs`, `src/bin/results_server/main.rs`, `reference/results-server.md`, `docs/results-server.md`.
The server checks the DB and its link count before opening the side lock, then opens SQLite by path. The lock open follows symlinks with no type or link check. Local publishers and the reader's migration path ignore the lock, and a server can be pointed at the user's local DB and migrate it. `open_read_only` can migrate and create side files. `open_existing` opens and drops a connection to prove existence, then opens again. Serve creates the parent, `FILE.lock`, enables WAL and migrates before binding, so a port conflict leaves a migrated store. The CLI accepts any absolute DB path with ordinary permissions. A `wal` answer doesn't prove the mount gives the locking semantics needed.
Enforcement: explicit store-opening roles with one migration owner, a service-owned data directory, and mutations ordered after bind.
Reported by: bench, results-server-and-stores.

## STORE-019 - Deploy hands process policy to the host

Sites: `src/bin/results_server/deploy.rs`, `src/bin/results_server/deploy_tests.rs`, `docs/results-server.md`.
A relative restart program depends on the inherited PATH and runs from the resolved home with the inherited environment and streams. Spawn is left to discover bad argv. A descendant holding output can keep the caller's pipe open. Declaration reading happens before the deadline with an unbounded `read_to_string`, so a FIFO blocks. `recv_timeout` maps a disconnected worker and an elapsed deadline to the same message, and `stop` discards wait and kill errors and can say "killed" after a failed reap. Deploy's 300 s / 10 s / 100 ms-2 s backoff loop is separate from the client's, and its tests need real sleeps and processes.
Enforcement: a validated restart plan with an injected launcher, typed termination and watchdog outcomes, and shared retry primitives with an injected clock.
Reported by: results-server-and-stores.

## STORE-020 - The server reads its stamp lazily

Sites: `src/bin/results_server/identity.rs`, `src/bin/results_server/http.rs`.
A process-wide `OnceLock` does synchronous file I/O on a Tokio thread at the first GET, so concurrent first requests wait together and readiness is advertised before the stamp is known. Without `/proc/self/exe` the server runs but can't confirm a deploy. See also `hygiene-cargo.md` (build stamp format).
Enforcement: resolve identity before admission through an injected reader.
Reported by: results-server-and-stores.

## Typed stores and history

## STORE-021 - SQLite lifecycle rules have two implementations

Sites: `src/db/store.rs`, `src/bench/store/schema.rs`, `src/db/migrate.rs`, `src/db/run_store.rs`, `src/history.rs`, `src/history_cmd.rs`, `src/visual/db.rs`.
Typed stores and the bench store each own the busy timeout, foreign keys, WAL, schema stamps, migration transactions and rollback. Bench verifies WAL, refuses negative versions and tests for an unstamped foreign schema. The typed helper only asks for WAL and accepts any version below its maximum, so a foreign DB with a plausible `user_version` can receive DDL. Neither stamps an application identity, and results has a litehtml-stamp exception. Typed `RunStore` and history use `Path::exists`, so an inaccessible path reads as no history. History resolves the data root twice and classifies "environmental" failures from English substrings, then skips recording. Bench provenance discards history errors. Both engines discard rollback failure. The visual opener can create, migrate or resume a legacy import, and the import materialises tables and inserts with foreign keys off. The read-only migration defect is in `bugs.md`.
Enforcement: shared connection and migration machinery with application identity, one open-existing result (absent, inaccessible, foreign, invalid), and an explicit write capability for migration.
Reported by: results-server-and-stores, visual-projects, worktree-clean-git-deps.

## STORE-022 - The two typed files have no common transaction

Sites: `src/db/run_store.rs`, `src/invalidate_cmd.rs`, `src/db/compare.rs`, `src/db/write.rs`, `src/db/sidecar.rs`.
A delete can commit results, fail on sidecar, stop partway through a set and never print the counts already committed. Other readers can see that partial state, and preview duplicates the eight-byte slicing and prints elapsed ms. Typed compare reads A, B and children independently. Results deletes legacy children by hand because early schemas lack cascade, sidecar keeps its own child list and alias deletion, and sidecar deletion counts summary rows, so metadata-only ids delete with a count of zero.
Enforcement: import typed history into one owned store, or a durable recovery record with defined reader snapshots. Schema coverage tests from the foreign-key graph.
Reported by: results-server-and-stores, worktree-clean-git-deps.

## STORE-023 - Historical constants are both contracts and drift points

Sites: `src/db/schema.rs`, `src/db/query.rs`, `src/db/migrate.rs`, `src/run_invocation.rs`, `src/bench/store/write.rs`, `src/main_parts/bootstrap.rs`, `src/db/types.rs`, `src/db/write.rs`.
The `pbfhogg` project fallback appears in schema, row mapping and qualification migration. `qualified-v1` is recognised by both the encoder and a literal prefix check. Typed ending strings and exit shorthand have SQL, fixture, decoder and presentation spellings. Bootstrap builds a `RunInvocation` and its arm key for every command and can reject one on old arm-key assumptions, while production consumers read only `.argv`. `positional_tail`, `arm_key` and the always-`None` `arm_qualifier` are read only by the fixture writer, while the historical transforms are load-bearing in migrations. Arm-key normalisation mirrors hardcoded CLI option names.
Enforcement: keep the historical copies explicit with old-file fixtures, move current arm-key construction behind test config, and pin migration transforms alongside their SQL.
Reported by: results-server-and-stores, launch-and-install.

## STORE-024 - Query strings mean different things per store

Sites: `src/db/query.rs`, `src/bench/store/list.rs`, `src/history.rs`.
Typed commit search matches in both directions for abbreviated commits, while bench search matches a full-SHA prefix. Typed grep searches serialised invocation strings, including JSON, while bench grep searches decoded argv and child env. History's command substring searches serialised JSON.
Enforcement: a shared semantic filter representation with the abbreviated-commit case as a legacy adapter.
Reported by: results-server-and-stores.

## STORE-025 - Retired typed-history writers and tables survive

Sites: `src/db/sidecar.rs`, `src/db/schema.rs`, `src/health.rs`, `src/cli/schema.rs`, `reference/measure-harness.md`.
`ResultsDb::insert`, sidecar `store_session`, `set_latest`, `reopen`, `backup_to_path` and `sidecar_forced` are test-only. `library_documents` is created and migrated with no reader or writer. Sidecar backup directories still have a size reporter. Help describes retired `--bench`, `--hotpath`, `--alloc` and `--force`.
Enforcement: a static reference check for current DB tables and test-only writers.
Reported by: results-server-and-stores.

## STORE-026 - History embeds ambient facts

Sites: `src/history.rs`, `src/bench/runner/mod.rs`, `src/main_parts/bootstrap.rs`.
SQLite supplies `datetime('now')`, and `latest` means the greatest rowid, with no cutoff at bench start, so a concurrent completion or clock step gives a negative "ago" or a command that finished after the reported start. The slow filter subtracts lock wait in SQL while display clamps separately. History is recorded even for read-only commands.
Enforcement: supply the clock and completed command identity at the boundary, with one own-time expression.
Reported by: results-server-and-stores.

## STORE-027 - Durable stores grow indefinitely

Sites: `src/bench/store/mod.rs`, `reference/bench-store.md`, `docs/commands/clean.md`, `src/history.rs`, `src/visual/db.rs`, `src/runnables.rs`, `src/sidecar.rs`, `src/bin/results_server/http.rs`.
Bench runs, configs, evidence and marks are append-only, and the docs say `bench.db` is never pruned. History grows per invocation. Visual history grows per run, partial failures included, and `latest` orders by wall time first. Snapshots and rustflags caches accumulate until an explicit clean, and an unconfigured checkout can't run the documented snapshot cleanup. The sidecar can warn once per tick on invalid FIFO input, and the server logs every refusal.
Enforcement: per-store retention or archival policy (an operator decision for history), quotas for regenerable caches, rate-limited diagnostics, and insertion-sequence ordering.
Reported by: bench, results-server-and-stores, config-cli-startup, launch-and-install, visual-projects.

## STORE-028 - Results write roots can escape their owner

Sites: `src/clean/layout.rs`, `src/user_dirs.rs`, `src/db/run_store.rs`, `src/bin/results_server/owner.rs`.
A relative HOME can route history and bench writes into the checkout. Existing typed DB paths can be symlinks, so read-triggered migrations or forced deletion write elsewhere. No normal query writes to Cargo's cache or build output.
Enforcement: path-capability APIs with tests for symlinks and relative roots, with cross-root destinations requiring an explicit capability. See also `hygiene-host.md` (HOME).
Reported by: results-server-and-stores.

## Sidecar capture

## STORE-029 - BROKKR_MARKER_FIFO is a cross-project resolution rule

Sites: `src/main_parts/bootstrap.rs`, `../pbfhogg/crates/cli/src/host.rs`, `../pbfhogg/crates/bench/src/host.rs`, `../pbfhogg/crates/harness/src/runner.rs`, `../nidhogg/crates/server/src/host.rs`, `../elivagar/crates/lib/src/telemetry/fifo.rs`, `../ratatoskr/crates/app/src/harness/mod.rs`.
Brokkr exports one constant and strips inherited values at bootstrap. Pbfhogg's CLI and bench read it with `var_os` and warn on open failure, while its harness requires it. Nidhogg caches the first result, failure included, silently. Elivagar re-reads it per emission, keys its cache by path, resets its clock on open and discards write errors. Ratatoskr uses UTF-8 `var`, treats empty as absent and propagates a blocking error. Empty values, an unavailable reader or a late path change give different outcomes.
Enforcement: one producer API with explicit required or optional policy, shared where the dependency graph allows, and a cross-project contract suite. Producer sides filed as pbfhogg#85, nidhogg#10, elivagar#14 and ratatoskr#5.
Reported by: results-server-and-stores.

## STORE-030 - The FIFO grammar and clock origin are ported by hand

Sites: `src/sidecar.rs`, `../pbfhogg/crates/library/src/sidecar.rs`, `../pbfhogg/crates/lib/src/debug.rs`, `../elivagar/crates/lib/src/telemetry/fifo.rs`, `../ratatoskr/crates/app/src/harness/mod.rs`, `docs/commands/measure.md`, `src/db/sidecar.rs`.
Pbfhogg's reader and brokkr each implement the 64-byte call id, 400-byte sealing name, terminal strings, drop count and record grammar. Pbfhogg's writer has its own copy of the limits, including the 512-byte atomic write. Elivagar uses `write_all` with per-OS `O_NONBLOCK` numbers. Ratatoskr uses `writeln!` with no line budget. Brokkr samples against boot time from before spawn, while elivagar stamps `Instant` since its first successful open and ratatoskr since harness start, so startup delay and suspend shift markers, and an early shift can sit wholly inside the legal range. The 10^15 us historical heuristic is an implicit migration assumption. The tag-channel defect is in `bugs.md`.
Enforcement: a small shared codec owning framing, names and single-write emission, shared byte fixtures, and a producer-reported clock identity. Producer sides filed as pbfhogg#85, elivagar#14 and ratatoskr#5.
Reported by: results-server-and-stores.

## STORE-031 - The measurement-wide capture cap is per child

Sites: `docs/commands/measure.md`, `src/sidecar.rs`, `src/bench/runner/host.rs`, `src/bench/store/document.rs`.
The docs and `MeasurementTunables` describe budgets across a measurement's runs, but each launch gets a fresh FIFO, resetting 100,000 samples, 10,000 markers, 20,000 counters and the 4,096-id census. Publication validation doesn't check these caps. FIFO construction and supervision take separate tunables, so one capture can mix policies.
Enforcement: a run-owned measurement budget distinct from a per-child cap, validated at publication.
Reported by: results-server-and-stores.

## STORE-032 - Sidecar sampling and FIFO lifetime

Sites: `src/sidecar.rs`, `src/sidecar/namespace.rs`, `src/clean.rs`.
The sampler opens three `/proc/PID` files by number and needs all three to succeed. It verifies the PID in `stat` but not the generation across reads. Permission, namespace and parse failures drop the whole sample, while missing `io` or `status` fields become zero. FIFO cleanup takes the exclusive lock, but enumeration takes only a path and relies on the caller to hold the guard, and it discards directory, entry and probe errors, as do create and destroy for unlink. Any nonempty token matches the cleanup envelope, and an empty or non-UTF-8 project path fails before launch. The "last marker" status is one global slot that every create, drop and drain updates, tagged markers included, so overlapping captures interfere. The cleanup sweep reports only removals and doesn't name an incomplete scan.
Enforcement: an identity-bound injected procfs reader with optional fields and counted failures, discovery tied to the exclusive guard, and capture-scoped activity tokens.
Reported by: results-server-and-stores, worktree-clean-git-deps.

## STORE-033 - Sidecar view options are validated in stages

Sites: `src/cli/schema.rs`, `src/sidecar_fmt.rs`, `src/sidecar_cmd/views.rs`, `src/main_parts/dispatch.rs`.
`--where` is parsed before the query and again when applied, with a branch (unreachable by today's parser) that would accept every row for an unknown operator. `--range` is parsed after samples and provenance are fetched, skipped for empty sets, and accepts nonfinite and reversed ranges. `--every 0` means no filter. `--run` can accompany `--compare`.
Enforcement: one typed view request with a finite checked range and nonzero stride, and a CLI argument-matrix test.
Reported by: results-server-and-stores.

## STORE-034 - Typed provenance opens historical paths on today's host

Sites: `src/sidecar_fmt.rs`, `src/preflight/digest.rs`.
Provenance opens and hashes the stored binary path with no type, byte or time limit, so a relative path resolves against today's cwd and a FIFO or device can block even `--markers`. Every failure becomes "not found (deleted or moved)".
Enforcement: an explicit verification step through a bounded regular-file reader that reports its reason.
Reported by: results-server-and-stores.

## STORE-035 - Hotpath parsing accepts malformed entries

Sites: `src/db/hotpath.rs`, `src/hotpath_fmt.rs`.
Function and thread entries are built without requiring a name or a recognised metric, so malformed data renders a blank table. Diff maps by name with last-wins but keeps repeated names in output order, so duplicates show the same value and use up `--top`. Thread rows, names and widths are unbounded.
Enforcement: typed entries with partial-loss information, a stable comparison identity, and bounded display cells.
Reported by: results-server-and-stores.

## Visual projects

## STORE-036 - Visual approval omits inputs that determine the image

Sites: `src/git.rs`, `src/git/tree_state.rs`, `src/visual/engine.rs`, `src/visual_approve_cmd.rs`, `src/cli/schema.rs`, `docs/commands/visual.md`.
Approval reuses measurement cleanliness, which exempts the root and included config even though they choose modes, paths, fonts, dimensions and scoring. Ignored or external assets escape Git status. Litehtml checks width and reference id, and sluggrs neither. The commit is nine hex characters, collected separately from status and build, and Git failures become "requires a clean git tree". Batch approval reopens the store and reads Git per id, so a later bad id leaves earlier approvals committed (as documented) with no batch outcome, and duplicate ids or aliases repeat the write. Clap and the handler check combinations twice.
Enforcement: bind approval to a full commit plus a render and scoring input fingerprint, and resolve and publish batches transactionally.
Reported by: visual-projects.

## STORE-037 - Visual artifacts live in project fixture trees by convention

Sites: `src/litehtml/cmd.rs`, `src/sluggrs/cmd.rs`, `src/atomic_write.rs`, `../litehtml-rs/.gitignore`, `../sluggrs/.gitignore`.
Litehtml spells `fixtures`, `pipeline.png`, `pipeline.json`, `diff.png` and the offender path in deletion, checks, scoring, capture and diagnostics, and the sibling renderer owns two output names. Sluggrs shares its approved path but repeats `output.png`. Consumer ignore rules keep generated files out of commits. Directory symlinks can send operations outside the tree, and the staging and replacement paths resolve file symlinks. `exists()` turns inspection errors into absence. Stale-file removal and `offenders.txt` writes abort the run while per-fixture failures don't, removing an empty offender list discards permission errors, and failed capture stages survive until retry.
Enforcement: pipeline output, diffs, offenders and staging in a `ProjectState` generation directory, a brokkr reference store, directory capabilities, and typed run versus item failures.
Reported by: visual-projects.

## STORE-038 - Visual scoring policy and ids are resolved per backend

Sites: `src/config_parts/project_config/litehtml.rs`, `src/config_parts/project_config/sluggrs.rs`, `src/visual/status.rs`, `src/visual/compare.rs`, `src/visual/engine.rs`, `src/litehtml/cmd.rs`, `src/sluggrs/cmd.rs`, `src/visual/db.rs`.
Both backends validate and resolve fuzz and ratchet tolerance, with matching defaults today. Litehtml rejects zero width while sluggrs accepts zero width or height, and neither has a practical maximum. Both accept nonfinite and out-of-range thresholds, and a `NaN` makes comparisons false. Fixture and font readability is checked after build or capture setup. The path-safe id predicate admits NUL, control characters, overlong names and case variants, and exact-id lookup is repeated in each backend before the shared resolver. Persisted status strings are spelled in `as_str`, `FromStr` and a separate `STALE` literal. `Layout::for_subject` makes some SQL branches unreachable, `insert_run`'s last branch needs a state neither subject has, and a second "no scored result" check follows a helper that already refuses `None`. Changing what config loads needs approval.
Enforcement: one resolved policy with numeric types and units, a validated id with a portable storage key, and one exhaustive outcome model.
Reported by: visual-projects.

## STORE-039 - The litehtml layout contract spans Rust, JS and the sibling

Sites: `src/litehtml/compare.rs`, `src/litehtml/capture.js`, `../litehtml-rs/src/style.rs`, `../litehtml-rs/src/lib.rs`, `src/litehtml/db.rs`, `src/config_parts/project_config/litehtml.rs`, `src/visual/compare.rs`.
The scorer and litehtml-rs have matching inline-tag sets today, and prepare adds `br` and `img`. The test checks the local superset by scraping JS and never runs the sibling. The DOM path delimiter, sibling indexing, JSON fields and precision are cross-process contracts. JS `Math.round` and Rust `round` disagree at negative halves, and the `0.05px` cutoff depends on Chrome's `0.1px` quantum. Width goes consistently to Chrome and the pipeline, but neither the producer's viewport nor sluggrs' output dimensions are checked, and the pixel comparator pads mismatched images into a plausible score. Viewport height, scale, fonts, media and browser build are inputs too. The `800px` historical fallback is a data-format fact.
Enforcement: a generated schema and vocabulary, producer-to-consumer golden cases, a producer receipt with dimension validation, and migration fixtures for the fallback. Producer side filed as litehtml-rs#2.
Reported by: visual-projects.

## STORE-040 - PNG comparison amplifies resources

Sites: `src/visual/compare.rs`, `docs/commands/visual.md`.
It holds decoded images, two padded RGBA copies and a full diff, and a wide-short image against a narrow-tall one makes a canvas much larger than either. There's no ceiling on size arithmetic or allocation, and indexed or non-8-bit PNGs fail after decoding.
Enforcement: checked dimensions with pixel and byte budgets, one decoder contract, and a bounded diff writer.
Reported by: visual-projects.

## STORE-041 - Renderer and font selection depend on checkout and host

Sites: `src/litehtml/cmd.rs`, `../litehtml-rs/src/text.rs`, `src/sluggrs/cmd.rs`, `../sluggrs/examples/snapshot.rs`.
`prepare` reads `fixtures/Ahem.ttf` under the config root. Litehtml-rs reads it under compile-time `CARGO_MANIFEST_DIR`, skips a missing font silently and initialises fonts from the host, and the two roots diverge in the parent-config layout. The renderer is a default release build for litehtml and a hardcoded `snapshot` example for sluggrs. Optional font failures go to stderr, which is discarded on success.
Enforcement: pass a resolved renderer and font bundle with identities, and a parent-config integration fixture. Sibling sides filed as litehtml-rs#2 and sluggrs#4.
Reported by: visual-projects.

## STORE-042 - The Node runtime has spread install and launch policy

Sites: `src/litehtml/cmd.rs`, `scripts/litehtml-prepare/package.json`, `src/cli/visibility.rs`, `src/env.rs`, `src/clean.rs`, `src/user_dirs.rs`.
`PUPPETEER_CACHE_DIR` and `puppeteer` are restated by install, capture and the recovery command. Only package install sets `PUPPETEER_SKIP_DOWNLOAD`. Children inherit the environment, run in the project cwd and find `node` and `pnpm` on PATH. The cache key excludes Node, pnpm, platform, architecture and browser executable, and the package declares neither engines nor a pnpm version. The package and browser installers hold separate unbounded blocking locks across 600 s child runs. A ready marker plus a directory counts as installed, so a deleted browser causes repeated failures without repair. Failed cleanup is ignored. `html-extract` and `outline` use the cache without the global lock while `clean` can delete a stale key, and cache enumeration drops read errors. A relative HOME gives a relative cache path that Rust and Node resolve against different cwds. The pnpm content store is the ordinary user store.
Enforcement: one resolved Node and browser toolchain passed to every stage, a cache owner with per-key leases, deadlines and verified publication, and the pnpm store under a brokkr cache.
Reported by: visual-projects.

## STORE-043 - Visual child work is bounded per step only

Sites: `src/output/child.rs`, `src/litehtml/capture.js`, `scripts/litehtml-prepare/prepare.js`, `src/main_parts/dispatch.rs`, `src/litehtml/compare.rs`.
Renderer, capture, prepare, extract and outline children run with `Duration::MAX`. The navigation timeout doesn't cover font readiness, traversal, screenshot or shutdown. Images are fetched serially with no cumulative budget, and a response that keeps trickling outlives the inactivity timeout. `visual` and `prepare` hold the cross-project lock throughout. Capture runs with sandboxing off and scripts and network on, and takes layout and screenshot in separate page operations. The sidecar records time and versions but verifies only width and id. Image headers, attributes, SVG sizes and fallback ratios feed PNG allocations with no pixel cap. HTML, both dumps, all offenders and the outline are materialised, and outline recurses fully whatever `--depth` says. Style-size regexes, tracking thresholds and data-URI rules are repeated across prepare, extract and outline. The smoke test is manual. `isMsoComment` has no caller, outline's `isLastChild` and section `index` don't affect output, and nothing creates `.miss` though prepare probes for it.
Enforcement: wall budgets with phase attribution, an explicit browser isolation and network policy, decoded-size and depth budgets, one HTML implementation, and browser-based fidelity fixtures.
Reported by: visual-projects.

## STORE-044 - Visual messages bypass event semantics

Sites: `src/output/console.rs`, `src/litehtml/cmd.rs`, `src/sluggrs/cmd.rs`.
Both tags use unconditional `emit_tag`, so progress shows in quiet mode. `ERROR` and `note` are hand-written per backend. Install errors hand-format durations. Offender links are built from `fixtures/...` instead of the path renderer, so they're wrong when invoked below the config root. Capture success stderr is discarded, and failure paths vary between full stderr, full stdout and stderr, and a generic message.
Enforcement: one renderer with a persisted bounded structured reason (run, item, phase, path, exit or signal).
Reported by: visual-projects.

## Datasets

## STORE-045 - Dataset paths and identity have several owners

Sites: `src/datasets/status.rs`, `src/bench/view.rs`, `src/clean/layout.rs`, `src/config_parts/parser.rs`, `src/config_parts/schema.rs`, `src/bench/params.rs`, `../pbfhogg/crates/devkit/src/registry.rs`, `../pbfhogg/crates/devkit/src/selection.rs`, `docs/brokkr.toml.datasets.md`.
Inventory enumerates file kinds, the bench view walks raw TOML, and cleanup reconstructs PBF and OSC paths itself, including the `base` sentinel and an old OSC fallback. Brokkr accepts a whitespace-only file name and arbitrary OSC keys, while pbfhogg trims and needs canonical sequences. The `Bbox` invariant is split between a public parser and `FromStr`, and pbfhogg has its own with a different range policy. `path` and `xxh128` are independently optional, so a pin with no path loads and does nothing. "Under data" is only documentation. Inventory rehashes aliases, has no work budget, conflates unreadable with absent, and keeps only the first line of errors. Validation defects are in `bugs.md`.
Enforcement: one typed registry (file, tree, sequence, bounds and pin types) used by inventory, binding, cleanup and standalone consumers, with shared config fixtures. The devkit parser divergence is filed as pbfhogg#86.
Reported by: visual-projects.

## STORE-046 - Dataset registration now has a different writer

Sites: `docs/brokkr.toml.md`, `docs/projects/pbfhogg.md`, `../pbfhogg/crates/devkit/src/registry.rs`.
The in-tree commands are gone, and the writer is `pbfhogg-devkit`. It reads an editable document and later atomically replaces it, without comparing the current bytes or holding a transaction, so manual edits can be lost, and a directory-sync failure after rename can report failure after the edit took effect.
Enforcement: version-checked updates with a published or unconfirmed outcome. Cross-repo, filed as pbfhogg#86. The brokkr side is the stale writer claim in `docs/brokkr.toml.md`.
Reported by: visual-projects.

## STORE-047 - Retired project and dataset config paths

Sites: `src/config_parts/project_config/mod.rs`, `src/config_parts/project_config/piners.rs` (and the other retired adapters), `src/config_parts/schema.rs`, `src/datasets/element_ids.rs`, `../elivagar/crates/devkit/src/tilegen.rs`.
Each retired project module only probes its key and warns, with ownership spelled separately in `ProjectConfig::parse` and the foreign-section table, and the values still reach scripts. `PbfExpect`, pair expectations and element id lists have no production reader. `data_dir` has no built-in consumer but reaches scripts. PMTiles, `locations_on_ways` and `force_sorted` have external consumers.
Enforcement: one project-section catalogue that supplies acceptance, visibility, the warning and the docs, and an explicit compatibility boundary for validation-only fields.
Reported by: visual-projects.
