//! Launch reservations protect script-bench outputs, scratch, spools and inputs
//! from conflicting launches and brokkr mutations. Disposable resources belong
//! to leases and are reclaimed only after their last holder exits.
//!
//! - **Liveness is a lease the child carries.** Before the spawn, brokkr
//!   creates a lock file in its per-user state and flocks it; the child
//!   inherits that open file description (`launch`), so the flock lives as
//!   long as any process holding it, whatever PID namespace it is in. A lease
//!   an independent open can lock exclusively has no holder left. The orphan
//!   reap stops survivors of a dead brokkr; it never authorizes a deletion.
//! - **Ownership is a durable record.** The record names every resource the
//!   launch declared - inputs read, outputs written, the private directory -
//!   and is synced before the private directory exists, so after any crash a
//!   private directory has a record naming it. Only private resources are ever
//!   removed by reclamation: one whose recorded identity matches, or, when a
//!   crash came between its creation and the record of its identity (and for
//!   a disposable file, which has none), the one at its 128-bit random name.
//!   A script-bench call's borrow, which holds nothing private, is the one
//!   lease left unsynced (`store`).
//! - **Every mutation is gated.** Deletions, renames and replacements that can
//!   touch such paths go through `guard`, which refuses when a reservation's
//!   resource overlaps an operand; legacy scratch cleanup also preserves
//!   trees holding a pbfhogg backup. Admission refuses a launch whose resources overlap a
//!   reservation when either side writes.
//!
//! One critical section (`serialize`: a process-wide mutex and an exclusive
//! flock on the store's serial lock) makes each scan with the admission,
//! release or gated mutation it decides atomic, within a process and across
//! brokkr processes, holders of the global lock or not.
//!
//! A script-bench iteration is the one tied shape (`iteration`): an owner
//! lease brokkr alone holds, naming the run's inputs and the iteration's
//! private directory, and per child a borrow naming its owner, which the
//! child inherits instead. A held borrow pins its owner, so a free owner is
//! kept while any process can still use what it owns
//! (`reference/bench-resources.md`).
//!
//! The layer covers reserved child launches,
//! leftover scratch and verify output, worktrees, and the
//! iterations of a script bench of any project, and the recursive removals
//! of every configured directory such an iteration can sit under (any
//! project's measured scratch trees, the ingest outputs the retired nidhogg
//! commands left).
//! brokkr's own fixed-name state files stay outside it, so a lease store problem never stops them; a broken
//! store does keep those configured trees, which is the safe direction.
//!
//! The guarantee is for cooperating brokkr processes of one user on one host
//! sharing one `HOME` on a local filesystem with kernel `flock` semantics, the
//! scope of the global lock and the compile lease; it rests on a child
//! and every process it starts keeping the borrow descriptor it inherited
//! while it can still use the iteration. Other users, other hosts, bind-mount
//! aliases of one directory and an NFS `HOME` are outside it.

mod guard;
mod iteration;
#[cfg(test)]
mod iteration_tests;
mod launch;
mod paths;
mod store;

pub(crate) use guard::{is_refusal, mutate, remove_dir_all, remove_file, remove_reserved_tree};
pub(crate) use iteration::{Call, Iteration, NameArea, Names, Preview, Spool, open_input};
pub(crate) use launch::{
    LaunchSpec, PRIVATE_DIR_PLACEHOLDER, ReadHold, hold_reads, inherit_in_child, take_armed,
};
pub(crate) use store::Access;

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Reclaim every free lease with its private directories; one line for each
/// lease kept, saying why (`brokkr clean` prints them).
pub(crate) fn reclaim() -> Vec<String> {
    let Some(store) = store() else {
        return Vec::new();
    };
    let _serial = match serialize() {
        Ok(serial) => serial,
        Err(e) => return vec![format!("launch leases were not reclaimed: {e}")],
    };
    let scan = store.scan();
    let root = store.root().display().to_string();
    // A kept lease is never dropped automatically; these say what to remove
    // by hand, once whatever it names is no longer in use.
    let by_hand =
        |id: &str| format!("once nothing uses its paths, remove every {root}/{id}.* file by hand");
    let mut lines: Vec<String> = scan
        .reservations
        .iter()
        .map(|r| match &r.retained {
            None => format!(
                "launch lease {} is still held by a running process; kept",
                r.id
            ),
            Some(why) => format!("launch lease {} kept: {why}; {}", r.id, by_hand(&r.id)),
        })
        .collect();
    lines.extend(scan.unknown.iter().map(|(id, why)| {
        format!(
            "launch lease {id} cannot be established ({why}); kept, and it blocks launches, \
             gated cleanup and script-bench iterations and their reclamation until resolved; {}",
            by_hand(id)
        )
    }));
    lines
}

static SERIAL: Mutex<()> = Mutex::new(());

/// The reservation critical section: the process-wide mutex, and an
/// exclusive flock on the store's serial lock, so a scan, an admission, a
/// release and the gated mutation that follows it are atomic across brokkr
/// processes too, including ones that do not hold the global brokkr lock (a
/// staged write in `brokkr guard --install`). Without a store - `$HOME`
/// unset - no lease can exist anywhere, and the mutex alone is taken.
pub(super) struct Serial {
    // Declared first: the flock is released before the mutex.
    _flock: Option<std::fs::File>,
    _mutex: MutexGuard<'static, ()>,
}

/// Enter the critical section. Failing to take the store's serial lock fails
/// the operation, the safe direction: nothing is admitted or mutated without
/// the exclusion the decision rests on.
fn serialize() -> std::io::Result<Serial> {
    let mutex = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let flock = match store() {
        Some(store) => Some(store.serial_lock().map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("the launch lease store's serial lock: {e}"),
            )
        })?),
        None => None,
    };
    Ok(Serial {
        _flock: flock,
        _mutex: mutex,
    })
}

/// The lease store: the per-user one, or in unit tests one scratch store for
/// the whole test process, so no test reads or reclaims real leases.
fn store() -> Option<store::Store> {
    #[cfg(test)]
    {
        static TEST_STORE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let root = TEST_STORE
            .get_or_init(|| crate::test_scratch::scratch("reservations", "process-store"));
        Some(store::Store::at(root.clone(), root.clone()))
    }
    #[cfg(not(test))]
    {
        store::Store::user()
    }
}
