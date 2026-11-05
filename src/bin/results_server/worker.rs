//! The single worker that owns the store. Every request, reads included,
//! becomes a job on one bounded queue; the worker decodes its body, runs it
//! and encodes the answer, so no async thread runs unbounded synchronous
//! work and every write is serial.
//!
//! A job is `Queued`, then `Running` or `Withdrawn`, by compare-and-swap on
//! both sides: the worker claims a job before touching it, and a handler that
//! stops waiting, for whatever reason (its deadline, the connection's, the
//! connection closing), withdraws it. Winning the withdrawal proves the job
//! never ran; losing it to `Running` leaves its outcome unknown. A job whose
//! deadline passed before the worker reached it is withdrawn by the worker.
//! The admission a job carries (its in-flight slot and byte reservation)
//! drops with the job, in the worker, not when its handler gives up.
//!
//! The worker ends the process, through its `Gone` guard, whenever it stops:
//! a panic outside a job, the queue closing, or a job that left the
//! connection inside a transaction, which would poison every later request.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, oneshot};

use crate::store::Store;
use crate::store::wire::{self, Kind, Reply};

pub(crate) const QUEUED: u8 = 0;
pub(crate) const RUNNING: u8 = 1;
pub(crate) const WITHDRAWN: u8 = 2;

/// One request handed to the worker.
pub(crate) struct Job {
    pub(crate) body: hyper::body::Bytes,
    pub(crate) state: Arc<AtomicU8>,
    pub(crate) deadline: Instant,
    pub(crate) reply: oneshot::Sender<Reply>,
    /// The admission this job holds until the worker drops it.
    #[expect(
        dead_code,
        reason = "held for its drop: the admission ends with the job"
    )]
    pub(crate) held: Vec<OwnedSemaphorePermit>,
}

pub(crate) type Queue = SyncSender<Job>;

/// Fires when the worker stops, however it stops.
struct Gone(Option<oneshot::Sender<()>>);

impl Drop for Gone {
    fn drop(&mut self) {
        if let Some(gone) = self.0.take()
            && gone.send(()).is_err()
        {
            crate::log("the store worker stopped after the server did");
        }
    }
}

/// Start the worker on `store`. The receiver fires when it stops. A thread
/// that cannot be spawned fails startup, so nothing is served, and nothing
/// answers the identity route, without a worker behind it.
pub(crate) fn start(
    store: Store,
    capacity: usize,
) -> Result<(Queue, oneshot::Receiver<()>), String> {
    let (queue, jobs) = sync_channel(capacity);
    let (gone, worker_gone) = oneshot::channel();
    std::thread::Builder::new()
        .name("store-worker".into())
        .spawn(move || {
            let _gone = Gone(Some(gone));
            run(store, &jobs);
        })
        .map_err(|e| format!("cannot start the store worker: {e}"))?;
    Ok((queue, worker_gone))
}

fn run(mut store: Store, jobs: &Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        if !process(&mut store, job) {
            return;
        }
    }
}

/// Run one job. False when it left the connection inside a transaction:
/// its answer is not delivered, and the worker must stop.
pub(crate) fn process(store: &mut Store, job: Job) -> bool {
    if !claim(&job.state, job.deadline) {
        return true;
    }
    run_claimed(store, job)
}

/// Run a job the worker has claimed, deliver its answer unless the
/// connection was left inside a transaction, and drop the job with its
/// admission. False when the worker must stop.
pub(crate) fn run_claimed(store: &mut Store, job: Job) -> bool {
    let reply = catch_unwind(AssertUnwindSafe(|| wire::serve(store, &job.body)))
        .unwrap_or_else(|_| Reply::failure(Kind::Internal, "the request panicked in the store"));
    if store.in_transaction() {
        crate::log("a request left the store connection inside a transaction; stopping");
        return false;
    }
    if job.reply.send(reply).is_err() {
        // The handler stopped waiting after the job ran; its client hears
        // an uncertain outcome and a retry is answered from the store.
    }
    true
}

/// The worker's claim: `Queued` to `Running` while the deadline holds, else
/// the job is withdrawn. True when the job is the worker's to run.
pub(crate) fn claim(state: &AtomicU8, deadline: Instant) -> bool {
    if Instant::now() >= deadline {
        withdraw(state);
        return false;
    }
    state
        .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Withdraw a job that has not started; the job's state afterwards:
/// `WITHDRAWN` when it never ran (now or before), `RUNNING` when it had
/// started.
pub(crate) fn withdraw(state: &AtomicU8) -> u8 {
    match state.compare_exchange(QUEUED, WITHDRAWN, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => WITHDRAWN,
        Err(current) => current,
    }
}

/// A handler's hold on its queued job: dropping it, for any reason,
/// withdraws the job if it has not started.
pub(crate) struct Waiting(pub(crate) Arc<AtomicU8>);

impl Drop for Waiting {
    fn drop(&mut self) {
        withdraw(&self.0);
    }
}

/// The reply for a handler that got none: its wait expired (`expired`), or
/// the worker dropped the job unanswered.
pub(crate) fn unanswered(state: &AtomicU8, expired: bool) -> Reply {
    match withdraw(state) {
        WITHDRAWN => Reply::failure(
            Kind::WaitExpired { withdrawn: true },
            "the store did not reach the request in time; it was withdrawn unrun",
        ),
        _ if expired => Reply::failure(
            Kind::WaitExpired { withdrawn: false },
            "the store did not answer in time; the request had started and may still apply",
        ),
        _ => Reply::failure(
            Kind::Internal,
            "the store worker stopped while running the request; it may have applied",
        ),
    }
}
