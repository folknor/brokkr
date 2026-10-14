# Design hunt: process-control

Scope: `src/lockfile.rs`, `src/lock_service.rs`, `src/hold.rs`, `src/guard.rs`,
`src/bin/rustc_guard.rs`, `src/stray.rs`, `src/test_orphans.rs`,
`src/shutdown.rs`, `src/disk_gate.rs`, `src/oom.rs`, `src/toolchain.rs`,
`src/cargo_env.rs`, `src/user_dirs.rs`, `src/atomic_write.rs`. Followed out into
`src/main_parts/commands.rs` (`lock`/`kill`), `src/output.rs` and
`src/test_runner.rs` (the spawn sites) where the decisions led there.

The short version: the individual mechanisms are careful and well argued. The
weakness is that the *protocol* they implement together - one lock file format,
one capability hash, one set of on-disk paths under `~/.brokkr`, one process
identity, one "may I signal this pid" rule, one "what every child carries" rule -
has no single owner. Each piece is re-derived at every site that needs it, and
the binary split (`brokkr` vs `brokkr-rustc-guard`) is used as the excuse for the
largest of those duplications when it does not have to be.

---

## 0. Bugs and near-bugs found along the way

These come first because they are concrete.

### 0.1 `lockfile::status()` probes with an EXCLUSIVE flock, and that makes the guard refuse real compiles

`status()` decides "is anybody holding?" by trying `LOCK_EX | LOCK_NB` on
`brokkr.lock`. When nobody holds the lock, that probe briefly *becomes* the
holder. The guard (`rustc_guard.rs::lock_is_held`) probes with `LOCK_SH | LOCK_NB`,
gets `EWOULDBLOCK` during that window, decides a hold is live, and reads `auth=`.
After a normal release `auth` is empty, because the file was truncated, so the
guard returns `Refused { why: "a brokkr hold is starting up ..." }` and exits 1.
A foreign cargo's rustc, or one under `BROKKR_CARGO`-less tooling, therefore fails
whenever it races an idle `brokkr lock`, `brokkr kill`, `brokkr strays`, or a
`stray::current_ownership()` call made outside a hold (`brokkr strays` runs it
unlocked). If the refused invocation is a `--print` that `classify` did not
recognise as a query, the refusal can also be cached in `.rustc_info.json`, which
is the poisoning failure the guard comments call out.

The underlying cause is a decision made twice: two different probes answer "is
the lock held?", and they disagree in what they do to the lock. The fix is one
probe, shared (`LOCK_SH|LOCK_NB`), used by both `status()` and the guard. A shared
probe never conflicts with another prober or with a guard. It conflicts only with
an exclusive holder, which is the question being asked. A waiting acquirer's
`LOCK_EX|NB` can fail against a momentary shared probe, but it polls anyway. See
§2.1 for where the shared probe should live.

### 0.2 `guard --install/--remove` rewrite the user's global cargo config with `std::fs::write`

`guard.rs::do_install` and `do_remove` both call `std::fs::write(config_path,
doc.to_string())` on `$CARGO_HOME/config.toml`, which every cargo on the machine
reads. `atomic_write.rs`'s header says a plain truncate-then-fill write "is a
bug" and that this module is the one copy of the sequence. A kill or a full disk
at the wrong moment leaves a truncated global cargo config. The fix is
`atomic_write::replace`, which also keeps a symlinked `~/.cargo/config.toml`
(dotfile managers) intact, where `fs::write` follows the link anyway. Nothing
stops this drift structurally, either. "Files brokkr edits that it does not own"
has no type and no choke point (§3.6).

`hold::cargo_config_overrides` also truncate-writes `~/.brokkr/nextest-env.toml`.
That is less serious, because brokkr owns the file and holds are serialised, but
it is the same hand-rolled pattern.

### 0.3 The stray reaper kills brokkr's own ancestors, and the orphan reaper exempts them

`stray.rs`'s test `descendant_expansion_never_crosses_into_brokkrs_own_subtree`
asserts that in `cargo(10) -> brokkr(20, us) -> cargo(30)` the outer cargo is a
stray **and is killed**. Expansion is stopped at brokkr, but the root itself
goes. So `cargo run --bin brokkr -- check`, or any brokkr launched under a cargo
(a script check run by an outer brokkr's cargo, an xtask), SIGKILLs its own
parent cargo at the first fresh hold. `test_orphans::select` makes the opposite
call on purpose: "Exempt: this process, its ancestors and its descendants". Two
reapers answer "is my ancestor fair game?" in opposite ways, and neither mentions
the other. Even if killing the parent cargo is intended (it does hold the
target-dir lock), it is a decision that should be stated once, in one ownership
type used by both reapers (§2.3).

### 0.4 `lock_service` round-trips typed state through a stringly map

The holder builds its status reply from typed `Identity` + `Snapshot`, encodes it
as `key=value` lines, and the client in `commands.rs::print_holder_status` reads
it back with `s.get("draining") == "1"`, `s.get("mocks").parse::<usize>()`,
`s.get("held_secs").parse::<u64>()` and so on. A key renamed on one side reads as
empty on the other, and nothing would notice. That is not a bug today, but it is
the most fragile seam in the scope (§1.4).

### 0.5 Smaller

- `oom::protect_child` does the opposite of its name: it sets `oom_score_adj =
  1000`, making the child the OOM killer's *preferred* victim, to protect the
  desktop. Rename it (`prefer_oom_kill`) or fold it into the spawn policy (§2.5).
- `disk_gate::check` exempts commands by comparing `ctx.command` to the strings
  `"clean"` and `"env"`. A renamed subcommand silently loses its exemption (§1.6).
- `disk_gate` depends on `crate::env` (the `brokkr env` command module) for
  `probe_path`/`format_bytes`. A host-wide gate that runs inside lock acquisition
  depends on a command's presentation module. The edge should point the other way:
  a `fs_probe` module owned below both.
- `lock_service::escape`/`unescape` (chained `str::replace`) and
  `lockfile::escape_value`/`unescape_value` (a char-wise scanner) are two
  implementations of the same `%25/%0A/%0D` encoding. They agree on every case I
  traced, but only by construction, and a third escape for a third byte would
  have to be added twice.
- `main_parts/commands.rs::proc_start_epoch` is a fourth starttime-to-time
  conversion, next to `lockfile::process_uptime_str`,
  `lock_service::self_cpu_secs` and `check_cmd/watchdog.rs`'s use of
  `proc_starttime`. Each re-reads `_SC_CLK_TCK` and does its own float maths.

---

## 1. Axes that should be types

### 1.1 Process identity: `(u32, String)` everywhere

A process identity is a pid, a starttime token and a boot id, and in some places
a namespace pair. Today it is spelled as:

- `LockState::child: Option<(u32, String)>`, `mocks: Vec<(u32, String)>`
- `LockInfo { pid: u32, starttime: String, boot_id: String, pid_ns: String, time_ns: String, .. }`
- `stray::Stray { pid, starttime: String }`, `stray::ProcEntry { starttime: String }`
- `test_orphans::ProcRow { starttime: String }`, `Orphan { pid, starttime: String }`
- `shutdown::descendants -> Vec<(u32, String)>`
- `commands.rs::KillTarget { pid, starttime: String, .. }`
- `nidhogg/server.rs`'s pid record

Consequences visible in the code:

- An empty `String` means "unreadable, fail closed", and every consumer has to
  remember to check `starttime.is_empty()`. `kill_descendants`, `stray::kill`'s
  closure and `test_orphans::sigkill_verified` each do. A new consumer that
  forgets compares `""` to `""`... in fact `proc_starttime` never returns `""`, so
  that would fail closed by accident, but only by accident.
- `test_orphans::select` parses the starttime string back to `u64` to sort it,
  with `unwrap_or(0)`.
- `verify_identity(pid, starttime, boot_id)` takes three loose arguments. The
  child and mock tuples carry no boot id of their own and borrow the holder's
  (`&info.boot_id`), which is right but nowhere stated in a type.
- Namespace inodes are `String` in `LockInfo` and get re-parsed to `u64` in
  `foreign_holder_ns` and stringified again in `shares_namespaces`.

Proposed: a `proc_identity` module (or `process::identity`) owning:

```rust
pub struct Pid(NonZeroU32);
pub struct StartTicks(u64);          // parsed once, never a String
pub struct BootId(String);           // validated non-empty
pub struct NsInode(u64);
pub struct ProcIdentity { pid: Pid, start: StartTicks }      // "this generation of this pid"
pub struct HostScope { boot: BootId, pid_ns: NsInode, time_ns: NsInode } // "where the numbers mean something"
```

`Option<ProcIdentity>` replaces the empty-string sentinel. One
`ProcStat::read(pid)` parses `/proc/<pid>/stat` once (comm, state, ppid,
starttime, utime, stime) and is the only parser. Today there are seven:
`lockfile::proc_starttime`, `lockfile::self_starttime`,
`lock_service::self_cpu_secs`, `shutdown::proc_ppid_and_starttime`,
`stray::read_proc`, `test_orphans::read_table`, `test_orphans::is_running`, plus
the guard's own copy. A `ProcTable::snapshot()` serves both reapers and
`shutdown::descendants`, which each walk `/proc` with their own row struct today.

### 1.2 "Verified from here" should be a type, not a pair of calls at each site

The question "do the recorded numbers mean anything in this namespace, and is the
pid still that process?" is answered by calling `shares_namespaces(info)` and then
`verify_identity(info.pid, ..)`, in that order, at:

- `stray::current_ownership`
- `commands.rs` `lock` printing (lines around the `holder_verified` local)
- `commands.rs::print_holder_status` (sidecar marker, `shares_namespaces` only)
- `commands.rs::cmd_kill` (hard path) and `kill_graceful` (numeric fallback)
- `commands.rs::hard_kill`'s comment "Callers have checked `shares_namespaces`",
  a precondition carried in prose

`hard_kill(info: &LockInfo)` therefore accepts an unverified `LockInfo`, and its
safety rests on that comment. Proposed: `LockInfo::in_this_namespace(&self) ->
Option<LocalHolder<'_>>` returns a value that exists only when the namespace
check passes. `LocalHolder::verify() -> Option<VerifiedProcess>` returns one that
exists only after the starttime check and that carries the pidfd (today's
`open_verified_pidfd` + `KillTarget`). `hard_kill(LocalHolder)` and
`kill_descendants(&VerifiedProcess)` then cannot be called on the wrong thing.
`verified_summary`, `verified_uptime` and `open_verified_pidfd` all become methods
on `VerifiedProcess`, and the "verify, act, re-verify" sandwich is written once.

### 1.3 Tokens: four 32-hex-char strings with no type

- the capability nonce (`hold::mint_nonce`)
- its published hash (`hold::auth_hash`, and the guard's copy)
- the acquisition id (`lock_service::mint_id` - literally `mint_nonce()`)
- the orphan run token (`test_orphans::register_in` - also `mint_nonce()`)

Validation is duplicated (`lock_service::valid_id` and `test_orphans::is_token`
are the same function). Nothing in the types stops an acquisition id being passed
where a capability is expected. That is the one mix-up the design explicitly
cares about: the lock_service header says the id is "deliberately NOT the
compilation capability, whose raw value must stay unpublished". Proposed:
`Nonce` (secret, never `Display`, no `Debug` contents), `AuthHash` (derived only
via `Nonce::auth_hash()`, the only thing written to the lock file), `AcqId`
(public), `RunToken` (public). They share one private `Hex128` with
`parse`/`mint`. Then `LockState::auth: String` becomes `Option<AuthHash>`, and
"empty means draining" turns into `None` rather than a string comparison the
guard also performs.

### 1.4 The lock record and the status reply are untyped wire formats with two parsers

The lock file is written by `rewrite_from_state` and read by
`parse_lock_contents` (`HashMap<&str,&str>`, first-occurrence-wins). It is also
read by the guard's `read_auth` (a line scan for `auth=`). The status reply is
written by `status_reply` and read back as `HolderStatus { fields: Vec<(String,
String)> }`, queried by string key from `commands.rs`. Proposed: one
`lock_record` module that owns the key names as constants or an enum, the
escaping (once), `encode(&Record) -> Vec<u8>` and `decode(&[u8]) ->
Option<Record>`. The status reply should be a typed `HolderStatus { identity:
Identity, snapshot: Snapshot, held: Duration, cpu: Option<Duration>, rss_kb:
Option<u64>, .. }` decoded by the client module, not by the CLI renderer. The
guard binary includes the same module (§3.1), so `auth=` is no longer a literal
in two crates' worth of code.

### 1.5 Outcomes that reach callers only as prose

- `lockfile::acquire` fails as `DevError::Lock(String)` for at least seven
  distinct, actionable reasons: nested-descendant refusal, inside-compilation
  refusal, drain timeout, flock infrastructure failure, metadata publish failure,
  no entropy, socket bind failure. The disk gate uses `Preflight(Vec<String>)`. A
  machine caller (`check --json`, an agent) cannot tell "your ancestor holds it,
  run it elsewhere" from "the kernel is out of lock records". Proposed: an
  `AcquireRefusal` enum carried in `DevError::Lock`, with the prose produced by
  `Display`. The drain timeout in particular says in its own text that it "is not
  a transient busy signal", which is exactly the kind of fact a retry loop needs
  as data.
- `stray::kill` returns `(usize, usize)`, a bare tuple of two counts with
  different meanings.
- `cargo_env::FAILED: Mutex<Option<(String, Option<&'static str>)>>`: the phase
  is a `&'static str` from `check_cmd::current_phase()`. Phases are surely an
  enum in `check_cmd`; this stores a string, so the stored value cannot be
  matched on.
- `PidfdRefusal` is a good example of the right shape, and `KillTarget`
  immediately flattens it back into a `String` via `map_err`.

### 1.6 Command identity as `&str` in `LockContext`

`LockContext { project: &str, command: &str, project_root: &str }`. The disk gate
branches on `command` (§0.5), and the lock file and status reply display it.
`project` is a string even though `Project` is an enum. `project_root` is a
`&str`, not a `&Path`, and `disk_gate` converts it back with `Path::new`.
Proposed: `LockContext { project: Option<Project>, command: CommandKind,
project_root: &Path }`, where `CommandKind` (or the clap `Command`
discriminant) answers `fn gated_by_disk(&self) -> bool` itself, so the exemption
lives with the command definition.

### 1.7 `toolchain`'s armed directory and `HoldEffects`

`toolchain::DISABLE_DIR` is a process-global `Mutex<Option<PathBuf>>` that
`with_worktree` disarms and re-arms around its acquire (`arm` returns the previous
value, and the caller must restore it). That is a parameter of the acquisition
smuggled through a global. `LockContext` (or an `AcquireOptions`) should carry
`disable_toolchain_in: Option<&Path>`, and the `--commit` path should pass its
worktree path explicitly. The global, the arm/disarm dance and
`restore_armed`'s second copy of the restore rule all disappear: the watchdog
backstop could read the path from the live `LockInner` via `current_hold()`.

---

## 2. Decisions made in more than one place

### 2.1 "Is the brokkr lock held right now?"

Sites: `lockfile::status` (exclusive probe), `rustc_guard::lock_is_held` (shared
probe), `lockfile::wait_for_flock` (implicitly). They **already disagree** in
side effect (§0.1). Owner: one `probe_held(path) -> Result<bool, io::Error>`
using `LOCK_SH|LOCK_NB`, in the shared protocol module (§3.1).

### 2.2 The capability hash and the lock-file key it lives under

Sites: `hold::auth_hash` and `rustc_guard::auth_hash` (byte-identical, "keep them
edited together"); `lockfile::rewrite_from_state` writes `auth=` and
`rustc_guard::read_auth` reads it. Env names are also duplicated as literals in
the guard: `BROKKR_HOLD_NONCE`, `BROKKR_COMPILE_LEASE`, `BROKKR_CARGO`
(`hold.rs` even has a comment block explaining why `BROKKR_CARGO` cannot be a
shared constant). The guard protocol version is `GUARD_PROTOCOL = 1` in
`guard.rs` and `"protocol=1 capabilities=..."` as a string literal in the guard.
None of these copies is tested pairwise. Owner: §3.1.

### 2.3 "Which processes may brokkr kill?"

Three reapers, three answers:

| site | exempts | identity check | signal |
|---|---|---|---|
| `stray::kill` | `me`, verified holder, holder's foreign namespace, their subtrees | starttime re-read | `kill(pid)` |
| `test_orphans::reap_in` | `me`, my ancestors, my descendants | starttime re-read | `kill(pid)` |
| `shutdown::kill_descendants` | nothing (root's subtree only) | starttime from walk | `kill(pid)` |
| `commands.rs::hard_kill` | n/a | boot id + starttime + pidfd | `pidfd_send_signal` / `kill(-pgid)` |

The verified-SIGKILL primitive is written three times (`stray`'s closure,
`test_orphans::sigkill_verified`, the loop in `kill_descendants`). The pidfd path
exists in `lockfile` but none of the reapers use it, even though it is strictly
better (no recycle window). The ancestor question disagrees (§0.3). Owner: a
`process::reap` module with `ProcTable`, one `Exemptions { me, ancestors_of_me:
bool, owners: Vec<Pid>, foreign_ns: Option<NsInode> }`, and one
`kill_verified(&ProcIdentity) -> Delivery` that opens a pidfd. The three reapers
then reduce to their selection rules (cargo-family name, env token, subtree),
which are the parts that genuinely differ.

### 2.4 "Is the recorded holder meaningful from here?"

See §1.2. The sites are listed there. They agree today, but the precondition on
`hard_kill` is prose only.

### 2.5 "What does every child brokkr starts carry, and in what order?"

The sequence `cargo_env::apply` -> `oom::protect_child` -> (`die_with_parent`) ->
(`test_orphans::stamp`) -> `hold::stamp` ("last, so nothing displaces it") is
written out at:

- `output.rs` three times (the captured, sidecar and passthrough runners)
- `test_runner.rs` once (with the two extra steps)
- `ratatoskr/saehrimnir.rs`, `nidhogg/server.rs` and `nidhogg/bench_tiles.rs`
  (`hold::stamp` only)
- `hold::cargo_config_overrides` (a union of the capability, rustc-info cache and
  orphan token, for nextest)

The rule "every child under a hold carries the capability" is described in
`hold::stamp`'s doc as covering "every child brokkr starts under a hold".
Whether the three project-specific sites also need `protect_child`,
`die_with_parent` or the orphan token is decided independently at each one. The
nextest file is a second encoding of the same env policy in TOML. Owner: a
`spawn::ChildPolicy` (or a `BrokkrCommand` builder wrapping `Command`) with
named profiles - `Cargo`, `TestProcess`, `Service`, `Plain` - that applies every
step in the one correct order. The same policy should be able to `render_env() ->
Vec<(&str, String)>`, so `cargo_config_overrides` serialises the policy rather
than re-listing it. Then `hold`, `test_orphans`, `oom` and the death-signal piece
of `shutdown` each export a step, and none of them is called directly from
call sites.

### 2.6 "Where does brokkr's per-user state live?"

`$HOME/.brokkr` is constructed independently in `lockfile::lock_path`,
`lock_service::socket_dir`, `hold::cargo_config_overrides`,
`test_orphans::runs_dir`, `worktree.rs`, `test_runner/harness_shim.rs`, and twice
in the guard (`brokkr_dir`, `lock_is_held`). They disagree on an empty `$HOME`:
`user_dirs::home()` treats empty as unset, while every one of these sites uses
`env::var("HOME")` or `var_os`, which accept `""` and resolve `.brokkr` against
the cwd. That is precisely the drift `user_dirs.rs`'s header says the module was
created to end ("an empty `XDG_DATA_HOME` was taken at its word"). An empty
`$HOME` in a sandbox would put the global lock at `./.brokkr/brokkr.lock`,
splitting the mutex per cwd. Owner: `user_dirs::brokkr_state_dir()` plus typed
accessors (`lock_file()`, `compile_lock()`, `socket_dir()`, `test_runs_dir()`,
`nextest_env()`, `worktrees()`), with the guard including the same module.

### 2.7 "Is this wrapper ours?"

`guard.rs::do_install` compares the configured wrapper to the exact resolved
path. `do_remove`, `stale_warning` and `status` use `ends_with(GUARD_BIN)`. A
wrapper at `/opt/x/brokkr-rustc-guard` from another install is "ours" for
removal but "not ours" for install, which then refuses to overwrite it. Owner:
one `fn classify_wrapper(&str) -> Wrapper { ThisInstall, OtherBrokkrGuard(Path),
Foreign(String), None }`.

### 2.8 "Has a stop been requested?"

`shutdown::is_shutdown_requested()` folds in `stop_committed()`.
`lockfile::wait_for_flock` checks both (`is_shutdown_requested() ||
stop_committed()`), which is redundant and suggests the caller does not trust the
fold. `current_hold` and `refuse_if_stopping` check `stop_committed()` alone.
`EXIT_CLEANUP` silently lifts `stop_committed` but not `SHUTDOWN_REQUESTED`.
Three atomics (`SHUTDOWN_REQUESTED`, `STOP_COMMITTED`, `EXIT_CLEANUP`) plus
`GUARD_ACTIVE` encode one state machine, roughly `Running -> Requested ->
Committed -> ExitCleanup`, and each query is a hand-written boolean formula over
them. Owner: a single `AtomicU8` state with named transitions and two named
queries (`admits_new_work()`, `should_unwind()`). The difference between a
guard-scoped request (reset at guard exit) and a sticky commit is real and should
stay, but as states, not as independently reset flags.

---

## 3. Structure

### 3.1 The guard binary's "self-contained" rule is the root of the worst duplication

`src/bin/rustc_guard.rs` justifies copying `auth_hash`, the `/proc` parse, the
paths, the env names, the lock-file key and the probe by "the package has no lib
target". That does not require copying. A `#[path = "../protocol.rs"] mod
protocol;` in the guard, with `protocol.rs` dependency-free apart from `libc` and
`xxhash`, gives both binaries one source with no lib target, no clap and no
startup cost. Put in it: the paths (§2.6), the env names, `AuthHash`/`Nonce`
(§1.3), `probe_held` (§2.1), `read_auth`/the lock-record decoder for the `auth`
field, `GUARD_PROTOCOL` and the handshake line format (printed by one side,
parsed by the other), and the compile-lease open/flock. The guard keeps
`classify`, `Decision` and `main`. "Keep them edited together" comments then
become compile errors.

### 3.2 `lockfile.rs` is five modules

At roughly 2000 lines it holds:

1. the flock mutex, re-entry registry and `LockGuard`
   (`acquire_at`, `current_hold`, `HELD`)
2. the acquisition protocol (`HoldEffects`, `drain_and_authorize`,
   `drain_compile_leases`, the ordering argument)
3. the lock-record wire format (`rewrite_from_state`, `parse_lock_contents`,
   escaping, double-read)
4. process identity and verification (`proc_starttime`, `verify_identity`,
   `pidfd_open`, `pidfd_send_signal`, `ns_inode`, `shares_namespaces`,
   `foreign_holder_ns`, `local_boot_id`)
5. presentation (`verified_summary`, `process_summary_unverified`,
   `format_duration`, `format_bytes*`, `same_agent_hint`, `nested_refusal`)

Item 4 is used by `stray`, `test_orphans`, `shutdown`, `nidhogg::server`,
`harness_shim`, `watchdog` and `commands.rs`. It is the most widely depended-on
thing in the scope and lives in a file named for something else, so every
consumer imports "lockfile" to ask about a process. Item 5 is CLI rendering for
`brokkr lock` and belongs beside `cmd_lock` (or in a `lock_cmd` module, together
with the 450 lines of `lock`/`kill` handling currently in
`main_parts/commands.rs`). Proposed layout:

```
src/process/      identity.rs (ProcStat, ProcIdentity, HostScope, pidfd),
                  table.rs (ProcTable snapshot), reap.rs (exemptions, kill_verified),
                  spawn.rs (ChildPolicy)
src/lock/         mod.rs (acquire, LockGuard, current_hold), hold_protocol.rs
                  (drain/authorize/HoldEffects), record.rs (wire format),
                  service.rs (today's lock_service), probe.rs
src/protocol.rs   shared with the guard binary (§3.1)
src/lock_cmd.rs   `brokkr lock` / `brokkr kill` rendering and kill orchestration
```

`stray`, `test_orphans` and `shutdown::descendants/kill_descendants` become
selection policies over `process::table` + `process::reap`.

### 3.3 `shutdown.rs` mixes three unrelated jobs

1. signal dispositions and the stop state machine (`SigtermGuard`,
   `STOP_COMMITTED`, `EXIT_CLEANUP`)
2. the test-group reaper table (`GroupReaper`, `REAP_PGIDS`) and
   `die_with_parent`, which are test-runner process containment
3. `/proc` tree walking and killing (`descendants`, `kill_descendants`), plus
   `isolate_process_group`

Item 3 belongs in `process::reap`. Item 2 is half a spawn policy
(`die_with_parent` is a pre-exec step, §2.5) and half signal-handler state. Only
item 1 is "shutdown". `GroupReaper` has to coordinate with `SigtermGuard`
through `GUARD_ACTIVE` because both install handlers for the same signals. One
module owning SIGINT/SIGTERM dispositions (a single handler that kills
registered groups, then either sets the flag or re-raises depending on whether a
guard window is open) would remove the handoff code
(`reinstall_reaper_if_needed`, the `saved` dispositions).

### 3.4 `HoldEffects` is a test seam that has become the protocol's spine

`HoldEffects` is a struct of six function pointers and two bools, explained as
"a seam, so unit tests can take real flocks". In practice it is also the only
place that lists everything a fresh hold does: drain, toolchain, capability,
reaps, guard warning, disk gate, service. That list is the protocol, and the
order matters (the disk gate before the drain, the toolchain after the drain and
before the capability). The order lives in `acquire_at`'s and
`drain_and_authorize`'s bodies, with long comments. `INERT` in the tests turns
every effect off together, so no test exercises any ordering between them.
Proposed: make the hold a typed state machine - `Flocked -> Gated -> Drained ->
Authorized -> Live` - where each transition consumes the previous state.
"Nothing fallible between publishing the capability and constructing
`LockInner`" is then a property of the `Authorized -> Live` transition, and the
order holds by construction. Keep effects injectable per transition, so a test
can enable one effect (just the disk gate, say) rather than all-or-nothing.

`host_after_fresh_hold` also calls `crate::check_cmd::activate_run_log()` from
`host_drain`. The lock reaches up into `check_cmd`, which is an upward edge. The
run log should be opened by the command before it acquires.

### 3.5 `cargo_env` is not process control

`cargo_env` is a build-input feature (a `[cargo_env]` config section evaluated
per cargo spawn). Its process-control content is the bounded `sh -c` evaluator
(deadline, drains, group kill), which duplicates what
`output::run_captured_with_env_and_deadline` already does: `guard.rs` uses that
for its probe. Owner: evaluation should go through the shared bounded runner, and
the module belongs with the spawn policy (§2.5), as the `Cargo` profile's
pre-spawn step. It also reaches into `check_cmd::current_phase()` to label its
failure. That is another upward edge, which an explicit phase passed in, or a
typed phase (§1.5), would remove.

`cargo_env::apply` decides "is this program cargo?" by
`file_name() == "cargo"`. That breaks for `cargo` invoked through rustup's
`+toolchain` proxy path variants, or a wrapper. If the spawn policy carried a
`Cargo` profile chosen by the caller, the name sniffing would not be needed.

### 3.6 "Files brokkr edits but does not own" have no home

`guard.rs` edits `$CARGO_HOME/config.toml`. `toolchain.rs` renames
`rust-toolchain.toml` in a foreign checkout. `download_toml.rs` edits
`brokkr.toml`. Only the last goes through `atomic_write`. A small
`foreign_file::edit(path, |doc| ..)` that resolves symlinks, preserves
permissions, writes atomically and refuses on parse failure would make §0.2
impossible to reintroduce.

### 3.7 Test layout

- `lockfile.rs`'s tests run with `INERT` and so cover re-entry, the record and
  the nested refusal, but not one cross-effect ordering. The ordering is the
  thing with a history of bugs (the capability published before fallible
  toolchain work, the `OnceLock` nonce). The typed transitions in §3.4 would let
  each ordering rule be a test.
- `hold::stamp_is_a_no_op_without_a_capability` asserts almost nothing (its own
  comment says so) because the capability is a process global. With a
  `ChildPolicy` value (§2.5) the stamp would be a pure function of
  `(policy, capability)`, and the test could be exact.
- The guard's `classify` tests are good and live where the code is. The guard's
  `decide()` has no test, because it reads env and `$HOME` directly. Inject a
  `ProtocolPaths` and an env lookup to make it testable. `decide` is the
  security-relevant half.
- `stray` tests assert the ancestor-kill behaviour (§0.3) as desired. Whichever
  way the decision goes, a test in `test_orphans` asserts the opposite for the
  analogous case, and no test spans both.

---

## 4. If I were rewriting it

1. Create `src/protocol.rs`, shared with the guard via `#[path]`: paths, env
   names, token types, auth hash, held-probe, lease open, handshake. Delete every
   "keep edited together" comment. This also fixes §0.1 and §2.6.
2. Create `src/process/`: `ProcStat`/`ProcTable`/`ProcIdentity`/`HostScope`,
   pidfd-based `kill_verified`, one exemptions type, and `ChildPolicy`. Rebuild
   `stray`, `test_orphans`, `kill_descendants` and `hard_kill` on it, and decide
   §0.3 once.
3. Split `lockfile.rs` into `lock/{mod,hold_protocol,record,service,probe}`, with
   typed transitions replacing `HoldEffects`-as-ordering, and a typed
   `AcquireRefusal`.
4. Move `lock`/`kill` presentation and orchestration out of
   `main_parts/commands.rs` into `lock_cmd.rs`, consuming typed `HolderStatus`
   and `VerifiedProcess`.
5. Fold `shutdown` down to one signal-disposition owner and a state enum.
6. Pass the toolchain-disable directory and the command kind through
   `LockContext`, deleting the `DISABLE_DIR` global and the disk gate's string
   exemption list.
